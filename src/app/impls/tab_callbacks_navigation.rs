use super::*;
use crate::app::tab_callbacks::move_tab_in_place;

pub(crate) fn wire_tab_navigation(window: &AppWindow, ctx: &AppContext) {
    let AppContext {
        layout,
        content_size,
        tabs_model,
        panes_model,
        splitters_model,
        bufs,
        ..
    } = ctx;

    // Ctrl+Tab / Ctrl+Shift+Tab cycle within the currently focused pane (#294).
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        let bufs_cycle = bufs.clone();
        window.on_cycle_tab(move |reverse: bool| {
            let next = layout.borrow_mut().cycle_focused_tab(reverse);
            let Some(id) = next else {
                return;
            };
            if let Some(w) = weak.upgrade() {
                refresh_panes(
                    &w,
                    &layout,
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
                rebuild_tab_display(&w, &bufs_cycle, &id);
            }
        });
    }

    // Select a tab inside a pane: make it that pane's active tab and focus the
    // pane. refresh_panes propagates active-tab-id (→ sidebar refresh).
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        let bufs_tab_sel = bufs.clone();
        window.on_pane_tab_selected(move |pane_id: i32, id: SharedString| {
            let id = id.to_string();
            {
                let mut lay = layout.borrow_mut();
                lay.focused = pane_id as u64;
                if let Some(l) = lay.leaf_mut(pane_id as u64)
                    && l.tabs.iter().any(|t| t == &id)
                {
                    l.active = id.clone();
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
                // Tab just became visible — render any output ingested while it
                // was in the background (e.g. another session was unzipping).
                rebuild_tab_display(&w, &bufs_tab_sel, &id);
            }
        });
    }

    // Drag-to-reorder within a pane's strip: move the tab at `from` one slot in
    // `dir`. Only the pane's own tab order changes; content shows by active id.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_tab_reorder(move |pane_id: i32, from: i32, dir: i32| {
            let moved = {
                let mut lay = layout.borrow_mut();
                match lay.leaf_mut(pane_id as u64) {
                    Some(l) => move_tab_in_place(&mut l.tabs, from, dir),
                    None => false,
                }
            };
            if !moved {
                return;
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
