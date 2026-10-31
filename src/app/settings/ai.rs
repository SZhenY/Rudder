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
use crate::ui::{AppWindow, McpClientRow, McpIface};

pub(crate) fn bind(window: &AppWindow, store: &Store) {
    let ifaces = Rc::new(VecModel::<McpIface>::default());
    let clients = Rc::new(VecModel::<McpClientRow>::default());
    refresh(window, store, &ifaces, &clients);

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
                    s.upsert_mcp_client_grant(&client_id, granted);
                });
                apply(&store);
            }
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
            let token = crate::mcp::new_token();
            persist(&store, |s| s.set_mcp_token(token));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &clients);
            }
        });
    }

    // 服务在 tokio 线程上启停，UI 只能轮询状态。
    let timer = slint::Timer::default();
    {
        let weak = window.as_weak();
        let store = store.clone();
        let clients = clients.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(1),
            move || {
                if let Some(window) = weak.upgrade() {
                    publish_status(&window, &store, &clients);
                }
            },
        );
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let ifaces = ifaces.clone();
        let clients = clients.clone();
        window.on_set_mcp_audit_enabled(move |value: bool| {
            persist(&store, |s| s.set_mcp_audit_enabled(value));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &clients);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let ifaces = ifaces.clone();
        let clients = clients.clone();
        window.on_set_mcp_audit_retention(move |text: SharedString| -> bool {
            let trimmed = text.trim();
            let Ok(days) = trimmed.parse::<u16>() else {
                return false;
            };
            if days > 365 {
                return false;
            }
            persist(&store, |s| s.set_mcp_audit_retention_days(days));
            apply(&store);
            if let Some(window) = weak.upgrade() {
                refresh(&window, &store, &ifaces, &clients);
            }
            true
        });
    }
    {
        window.on_open_mcp_log_dir(move || {
            let dir = crate::config::log_dir();
            let _ = std::fs::create_dir_all(&dir);
            open_directory(&dir);
        });
    }

    // Timer 被 drop 就停 —— 与侧栏采样器同一套做法。
    Box::leak(Box::new(timer));
}

pub(crate) fn apply(store: &Store) {
    let snapshot = McpConfigSnapshot::from_store(&mut store.borrow_mut());
    crate::mcp::reload(snapshot);
}

fn refresh(
    window: &AppWindow,
    store: &Store,
    ifaces: &Rc<VecModel<McpIface>>,
    clients: &Rc<VecModel<McpClientRow>>,
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
    // 端口或令牌一改，可粘贴的配置就跟着变；这里推两份（明文 / 掩码），界面按
    // 「显示」开关自己选，不必把窗口里的可见性状态传回来。
    window.set_mcp_json(client_config(store, false).into());
    window.set_mcp_json_masked(client_config(store, true).into());

    {
        let store = store.borrow();
        window.set_mcp_audit_enabled(store.mcp_audit_enabled());
        window.set_mcp_audit_retention(
            SharedString::from(store.mcp_audit_retention_days().to_string()),
        );
        window.set_mcp_audit_retention_valid(true);
    }
    window.set_mcp_audit_degraded(crate::mcp::audit_degraded());

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

    publish_status(window, store, clients);
}

fn publish_status(
    window: &AppWindow,
    store: &Store,
    clients: &Rc<VecModel<McpClientRow>>,
) {
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
    // 服务起来后监听地址才算得出来（局域网网口尤其），配置片段跟着刷。
    window.set_mcp_json(client_config(store, false).into());
    window.set_mcp_json_masked(client_config(store, true).into());
    publish_clients(window, clients);
    publish_recent_calls(window);
    crate::app::settings_window::resync_if_open(window);
}

/// 客户端列表：每个客户端先一行 header，再跟 12 行权限开关。
///
/// 扁平模型而不是嵌套模型：Slint 里一层 `for` 就能渲染，开关回调带
/// `session_id + key` 即可定位是哪个客户端的哪一位。
fn client_rows(en: bool) -> Vec<McpClientRow> {
    let mut rows = Vec::new();
    for session in crate::mcp::sessions_info() {
        rows.push(McpClientRow {
            kind: "header".into(),
            session_id: session.session_id.as_str().into(),
            client: session.client.as_str().into(),
            since: format!(
                "{} · {}m{}s",
                session.connected_wall,
                session.connected_secs / 60,
                session.connected_secs % 60
            )
            .into(),
            status: format!(
                "{} · {}",
                t("已接入", "connected"),
                granted_summary(&session.granted, en)
            )
            .into(),
            key: SharedString::default(),
            label: SharedString::default(),
            desc: SharedString::default(),
            enabled: false,
            risk: SharedString::default(),
            risk_note: SharedString::default(),
            group: SharedString::default(),
        });
        for (bit, risk, group) in crate::mcp::bits_with_heads() {
            rows.push(McpClientRow {
                kind: "permission".into(),
                session_id: session.session_id.as_str().into(),
                client: SharedString::default(),
                since: SharedString::default(),
                status: SharedString::default(),
                key: bit.key.into(),
                label: if en { bit.label_en } else { bit.label_zh }.into(),
                desc: if en { bit.desc_en } else { bit.desc_zh }.into(),
                enabled: session.granted.allows(bit.key),
                risk: risk
                    .map(|risk| if en { risk.label_en() } else { risk.label_zh() })
                    .unwrap_or_default()
                    .into(),
                risk_note: risk
                    .map(|risk| if en { risk.note_en() } else { risk.note_zh() })
                    .unwrap_or_default()
                    .into(),
                group: group
                    .map(|group| if en { group.label_en() } else { group.label_zh() })
                    .unwrap_or_default()
                    .into(),
            });
        }
    }
    rows
}

/// 客户端标题行上的授权摘要。最多列 4 项，超出用计数，避免撑爆一行。
fn granted_summary(perms: &crate::mcp::Permissions, en: bool) -> String {
    const MAX: usize = 4;
    let mut parts: Vec<&str> = crate::mcp::PERMISSION_BITS
        .iter()
        .filter(|bit| perms.allows(bit.key))
        .map(|bit| if en { bit.short_en } else { bit.short_zh })
        .collect();
    if parts.is_empty() {
        return t("未授予权限", "no permissions").to_string();
    }
    if parts.len() > MAX {
        let rest = parts.len() - MAX;
        parts.truncate(MAX);
        return format!(
            "{} {}",
            parts.join(" · "),
            if en {
                format!("+{rest} more")
            } else {
                format!("…等 {rest} 项")
            }
        );
    }
    parts.join(" · ")
}

fn publish_clients(window: &AppWindow, clients: &Rc<VecModel<McpClientRow>>) {    let rows = client_rows(window.get_lang_en());
    // 行数没变就逐行更新，别整表替换（否则开关会被重建，动画丢失）。
    if clients.row_count() == rows.len() {
        for (index, row) in rows.into_iter().enumerate() {
            clients.set_row_data(index, row);
        }
    } else {
        clients.set_vec(rows);
    }
    window.set_mcp_clients(ModelRc::from(clients.clone()));
}

/// "最近调用"列表：长度与首行都没变就不重建模型，避免每秒抖动。
fn publish_recent_calls(window: &AppWindow) {
    let entries = crate::mcp::recent_calls();
    let current = window.get_mcp_recent_calls();
    let unchanged = current.row_count() == entries.len()
        && current
            .row_data(0)
            .map(|row| {
                entries
                    .first()
                    .is_some_and(|entry| row.at == entry.at.as_str() && row.tool == entry.tool.as_str())
            })
            .unwrap_or(entries.is_empty());
    if !unchanged {
        let rows: Vec<crate::ui::AuditRow> = entries
            .into_iter()
            .map(|entry| crate::ui::AuditRow {
                at: entry.at.into(),
                tool: entry.tool.into(),
                result: if entry.code.is_empty() {
                    SharedString::from("ok")
                } else {
                    SharedString::from(entry.code)
                },
                ok: entry.ok,
                ms: entry.ms.min(i32::MAX as u64) as i32,
                client: entry.client.into(),
            })
            .collect();
        window.set_mcp_recent_calls(ModelRc::from(Rc::new(VecModel::from(rows))));
    }
}

/// 用系统文件管理器打开目录。
fn open_directory(dir: &std::path::Path) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

/// `rk_9f2a…7c31`：足够辨认是哪一枚，又不至于把令牌摊在屏幕上。
/// 可直接粘进 MCP 客户端配置文件的片段。`masked` 只影响显示，复制始终走明文。
///
/// 手拼字符串而不用 `serde_json`：键按常见写法排序（`type` / `url` / `headers`），
/// 用户看到的就是能直接用的那份；内容只有端口、地址与 base64-ish 令牌，无需转义。
fn client_config(store: &Store, masked: bool) -> String {
    let (port, token) = {
        let store = store.borrow();
        (store.mcp_port(), store.mcp_token().to_string())
    };
    // 还没有令牌 = 服务从没启用过，此时给出的配置连不上，不如不给。
    if token.is_empty() {
        return String::new();
    }
    let token = if masked { mask_token(&token) } else { token };
    // 地址用正在监听的第一个（`desired_addrs` 排序后回环在前）；服务没起来时
    // 按端口拼回环地址，用户仍可先配好客户端再开服务。
    let url = crate::mcp::status()
        .urls()
        .into_iter()
        .next()
        .unwrap_or_else(|| format!("http://127.0.0.1:{port}/mcp"));
    format!(
        "{{\n  \"mcpServers\": {{\n    \"rudder\": {{\n      \"type\": \"http\",\n      \"url\": \"{url}\",\n      \"headers\": {{\n        \"Authorization\": \"Bearer {token}\"\n      }}\n    }}\n  }}\n}}"
    )
}

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
