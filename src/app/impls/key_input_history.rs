use super::*;

pub(super) fn wire_key_history(window: &AppWindow, app: &AppContext) {
    let store = app.store.clone();

    // History search (#101): filter the dropdown by a case-insensitive substring.
    // The current query is shared so a delete from a filtered view re-filters.
    let hist_query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let hist_query = hist_query.clone();
        window.on_search_history(move |query: SharedString| {
            *hist_query.borrow_mut() = query.to_string();
            if let Some(w) = weak.upgrade() {
                let (view, preview) = history_view_models(&store_rc.borrow(), &query);
                w.set_history_view(view);
                w.set_history_preview(preview);
            }
        });
    }
    // Delete a history entry by its command text (#101) — index-free so it works
    // from the filtered dropdown view.
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let hist_query = hist_query.clone();
        window.on_delete_history_cmd(move |cmd: SharedString| {
            {
                let mut s = store_rc.borrow_mut();
                if let Some(idx) = s.command_history().iter().position(|c| c == cmd.as_str()) {
                    s.remove_command_history(idx);
                    s.save_logging();
                }
            }
            if let Some(w) = weak.upgrade() {
                let s = store_rc.borrow();
                w.set_command_history(history_model(&s));
                let (view, preview) = history_view_models(&s, &hist_query.borrow());
                w.set_history_view(view);
                w.set_history_preview(preview);
            }
        });
    }
    // Runtime-only collapse state for quick-command groups (#55) — like the
    // welcome session groups, this is not persisted across restarts. Starts with
    // every group collapsed (default-collapsed view).
}
