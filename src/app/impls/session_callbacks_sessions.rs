use super::*;

pub(super) fn wire_session_list(window: &AppWindow, ctx: &AppContext, scope: &SessionEditScope) {
    let AppContext {
        store,
        sessions_model,
        ..
    } = ctx;
    let edit_forwards = scope.edit_forwards.clone();
    let edit_triggers = scope.edit_triggers.clone();

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
                w.set_dialog_proxy_user(session.proxy_user.clone().into());
                // 代理密码与登录密码同样不回显：留空保存时沿用已存的那份。
                w.set_dialog_proxy_password("".into());
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
                    let Some(group) = normalize_move_group(group.as_str()) else {
                        return;
                    };
                    let mut moved = orig;
                    moved.group = group;
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
}
