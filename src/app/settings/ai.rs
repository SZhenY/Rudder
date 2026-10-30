//! AI 设置页：MCP 服务的开关、监听面、令牌与工具权限。
//!
//! 状态（运行中 / 绑定失败 / 当前地址）由 `crate::mcp` 的 `McpStatus` 提供，
//! 这里用 1 Hz 定时器拉取 —— 服务在 tokio 线程上跑，UI 侧没有别的通知渠道。

use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, Model as _, ModelRc, SharedString, VecModel};
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
            // 服务关着时下级控件是禁用态；这里再挡一次，避免陈旧点击改到配置。
            if !store.borrow().mcp_enabled() {
                return;
            }
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
            if !store.borrow().mcp_enabled() {
                return;
            }
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
            // 同上：关闭态下不允许改授权位。
            if !store.borrow().mcp_enabled() {
                return;
            }
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
    // 服务关着时，"监听范围"整段是禁用态：**显示**为关，但配置里原来的值保留，
    // 重新打开服务后原样恢复（不是把用户的授权抹掉）。
    window.set_mcp_lan_enabled(enabled && lan);
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
    // 服务关闭时把权限位**显示**成全关（配置不动）。这样拨掉总开关时，下面六个
    // 开关会各自从"开"过渡到"关"，而不是停在开的状态却不可点。
    let selected = if enabled {
        selected
    } else {
        [false; 6]
    };
    let rows = PERMISSIONS
        .iter()
        .enumerate()
        .map(|(index, (key, zh, en))| McpPermission {
            key: SharedString::from(*key),
            label: SharedString::from(t(zh, en)),
            enabled: selected[index],
        })
        .collect::<Vec<_>>();
    // 逐行 `set_row_data` 而不是整表 `set_vec`：整表替换会让 Slint 重建每一个
    // 开关元素，重建出来的元素没有"从旧值切到新值"这一步，动画自然不播。
    if permissions.row_count() == rows.len() {
        for (index, row) in rows.into_iter().enumerate() {
            permissions.set_row_data(index, row);
        }
    } else {
        permissions.set_vec(rows);
    }
    window.set_mcp_permissions(ModelRc::from(permissions.clone()));

    let selected_ifaces = store.borrow().mcp_lan_ifaces().to_vec();
    let iface_rows = enumerate_ifaces()
        .into_iter()
        .map(|(name, addr)| McpIface {
            enabled: enabled && selected_ifaces.iter().any(|iface| iface == &name),
            name: SharedString::from(name),
            addr: SharedString::from(addr),
        })
        .collect::<Vec<_>>();
    // 同上：行数没变就逐行更新，别整表替换（否则开关也是"瞬变无动画"）。
    if ifaces.row_count() == iface_rows.len() {
        for (index, row) in iface_rows.into_iter().enumerate() {
            ifaces.set_row_data(index, row);
        }
    } else {
        ifaces.set_vec(iface_rows);
    }
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
