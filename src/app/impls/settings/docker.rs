//! Docker 页设置：自动探索标签 + 命令执行权限。

use slint::ComponentHandle;

use super::{Store, persist};
use crate::config::fresh_config;
use crate::mcp::{McpConfigSnapshot, reload};
use crate::ui::AppWindow;

/// 开关变更后同步 MCP 的快照（Docker 工具与 UI 共用同一个 sudo 设置）。
fn reload_mcp(store: &Store) {
    let snapshot = McpConfigSnapshot::from_store(&mut store.borrow_mut());
    reload(snapshot);
}

/// 播种 + 注册持久化回调。
pub(crate) fn bind(w: &AppWindow, store: &Store) {
    w.set_docker_auto_discover(store.borrow().docker_auto_discover());
    w.set_docker_use_sudo(store.borrow().docker_use_sudo());

    let weak = w.as_weak();
    let store_auto = store.clone();
    w.on_set_docker_auto_discover(move |enabled| {
        persist(&store_auto, |s| s.set_docker_auto_discover(enabled));
        if let Some(w) = weak.upgrade() {
            w.set_docker_auto_discover(enabled);
            // 开关的下降沿要把已自动打开的标签收掉，上升沿要补开；统一刷新一次。
            w.invoke_refresh_docker();
        }
    });

    let weak = w.as_weak();
    let store_sudo = store.clone();
    w.on_set_docker_use_sudo(move |enabled| {
        persist(&store_sudo, |s| s.set_docker_use_sudo(enabled));
        reload_mcp(&store_sudo);
        if let Some(w) = weak.upgrade() {
            w.set_docker_use_sudo(enabled);
            // 立刻用新权限重跑一次，失败会弹权限提示。
            w.invoke_refresh_docker_now();
        }
    });
}

/// 「还原本页默认」：自动探索恢复为开，sudo 恢复为开。
pub(crate) fn reset(w: &AppWindow, store: &Store) {
    let fresh = fresh_config().layout;
    let auto = fresh.docker_auto_discover;
    let sudo = fresh.docker_use_sudo;
    persist(store, |s| {
        s.set_docker_auto_discover(auto);
        s.set_docker_use_sudo(sudo);
    });
    reload_mcp(store);
    w.set_docker_auto_discover(auto);
    w.set_docker_use_sudo(sudo);
    w.invoke_refresh_docker();
}
