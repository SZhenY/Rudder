use super::*;

pub(super) fn wire_session_advanced(
    window: &AppWindow,
    ctx: &AppContext,
    scope: &SessionEditScope,
) {
    let AppContext {
        store,
        tabs_model,
        terminals_model,
        layout,
        content_size,
        panes_model,
        splitters_model,
        bufs,
        render_gates,
        sftp_last_cwd,
        tab_statuses,
        tab_titles,
        handles,
        sftp_handles,
        ..
    } = ctx;
    let edit_forwards = scope.edit_forwards.clone();
    let edit_triggers = scope.edit_triggers.clone();

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
        // These two are read (not moved) inside the closure, so they must be
        // owned clones: the bindings above are borrows of `ctx`, and capturing
        // one would require the context to outlive the closure.
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        let bufs = bufs.clone();
        let render_gates = render_gates.clone();
        let sftp_last_cwd = sftp_last_cwd.clone();
        let tab_statuses = tab_statuses.clone();
        let handles = handles.clone();
        let sftp_handles = sftp_handles.clone();
        let tab_titles = tab_titles.clone();
        // 会话启动上下文：一次装配、闭包内借用（闭包不能捕获 `&AppContext`）。
        let connect_ctx = ConnectCtx::from_app(window.as_weak(), ctx);
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
            let Some(window) = weak.upgrade() else {
                return;
            };
            let env = TabEnv {
                window: &window,
                store: &store,
                handles: &handles,
                tab_titles: &tab_titles,
                tab_statuses: &tab_statuses,
                tabs_model: &tabs_model,
                terminals_model: &terminals_model,
                bufs: &bufs,
                render_gates: &render_gates,
                sftp_handles: &sftp_handles,
                sftp_last_cwd: &sftp_last_cwd,
                layout: &layout,
                panes_model: &panes_model,
                splitters_model: &splitters_model,
                content_size: &content_size,
            };
            tab_ops::open_session_tab(&env, &connect_ctx, session);
        });
    }
}
