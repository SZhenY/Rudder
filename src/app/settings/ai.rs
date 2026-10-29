//! AI 设置页：MCP 服务的开关、监听面、令牌与工具权限。
//!
//! 状态（运行中 / 绑定失败 / 当前地址）由 `crate::mcp` 的 `McpStatus` 提供，
//! 这里用 1 Hz 定时器拉取 —— 服务在 tokio 线程上跑，UI 侧没有别的通知渠道。

use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use sysinfo::Networks;

use super::{Store, persist};
use crate::i18n::t;
use crate::mcp::McpConfigSnapshot;
use crate::ui::{AppWindow, McpIface, McpPermission};

/// 五个权限位。key 必须与 `crate::mcp::Permissions::allows` 的字符串一致。
const PERMISSIONS: &[(&str, &str, &str)] = &[
    ("session_open_close", "开关会话", "Open and close sessions"),
    ("terminal_input", "写入终端", "Type into terminals"),
    ("exec", "执行命令", "Run one-shot commands"),
    ("settings_write", "修改设置", "Change settings"),
    ("host_write", "增删主机", "Create and delete hosts"),
    ("file_transfer", "上传下载文件", "Upload and download files"),
];

pub(crate) fn bind(window: &AppWindow, store: &Store) {
    let ifaces = Rc::new(VecModel::<McpIface>::default());
    let permissions = Rc::new(VecModel::<McpPermission>::default());
    refresh(window, store, &ifaces, &permissions);

    {
        let store = store.clone();
        let weak = window.as_weak();
        let ifaces = ifaces.clone();
        let permissions = permissions.clone();
        window.on_set_mcp_enabled(move |enabled| {
            persist(&store, |s| s.set_mcp_enabled(enabled));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &permissions);
            }
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        let ifaces = ifaces.clone();
        let permissions = permissions.clone();
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
                refresh(&window, &store, &ifaces, &permissions);
            }
            true
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        let ifaces = ifaces.clone();
        let permissions = permissions.clone();
        window.on_set_mcp_lan(move |enabled| {
            persist(&store, |s| s.set_mcp_lan_enabled(enabled));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &permissions);
            }
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        let ifaces = ifaces.clone();
        let permissions = permissions.clone();
        window.on_toggle_mcp_iface(move |name| {
            persist(&store, |s| s.toggle_mcp_lan_iface(name.as_str()));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &permissions);
            }
        });
    }
    {
        let store = store.clone();
        let weak = window.as_weak();
        let ifaces = ifaces.clone();
        let permissions = permissions.clone();
        window.on_toggle_mcp_permission(move |key| {
            persist(&store, |s| toggle_permission(s, key.as_str()));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &permissions);
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
        let weak = window.as_weak();
        let ifaces = ifaces.clone();
        let permissions = permissions.clone();
        window.on_regenerate_mcp_token(move || {
            let token = crate::mcp::new_token();
            persist(&store, |s| s.set_mcp_token(token));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &permissions);
            }
        });
    }

    // 服务在 tokio 线程上启停，UI 只能轮询状态。
    let timer = slint::Timer::default();
    {
        let weak = window.as_weak();
        let store = store.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(1),
            move || {
                if let Some(window) = weak.upgrade() {
                    publish_status(&window, &store);
                }
            },
        );
    }
    // Timer 被 drop 就停 —— 与侧栏采样器同一套做法。
    Box::leak(Box::new(timer));
}

fn toggle_permission(store: &mut crate::config::ConfigStore, key: &str) {
    match key {
        "session_open_close" => {
            let value = !store.mcp_allow_session_open_close();
            store.set_mcp_allow_session_open_close(value);
        }
        "terminal_input" => {
            let value = !store.mcp_allow_terminal_input();
            store.set_mcp_allow_terminal_input(value);
        }
        "exec" => {
            let value = !store.mcp_allow_exec();
            store.set_mcp_allow_exec(value);
        }
        "settings_write" => {
            let value = !store.mcp_allow_settings_write();
            store.set_mcp_allow_settings_write(value);
        }
        "host_write" => {
            let value = !store.mcp_allow_host_write();
            store.set_mcp_allow_host_write(value);
        }
        "file_transfer" => {
            let value = !store.mcp_allow_file_transfer();
            store.set_mcp_allow_file_transfer(value);
        }
        _ => {}
    }
}

fn apply(store: &Store) {
    let snapshot = McpConfigSnapshot::from_store(&mut store.borrow_mut());
    crate::mcp::reload(snapshot);
}

fn refresh(
    window: &AppWindow,
    store: &Store,
    ifaces: &Rc<VecModel<McpIface>>,
    permissions: &Rc<VecModel<McpPermission>>,
) {
    let (enabled, port, lan, masked, plain) = {
        let store = store.borrow();
        (
            store.mcp_enabled(),
            store.mcp_port(),
            store.mcp_lan_enabled(),
            mask_token(store.mcp_token()),
            store.mcp_token().to_string(),
        )
    };
    window.set_mcp_enabled(enabled);
    window.set_mcp_port(SharedString::from(port.to_string()));
    window.set_mcp_lan_enabled(lan);
    window.set_mcp_token_masked(masked.into());
    window.set_mcp_token_plain(plain.into());

    let selected = {
        let store = store.borrow();
        [
            store.mcp_allow_session_open_close(),
            store.mcp_allow_terminal_input(),
            store.mcp_allow_exec(),
            store.mcp_allow_settings_write(),
            store.mcp_allow_host_write(),
            store.mcp_allow_file_transfer(),
        ]
    };
    permissions.set_vec(
        PERMISSIONS
            .iter()
            .enumerate()
            .map(|(index, (key, zh, en))| McpPermission {
                key: SharedString::from(*key),
                label: SharedString::from(t(zh, en)),
                enabled: selected[index],
            })
            .collect::<Vec<_>>(),
    );
    window.set_mcp_permissions(ModelRc::from(permissions.clone()));

    let selected_ifaces = store.borrow().mcp_lan_ifaces().to_vec();
    ifaces.set_vec(
        enumerate_ifaces()
            .into_iter()
            .map(|(name, addr)| McpIface {
                enabled: selected_ifaces.iter().any(|iface| iface == &name),
                name: SharedString::from(name),
                addr: SharedString::from(addr),
            })
            .collect::<Vec<_>>(),
    );
    window.set_mcp_ifaces(ModelRc::from(ifaces.clone()));

    publish_status(window, store);
}

fn publish_status(window: &AppWindow, store: &Store) {
    let enabled = store.borrow().mcp_enabled();
    let status = crate::mcp::status();
    let (text, error, urls) = if let Some(error) = status.error.clone() {
        (t("启动失败", "Failed to start").to_string(), error, Vec::new())
    } else if status.running {
        let urls = status.urls();
        let first = urls.first().cloned().unwrap_or_default();
        (
            format!("{} · {first}", t("运行中", "Running")),
            String::new(),
            urls,
        )
    } else if !enabled {
        (t("未启用", "Disabled").to_string(), String::new(), Vec::new())
    } else {
        (t("正在启动", "Starting").to_string(), String::new(), Vec::new())
    };
    window.set_mcp_status(text.into());
    window.set_mcp_status_error(error.into());
    window.set_mcp_urls(ModelRc::from(Rc::new(VecModel::from(
        urls.into_iter().map(SharedString::from).collect::<Vec<_>>(),
    ))));
    crate::app::settings_window::resync_if_open(window);
}

/// `rk_9f2a…7c31`：足够辨认是哪一枚，又不至于把令牌摊在屏幕上。
fn mask_token(token: &str) -> String {
    if token.is_empty() {
        return String::new();
    }
    if token.len() <= 12 {
        return "•".repeat(token.len());
    }
    format!("{}…{}", &token[..6], &token[token.len() - 4..])
}

/// 可监听的局域网网口（IPv4、非回环、非链路本地）。
fn enumerate_ifaces() -> Vec<(String, String)> {
    let networks = Networks::new_with_refreshed_list();
    let mut rows = Vec::new();
    for (name, data) in networks.iter() {
        for network in data.ip_networks() {
            if let std::net::IpAddr::V4(v4) = network.addr
                && !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_unspecified()
            {
                rows.push((name.clone(), network.addr.to_string()));
            }
        }
    }
    rows.sort();
    rows
}
