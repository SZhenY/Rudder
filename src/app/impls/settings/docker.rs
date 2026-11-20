//! Docker 页设置：连接 SSH 后自动探索容器。

use slint::ComponentHandle;

use super::{Store, persist};
use crate::config::fresh_config;
use crate::ui::AppWindow;

/// 播种 + 注册持久化回调。
pub(crate) fn bind(w: &AppWindow, store: &Store) {
    w.set_docker_auto_discover(store.borrow().docker_auto_discover());
    let weak = w.as_weak();
    let store = store.clone();
    w.on_set_docker_auto_discover(move |enabled| {
        persist(&store, |s| s.set_docker_auto_discover(enabled));
        if let Some(w) = weak.upgrade() {
            w.set_docker_auto_discover(enabled);
        }
    });
}

/// 「还原本页默认」：自动探索恢复为开。
pub(crate) fn reset(w: &AppWindow, store: &Store) {
    let default = fresh_config().layout.docker_auto_discover;
    persist(store, |s| s.set_docker_auto_discover(default));
    w.set_docker_auto_discover(default);
}
