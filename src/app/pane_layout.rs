//! Pane layout wiring: flattening the split tree into Slint models.

use std::cell::RefCell;
use std::rc::Rc;

use slint::{ComponentHandle as _, Model as _, ModelRc, VecModel};

use crate::config::ConfigStore;
use crate::ui::{AppWindow, PaneInfo, SplitterInfo, TabInfo, TerminalState};

use i_slint_backend_winit::WinitWindowAccessor;
use super::set_terminal_row;

/// Persist the current panel docking layout (both panels' edge + size) and the
/// window size, so the next launch restores the user's arrangement. Called on
/// every exit path (#dock).
pub(crate) fn save_layout(win: &AppWindow, store: &Rc<RefCell<ConfigStore>>) {
    let scale = win.window().scale_factor().max(0.01);
    let size = win.window().size();
    let w = size.width as f32 / scale;
    let h = size.height as f32 / scale;
    let mut s = store.borrow_mut();
    s.set_sidebar_width(win.get_sidebar_width());
    s.set_sidebar_height(win.get_sidebar_height());
    s.set_sidebar_dock(win.get_sidebar_dock().to_string());
    s.set_sidebar_collapsed(win.get_sidebar_collapsed());
    s.set_sftp_panel_width(win.get_sftp_panel_width());
    s.set_sftp_panel_height(win.get_sftp_panel_height());
    s.set_sftp_dock(win.get_sftp_dock().to_string());
    s.set_quick_panel_open(win.get_quick_panel_open());
    s.set_quick_panel_collapsed(win.get_quick_panel_collapsed());
    s.set_quick_panel_width(win.get_quick_panel_width());
    s.set_quick_panel_height(win.get_quick_panel_height());
    s.set_quick_panel_dock(win.get_quick_panel_dock().to_string());
    s.set_welcome_sidebar_width(win.get_welcome_sidebar_width());
    s.set_welcome_sidebar_dock(win.get_welcome_sidebar_dock().to_string());
    s.set_welcome_collapsed(win.get_welcome_collapsed());
    // A maximized size isn't a useful "preferred" size to restore to, so only
    // remember the windowed size. Ask the native window too, because the Slint
    // property can lag during startup/shutdown on frameless Windows (#234).
    let native_maximized = win
        .window()
        .with_winit_window(|ww| ww.is_maximized())
        .unwrap_or_else(|| win.get_window_maximized());
    let (saved_w, saved_h) = s.window_size();
    if !native_maximized && (saved_w <= 0.0 || saved_h <= 0.0) && w > 200.0 && h > 200.0 {
        // Normal resize events keep this cache current. Only fall back to the
        // close-time geometry for a first run where no valid resize was seen;
        // do not issue a new native resize while the window is shutting down.
        s.set_window_size(w, h);
    }
    let _ = s.save();
}
/// Re-flatten the split-tree `layout` for the current content-area size and push
/// the result into the AppWindow's `panes` / `splitters` models. Also keeps the
/// single global `active-tab-id` pointing at the focused pane's active tab — the
/// sidebar and key routing still read that one id.
/// True when two tab sub-models hold the same ids in the same order.
pub(crate) fn tabs_eq(a: &ModelRc<TabInfo>, b: &ModelRc<TabInfo>) -> bool {
    if a.row_count() != b.row_count() {
        return false;
    }
    (0..a.row_count()).all(|i| match (a.row_data(i), b.row_data(i)) {
        // Title must be compared too: refresh_panes reuses the *existing* tab
        // sub-model when this returns true, so an id-only comparison made a
        // rename invisible on the tab strip (the stale model still carried the
        // old title) — see #rename-invisible.
        (Some(x), Some(y)) => x.id == y.id && x.title == y.title,
        _ => false,
    })
}
/// Find the terminal row with `tab_id`, apply `mutator`, and write it back.
pub(crate) fn update_terminal_row(
    model: &VecModel<TerminalState>,
    tab_id: &str,
    mutator: impl FnOnce(&mut TerminalState),
) {
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i)
            && row.id.as_str() == tab_id
        {
            mutator(&mut row);
            model.set_row_data(i, row);
            return;
        }
    }
}
pub(crate) fn refresh_panes(
    window: &AppWindow,
    layout: &crate::layout::Layout,
    content: (f32, f32),
    tabs_model: &VecModel<TabInfo>,
    panes_model: &VecModel<PaneInfo>,
    splitters_model: &VecModel<SplitterInfo>,
) {
    let (cw, ch) = (content.0.max(1.0), content.1.max(1.0));
    let (panes, splits) = layout.flatten(0.0, 0.0, cw, ch);

    let pane_infos: Vec<PaneInfo> = panes
        .iter()
        .map(|p| {
            // Map this pane's tab ids to their TabInfo rows (skipping any not yet
            // in the model).
            let tabs: Vec<TabInfo> = p
                .tabs
                .iter()
                .filter_map(|tid| {
                    (0..tabs_model.row_count()).find_map(|i| {
                        let row = tabs_model.row_data(i)?;
                        (row.id.as_str() == tid.as_str()).then_some(row)
                    })
                })
                .collect();
            // Only the pane touching the top-right corner keeps room for the
            // floating toolbar icons (#122).
            let top_right = p.x + p.w >= cw - 0.5 && p.y <= 0.5;
            PaneInfo {
                id: p.id as i32,
                x: p.x,
                y: p.y,
                w: p.w,
                h: p.h,
                active_id: p.active.clone().into(),
                focused: p.focused,
                reserve_right: if top_right { 160.0 } else { 0.0 },
                tabs: ModelRc::from(Rc::new(VecModel::from(tabs))),
            }
        })
        .collect();

    // Update the models IN PLACE rather than replacing them, so the `for pane` /
    // `for sp` elements are reused: this keeps terminals from being recreated on
    // every refresh AND preserves the splitter's pointer-grab during a drag (a
    // fresh model would destroy the element mid-drag and drop the grab). When the
    // structure changes (split/close → different row count) a full rebuild is fine
    // since no drag is in flight.
    if panes_model.row_count() == pane_infos.len() {
        for (i, mut r) in pane_infos.into_iter().enumerate() {
            if let Some(old) = panes_model.row_data(i) {
                // Reuse the existing tab sub-model when the tabs are unchanged so a
                // geometry-only refresh doesn't churn the tab strips.
                if old.id == r.id && tabs_eq(&old.tabs, &r.tabs) {
                    r.tabs = old.tabs;
                }
            }
            panes_model.set_row_data(i, r);
        }
    } else {
        panes_model.set_vec(pane_infos);
    }

    let split_infos: Vec<SplitterInfo> = splits
        .iter()
        .map(|s| SplitterInfo {
            split_id: s.split_id as i32,
            x: s.x,
            y: s.y,
            w: s.w,
            h: s.h,
            vertical: s.vertical,
        })
        .collect();
    if splitters_model.row_count() == split_infos.len() {
        for (i, r) in split_infos.into_iter().enumerate() {
            splitters_model.set_row_data(i, r);
        }
    } else {
        splitters_model.set_vec(split_infos);
    }

    if let Some(fp) = panes.iter().find(|p| p.focused)
        && window.get_active_tab_id().as_str() != fp.active.as_str()
    {
        window.set_active_tab_id(fp.active.clone().into());
    }
}

// ---------------------------------------------------------------------------
// Tab callbacks
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// SFTP callbacks
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Raw keystroke forwarding and PTY resize
// ---------------------------------------------------------------------------

/// Font zoom shared by the session-wide and window-wide shortcut paths.
/// `window_wide` changes the shared `term_font_size` (and clears every
/// per-session override so the new size visibly applies to all); otherwise the
/// active session gets its own override. Direction: +1 larger, -1 smaller,
/// 0 reset. Zoom never persists globally — the Settings stepper owns that.
/// Changing the size re-measures the cell grid, which triggers the PTY resize.
pub(crate) fn zoom_term_font(
    w: &AppWindow,
    tab_id: &str,
    direction: i32,
    window_wide: bool,
    store: &Rc<RefCell<ConfigStore>>,
) {
    use slint::{Model as _, VecModel};
    let settings_size = store.borrow().font_size() as i32;
    if window_wide {
        let next = if direction == 0 {
            settings_size
        } else {
            w.get_term_font_size() as i32 + direction
        };
        w.set_term_font_size(next.clamp(8, 32) as f32);
        // Per-tab overrides would pin sessions at their old size and defeat
        // "zoom everything", so drop them.
        let terminals = w.get_terminals();
        let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
            return;
        };
        let overrides: Vec<String> = (0..tm.row_count())
            .filter_map(|i| {
                let row = tm.row_data(i)?;
                (row.font_size > 0.0).then(|| row.id.to_string())
            })
            .collect();
        for id in overrides {
            set_terminal_row(w, &id, |r| r.font_size = 0.0);
        }
        return;
    }
    if tab_id.is_empty() || tab_id == "welcome" {
        return;
    }
    let Some(current) = (0..w.get_terminals().row_count())
        .find_map(|i| {
            let row = w.get_terminals().row_data(i)?;
            (row.id.as_str() == tab_id).then_some(row.font_size)
        })
    else {
        return;
    };
    let base = if current > 0.0 {
        current as i32
    } else {
        w.get_term_font_size() as i32
    };
    let next = if direction == 0 {
        settings_size
    } else {
        base + direction
    };
    set_terminal_row(w, tab_id, |r| r.font_size = next.clamp(8, 32) as f32);
}
