//! 专注（zen）模式的持久化与快捷键翻转。
//!
//! 这里以前是「布局页」的全部接线：面板停靠边 / 折叠态 / 尺寸，以及它们**派生状态的
//! 重算**（同侧面板冲突消解、welcome 页在侧栏与窗格树之间迁移、旧停靠区几何量…）。
//! 那些设置项随旧外壳一起删除 —— 侧栏 / 欢迎侧栏 / 快捷面板 / SFTP 停靠区在新外壳里
//! 都不存在了（终端区 = rail + 页面 + 右侧工具面板的一张网格），它们既无渲染方也无
//! 停靠方；`sidebar_dock` / `welcome_as_sidebar` / `quick_panel_*` 等选项与设置页
//! 「布局」一并下线，见 CHANGELOG。
//!
//! 只剩 zen（专注）模式：`zen-mode` 仍被新外壳消费（收起 rail 与标签条），
//! 开关走 ⌘⌥Z / Ctrl+Alt+Z，值落盘。

use slint::ComponentHandle;

use super::{Store, persist};
use crate::ui::AppWindow;

/// 播种 + 注册持久化回调。
pub(crate) fn bind(window: &AppWindow, store: &Store) {
    // Zen (focus) mode: sidebar + tab strip hidden, persisted across launches.
    window.set_zen_mode(store.borrow().zen_mode());

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
}
