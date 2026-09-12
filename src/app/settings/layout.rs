//! 布局页：面板停靠 / 折叠 / 尺寸 —— 以及它们**派生状态的重算**。
//!
//! 布局与其它页最大的不同：改一个 `sidebar_dock` 或 `welcome_as_sidebar`，还要
//! 重算停靠边冲突消解、各面板几何量、以及 welcome 页在侧栏与窗格树之间的位置。
//! 因此该页的核心不是"播种若干属性"，而是下面这个 `apply_layout_prefs` ——
//! **启动播种与「还原本页默认」共用它**，保证两条路径不会各写一半。

use super::Store;
use crate::ui::AppWindow;

/// Dispatch one `reset-page` request from the UI to the page that owns it.
/// Re-apply the persisted layout preferences to the live window: panel docking
/// sides and sizes, collapse states, and — critically — the conflict resolution
/// between panels that would otherwise share the same dock edge. Two expanded
/// panels docked on one side overlap, which is exactly the "broken layout"
/// reported after resetting the layout page.
///
/// Shared by startup seeding and by the layout page's "restore defaults".
pub(crate) fn apply_layout_prefs(w: &AppWindow, store: &Store) {
    let s = store.borrow();
    let collapse_sidebar = s.collapse_sidebar_default();
    let collapse_sftp = s.collapse_sftp_default();
    let sidebar_dock = s.sidebar_dock();
    let welcome_as_sidebar = s.welcome_as_sidebar();
    let quick_commands_as_sidebar = s.quick_commands_as_sidebar();
    let quick_panel_open = quick_commands_as_sidebar && s.quick_panel_open();
    let quick_panel_collapsed = s.quick_panel_collapsed();
    let quick_panel_dock = s.quick_panel_dock();
    let welcome_sidebar_dock = s.welcome_sidebar_dock();
    let mut sidebar_collapsed = s.sidebar_collapsed().unwrap_or(collapse_sidebar);
    let mut welcome_collapsed = s.welcome_collapsed().unwrap_or(false);
    if welcome_as_sidebar
        && sidebar_dock == welcome_sidebar_dock
        && !sidebar_collapsed
        && !welcome_collapsed
    {
        sidebar_collapsed = true;
    }
    if quick_panel_open && !quick_panel_collapsed {
        if sidebar_dock == quick_panel_dock {
            sidebar_collapsed = true;
        }
        if welcome_as_sidebar && welcome_sidebar_dock == quick_panel_dock {
            welcome_collapsed = true;
        }
    }
    w.set_collapse_sidebar_default(collapse_sidebar);
    w.set_collapse_sftp_default(collapse_sftp);
    // Restore the persisted panel docking layout (#dock).
    w.set_sidebar_width(s.sidebar_width());
    w.set_sidebar_height(s.sidebar_height());
    w.set_sidebar_dock(sidebar_dock.into());
    w.set_sftp_panel_width(s.sftp_panel_width());
    w.set_sftp_panel_height(s.sftp_panel_height());
    w.set_sftp_dock(s.sftp_dock().into());
    w.set_quick_commands_as_sidebar(quick_commands_as_sidebar);
    w.set_quick_panel_open(quick_panel_open);
    w.set_quick_panel_collapsed(quick_panel_collapsed);
    w.set_quick_panel_width(s.quick_panel_width());
    w.set_quick_panel_height(s.quick_panel_height());
    w.set_quick_panel_dock(quick_panel_dock.into());
    w.set_welcome_as_sidebar(welcome_as_sidebar);
    w.set_welcome_sidebar_width(s.welcome_sidebar_width());
    w.set_welcome_sidebar_dock(welcome_sidebar_dock.into());
    w.set_welcome_collapsed(welcome_collapsed);
    w.set_sidebar_collapsed(sidebar_collapsed);
    w.set_wallpaper_overlay(s.wallpaper_overlay());
    w.set_update_check_enabled(s.update_check_enabled()); // #184
    // 动画开关此前是 Slint-only 全局、从不持久化；现在与其它偏好一样由 config 驱动。
    w.set_animations_enabled(s.animations_enabled());
    if collapse_sftp {
        w.set_sftp_collapsed(true);
        w.set_sftp_saved_height(s.sftp_panel_height());
    }
}
