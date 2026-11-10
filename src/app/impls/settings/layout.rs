//! 布局页：面板尺寸 / 折叠态 / 命令栏显隐的播种与持久化。
//!
//! 面板停靠边已固定（SFTP 与快捷命令都在右侧），不再有冲突消解与窗格树迁移。
//! **启动播种与「还原本页默认」共用 `apply_layout_prefs`**，保证两条路径不会各写一半。

use slint::ComponentHandle;

use super::{Store, persist};
use crate::ui::{ AnimationSettings, AppWindow, Theme };

/// Re-apply the persisted layout preferences to the live window: panel sizes,
/// collapse states and command-bar visibility.
///
/// Shared by startup seeding and by the layout page's "restore defaults".
pub(crate) fn apply_layout_prefs(w: &AppWindow, store: &Store) {
    let s = store.borrow();
    let collapse_sidebar = s.collapse_sidebar_default();
    let collapse_sftp = s.collapse_sftp_default();
    let quick_commands_as_sidebar = s.quick_commands_as_sidebar();
    let quick_panel_open = quick_commands_as_sidebar && s.quick_panel_open();
    let quick_panel_collapsed = s.quick_panel_collapsed();
    let sidebar_collapsed = s.sidebar_collapsed().unwrap_or(collapse_sidebar);
    w.set_collapse_sidebar_default(collapse_sidebar);
    w.set_collapse_sftp_default(collapse_sftp);
    // 还原持久化的面板尺寸与折叠态。
    w.set_sftp_panel_width(s.sftp_panel_width());
    w.set_sftp_panel_height(s.sftp_panel_height());
    w.set_quick_commands_as_sidebar(quick_commands_as_sidebar);
    w.set_quick_panel_open(quick_panel_open);
    w.set_quick_panel_collapsed(quick_panel_collapsed);
    w.set_quick_panel_width(s.quick_panel_width());
    w.set_quick_panel_height(s.quick_panel_height());
    w.set_sidebar_collapsed(sidebar_collapsed);
    // 命令栏显隐：与工具栏图标共用同一个窗口属性 —— 必须从 config 推回 UI，
    // 否则「还原本页默认」只改了 config，界面（含工具栏图标）停在旧状态，
    // 表现为"点了还原没有任何反应"。
    w.set_show_cmd_bar(!s.cmd_bar_hidden());
    w.global::<Theme>().set_panel_alpha(s.wallpaper_overlay());
    w.set_update_check_enabled(s.update_check_enabled()); // #184
    // 动画开关此前是 Slint-only 全局、从不持久化；现在与其它偏好一样由 config 驱动。
    w.global::<AnimationSettings>().set_enabled(s.animations_enabled());
    // 时序验收用的全局倍速：`RUDDER_ANIM_SPEED=2` 全部动效慢一倍，默认 1。
    if let Ok(v) = std::env::var("RUDDER_ANIM_SPEED")
        && let Ok(f) = v.trim().parse::<f32>()
    {
        w.global::<AnimationSettings>().set_speed(f.clamp(0.1, 10.0));
    }
}

/// 开关快捷命令侧栏：持久化、同步主窗口、开合面板、刷新设置窗口。
/// 设置页和工具栏按钮都走这里，状态才不会各说各话。
pub(crate) fn apply_quick_commands_as_sidebar(w: &AppWindow, store: &Store, v: bool) {
    persist(store, |s| {
        s.set_quick_commands_as_sidebar(v);
        s.set_quick_panel_open(v);
    });
    w.set_quick_commands_as_sidebar(v);
    if v {
        // 打开时先以折叠条出现，用户再展开。
        w.set_quick_panel_collapsed(true);
        w.set_quick_panel_open(true);
    } else {
        w.set_quick_panel_open(false);
        w.set_quick_panel_collapsed(false);
    }
    crate::app::settings_window::resync_if_open(w);
}

/// 播种 + 注册持久化回调。
pub(crate) fn bind(window: &AppWindow, store: &Store) {
    {
        let weak = window.as_weak();
        let store = store.clone();
        // UI 传上来的是正向值（显示与否），config 存反向 —— 翻转只在这里与
        // apply_layout_prefs / settings_ui 播种三处。
        window.on_set_show_cmd_bar(move |shown| {
            persist(&store, |s| {
                s.set_cmd_bar_hidden(!shown);
            });
            if let Some(w) = weak.upgrade() {
                w.set_show_cmd_bar(shown);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_zen_mode(move |enabled| {
            persist(&store, |s| {
                s.set_zen_mode(enabled);
            });
            // 主窗口属性是设置窗的播种源，不回写会在重开时退回旧值。
            if let Some(w) = weak.upgrade() {
                w.set_zen_mode(enabled);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_toggle_zen_key(move || {
            if let Some(w) = weak.upgrade() {
                let next = !w.get_zen_mode();
                persist(&store, |s| {
                    s.set_zen_mode(next);
                });
                w.set_zen_mode(next);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_collapse_sidebar_default(move |v| {
            persist(&store, |s| {
                s.set_collapse_sidebar_default(v);
            });
            // 主窗口属性是设置窗每次打开时的播种源，不回写就会显示旧值。
            if let Some(w) = weak.upgrade() {
                w.set_collapse_sidebar_default(v);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_quick_commands_as_sidebar(move |v| {
            if let Some(w) = weak.upgrade() {
                apply_quick_commands_as_sidebar(&w, &store, v);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_sidebar_collapsed(move |v| {
            persist(&store, |s| {
                s.set_sidebar_collapsed(v);
            });
            if let Some(w) = weak.upgrade() {
                w.set_sidebar_collapsed(v);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_collapse_sftp_default(move |v| {
            persist(&store, |s| {
                s.set_collapse_sftp_default(v);
            });
            if let Some(w) = weak.upgrade() {
                w.set_collapse_sftp_default(v);
            }
        });
    }

}

/// 「还原本页默认」：替换布局域为出厂默认，再走与 `apply_layout_prefs` 相同的重算。
pub(crate) fn reset(w: &AppWindow, store: &Store) {
    let d = crate::config::ConfigFile::default();
    // 命令栏：与工具栏图标共用同一个窗口属性，还原必须立即回推 UI，
    // 否则只改了 config、界面停在旧状态（用户报告的"点还原没反应"）。
    // 本页属性名 show-cmd-bar（正向），config 仍是 hide_cmd_bar（反向）。
    w.set_show_cmd_bar(!d.layout.hide_cmd_bar);
    {
        persist(store, |s| {
            s.set_quick_commands_as_sidebar(d.layout.quick_commands_as_sidebar);
            s.set_collapse_sidebar_default(d.layout.collapse_sidebar_default);
            s.set_cmd_bar_hidden(d.layout.hide_cmd_bar);
            s.set_collapse_sftp_default(d.layout.collapse_sftp_default);
        });
    }
    // 布局是派生状态：尺寸/折叠态变了之后各面板几何量要重算。属性是双向绑定的，
    // 在回调里直接改会递归销毁子树（#323），所以延迟一帧执行。
    let weak = w.as_weak();
    let r_store = (*store).clone();
    slint::Timer::single_shot(std::time::Duration::ZERO, move || {
        let Some(w) = weak.upgrade() else { return };
        apply_layout_prefs(&w, &r_store);
    });
}
