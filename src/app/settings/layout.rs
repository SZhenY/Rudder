//! 布局页：面板停靠 / 折叠 / 尺寸 —— 以及它们**派生状态的重算**。
//!
//! 布局与其它页最大的不同：改一个 `sidebar_dock` 或 `welcome_as_sidebar`，还要
//! 重算停靠边冲突消解、各面板几何量、以及 welcome 页在侧栏与窗格树之间的位置。
//! 因此该页的核心不是"播种若干属性"，而是下面这个 `apply_layout_prefs` ——
//! **启动播种与「还原本页默认」共用它**，保证两条路径不会各写一半。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use slint::{ComponentHandle, VecModel};

use super::{Store, persist};
use crate::app::pane_layout::refresh_panes;
use crate::ssh::SessionHandle;
use crate::ui::{ AnimationSettings, AppWindow, PaneInfo, SplitterInfo, TabInfo, Theme };

/// Dispatch one `reset-page` request from the UI to the page that owns it.
/// Re-apply the persisted layout preferences to the live window: panel docking
/// sides and sizes, collapse states, and — critically — the conflict resolution
/// between panels that would otherwise share the same dock edge. Two expanded
/// panels docked on one side overlap, which is exactly the "broken layout"
/// reported after resetting the layout page.
///
/// Shared by startup seeding and by the layout page's "restore defaults".
/// 布局迁移需要的窗格与模型句柄（字段全是 `Rc`，克隆廉价）。
#[derive(Clone)]
pub(crate) struct PaneHandles {
    pub layout: Rc<RefCell<crate::layout::Layout>>,
    pub content_size: Rc<std::cell::Cell<(f32, f32)>>,
    pub tabs_model: Rc<VecModel<TabInfo>>,
    pub panes_model: Rc<VecModel<PaneInfo>>,
    pub splitters_model: Rc<VecModel<SplitterInfo>>,
}

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
    let (sidebar_collapsed, welcome_collapsed) = resolve_collapsed(CollapseInputs {
        sidebar_collapsed: s.sidebar_collapsed().unwrap_or(collapse_sidebar),
        welcome_collapsed: s.welcome_collapsed().unwrap_or(false),
        sidebar_dock: &sidebar_dock,
        welcome_sidebar_dock: &welcome_sidebar_dock,
        welcome_as_sidebar,
        quick_panel_open,
        quick_panel_collapsed,
        quick_panel_dock: &quick_panel_dock,
    });
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
    // 命令栏显隐：与工具栏图标共用同一个窗口属性 —— 必须从 config 推回 UI，
    // 否则「还原本页默认」只改了 config，界面（含工具栏图标）停在旧状态，
    // 表现为"点了还原没有任何反应"。
    w.set_show_cmd_bar(!s.cmd_bar_hidden());
    w.global::<Theme>().set_panel_alpha(s.wallpaper_overlay());
    w.set_update_check_enabled(s.update_check_enabled()); // #184
    // 动画开关此前是 Slint-only 全局、从不持久化；现在与其它偏好一样由 config 驱动。
    w.global::<AnimationSettings>().set_enabled(s.animations_enabled());
    if collapse_sftp {
        w.set_sftp_collapsed(true);
    }
}

/// 播种 + 注册持久化回调。
///
/// `handles` 用于侧栏折叠时暂停/恢复各会话的资源监控（上游 b17da25）；
/// `panes` 供 `welcome_as_sidebar` 在侧栏与窗格树之间迁移。
pub(crate) fn bind(
    window: &AppWindow,
    store: &Store,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    panes: &PaneHandles,
) {
    let layout = &panes.layout;
    let content_size = &panes.content_size;
    let tabs_model = &panes.tabs_model;
    let panes_model = &panes.panes_model;
    let splitters_model = &panes.splitters_model;

    {
        let store = store.clone();
        // UI 传上来的是正向值（显示与否），config 存反向 —— 翻转只在这里与
        // apply_layout_prefs / settings_ui 播种三处。
        window.on_set_show_cmd_bar(move |shown| {
            persist(&store, |s| {
                s.set_cmd_bar_hidden(!shown);
            });
        });
    }

    {
        let store = store.clone();
        window.on_set_zen_mode(move |enabled| {
            persist(&store, |s| {
                s.set_zen_mode(enabled);
            });
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
        let store = store.clone();
        window.on_set_collapse_sidebar_default(move |v| {
            persist(&store, |s| {
                s.set_collapse_sidebar_default(v);
            });
        });
    }

    {
        let store = store.clone();
        window.on_set_quick_commands_as_sidebar(move |v| {
            persist(&store, |s| {
                s.set_quick_commands_as_sidebar(v);
            });
        });
    }

    {
        let store = store.clone();
        window.on_persist_sidebar_width(move |w| {
            persist(&store, |s| {
                s.set_sidebar_width(w);
            });
        });
    }

    {
        let store = store.clone();
        let handles = handles.clone();
        window.on_set_sidebar_collapsed(move |v| {
            persist(&store, |s| {
                s.set_sidebar_collapsed(v);
            });
            // Pause resource monitoring for every live session while the
            // sidebar is hidden; resume when it comes back (upstream b17da25).
            for handle in handles.borrow().values() {
                handle.set_resource_monitoring(!v);
            }
        });
    }

    {
        let store = store.clone();
        window.on_persist_welcome_sidebar_width(move |w| {
            persist(&store, |s| {
                s.set_welcome_sidebar_width(w);
            });
        });
    }

    {
        let store = store.clone();
        window.on_persist_welcome_sidebar_dock(move |dock| {
            persist(&store, |s| {
                s.set_welcome_sidebar_dock(dock.to_string());
            });
        });
    }

    {
        let store = store.clone();
        window.on_set_welcome_collapsed(move |v| {
            persist(&store, |s| {
                s.set_welcome_collapsed(v);
            });
        });
    }

    {
        let store = store.clone();
        window.on_set_collapse_sftp_default(move |v| {
            persist(&store, |s| {
                s.set_collapse_sftp_default(v);
            });
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_set_welcome_as_sidebar(move |v| {
            // Persist first: saving config never touches the Slint tree, and
            // doing it synchronously means an immediate window close cannot
            // lose the preference. Only the property/layout transition has to
            // wait — it is two-way-bound through InterfacePanel and changing it
            // destroys/recreates the Welcome subtree that owns the Switch, so
            // defer the *entire* transition until this callback has returned;
            // deferring only refresh_panes still destroys the component tree
            // recursively on Windows (#323).
            {
                persist(&store, |s| {
                    s.set_welcome_as_sidebar(v);
                });
            }
            let weak = weak.clone();
            let layout = layout.clone();
            let content_size = content_size.clone();
            let tabs_model = tabs_model.clone();
            let panes_model = panes_model.clone();
            let splitters_model = splitters_model.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                if let Some(w) = weak.upgrade() {
                    w.set_welcome_as_sidebar(v);
                    {
                        let mut lay = layout.borrow_mut();
                        if v {
                            lay.remove_tab("welcome");
                        } else if lay.leaf_of_tab("welcome").is_none() {
                            lay.add_tab("welcome".into());
                        }
                    }
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
        });
    }
}

/// 「还原本页默认」：替换布局域为出厂默认，再走与 `apply_layout_prefs` 相同的重算。
/// 注：侧栏宽度是拖拽产生的交互状态（设置页无对应控件），不纳入还原。
pub(crate) fn reset(w: &AppWindow, store: &Store, panes: &PaneHandles) {
    let d = crate::config::ConfigFile::default();
    // 命令栏：与工具栏图标共用同一个窗口属性，还原必须立即回推 UI，
    // 否则只改了 config、界面停在旧状态（用户报告的"点还原没反应"）。
    // 本页属性名 show-cmd-bar（正向），config 仍是 hide_cmd_bar（反向）。
    w.set_show_cmd_bar(!d.layout.hide_cmd_bar);
    {
        persist(store, |s| {
            s.set_welcome_as_sidebar(d.layout.welcome_as_sidebar);
            s.set_quick_commands_as_sidebar(d.layout.quick_commands_as_sidebar);
            s.set_collapse_sidebar_default(d.layout.collapse_sidebar_default);
            s.set_cmd_bar_hidden(d.layout.hide_cmd_bar);
            s.set_collapse_sftp_default(d.layout.collapse_sftp_default);
            s.set_sidebar_dock(d.layout.sidebar_dock.clone());
        });
    }
    // 布局是派生状态：sidebar_dock / welcome_as_sidebar 变了之后，dock 冲突消解、
    // 各面板几何量、窗格树都必须重算 —— 只 set 属性会留下陈旧的窗格模型，表现为
    // 面板相互重叠（用户报告的布局错乱）。而 welcome_as_sidebar 又是双向绑定
    // 属性，在回调里直接改会递归销毁 Welcome 子树（#323），所以整个视觉迁移
    // 延迟一帧执行 —— 与运行时开关 on_set_welcome_as_sidebar 同一套路。
    let weak = w.as_weak();
    let r_store = (*store).clone();
    let r = panes.clone();
    slint::Timer::single_shot(std::time::Duration::ZERO, move || {
        let Some(w) = weak.upgrade() else { return };
        apply_layout_prefs(&w, &r_store);
        // welcome 页在侧栏与窗格树之间迁移，必须与属性保持一致。
        let welcome = r_store.borrow().welcome_as_sidebar();
        {
            let mut lay = r.layout.borrow_mut();
            if welcome {
                lay.remove_tab("welcome");
            } else if lay.leaf_of_tab("welcome").is_none() {
                lay.add_tab("welcome".into());
            }
        }
        refresh_panes(
            &w,
            &r.layout,
            r.content_size.get(),
            &r.tabs_model,
            &r.panes_model,
            &r.splitters_model,
        );
    });
}
/// 收起状态消解的输入 —— 就是 `apply_layout_prefs` 从 config 里读出来的那几项。
/// （打成一个结构体而不是 8 个参数：clippy 的 `too_many_arguments` 上限是 7。）
struct CollapseInputs<'a> {
    sidebar_collapsed: bool,
    welcome_collapsed: bool,
    sidebar_dock: &'a str,
    welcome_sidebar_dock: &'a str,
    welcome_as_sidebar: bool,
    quick_panel_open: bool,
    quick_panel_collapsed: bool,
    quick_panel_dock: &'a str,
}

/// 面板收起状态的消解：**同一侧只能有一个展开的面板**，否则两个叠在同一侧。
///
/// 抽出成纯函数：这是"哪些面板要收起来"的唯一真相源，四向停靠两两组合都靠它。
fn resolve_collapsed(
    inputs: CollapseInputs<'_>,
) -> (bool, bool) {
    let CollapseInputs {
        mut sidebar_collapsed,
        mut welcome_collapsed,
        sidebar_dock,
        welcome_sidebar_dock,
        welcome_as_sidebar,
        quick_panel_open,
        quick_panel_collapsed,
        quick_panel_dock,
    } = inputs;
    // 欢迎页当侧栏且与侧栏同侧 → 收侧栏（两个都展开会叠在一起）。
    if welcome_as_sidebar
        && sidebar_dock == welcome_sidebar_dock
        && !sidebar_collapsed
        && !welcome_collapsed
    {
        sidebar_collapsed = true;
    }
    // 快捷面板开着且没被手动收起 → 谁与它同侧就收谁。
    if quick_panel_open && !quick_panel_collapsed {
        if sidebar_dock == quick_panel_dock {
            sidebar_collapsed = true;
        }
        if welcome_as_sidebar && welcome_sidebar_dock == quick_panel_dock {
            welcome_collapsed = true;
        }
    }
    (sidebar_collapsed, welcome_collapsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认：两侧各一个面板、都展开、快捷面板关着。
    fn inputs<'a>(sidebar_dock: &'a str, welcome_dock: &'a str) -> CollapseInputs<'a> {
        CollapseInputs {
            sidebar_collapsed: false,
            welcome_collapsed: false,
            sidebar_dock,
            welcome_sidebar_dock: welcome_dock,
            welcome_as_sidebar: true,
            quick_panel_open: false,
            quick_panel_collapsed: false,
            quick_panel_dock: sidebar_dock,
        }
    }

    #[test]
    fn no_conflict_keeps_both_panels_expanded() {
        let (side, welcome) = resolve_collapsed(inputs("left", "right"));
        assert_eq!((side, welcome), (false, false), "不同侧 → 都保持展开");
    }

    /// 欢迎页当侧栏且**同侧** → 收侧栏（否则两个面板叠在同一侧）。
    #[test]
    fn welcome_on_the_same_side_collapses_the_sidebar() {
        let (side, welcome) = resolve_collapsed(inputs("left", "left"));
        assert_eq!((side, welcome), (true, false));
    }

    /// 快捷面板与侧栏同侧 → 收侧栏；与欢迎页同侧 → 收欢迎页；可以同时发生。
    #[test]
    fn quick_panel_collapses_whoever_shares_its_side() {
        let mut quick_left = inputs("left", "right");
        quick_left.quick_panel_open = true;
        let (side, welcome) = resolve_collapsed(quick_left);
        assert_eq!((side, welcome), (true, false), "只收同侧的侧栏");

        let mut quick_welcome = inputs("right", "left");
        quick_welcome.quick_panel_open = true;
        quick_welcome.quick_panel_dock = "left"; // 与欢迎页同侧，而不是与侧栏同侧
        let (side, welcome) = resolve_collapsed(quick_welcome);
        assert_eq!((side, welcome), (false, true), "只收同侧的欢迎页");

        let mut all_left = inputs("left", "left");
        all_left.quick_panel_open = true;
        let (side, welcome) = resolve_collapsed(all_left);
        assert_eq!((side, welcome), (true, true), "三个同侧 → 全收");
    }

    /// 已经手动收起的面板不再被"同侧冲突"改回来；快捷面板被手动收起时不参与消解。
    #[test]
    fn manual_collapse_and_closed_quick_panel_are_respected() {
        let mut already = inputs("left", "left");
        already.sidebar_collapsed = true;
        let (side, welcome) = resolve_collapsed(already);
        assert_eq!((side, welcome), (true, false), "已收的保持收起");

        let mut quick_collapsed = inputs("left", "right");
        quick_collapsed.quick_panel_open = true;
        quick_collapsed.quick_panel_collapsed = true;
        let (side, welcome) = resolve_collapsed(quick_collapsed);
        assert_eq!((side, welcome), (false, false), "快捷面板自己收着 → 不影响别人");
    }
}
