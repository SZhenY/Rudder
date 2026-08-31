//! Session-lifecycle callback wiring, extracted from `app.rs` (refactor plan
//! stage D step 2). Slint-thread-only: connects every `on_*` session event
//! (connect / disconnect / resize / rename / triggers …) to session state.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use tokio::runtime::Runtime;
use crate::resource::{LocalSnap, NetHist, TabStatus, TabStatuses};
use crate::session::ConnectCtx;
use crate::sftp::{SftpHandles, SftpLastCwd};
use crate::ssh::{SessionEvent, SessionHandle, test_session_auth};
use crate::app::render_tickets::RENDER_MIN_INTERVAL;
use crate::terminal::{
    CsiState, OutputHighlightPreset, RenderGates, TabRenderGate, TermBuffer, TermBuffers, compile_output_rules, new_term,
};
use crate::ui::*;
use crate::app::pane_layout::refresh_panes;
use crate::app::port_forward::{blank_forward_draft, forward_drafts, forward_model, validated_port_forwards};
use crate::app::session_models::{builtin_local_sessions, jump_candidates, parse_batch_import, refresh_session_rows_in_place, session_from_draft, session_groups_model, sync_sessions_to_model, sync_sessions_to_model_with_filter};
use crate::app::auth_dialogs::{enqueue_cred_prompt, enqueue_hostkey_prompt, enqueue_mfa_prompt};
use crate::app::session_runtime::{resolve_jump, start_session_in_tab};
use crate::app::session_trigger::{blank_trigger_draft, trigger_drafts, trigger_model, validated_triggers};
use crate::app::{
    split_proxy, sync_sessions_for_window,
    tab_title_len,
};
use crate::config::{AuthMethod, ConfigStore, Secret, Session, SessionKind, is_reserved_session_group};
use crate::i18n::t;

pub(crate) struct SessionWireCtx<'a> {
    pub(crate) window: &'a AppWindow,
    pub(crate) store: Rc<RefCell<ConfigStore>>,
    pub(crate) sessions_model: Rc<VecModel<SessionInfo>>,
    pub(crate) tabs_model: Rc<VecModel<TabInfo>>,
    pub(crate) terminals_model: Rc<VecModel<TerminalState>>,
    pub(crate) layout: Rc<RefCell<crate::layout::Layout>>,
    pub(crate) content_size: Rc<std::cell::Cell<(f32, f32)>>,
    pub(crate) panes_model: Rc<VecModel<PaneInfo>>,
    pub(crate) splitters_model: Rc<VecModel<SplitterInfo>>,
    pub(crate) handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    pub(crate) bufs: TermBuffers,
    pub(crate) render_gates: RenderGates,
    pub(crate) runtime: Arc<Runtime>,
    pub(crate) last_term_size: Arc<Mutex<(u32, u32)>>,
    pub(crate) sftp_handles: SftpHandles,
    pub(crate) sftp_last_cwd: SftpLastCwd,
    pub(crate) tab_statuses: TabStatuses,
    pub(crate) local_snap: LocalSnap,
    pub(crate) local_net_hist: NetHist,
    pub(crate) sftp_follow_cd: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) tab_titles: Rc<RefCell<HashMap<String, String>>>,
}

pub(crate) fn wire_session_callbacks(ctx: SessionWireCtx) {
    let SessionWireCtx {
        window,
        store,
        sessions_model,
        tabs_model,
        terminals_model,
        layout,
        content_size,
        panes_model,
        splitters_model,
        handles,
        bufs,
        render_gates,
        runtime,
        last_term_size,
        sftp_handles,
        sftp_last_cwd,
        tab_statuses,
        local_snap,
        local_net_hist,
        sftp_follow_cd,
        tab_titles,
    } = ctx;
    // on_connect_session moves panes_model/splitters_model into its closure;
    // the rename handler below needs its own handle, so clone up front.
    let panes_model_rename = panes_model.clone();
    let splitters_model_rename = splitters_model.clone();
    // Working set of port forwards (#56) for the session being created/edited.
    // The forward add/delete callbacks mutate it; saving reads it into
    // Session.forwards; opening the dialog (new/edit) resets it.
    let edit_forwards: Rc<RefCell<Vec<PortFwd>>> =
        Rc::new(RefCell::new(vec![blank_forward_draft()]));

    // Working set of expect/send login triggers (#212), same lifecycle as the
    // forward drafts above. Responses stay blank when editing a saved rule.
    let edit_triggers: Rc<RefCell<Vec<TriggerDraft>>> =
        Rc::new(RefCell::new(vec![blank_trigger_draft()]));

    // Rebuild the session list as the user edits the Quick Connect search.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_host_search_changed(move |query| {
            if let Some(window) = weak.upgrade() {
                let query = if query.trim().is_empty() {
                    SharedString::new()
                } else {
                    query
                };
                window.set_host_search_query(query.clone());
                sync_sessions_to_model_with_filter(
                    &store.borrow(),
                    &sessions_model,
                    query.as_str(),
                );
            }
        });
    }

    // New session -> open dialog with blank draft.
    let weak = window.as_weak();
    let ef_new = edit_forwards.clone();
    let et_new = edit_triggers.clone();
    let store_ng = store.clone();
    window.on_new_session_clicked(move || {
        if let Some(w) = weak.upgrade() {
            *ef_new.borrow_mut() = vec![blank_forward_draft()];
            *et_new.borrow_mut() = vec![blank_trigger_draft()];
            w.set_session_groups(session_groups_model(&store_ng.borrow()));
            w.set_dialog_forwards(forward_model(&ef_new.borrow()));
            w.set_dialog_triggers(trigger_model(&et_new.borrow()));
            let empty = Session::new_empty();
            let (jump_labels, jump_ids, jump_idx) =
                jump_candidates(&store_ng.borrow(), &empty.id, "");
            w.set_jump_choices(jump_labels);
            w.set_jump_ids(jump_ids);
            w.set_dialog_jump_index(jump_idx);
            w.set_dialog_id(empty.id.into());
            w.set_dialog_name("".into());
            w.set_dialog_host("".into());
            w.set_dialog_port("22".into());
            // No default username (#110): leaving it blank makes the connect-time
            // prompt ask for it, Xshell-style.
            w.set_dialog_user("".into());
            w.set_dialog_auth("password".into());
            w.set_dialog_password("".into());
            w.set_dialog_key_path("".into());
            w.set_dialog_key_inline("".into());
            w.set_dialog_key_inline_mode(false);
            w.set_dialog_test_status("".into());
            w.set_dialog_proxy_type("none".into());
            w.set_dialog_proxy_hostport("".into());
            w.set_dialog_group("".into());
            w.set_dialog_kind("ssh".into());
            w.set_dialog_serial_port("".into());
            w.set_dialog_baud("115200".into());
            w.set_dialog_data_bits("8".into());
            w.set_dialog_stop_bits("1".into());
            w.set_dialog_parity("none".into());
            w.set_dialog_flow("none".into());
            w.set_dialog_encoding("UTF-8".into());
            w.set_dialog_disable_shell_integration(false);
            w.set_dialog_note("".into());
            w.set_dialog_editing(false);
            w.set_dialog_open(true);
        }
    });

    // Import hosts from ~/.ssh/config -> add them as sessions (skipping dups).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_import_ssh_config(move || {
            let hosts = crate::ssh::ssh_config::parse_default();
            let mut added = 0usize;
            if hosts.is_empty() {
                if let Some(w) = weak.upgrade() {
                    w.set_ssh_import_hint(
                        t("未找到 ~/.ssh/config", "no ~/.ssh/config found").into(),
                    );
                }
                return;
            }
            {
                let mut s = store.borrow_mut();
                for h in hosts {
                    // Skip if a session already has this alias, or the same
                    // host + user pair.
                    let dup = s
                        .sessions()
                        .iter()
                        .any(|x| x.name == h.alias || (x.host == h.hostname && x.user == h.user));
                    if dup {
                        continue;
                    }
                    let auth = if h.identity_file.is_empty() {
                        AuthMethod::Password
                    } else {
                        AuthMethod::Key
                    };
                    s.upsert(Session {
                        name: h.alias,
                        host: h.hostname,
                        port: h.port,
                        user: if h.user.is_empty() {
                            "root".into()
                        } else {
                            h.user
                        },
                        auth,
                        private_key_path: h.identity_file,
                        ..Session::new_empty()
                    });
                    added += 1;
                }
                if added > 0 {
                    let _ = s.save();
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let hint = if added > 0 {
                    format!("{} {}", t("已导入", "imported"), added)
                } else {
                    t("没有新主机可导入", "no new hosts to import").to_string()
                };
                w.set_ssh_import_hint(hint.into());
            }
        });
    }

    // Export all sessions to a portable JSON file (issue #46). Passwords are
    // obfuscated with the built-in export key; host/user/port stay plaintext.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_export_sessions(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_file_name("rudder-connections.json")
                .add_filter("JSON", &["json"])
                .save_file()
            {
                let res = store.borrow().export_to(&path);
                if let Some(w) = weak.upgrade() {
                    let hint = match res {
                        Ok(n) => format!("{} {}", t("已导出连接", "exported"), n),
                        Err(e) => format!("{}: {}", t("导出失败", "export failed"), e),
                    };
                    w.set_ssh_import_hint(hint.into());
                }
            }
        });
    }

    // Batch-import connections from pasted text (#150). One per line:
    // `host|port|user|password|name` (trailing fields optional).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_batch_import_confirm(move |text: SharedString| {
            let parsed = parse_batch_import(text.as_str());
            let total = parsed.len();
            let mut added = 0usize;
            {
                let mut s = store.borrow_mut();
                for sess in parsed {
                    // Skip a host/user/port we already have.
                    let dup = s
                        .sessions()
                        .iter()
                        .any(|x| x.host == sess.host && x.user == sess.user && x.port == sess.port);
                    if dup {
                        continue;
                    }
                    s.upsert(sess);
                    added += 1;
                }
                if added > 0 {
                    let _ = s.save();
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let hint = if total == 0 {
                    t("没有可导入的连接", "nothing to import").to_string()
                } else if added > 0 {
                    format!("{} {}/{}", t("已导入", "imported"), added, total)
                } else {
                    t("没有新连接可导入(已存在)", "no new connections (all exist)").to_string()
                };
                w.set_ssh_import_hint(hint.into());
            }
        });
    }

    // Import sessions from a portable JSON file (issue #46).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_import_sessions(move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
            {
                let res = store.borrow_mut().import_from(&path);
                if let Some(w) = weak.upgrade() {
                    let hint = match res {
                        Ok((added, skipped)) => {
                            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
                            format!(
                                "{} {} / {} {}",
                                t("已导入", "imported"),
                                added,
                                t("跳过重复", "skipped"),
                                skipped
                            )
                        }
                        Err(e) => format!("{}: {}", t("导入失败", "import failed"), e),
                    };
                    w.set_ssh_import_hint(hint.into());
                }
            }
        });
    }

    // Edit -> open dialog prefilled.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let ef_edit = edit_forwards.clone();
        let et_edit = edit_triggers.clone();
        window.on_edit_session(move |id: SharedString| {
            let id = id.to_string();
            let store = store.borrow();
            let Some(session) = store.get(&id) else {
                return;
            };
            *ef_edit.borrow_mut() = forward_drafts(&session.forwards);
            if ef_edit.borrow().is_empty() {
                ef_edit.borrow_mut().push(blank_forward_draft());
            }
            // Saved responses are never echoed into the dialog (#10); the
            // validator keeps them when the response box stays blank.
            *et_edit.borrow_mut() = trigger_drafts(&session.triggers);
            if et_edit.borrow().is_empty() {
                et_edit.borrow_mut().push(blank_trigger_draft());
            }
            if let Some(w) = weak.upgrade() {
                w.set_session_groups(session_groups_model(&store));
                w.set_dialog_forwards(forward_model(&ef_edit.borrow()));
                w.set_dialog_triggers(trigger_model(&et_edit.borrow()));
                w.set_dialog_id(session.id.clone().into());
                w.set_dialog_name(session.name.clone().into());
                w.set_dialog_host(session.host.clone().into());
                w.set_dialog_port(session.port.to_string().into());
                w.set_dialog_user(session.user.clone().into());
                w.set_dialog_auth(session.auth.as_str().into());
                // Never echo the stored password back into the UI (issue #10) —
                // leave it blank; a blank field on save keeps the existing one.
                w.set_dialog_password("".into());
                w.set_dialog_key_path(session.private_key_path.clone().into());
                w.set_dialog_key_inline("".into());
                w.set_dialog_key_inline_mode(!session.private_key_inline.is_empty());
                w.set_dialog_test_status("".into());
                let (proxy_type, proxy_hostport) = split_proxy(&session.proxy);
                w.set_dialog_proxy_type(proxy_type.into());
                w.set_dialog_proxy_hostport(proxy_hostport.into());
                let (jump_labels, jump_ids, jump_idx) =
                    jump_candidates(&store, &session.id, &session.jump_session_id);
                w.set_jump_choices(jump_labels);
                w.set_jump_ids(jump_ids);
                w.set_dialog_jump_index(jump_idx);
                w.set_dialog_group(session.group.clone().into());
                w.set_dialog_kind(session.kind.as_str().into());
                w.set_dialog_serial_port(session.serial_port.clone().into());
                w.set_dialog_baud(session.baud_rate.to_string().into());
                w.set_dialog_data_bits(session.data_bits.to_string().into());
                w.set_dialog_stop_bits(session.stop_bits.to_string().into());
                w.set_dialog_parity(session.parity.clone().into());
                w.set_dialog_flow(session.flow_control.clone().into());
                w.set_dialog_encoding(session.encoding.clone().into());
                w.set_dialog_disable_shell_integration(session.disable_shell_integration);
                w.set_dialog_note(session.note.clone().into());
                w.set_dialog_editing(true);
                w.set_dialog_open(true);
            }
        });
    }

    // Remove session.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_remove_session(move |id: SharedString| {
            {
                let mut s = store.borrow_mut();
                s.remove(id.as_ref());
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                // Touch a property so the list re-renders reliably.
                let _ = w.get_sessions();
            }
        });
    }

    // Duplicate a session: clone it with a fresh id and a " (copy)" name (#41).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_duplicate_session(move |id: SharedString| {
            {
                let mut s = store.borrow_mut();
                if let Some(orig) = s.get(id.as_ref()).cloned() {
                    let mut copy = orig;
                    copy.id = uuid::Uuid::new_v4().to_string();
                    copy.name = format!("{} (copy)", copy.name);
                    copy.last_used = None;
                    s.upsert(copy);
                    if let Err(err) = s.save() {
                        tracing::warn!("failed to save config: {err:#}");
                    }
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Move a session to another group (#41).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_move_session(move |id: SharedString, group: SharedString| {
            {
                let mut s = store.borrow_mut();
                if let Some(orig) = s.get(id.as_ref()).cloned() {
                    let mut moved = orig;
                    // "default" is the display label for ungrouped → store empty.
                    moved.group = if group.as_str().eq_ignore_ascii_case("default") {
                        String::new()
                    } else if is_reserved_session_group(group.as_str().trim()) {
                        // `system` belongs exclusively to built-in local shells.
                        return;
                    } else {
                        group.to_string()
                    };
                    s.upsert(moved);
                    if let Err(err) = s.save() {
                        tracing::warn!("failed to save config: {err:#}");
                    }
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Collapse / expand a group in the welcome list (#41). Toggling flips the
    // `collapsed` flag on every row of that group in place — no full re-sync —
    // so the open/closed state stays put until the list is actually rebuilt.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_toggle_group(move |group: SharedString| {
            // While a search filter is active the list shows only matches with
            // groups force-expanded; toggling would corrupt that view (#264).
            if weak
                .upgrade()
                .map(|window| !window.get_host_search_query().trim().is_empty())
                .unwrap_or(false)
            {
                return;
            }
            use slint::Model as _;
            let target = group.to_string();
            let n = sessions_model.row_count();
            // New state = the opposite of the group's first row.
            let mut new_state = false;
            for i in 0..n {
                if let Some(row) = sessions_model.row_data(i)
                    && row.group.as_str() == target
                {
                    new_state = !row.collapsed;
                    break;
                }
            }
            for i in 0..n {
                if let Some(mut row) = sessions_model.row_data(i)
                    && row.group.as_str() == target
                {
                    row.collapsed = new_state;
                    sessions_model.set_row_data(i, row);
                }
            }
            {
                let mut store = store.borrow_mut();
                store.set_session_group_collapsed(&target, new_state);
                if let Err(err) = store.save() {
                    tracing::warn!("failed to save Quick Connect folder state: {err:#}");
                }
            }
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Drag-to-reorder a host card among its same-group siblings. The stored
    // Vec order is the display order (no alphabetical sort), so a swap plus
    // re-sync is all it takes. Reordering while the list is filtered would map
    // visible hops onto the wrong stored neighbours, so bail out then — the
    // Slint side already disables the gesture while searching.
    //
    // Per-hop updates mutate the model IN PLACE (set_row_data): a full set_vec
    // rebuild would recreate the rows and drop the dragging row's pointer grab,
    // ending the drag after one hop. Saving + broadcasting are deferred to
    // reorder-session-end (pointer release).
    let sessions_dirty = Rc::new(std::cell::Cell::new(false));
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let sessions_dirty = sessions_dirty.clone();
        window.on_reorder_session(move |id: SharedString, dir: i32| {
            if weak
                .upgrade()
                .map(|window| !window.get_host_search_query().trim().is_empty())
                .unwrap_or(false)
            {
                return;
            }
            let moved = {
                let mut s = store.borrow_mut();
                s.reorder_session(id.as_str(), dir as isize)
            };
            if moved {
                sessions_dirty.set(true);
                let query = weak
                    .upgrade()
                    .map(|w| w.get_host_search_query().to_string())
                    .unwrap_or_default();
                refresh_session_rows_in_place(&store.borrow(), &sessions_model, &query);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let sessions_dirty = sessions_dirty.clone();
        window.on_reorder_session_end(move || {
            if !sessions_dirty.replace(false) {
                return;
            }
            if let Err(err) = store.borrow_mut().save() {
                tracing::warn!("failed to save config: {err:#}");
            }
            sync_sessions_to_model(&store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Group create / rename (#41).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_submit_group(move |orig: SharedString, name: SharedString| {
            let trimmed = name.trim();
            let error = {
                let s = store.borrow();
                if trimmed.is_empty() {
                    Some(t("请输入分组名称", "Enter a group name"))
                } else if is_reserved_session_group(trimmed) {
                    Some(t("该名称为系统保留分组", "This group name is reserved"))
                } else if (orig.is_empty() || !trimmed.eq_ignore_ascii_case(orig.as_str()))
                    && s.session_group_exists(trimmed)
                {
                    Some(t("分组已存在", "Group already exists"))
                } else {
                    None
                }
            };
            if let Some(message) = error {
                return SharedString::from(message);
            }
            {
                let mut s = store.borrow_mut();
                if orig.is_empty() {
                    s.add_group(trimmed.to_string());
                } else {
                    s.rename_group(orig.as_str(), trimmed.to_string());
                }
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
            SharedString::new()
        });
    }
    // Group delete (#41) — UI only offers this on empty groups.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_delete_group(move |name: SharedString| {
            {
                let mut s = store.borrow_mut();
                s.remove_group(name.as_ref());
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Dialog submit -> persist + (optionally) connect.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let edit_forwards = edit_forwards.clone();
        let edit_triggers = edit_triggers.clone();
        window.on_session_dialog_submit(move |draft: SessionDraft| {
            let id = draft.id.to_string();
            let forwards = match validated_port_forwards(&edit_forwards.borrow()) {
                Ok(forwards) => forwards,
                Err(message) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                    return;
                }
            };
            // Triggers keep their saved response when the box is left blank,
            // so the previously stored secrets must be passed along (#212).
            let saved_responses: Vec<Secret> = store
                .borrow()
                .get(&id)
                .map(|s| s.triggers.iter().map(|t| t.response.clone()).collect())
                .unwrap_or_default();
            let triggers = match validated_triggers(&edit_triggers.borrow(), &saved_responses) {
                Ok(triggers) => triggers,
                Err(message) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                    return;
                }
            };
            // The edit dialog never echoes the real password (issue #10): a blank
            // field while editing means "keep the existing password" rather than
            // "clear it".  Only overwrite when the user actually typed something.
            let password = if draft.password.is_empty() {
                store
                    .borrow()
                    .get(&id)
                    .map(|s| s.password.clone())
                    .unwrap_or_default()
            } else {
                Secret::new(draft.password.to_string())
            };
            let private_key_inline = if draft.private_key_inline_mode {
                if draft.private_key_inline.is_empty() {
                    store
                        .borrow()
                        .get(&id)
                        .map(|s| s.private_key_inline.clone())
                        .unwrap_or_default()
                } else {
                    Secret::new(draft.private_key_inline.to_string())
                }
            } else {
                Secret::default()
            };
            let private_key_path = if draft.private_key_inline_mode {
                String::new()
            } else {
                draft.private_key_path.to_string().replace('\\', "/")
            };
            let kind = crate::config::SessionKind::from_str(draft.kind.as_ref());
            // Auto-name: serial → port label; otherwise user@host, or just the
            // host when no username was given (#110).
            let auto_name = match kind {
                crate::config::SessionKind::Serial => {
                    format!("{} @{}", draft.serial_port, draft.baud_rate)
                }
                _ if draft.user.trim().is_empty() => draft.host.to_string(),
                _ => format!("{}@{}", draft.user, draft.host),
            };
            // Telnet defaults to port 23, SSH to 22; serial ignores port.
            let default_port = if kind == crate::config::SessionKind::Telnet {
                23
            } else {
                22
            };
            let new_session = Session {
                id,
                name: if draft.name.is_empty() {
                    auto_name
                } else {
                    draft.name.to_string()
                },
                host: draft.host.to_string(),
                port: if draft.port <= 0 {
                    default_port
                } else {
                    draft.port as u16
                },
                user: draft.user.to_string(),
                auth: AuthMethod::from_str(draft.auth.as_ref()),
                password,
                // Store the key path with forward slashes uniformly.
                private_key_path,
                private_key_inline,
                proxy: draft.proxy.to_string(),
                last_used: None,
                group: draft.group.to_string(),
                kind,
                local_distribution: String::new(),
                local_working_dir: String::new(),
                serial_port: draft.serial_port.to_string(),
                baud_rate: if draft.baud_rate <= 0 {
                    115_200
                } else {
                    draft.baud_rate as u32
                },
                data_bits: draft.data_bits as u8,
                stop_bits: draft.stop_bits as u8,
                parity: draft.parity.to_string(),
                flow_control: draft.flow_control.to_string(),
                encoding: draft.encoding.to_string(),
                forwards,
                triggers,
                disable_shell_integration: draft.disable_shell_integration,
                note: draft.note.to_string(),
                jump_session_id: draft.jump_session_id.to_string(),
            };
            {
                let mut s = store.borrow_mut();
                s.upsert(new_session);
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                w.set_dialog_open(false);
            }
        });
    }

    // Test connection from the session dialog. SSH tests use the same handshake,
    // host-key verification, proxy/jump routing, and authentication as a real
    // terminal connection (#276). Telnet and serial retain reachability tests.
    {
        let weak = window.as_weak();
        let runtime = runtime.clone();
        let store = store.clone();
        let edit_forwards = edit_forwards.clone();
        window.on_session_dialog_test(move |draft: SessionDraft| {
            let kind = draft.kind.to_string();
            if kind == "serial" {
                let port_name = draft.serial_port.to_string();
                let baud = if draft.baud_rate <= 0 {
                    115_200
                } else {
                    draft.baud_rate as u32
                };
                let weak_done = weak.clone();
                runtime.spawn(async move {
                    let message = match tokio::task::spawn_blocking(move || {
                        serialport::new(&port_name, baud)
                            .timeout(std::time::Duration::from_millis(800))
                            .open()
                    })
                    .await
                    {
                        Ok(Ok(_)) => t("连接正常", "Connection OK").to_string(),
                        Ok(Err(e)) => format!("{}: {e}", t("连接失败", "Connection failed")),
                        Err(e) => format!("{}: {e}", t("连接失败", "Connection failed")),
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_done.upgrade() {
                            w.set_dialog_test_status(message.into());
                        }
                    });
                });
                return;
            }

            let existing = store.borrow().get(draft.id.as_str()).cloned();
            let forwards = match validated_port_forwards(&edit_forwards.borrow()) {
                Ok(forwards) => forwards,
                Err(message) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                    return;
                }
            };
            // Triggers are irrelevant to an authentication probe (and must not
            // block it with a validation error), so start from an empty set.
            let session = session_from_draft(&draft, existing.as_ref(), forwards, Vec::new());
            let weak_done = weak.clone();

            if kind == "ssh" {
                let jump = resolve_jump(&store, &session);
                let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
                runtime.spawn(async move {
                    let mut test = Box::pin(test_session_auth(session, jump, events_tx));
                    let result = loop {
                        tokio::select! {
                            result = &mut test => break result,
                            event = events_rx.recv() => {
                                let Some(event) = event else { continue };
                                if matches!(
                                    event,
                                    SessionEvent::HostKeyPrompt { .. }
                                        | SessionEvent::CredentialPrompt { .. }
                                        | SessionEvent::MfaPrompt { .. }
                                ) {
                                    let weak_prompt = weak_done.clone();
                                    let _ = slint::invoke_from_event_loop(move || {
                                        let Some(w) = weak_prompt.upgrade() else { return };
                                        match event {
                                            SessionEvent::HostKeyPrompt {
                                                host,
                                                port,
                                                key_type,
                                                fingerprint,
                                                changed,
                                                responder,
                                            } => enqueue_hostkey_prompt(
                                                &w,
                                                host,
                                                port,
                                                key_type,
                                                fingerprint,
                                                changed,
                                                responder,
                                            ),
                                            SessionEvent::CredentialPrompt {
                                                session_id,
                                                host,
                                                user,
                                                need_user,
                                                need_password,
                                                responder,
                                            } => enqueue_cred_prompt(
                                                &w,
                                                session_id,
                                                host,
                                                user,
                                                need_user,
                                                need_password,
                                                responder,
                                            ),
                                            SessionEvent::MfaPrompt {
                                                session_id,
                                                host,
                                                prompt,
                                                echo,
                                                responder,
                                            } => enqueue_mfa_prompt(
                                                &w,
                                                session_id,
                                                host,
                                                prompt,
                                                echo,
                                                responder,
                                            ),
                                            _ => {}
                                        }
                                    });
                                }
                            }
                        }
                    };
                    let message = match result {
                        Ok(()) => t("连接正常", "Connection OK").to_string(),
                        Err(e) => format!("{}: {e:#}", t("连接失败", "Connection failed")),
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_done.upgrade() {
                            w.set_dialog_test_status(message.into());
                        }
                    });
                });
                return;
            }

            let host = session.host;
            let port = session.port;
            runtime.spawn(async move {
                let target = format!("{host}:{port}");
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    tokio::net::TcpStream::connect((host.as_str(), port)),
                )
                .await;
                let message = match result {
                    Ok(Ok(_)) => t("连接正常", "Connection OK").to_string(),
                    Ok(Err(e)) => format!("{}: {e}", t("连接失败", "Connection failed")),
                    Err(_) => format!("{}: {target}", t("连接超时", "Connection timed out")),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_done.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                });
            });
        });
    }

    // Cancel dialog.
    {
        let weak = window.as_weak();
        window.on_session_dialog_cancel(move || {
            if let Some(w) = weak.upgrade() {
                w.set_dialog_open(false);
            }
        });
    }

    // Private-key file picker: pick the private key and store its path with
    // forward-slash separators (uniform across Windows/Linux; russh accepts them).
    {
        let weak = window.as_weak();
        window.on_session_dialog_pick_key(move || {
            let mut dialog = rfd::FileDialog::new()
                .set_title(t("选择私钥文件", "Choose private key file"));
            // OpenSSH's standard macOS key names (id_ed25519, id_rsa, …) have
            // no extension. A native macOS extension filter makes those files
            // visible but disabled, so leave the picker unfiltered there (#325).
            // Other platforms retain the narrower existing filter.
            #[cfg(not(target_os = "macos"))]
            {
                dialog = dialog.add_filter(
                    t("SSH 私钥", "SSH private keys"),
                    &["ppk", "pem", "key"],
                );
            }
            // Start in ~/.ssh if it exists.
            if let Some(home) = directories::UserDirs::new().map(|u| u.home_dir().join(".ssh"))
                && home.is_dir()
            {
                dialog = dialog.set_directory(home);
            }
            if let Some(file) = dialog.pick_file() {
                let path = file.to_string_lossy().replace('\\', "/");
                if let Some(w) = weak.upgrade() {
                    w.set_dialog_key_path(path.into());
                }
            }
        });
    }

    // Add another editable port-forward row (#56, #277).
    {
        let weak = window.as_weak();
        let ef = edit_forwards.clone();
        window.on_add_forward(move || {
            ef.borrow_mut().push(blank_forward_draft());
            if let Some(w) = weak.upgrade() {
                w.set_dialog_forwards(forward_model(&ef.borrow()));
            }
        });
    }
    // Keep each editable row in the Rust-side working set. Saving validates and
    // converts all non-empty rows together, so no separate "added" state exists.
    {
        let ef = edit_forwards.clone();
        window.on_update_forward(move |index: i32, forward: PortFwd| {
            let i = index as usize;
            let mut forwards = ef.borrow_mut();
            if i < forwards.len() {
                forwards[i] = forward;
            }
        });
    }
    // Delete a port forward by index (#56).
    {
        let weak = window.as_weak();
        let ef = edit_forwards.clone();
        window.on_delete_forward(move |index: i32| {
            let i = index as usize;
            {
                let mut v = ef.borrow_mut();
                if i < v.len() {
                    v.remove(i);
                }
                if v.is_empty() {
                    v.push(blank_forward_draft());
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_dialog_forwards(forward_model(&ef.borrow()));
            }
        });
    }

    // Add another expect/send trigger row (#212).
    {
        let weak = window.as_weak();
        let et = edit_triggers.clone();
        window.on_add_trigger(move || {
            et.borrow_mut().push(blank_trigger_draft());
            if let Some(w) = weak.upgrade() {
                w.set_dialog_triggers(trigger_model(&et.borrow()));
            }
        });
    }
    // Keep each row in the Rust-side working set, like the forward drafts.
    {
        let et = edit_triggers.clone();
        window.on_update_trigger(move |index: i32, trigger: TriggerDraft| {
            let i = index as usize;
            let mut triggers = et.borrow_mut();
            if i < triggers.len() {
                triggers[i] = trigger;
            }
        });
    }
    // Delete a trigger by index (#212). Always keep one blank row so the
    // dialog never collapses to an empty list.
    {
        let weak = window.as_weak();
        let et = edit_triggers.clone();
        window.on_delete_trigger(move |index: i32| {
            let i = index as usize;
            {
                let mut v = et.borrow_mut();
                if i < v.len() {
                    v.remove(i);
                }
                if v.is_empty() {
                    v.push(blank_trigger_draft());
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_dialog_triggers(trigger_model(&et.borrow()));
            }
        });
    }

    // Connect session -> open a new terminal tab.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let tabs_model = tabs_model.clone();
        let terminals_model = terminals_model.clone();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let handles = handles.clone();
        let bufs = bufs.clone();
        let render_gates = render_gates.clone();
        let runtime = runtime.clone();
        let last_term_size = last_term_size.clone();
        let sftp_handles = sftp_handles.clone();
        let sftp_last_cwd = sftp_last_cwd.clone();
        let tab_statuses = tab_statuses.clone();
        let local_snap = local_snap.clone();
        let local_net_hist = local_net_hist.clone();
        let sftp_follow_cd = sftp_follow_cd.clone();
        window.on_connect_session(move |id: SharedString| {
            let id = id.to_string();
            let session = if id.starts_with("system:") {
                match builtin_local_sessions(store.borrow().wsl_profiles())
                    .into_iter()
                    .find(|s| s.id == id)
                {
                    Some(s) => s,
                    None => return,
                }
            } else {
                match store.borrow().get(&id).cloned() {
                    Some(s) => s,
                    None => return,
                }
            };
            let tab_id = format!("term-{}", uuid::Uuid::new_v4());
            let tab_title = session.name.clone();

            // Connection label shown in the sidebar / status line, per transport.
            let conn_label = match session.kind {
                SessionKind::Ssh => format!("{}@{}", session.user, session.host),
                SessionKind::Serial => {
                    format!("{} @{}", session.serial_port, session.baud_rate)
                }
                SessionKind::Telnet => format!("telnet {}:{}", session.host, session.port),
                SessionKind::Local => format!("local {}", session.name),
            };
            // Serial / Telnet have no SFTP side-channel.
            let has_sftp = session.kind == SessionKind::Ssh;

            // Seed the per-tab status so the sidebar shows "连接中 host" the
            // moment this tab becomes active (the `changed active-tab-id`
            // handler fires refresh-sidebar right after set_active_tab_id below).
            tab_statuses.lock().unwrap_or_else(|e| e.into_inner()).insert(
                tab_id.clone(),
                TabStatus {
                    host: conn_label.clone(),
                    user: session.user.clone(),
                    session_id: id.clone(),
                    state: 0,
                    is_local: session.kind == SessionKind::Local,
                    ..Default::default()
                },
            );

            // Register tab + terminal state (SFTP fields start empty/loading).
            tabs_model.push(TabInfo {
                id: tab_id.clone().into(),
                title_len: tab_title_len(&tab_title),
                title: tab_title.into(),
                kind: "terminal".into(),
                connected: false,
            });
            // Each session keeps its own SFTP collapse state + sizes, seeded from
            // the global defaults (the "collapse SFTP by default" pref and the
            // persisted panel sizes) so they no longer bleed across panes (#v0.5).
            let (sftp_collapsed_default, sftp_h_default, sftp_w_default) = weak
                .upgrade()
                .map(|w| {
                    (
                        w.get_collapse_sftp_default(),
                        w.get_sftp_panel_height(),
                        w.get_sftp_panel_width(),
                    )
                })
                .unwrap_or((false, 220.0, 380.0));
            terminals_model.push(TerminalState {
                id: tab_id.clone().into(),
                status: t("连接中...", "Connecting...").into(),
                spans: ModelRc::from(std::rc::Rc::new(VecModel::<TermSpan>::default())),
                cursor_row: 0,
                mouse_tracked: false,
                cursor_col: 0,
                rows_used: 0,
                scroll_max: 0,
                scroll_offset: 0,
                is_alt_screen: false,
                find_matches: ModelRc::from(std::rc::Rc::new(VecModel::<TermMatch>::default())),
                selection: ModelRc::from(std::rc::Rc::new(VecModel::<TermMatch>::default())),
                font_size: 0.0,
                sftp_path: "/".into(),
                sftp_entries: ModelRc::from(std::rc::Rc::new(VecModel::<SftpEntry>::default())),
                sftp_status: if has_sftp {
                    t("SFTP 连接中...", "SFTP connecting...").into()
                } else {
                    t(
                        "此会话类型不支持 SFTP",
                        "SFTP not available for this session",
                    )
                    .into()
                },
                sftp_loading: has_sftp,
                sftp_tree_nodes: ModelRc::from(std::rc::Rc::new(
                    VecModel::<SftpTreeNode>::default(),
                )),
                sftp_selected_count: 0,
                sftp_sort_key: "".into(),
                sftp_sort_dir: 0,
                sftp_available: has_sftp,
                tunnels: ModelRc::from(std::rc::Rc::new(VecModel::<TunnelInfo>::default())),
                sftp_collapsed: !has_sftp || sftp_collapsed_default,
                sftp_panel_height: sftp_h_default,
                sftp_panel_width: sftp_w_default,
                sftp_saved_height: sftp_h_default,
            });
            // Create the alacritty-backed terminal for this tab (default
            // 24×80; resized on the first terminal-resize callback). The
            // scrollback depth comes from the settings value
            // (scrollback_lines, clamped to 100..=1_000_000 in config.rs).
            let is_dark_now = weak.upgrade().map(|w| w.get_dark_mode()).unwrap_or(true);
            let (output_highlight, custom_highlight_rules) = {
                let settings = store.borrow();
                (
                    OutputHighlightPreset::from_settings(
                        settings.output_highlight_enabled(),
                        settings.output_highlight_preset(),
                    ),
                    compile_output_rules(settings.output_highlight_rules()),
                )
            };
            let (t24, p24) = new_term(24, 80, store.borrow().scrollback_lines());
            bufs.lock().unwrap_or_else(|e| e.into_inner()).insert(
                tab_id.clone(),
                Arc::new(Mutex::new(TermBuffer {
                    term: t24,
                    processor: p24,
                    find_query: String::new(),
                    is_dark: is_dark_now,
                    output_highlight,
                    custom_highlight_rules,
                    prev: Vec::new(),
                    view_offset: 0,
                    displayed_text: Vec::new(),
                    csi_state: CsiState::Normal,
                    csi_pending: Vec::new(),
                    raw: std::collections::VecDeque::new(),
                    rendered: Vec::new(),
                    scroll_cache: HashMap::new(),
                    render_gen: 0,
                    overline_active: false,
                    overline_start: None,
                    overline_ranges: Vec::new(),
                    sgr_buf: Vec::new(),
                    interactive_echo_until: std::time::Instant::now(),
                    json_format_output: store.borrow().json_format_output(),
                    mouse_tracked: false,
                })),
            );
            render_gates.lock().unwrap_or_else(|e| e.into_inner()).insert(
                tab_id.clone(),
                Arc::new(TabRenderGate::new(RENDER_MIN_INTERVAL)),
            );
            // No followed-cwd yet: the first OSC 7 always triggers a follow.
            sftp_last_cwd.lock().unwrap_or_else(|e| e.into_inner()).remove(&tab_id);
            // Add the new tab to the focused pane and re-flatten (this also sets
            // active-tab-id to the new tab via refresh_panes).
            layout.borrow_mut().add_tab(tab_id.clone());
            if let Some(w) = weak.upgrade() {
                refresh_panes(
                    &w,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }

            // Spawn the shell (+ SFTP) workers and their event-pump threads.
            // Shared with in-place reconnect (#79) via start_session_in_tab.
            let ctx = ConnectCtx {
                weak: weak.clone(),
                runtime: runtime.clone(),
                handles: handles.clone(),
                sftp_handles: sftp_handles.clone(),
                sftp_last_cwd: sftp_last_cwd.clone(),
                bufs: bufs.clone(),
                render_gates: render_gates.clone(),
                tab_statuses: tab_statuses.clone(),
                local_snap: local_snap.clone(),
                local_net_hist: local_net_hist.clone(),
                last_term_size: last_term_size.clone(),
                sftp_follow_cd: sftp_follow_cd.clone(),
                store: store.clone(),
            };
            start_session_in_tab(&tab_id, session, &ctx);
        });
    }

    // Duplicate a tab's connection (#v0.5): open a fresh tab to the same saved
    // session, landing in the same pane as the source tab.
    {
        let weak = window.as_weak();
        let tab_statuses = tab_statuses.clone();
        let layout = layout.clone();
        window.on_tab_duplicate(move |tab_id: SharedString| {
            let tab_id = tab_id.to_string();
            let session_id = tab_statuses
                .lock()
                .unwrap()
                .get(&tab_id)
                .map(|s| s.session_id.clone())
                .unwrap_or_default();
            if session_id.is_empty() {
                return;
            }
            // Land the new tab in the same pane as the source. Read the pane id
            // into a local first so the immutable borrow is dropped before the
            // borrow_mut (else RefCell panics on the overlapping borrow).
            let pane = layout.borrow().leaf_of_tab(&tab_id);
            if let Some(pane) = pane {
                layout.borrow_mut().focused = pane;
            }
            if let Some(w) = weak.upgrade() {
                w.invoke_connect_session(session_id.into());
            }
        });
    }

    // Rename session (tab context menu): open the dialog pre-filled with the
    // tab's current title.
    {
        use slint::Model as _;
        let weak = window.as_weak();
        let tabs_model = tabs_model.clone();
        window.on_tab_rename_request(move |tab_id: SharedString| {
            let tab_id = tab_id.to_string();
            if tab_id.is_empty() || tab_id == "welcome" {
                tracing::info!(target: "rudder_rename", "rename-request: rejected tab_id={:?}", tab_id);
                return;
            }
            tracing::info!(target: "rudder_rename", "rename-request: tab_id={:?}, current_title={:?}", tab_id, "...");
            let title = (0..tabs_model.row_count())
                .find_map(|i| {
                    let row = tabs_model.row_data(i)?;
                    (row.id.as_str() == tab_id).then(|| row.title.to_string())
                })
                .unwrap_or_default();
            if let Some(w) = weak.upgrade() {
                w.set_tab_rename_id(tab_id.into());
                w.set_tab_rename_value(title.into());
                w.set_tab_rename_open(true);
            }
        });
    }

    // Apply the new display name. An empty name clears the override and
    // restores the saved session's name. Display-only: the config is untouched.
    {
        use slint::Model as _;
        let weak = window.as_weak();
        let tabs_model = tabs_model.clone();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let panes_model = panes_model_rename.clone();
        let splitters_model = splitters_model_rename.clone();
        let tab_statuses = tab_statuses.clone();
        let tab_titles = tab_titles.clone();
        let store = store.clone();
        window.on_rename_tab(move |tab_id: SharedString, name: SharedString| {
            tracing::info!(target: "rudder_rename", "rename-tab invoked: tab_id={:?}, name={:?}", tab_id.as_str(), name.as_str());
            if let Some(w) = weak.upgrade() {
                w.set_tab_rename_open(false);
            }
            let tab_id = tab_id.to_string();
            let name = name.trim().to_string();
            let title = if name.is_empty() {
                tab_titles.borrow_mut().remove(&tab_id);
                let session_id = tab_statuses
                    .lock()
                    .unwrap()
                    .get(&tab_id)
                    .map(|s| s.session_id.clone())
                    .unwrap_or_default();
                // Two separate borrows: the builtin fallback must not run while
                // the store lookup's RefCell borrow is still alive.
                let saved = store.borrow().get(&session_id).map(|s| s.name.clone());
                saved.or_else(|| {
                    builtin_local_sessions(store.borrow().wsl_profiles())
                        .into_iter()
                        .find(|s| s.id == session_id)
                        .map(|s| s.name)
                })
            } else {
                tab_titles.borrow_mut().insert(tab_id.clone(), name.clone());
                Some(name)
            };
            let Some(title) = title else {
                return;
            };
            let mut matched = false;
            for i in 0..tabs_model.row_count() {
                if let Some(mut row) = tabs_model.row_data(i)
                    && row.id.as_str() == tab_id
                {
                    row.title_len = tab_title_len(&title);
                    row.title = title.clone().into();
                    tabs_model.set_row_data(i, row);
                    matched = true;
                    tracing::info!(target: "rudder_rename", "rename-tab: matched row {} for tab_id={:?}, new_title={:?}", i, tab_id, title);
                    break;
                }
            }
            if !matched {
                let ids: Vec<String> = (0..tabs_model.row_count())
                    .filter_map(|i| tabs_model.row_data(i).map(|r| r.id.to_string()))
                    .collect();
                tracing::warn!(target: "rudder_rename", "rename-tab: NO MATCH for tab_id={:?}, available ids={:?}", tab_id, ids);
            }
            // Panes mirror the tab model, so re-derive them to refresh the
            // tab strip in every pane that shows this session.
            if let Some(w) = weak.upgrade() {
                refresh_panes(
                    &w,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }
        });
    }
}