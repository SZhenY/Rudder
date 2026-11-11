use super::*;

pub(super) fn wire_session_tabs(window: &AppWindow, ctx: &AppContext) {
    let AppContext {
        store,
        tabs_model,
        layout,
        content_size,
        panes_model,
        splitters_model,
        tab_statuses,
        tab_titles,
        ..
    } = ctx;
    let panes_model_rename = panes_model.clone();
    let splitters_model_rename = splitters_model.clone();

    {
        let weak = window.as_weak();
        let tab_statuses = tab_statuses.clone();
        let layout = layout.clone();
        window.on_tab_duplicate(move |tab_id: SharedString| {
            let tab_id = tab_id.to_string();
            // Recover a poisoned map: the entries are still valid and the
            // alternative under panic = "abort" is losing the whole client.
            let session_id = tab_statuses
                .lock()
                .unwrap_or_else(|e| e.into_inner())
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
                tracing::debug!(target: "rudder_rename", "rename-request: rejected tab_id={:?}", tab_id);
                return;
            }
            tracing::debug!(target: "rudder_rename", "rename-request: tab_id={:?}, current_title={:?}", tab_id, "...");
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
            tracing::debug!(target: "rudder_rename", "rename-tab invoked: tab_id={:?}, name={:?}", tab_id.as_str(), name.as_str());
            if let Some(w) = weak.upgrade() {
                w.set_tab_rename_open(false);
            }
            let tab_id = tab_id.to_string();
            let name = name.trim().to_string();
            let title = if name.is_empty() {
                tab_titles.borrow_mut().remove(&tab_id);
                let session_id = tab_statuses
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&tab_id)
                    .map(|s| s.session_id.clone())
                    .unwrap_or_default();
                // Two separate borrows: the builtin fallback must not run while
                // the store lookup's RefCell borrow is still alive.
                let saved = store.borrow().get(&session_id).map(|s| s.name.clone());
                let builtin = builtin_local_sessions(store.borrow().wsl_profiles())
                    .into_iter()
                    .find(|s| s.id == session_id)
                    .map(|s| s.name);
                resolve_tab_title(&name, saved, builtin)
            } else {
                tab_titles.borrow_mut().insert(tab_id.clone(), name.clone());
                resolve_tab_title(&name, None, None)
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
                    tracing::debug!(target: "rudder_rename", "rename-tab: matched row {} for tab_id={:?}, new_title={:?}", i, tab_id, title);
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
                    &layout,
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }
        });
    }
}
