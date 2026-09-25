use super::*;

pub(super) fn wire_tab_callbacks(window: &AppWindow, ctx: &AppContext) {
    // Take the handles straight from the one shared context rather than a
    // hand-maintained copy of its field list — a field missing from that copy
    // is how the closed-tab `tab_statuses` leak arose.
    let AppContext {
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

    // Close a tab: tear down its session / buffers, drop it from the models, then
    // remove it from the split tree (which re-homes the pane's active tab and
    // collapses the pane if it becomes empty).
    {
        let weak = window.as_weak();
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
            if id == "welcome" {
                return;
            }
            // Drop any display-name override so a fresh connect starts clean.
            tab_titles.borrow_mut().remove(&id);
            if let Some(handle) = handles.borrow_mut().remove(&id) {
                handle.close();
            }
            // Closing a tab must never take the process down. Release builds
            // use panic = "abort", so a poisoned lock in this UI callback would
            // kill the whole client instead of just leaving one tab's
            // resources behind — skip the cleanup we cannot reach.
            if let Ok(mut h) = sftp_handles.lock()
                && let Some(sftp) = h.remove(&id)
            {
                sftp.close();
            }
            if let Ok(mut cwd) = sftp_last_cwd.lock() {
                cwd.remove(&id);
            }
            if let Ok(mut gates) = render_gates.lock()
                && let Some(gate) = gates.remove(&id)
            {
                gate.close();
            }
            if let Ok(mut b) = bufs.lock() {
                b.remove(&id);
            }
            // The status entry carries the tab's process list, network history
            // and disk rows. Nothing else in the app removes one, so leaving it
            // behind would retain all of that for the rest of the process run.
            if let Ok(mut s) = tab_statuses.lock() {
                s.remove(&id);
            }

            // Remove from tabs + terminals models.
            let mut idx = None;
            for i in 0..tabs_model.row_count() {
                if tabs_model
                    .row_data(i)
                    .map(|r| r.id.as_str() == id)
                    .unwrap_or(false)
                {
                    idx = Some(i);
                    break;
                }
            }
            if let Some(i) = idx {
                tabs_model.remove(i);
            }
            let mut tidx = None;
            for i in 0..terminals_model.row_count() {
                if terminals_model
                    .row_data(i)
                    .map(|r| r.id.as_str() == id)
                    .unwrap_or(false)
                {
                    tidx = Some(i);
                    break;
                }
            }
            if let Some(i) = tidx {
                terminals_model.remove(i);
            }

            layout.borrow_mut().remove_tab(&id);
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
            // In welcome-as-sidebar mode there is no welcome tab — the session list
            // lives in the left panel, so "+" has nothing to open.
            if weak
                .upgrade()
                .map(|w| w.get_welcome_as_sidebar())
                .unwrap_or(false)
            {
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

    // Click anywhere in a pane → focus it (drives which terminal the sidebar and
    // key routing follow). A single pane is always focused, so this is a no-op
    // until splits exist.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_focus(move |pane_id: i32| {
            {
                let mut lay = layout.borrow_mut();
                if lay.leaf(pane_id as u64).is_some() {
                    lay.focused = pane_id as u64;
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

    // Drag a splitter to re-balance the two panes it divides. `pos` is the new
    // boundary position in content coordinates along the split's axis; we look
    // the split's axis window up from a fresh flatten and convert it to a ratio.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_splitter_drag(move |split_id: i32, pos: f32, _vertical: bool| {
            {
                let mut lay = layout.borrow_mut();
                let (cw, ch) = content_size.get();
                let extent = {
                    let (_, splits) = lay.flatten(0.0, 0.0, cw.max(1.0), ch.max(1.0));
                    splits
                        .iter()
                        .find(|s| s.split_id == split_id as u64)
                        .map(|s| (s.axis_start, s.axis_len))
                };
                if let Some((start, len)) = extent {
                    lay.set_ratio(split_id as u64, start, len, pos);
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

    // Split a pane: peel `tab-id` out of pane `pane-id` into a new pane on the
    // given side ("left"/"right"/"up"/"down"). Needs >1 tab so the source pane
    // doesn't empty and immediately collapse back.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_split(
            move |pane_id: i32, tab_id: SharedString, dir: SharedString| {
                let tab_id = tab_id.to_string();
                {
                    let mut lay = layout.borrow_mut();
                    let can = lay
                        .leaf(pane_id as u64)
                        .map(|l| l.tabs.len() > 1 && l.tabs.iter().any(|t| t == &tab_id))
                        .unwrap_or(false);
                    if !can {
                        return;
                    }
                    let (d, before) = match dir.as_str() {
                        "left" => (crate::layout::Dir::Horizontal, true),
                        "right" => (crate::layout::Dir::Horizontal, false),
                        "up" => (crate::layout::Dir::Vertical, true),
                        _ => (crate::layout::Dir::Vertical, false), // "down"
                    };
                    lay.split(pane_id as u64, d, &tab_id, before);
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
            },
        );
    }

    // Merge a split pane back into another pane. The source pane's tabs are
    // appended to the first remaining pane, then the emptied source collapses.
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_pane_merge(move |pane_id: i32| {
            {
                let mut lay = layout.borrow_mut();
                lay.merge_leaf_into_other(pane_id as u64);
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
/// 把 `from` 位置的标签移动 `dir` 格（-1 左 / +1 右）；真的移动了才返回 true。
///
/// 抽成纯函数的原因：`from`/`dir` 直接来自 UI 拖拽事件（可能是任意整数），
/// 而 `Vec::remove` 一旦越界，release 构建（`panic = "abort"`）下是**整个客户端退出**。
/// 夹取与"到边界就什么都不做"必须能被单测钉住。
fn move_tab_in_place(tabs: &mut Vec<String>, from: i32, dir: i32) -> bool {
    let n = tabs.len() as i32;
    if n <= 1 {
        return false;
    }
    let from = from.clamp(0, n - 1);
    let to = (from + dir).clamp(0, n - 1);
    if from == to {
        return false;
    }
    let item = tabs.remove(from as usize);
    tabs.insert(to as usize, item);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tabs(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn move_tab_in_place_swaps_with_the_neighbour() {
        let mut t = tabs(&["a", "b", "c"]);
        assert!(move_tab_in_place(&mut t, 1, -1));
        assert_eq!(t, tabs(&["b", "a", "c"]), "左移一格");
        assert!(move_tab_in_place(&mut t, 0, 1));
        assert_eq!(t, tabs(&["a", "b", "c"]), "再右移回来");
    }

    /// 到边界就什么都不做并返回 false —— 调用方据此跳过重排 UI。
    #[test]
    fn move_tab_in_place_reports_no_op_at_the_edges() {
        let mut t = tabs(&["a", "b", "c"]);
        assert!(!move_tab_in_place(&mut t, 0, -1), "最左还往左");
        assert!(!move_tab_in_place(&mut t, 2, 1), "最右还往右");
        assert!(!move_tab_in_place(&mut t, 1, 0), "dir = 0");
        assert_eq!(t, tabs(&["a", "b", "c"]), "一次都不该动");
    }

    /// 单个 / 空列表直接拒绝；越界索引必须夹住（否则 `Vec::remove` panic -> 进程退出）。
    #[test]
    fn move_tab_in_place_clamps_hostile_indices() {
        let mut one = tabs(&["only"]);
        assert!(!move_tab_in_place(&mut one, 0, 1));
        assert!(!move_tab_in_place(&mut one, 99, -1), "越界也不能 panic");
        let mut empty: Vec<String> = Vec::new();
        assert!(!move_tab_in_place(&mut empty, 0, 1));

        let mut t = tabs(&["a", "b", "c"]);
        assert!(move_tab_in_place(&mut t, 99, -1), "越界的 from 夹到末尾后再移动");
        assert_eq!(t, tabs(&["a", "c", "b"]));
    }
}
