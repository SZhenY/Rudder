use super::*;

pub(super) fn wire_mcp_service(
    window: &AppWindow,
    store: &Store,
    ifaces: &Rc<VecModel<McpIface>>,
    clients: &Rc<VecModel<McpClientRow>>,
) {
    {
        let store = store.clone();
        let weak = window.as_weak();
        let ifaces = ifaces.clone();
        let clients = clients.clone();
        window.on_set_mcp_enabled(move |enabled| {
            persist(&store, |s| s.set_mcp_enabled(enabled));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &clients);
            }
        });
    }
    {
        // 执行通道：开 = run_command 走交互式 PTY（命令与输出在终端可见）。
        // 只改快照，不需要重建 listener。
        let store = store.clone();
        let weak = window.as_weak();
        let ifaces = ifaces.clone();
        let clients = clients.clone();
        window.on_set_mcp_pty_commands(move |enabled: bool| {
            persist(&store, |s| s.set_mcp_pty_commands(enabled));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &clients);
            }
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        let ifaces = ifaces.clone();
        let clients = clients.clone();
        window.on_set_mcp_client_trusted(move |client_id, trusted| {
            let id = client_id.to_string();
            if id.is_empty() {
                return;
            }
            persist(&store, |s| {
                if !s.set_mcp_client_trusted(&id, trusted) {
                    // 还没有记忆记录（从没授权过、或走的是 dev token 路径）：
                    // 先落一条空的，否则开关点完会静默回弹，用户以为点坏了。
                    s.upsert_mcp_client_grant(&id, Vec::new(), trusted);
                }
            });
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &clients);
            }
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        let ifaces = ifaces.clone();
        let clients = clients.clone();
        window.on_set_mcp_port(move |text| {
            let Ok(port) = text.trim().parse::<u16>() else {
                return false;
            };
            if port < 1024 {
                return false;
            }
            persist(&store, |s| s.set_mcp_port(port));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &clients);
            }
            true
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        let ifaces = ifaces.clone();
        let clients = clients.clone();
        window.on_set_mcp_lan(move |enabled| {
            // 服务关着时下级控件是禁用态；这里再挡一次，避免陈旧点击改到配置。
            if !store.borrow().mcp_enabled() {
                return;
            }
            persist(&store, |s| s.set_mcp_lan_enabled(enabled));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &clients);
            }
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        let ifaces = ifaces.clone();
        let clients = clients.clone();
        window.on_toggle_mcp_iface(move |name| {
            if !store.borrow().mcp_enabled() {
                return;
            }
            persist(&store, |s| s.toggle_mcp_lan_iface(name.as_str()));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &clients);
            }
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        let clients = clients.clone();
        window.on_toggle_mcp_client_permission(move |session_id, key, enabled| {
            if let Some(update) =
                crate::mcp::set_session_permission(session_id.as_str(), key.as_str(), enabled)
                && let Some(client_id) = update.client_id
            {
                let granted = update.granted;
                persist(&store, move |s| {
                    // 逐项改权限时保留原有的「永久信任」标记，别把它一并冲掉。
                    let trusted = s
                        .mcp_client_grants()
                        .iter()
                        .find(|grant| grant.id == client_id)
                        .map(|grant| grant.trusted)
                        .unwrap_or(false);
                    s.upsert_mcp_client_grant(&client_id, granted, trusted);
                });
                apply(&store);
            }
            if let Some(window) = weak.upgrade() {
                publish_status(&window, &store, &clients);
            }
        });
    }
    {
        // 展开/收起某个客户端的权限明细。改完立刻重算一次，不等 1 Hz 定时器。
        let store = store.clone();
        let weak = window.as_weak();
        let clients = clients.clone();
        window.on_toggle_mcp_client_expanded(move |session_id| {
            let id = session_id.to_string();
            EXPANDED_SESSIONS.with(|set| {
                let mut set = set.borrow_mut();
                if !set.remove(&id) {
                    set.insert(id);
                }
            });
            if let Some(window) = weak.upgrade() {
                publish_status(&window, &store, &clients);
            }
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        let clients = clients.clone();
        window.on_disconnect_mcp_client(move |session_id| {
            crate::mcp::disconnect_session(session_id.as_str());
            if let Some(window) = weak.upgrade() {
                publish_status(&window, &store, &clients);
            }
        });
    }
    {
        let store = store.clone();
        window.on_copy_mcp_token(move || {
            let token = store.borrow().mcp_token().to_string();
            if token.is_empty() {
                return;
            }
            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                let _ = clipboard.set_text(token);
            }
        });
    }
    {
        let store = store.clone();
        window.on_copy_mcp_json(move || {
            // 复制永远给明文：掩码只是用来遮屏，粘出去必须是能直接连的令牌。
            let text = client_config(&store, false);
            if text.is_empty() {
                return;
            }
            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                let _ = clipboard.set_text(text);
            }
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        let ifaces = ifaces.clone();
        let clients = clients.clone();
        window.on_regenerate_mcp_token(move || {
            // UI 已置灰，这里再挡一道：服务没开时换令牌没有任何意义，还会写盘。
            if !store.borrow().mcp_enabled() {
                return;
            }
            let token = crate::mcp::new_token();
            persist(&store, |s| s.set_mcp_token(token));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &clients);
            }
        });
    }
}
