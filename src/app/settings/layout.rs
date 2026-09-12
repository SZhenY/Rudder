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
use crate::ui::{AppWindow, PaneInfo, SplitterInfo, TabInfo};

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
        window.on_set_cmd_bar_hidden(move |hidden| {
            persist(&store, |s| {
                s.set_cmd_bar_hidden(hidden);
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
                        &layout.borrow(),
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
    {
        persist(store, |s| {
            s.set_welcome_as_sidebar(d.layout.welcome_as_sidebar);
            s.set_quick_commands_as_sidebar(d.layout.quick_commands_as_sidebar);
            s.set_collapse_sidebar_default(d.layout.collapse_sidebar_default);
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
            &r.layout.borrow(),
            r.content_size.get(),
            &r.tabs_model,
            &r.panes_model,
            &r.splitters_model,
        );
    });
}
