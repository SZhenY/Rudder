use super::*;
use crate::app::tab_ops::TabEnv;

pub(crate) fn wire_tab_lifecycle(window: &AppWindow, ctx: &AppContext) {
    let AppContext {
        store,
        tabs_model,
        terminals_model,
        layout,
        content_size,
        panes_model,
        splitters_model,
        handles,
        bufs,
        render_gates,
        sftp_handles,
        sftp_last_cwd,
        tab_titles,
        tab_statuses,
        ..
    } = ctx;

    // Close a tab: tear down its session / buffers, drop it from the models, then
    // remove it from the split tree (which re-homes the pane's active tab and
    // collapses the pane if it becomes empty).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let terminals_model = terminals_model.clone();
        let handles = handles.clone();
        let bufs = bufs.clone();
        let render_gates = render_gates.clone();
        let sftp_handles = sftp_handles.clone();
        let sftp_last_cwd = sftp_last_cwd.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        let tab_titles = tab_titles.clone();
        let tab_statuses = tab_statuses.clone();
        window.on_pane_tab_closed(move |_pane_id: i32, id: SharedString| {
            let id = id.to_string();
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
            tab_ops::close_tab(&env, &id);
        });
    }

    // "+" in a pane's strip: focus the welcome page (there is a single welcome
    // tab; move focus to whichever pane owns it and make it active).
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_new_tab(move |pane_id: i32| {
            {
                let mut lay = layout.borrow_mut();
                if let Some(owner) = lay.leaf_of_tab("welcome") {
                    lay.focused = owner;
                    if let Some(l) = lay.leaf_mut(owner) {
                        l.active = "welcome".into();
                    }
                } else {
                    lay.focused = pane_id as u64;
                    lay.add_tab("welcome".into());
                }
            }
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

    // Navigation "Hosts": show the welcome tab in the pane that owns it. The
    // page state itself is set in Slint before this callback runs.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_nav_select(move |page: i32| {
            if page != 0 {
                return;
            }
            {
                let mut lay = layout.borrow_mut();
                if let Some(owner) = lay.leaf_of_tab("welcome") {
                    lay.focused = owner;
                    if let Some(l) = lay.leaf_mut(owner) {
                        l.active = "welcome".into();
                    }
                } else {
                    lay.add_tab("welcome".into());
                }
            }
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
