//! Beta 页：实验性开关。当前只有「激进内存回收」，默认关闭、重启生效。

use super::{Store, persist};
use crate::ui::AppWindow;
use slint::ComponentHandle;

pub(crate) fn bind(window: &AppWindow, store: &Store) {
    let store_cb = store.clone();
    let weak = window.as_weak();
    window.on_set_aggressive_memory_reclaim(move |v: bool| {
        persist(&store_cb, |s| s.set_aggressive_memory_reclaim(v));
        // 主窗口属性是设置窗的播种源，不回写会在重开时退回旧值。
        if let Some(w) = weak.upgrade() {
            w.set_aggressive_memory_reclaim(v);
        }
    });
}
