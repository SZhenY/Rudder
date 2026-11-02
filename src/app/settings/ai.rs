//! AI 设置页：MCP 服务的开关、监听面、令牌与工具权限。
//!
//! 状态（运行中 / 绑定失败 / 当前地址）由 `crate::mcp` 的 `McpStatus` 提供，
//! 这里用 1 Hz 定时器拉取 —— 服务在 tokio 线程上跑，UI 侧没有别的通知渠道。

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, Model as _, ModelRc, SharedString, VecModel};
use sysinfo::Networks;

use super::{Store, persist};
use crate::i18n::t;
use crate::mcp::McpConfigSnapshot;
use crate::ui::{AppWindow, McpClientRow, McpIface};

thread_local! {
    /// 设置页里哪些客户端会话的权限明细是展开的。
    ///
    /// UI 态、不落盘，只属于本线程（与 `clients` 模型同一生命周期）。
    /// 放线程局部而不是穿参：刷新链路有 13 个调用点，新增参数会牵动每一个回调块。
    static EXPANDED_SESSIONS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

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

    bind_forget_grant(window, store, &ifaces, &clients);

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
    // 显示**真实的**配置值：服务关着时由 `interactive: mcp-enabled` 表达"现在不生效"。
    // 之前写成 `enabled && lan`，关服务后开关显示为关、网口行整段消失，看起来像配置丢了。
    window.set_mcp_lan_enabled(lan);
    window.set_mcp_token_masked(masked.into());
    window.set_mcp_token_plain(plain.into());

    {
        let store = store.borrow();
        window.set_mcp_audit_enabled(store.mcp_audit_enabled());
        window.set_mcp_audit_retention(SharedString::from(
            store.mcp_audit_retention_days().to_string(),
        ));
        window.set_mcp_audit_retention_valid(true);
    }
    window.set_mcp_pty_commands(store.borrow().mcp_pty_commands());
    window.set_mcp_audit_degraded(crate::mcp::audit_degraded());

    let selected_ifaces = store.borrow().mcp_lan_ifaces().to_vec();
    let iface_rows = enumerate_ifaces()
        .into_iter()
        .map(|(name, addr)| McpIface {
            enabled: selected_ifaces.iter().any(|iface| iface == &name),
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

thread_local! {
    /// 配置片段是否发布过。设置窗关着时只跳过"重复发布"，首次仍要写一遍 ——
    /// 否则第一次打开设置页会从主窗口拷到空文本，直到下一次刷新（≤ 1 秒）才填上。
    static JSON_PUBLISHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn publish_status(window: &AppWindow, store: &Store, clients: &Rc<VecModel<McpClientRow>>) {
    let enabled = store.borrow().mcp_enabled();
    let status = crate::mcp::status();
    let (text, error, urls) = if status.error.is_some() || status.error_kind.is_some() {
        // 标题按类别翻译，细节（地址 / 系统错误）原样跟在后面。
        let title = match status.error_kind {
            Some(crate::mcp::McpErrorKind::NoAddress) => {
                t("没有可监听的地址", "No address to listen on")
            }
            Some(crate::mcp::McpErrorKind::BindFailed) => t("绑定失败", "Failed to bind"),
            None => t("启动失败", "Failed to start"),
        };
        (
            title.to_string(),
            status.error.unwrap_or_default(),
            Vec::new(),
        )
    } else if status.running {
        // 地址交给下面的列表显示，状态行只留"运行中"—— 之前把首个 URL 拼进来，
        // 紧接着列表又全量列一遍，同一个地址出现两次。
        (
            t("运行中", "Running").to_string(),
            String::new(),
            status.urls(),
        )
    } else if !enabled {
        (
            t("未启用", "Disabled").to_string(),
            String::new(),
            Vec::new(),
        )
    } else {
        (
            t("正在启动", "Starting").to_string(),
            String::new(),
            Vec::new(),
        )
    };
    window.set_mcp_status(text.into());
    window.set_mcp_status_error(error.into());
    window.set_mcp_urls(ModelRc::from(Rc::new(VecModel::from(
        urls.into_iter().map(SharedString::from).collect::<Vec<_>>(),
    ))));
    // 顶部徽标用的是 `mcp-client-count`，设置窗关着也得刷，所以客户端列表不跳过。
    publish_clients(window, clients);
    publish_recent_calls(window);
    publish_recent_closes(window);
    publish_grants(window, store);
    // 配置片段与回显只服务设置页：窗口没开时不每秒把整份设置序列化两遍。
    if crate::app::settings_window::is_open() || !JSON_PUBLISHED.with(|flag| flag.get()) {
        // 服务起来后监听地址才算得出来（局域网网口尤其），配置片段跟着刷。
        window.set_mcp_json(client_config(store, false).into());
        window.set_mcp_json_masked(client_config(store, true).into());
        JSON_PUBLISHED.with(|flag| flag.set(true));
        crate::app::settings_window::resync_if_open(window);
    }
}

/// 客户端列表：每个客户端先一行 header，再跟 12 行权限开关。
///
/// 扁平模型而不是嵌套模型：Slint 里一层 `for` 就能渲染，开关回调带
/// `session_id + key` 即可定位是哪个客户端的哪一位。
/// 卡片副标题用的会话短标识 —— 同名同版本的两个客户端靠它区分。
fn session_tag(session_id: &str) -> String {
    format!("#{}", session_id.chars().take(6).collect::<String>())
}

fn client_rows(en: bool) -> Vec<McpClientRow> {
    session_rows(en, &crate::mcp::sessions_info())
}

/// 纯逻辑，便于单测：两条同名会话必须产出两组可区分的行。
fn session_rows(en: bool, sessions: &[crate::mcp::McpSessionInfo]) -> Vec<McpClientRow> {
    // 先克隆成局部：建行过程中会 `set_row_data`（触发布局/绑定求值），
    // 不能一直持有 RefCell 的借用。
    let expanded = EXPANDED_SESSIONS.with(|set| set.borrow().clone());
    let mut rows = Vec::new();
    for session in sessions {
        let is_open = expanded.contains(session.session_id.as_str());
        rows.push(McpClientRow {
            kind: "header".into(),
            session_id: session.session_id.as_str().into(),
            client: session.client.as_str().into(),
            since: {
                // 带上来源：`local` 表示本机，其余是来源 IP。
                // 同一个 token 多设备时，这是唯一能看出"这两张卡是同一台还是两台设备"的地方。
                let mut parts = vec![session_tag(&session.session_id)];
                if !session.peer_ip.is_empty() {
                    parts.push(session.peer_ip.clone());
                }
                parts.push(session.connected_wall.clone());
                parts.push(format!(
                    "{}m{}s",
                    session.connected_secs / 60,
                    session.connected_secs % 60
                ));
                parts.join(" · ")
            }
            .into(),
            status: format!(
                "{} · {}",
                t("已接入", "connected"),
                granted_summary(&session.granted, en)
            )
            .into(),
            client_id: session.client_id.clone().unwrap_or_default().into(),
            trusted: session.trusted,
            expanded: is_open,
            key: SharedString::default(),
            label: SharedString::default(),
            desc: SharedString::default(),
            enabled: false,
            risk: SharedString::default(),
            risk_note: SharedString::default(),
            group: SharedString::default(),
        });
        for row_bit in crate::mcp::bit_rows() {
            let bit = row_bit.bit;
            let risk = row_bit.risk;
            // 设置页没有折叠，每个功能区都要标题 —— 用 `group_always`（旧语义），
            // 不能用 `group`（只给可折叠的组，弹窗专用）。
            let group = row_bit.group_always;
            rows.push(McpClientRow {
                kind: "permission".into(),
                session_id: session.session_id.as_str().into(),
                client: SharedString::default(),
                since: SharedString::default(),
                status: SharedString::default(),
                client_id: SharedString::default(),
                trusted: false,
                expanded: is_open,
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
                    .map(|group| {
                        if en {
                            group.label_en()
                        } else {
                            group.label_zh()
                        }
                    })
                    .unwrap_or_default()
                    .into(),
            });
        }
    }
    rows
}

/// 还原本页默认。
///
/// 只重置**设置项**：令牌与已记住的授权是用户明确做过的决定，重置它们会踢掉所有
/// 正在连的客户端、让「永久信任」凭空消失 —— 那不是"还原默认"该干的事。
pub(crate) fn reset(w: &AppWindow, store: &Store) {
    let d = crate::config::fresh_config();
    let (port, lan, pty, audit, retention) = (
        d.mcp.mcp_port,
        d.mcp.mcp_lan_enabled,
        d.mcp.mcp_pty_commands,
        d.mcp.mcp_audit_enabled,
        d.mcp.mcp_audit_retention_days,
    );
    persist(store, move |s| {
        s.set_mcp_port(port);
        s.set_mcp_lan_enabled(lan);
        s.set_mcp_lan_ifaces(Vec::new());
        s.set_mcp_pty_commands(pty);
        s.set_mcp_audit_enabled(audit);
        s.set_mcp_audit_retention_days(retention);
    });
    apply(store);
    w.set_mcp_port(port.to_string().into());
    w.set_mcp_lan_enabled(lan);
    w.set_mcp_pty_commands(pty);
    w.set_mcp_audit_enabled(audit);
    w.set_mcp_audit_retention(retention.to_string().into());
}

/// 已记住的客户端（持久化授权）。离线也要能撤销，所以单独列一张表。
fn publish_grants(window: &AppWindow, store: &Store) {
    let en = window.get_lang_en();
    let rows: Vec<crate::ui::McpGrantRow> = store
        .borrow()
        .mcp_client_grants()
        .iter()
        .map(|grant| {
            // 记忆主键形如 `rudder 1.0.0@192.168.1.5`；没带后缀的是升级前的老记录。
            let (client, device) = match grant.id.rsplit_once('@') {
                Some((client, device)) => (client.to_string(), device.to_string()),
                None => (grant.id.clone(), String::new()),
            };
            crate::ui::McpGrantRow {
                id: grant.id.as_str().into(),
                client: client.into(),
                device: device.into(),
                permissions: granted_summary(&crate::mcp::grant_perms(grant), en).into(),
                trusted: grant.trusted,
            }
        })
        .collect();
    // 内容没变就不重建：这个函数每秒被 `publish_status` 调一次，重建会带来无谓的 UI 抖动。
    let current = window.get_mcp_grants();
    // 逐行比 id + trusted：只比首行的话，中间某条被撤销/改信任时检测不到。
    // grants 上限 32 条，这点比较成本可以忽略。
    let unchanged = current.row_count() == rows.len()
        && rows.iter().enumerate().all(|(index, fresh)| {
            current
                .row_data(index)
                .is_some_and(|row| row.id == fresh.id && row.trusted == fresh.trusted)
        });
    if unchanged {
        return;
    }
    window.set_mcp_grants(ModelRc::from(Rc::new(VecModel::from(rows))));
}

/// 撤销一条记忆授权（客户端不在线也能撤）。
fn bind_forget_grant(
    window: &AppWindow,
    store: &Store,
    ifaces: &Rc<VecModel<McpIface>>,
    clients: &Rc<VecModel<McpClientRow>>,
) {
    let store = store.clone();
    let weak = window.as_weak();
    let ifaces = ifaces.clone();
    let clients = clients.clone();
    window.on_forget_mcp_grant(move |id| {
        let id = id.to_string();
        if id.is_empty() {
            return;
        }
        persist(&store, |s| {
            s.remove_mcp_client_grant(&id);
        });
        apply(&store);
        if let Some(window) = weak.upgrade() {
            refresh(&window, &store, &ifaces, &clients);
        }
    });
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

fn publish_clients(window: &AppWindow, clients: &Rc<VecModel<McpClientRow>>) {
    let rows = client_rows(window.get_lang_en());
    // 行数没变就逐行更新，别整表替换（否则开关会被重建，动画丢失）。
    if clients.row_count() == rows.len() {
        for (index, row) in rows.into_iter().enumerate() {
            clients.set_row_data(index, row);
        }
    } else {
        clients.set_vec(rows);
    }
    // 会话断开后把它的展开态一起丢掉，避免集合无限增长；重连是新 session-id，默认收起。
    let alive: HashSet<String> = rows_header_ids(clients.as_ref());
    EXPANDED_SESSIONS.with(|set| set.borrow_mut().retain(|id| alive.contains(id)));
    window.set_mcp_clients(ModelRc::from(clients.clone()));
    // 顶部栏徽标：直接从建好的行里数 header，避免为拿计数再调一次 `sessions_info()`。
    let online = (0..clients.row_count())
        .filter(|index| {
            clients
                .row_data(*index)
                .is_some_and(|row| row.kind == "header")
        })
        .count();
    window.set_mcp_client_count(online as i32);
}

/// 当前模型里还活着的 session id（header 行）。
fn rows_header_ids(clients: &VecModel<McpClientRow>) -> HashSet<String> {
    (0..clients.row_count())
        .filter_map(|index| clients.row_data(index))
        .filter(|row| row.kind == "header")
        .map(|row| row.session_id.to_string())
        .collect()
}

/// "最近断开"列表：长度与首行都没变就不重建模型，避免每秒抖动。
fn publish_recent_closes(window: &AppWindow) {
    let entries = crate::mcp::recent_closes();
    let en = window.get_lang_en();
    let current = window.get_mcp_recent_closes();
    let unchanged = current.row_count() == entries.len()
        && current
            .row_data(0)
            .map(|row| {
                entries.first().is_some_and(|entry| {
                    row.at == entry.at.as_str() && row.session == entry.session8.as_str()
                })
            })
            .unwrap_or(entries.is_empty());
    if unchanged {
        return;
    }
    let rows: Vec<crate::ui::McpCloseRow> = entries
        .into_iter()
        .map(|entry| {
            let mut reason = entry.reason.label(en).to_string();
            // 刚连上就断的情况单独点出来 —— "授权完又没了"最容易让人困惑。
            if entry.alive_secs < 60 {
                reason.push_str(&if en {
                    format!(" ({}s after connect)", entry.alive_secs)
                } else {
                    format!("（接入 {} 秒即断开）", entry.alive_secs)
                });
            }
            crate::ui::McpCloseRow {
                at: entry.at.into(),
                session: format!("#{}", entry.session8).into(),
                // 汇总条目（服务停止一次关掉多个会话）的文案按语言现拼，
                // 不能在 Rust 里写死中文，否则英文界面会出现中文。
                client: match entry.count {
                    Some(count) => {
                        if en {
                            format!("{count} sessions")
                        } else {
                            format!("{count} 个会话")
                        }
                    }
                    None => entry.client,
                }
                .into(),
                reason: reason.into(),
                alarm: entry.reason.alarm(),
            }
        })
        .collect();
    window.set_mcp_recent_closes(ModelRc::from(Rc::new(VecModel::from(rows))));
}

thread_local! {
    /// 上次渲染时的环版本号。初值 `u64::MAX` 保证第一次一定重建。
    static LAST_CALLS_REV: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
}

/// "最近调用"列表：环的版本号没变就不重建模型。
///
/// 原来先 `recent_calls()` 把 200 条整份 clone 出来、再比较首行决定"没变" ——
/// 每秒一次的轮询里那是纯浪费（约 1400 次字符串分配/秒）。
fn publish_recent_calls(window: &AppWindow) {
    let rev = crate::mcp::recent_calls_rev();
    let changed = LAST_CALLS_REV.with(|slot| {
        if slot.get() == rev {
            false
        } else {
            slot.set(rev);
            true
        }
    });
    if !changed && window.get_mcp_recent_calls().row_count() > 0 {
        return;
    }
    let entries = crate::mcp::recent_calls();
    {
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

#[cfg(test)]
mod session_row_tests {
    use super::{session_rows, session_tag};

    fn info(session_id: &str, client: &str) -> crate::mcp::McpSessionInfo {
        crate::mcp::McpSessionInfo {
            session_id: session_id.to_string(),
            client: client.to_string(),
            connected_wall: "10:22:31".to_string(),
            connected_secs: 12,
            granted: crate::mcp::Permissions::default(),
            peer_ip: "local".to_string(),
            client_id: Some(client.to_string()),
            trusted: false,
        }
    }

    /// 两个同名同版本客户端必须是两组能区分的行 —— 否则用户会把它们当成一条，
    /// 从而觉得"前一个客户端消失了"。
    #[test]
    fn identical_client_names_produce_separate_tagged_rows() {
        let sessions = vec![
            info("aaaa1111", "WorkBuddy Connector 5.9.3"),
            info("bbbb2222", "WorkBuddy Connector 5.9.3"),
        ];
        let rows = session_rows(false, &sessions);
        let headers: Vec<_> = rows.iter().filter(|row| row.kind == "header").collect();
        assert_eq!(headers.len(), 2);
        assert_ne!(headers[0].session_id, headers[1].session_id);
        assert!(headers[0].since.to_string().contains("#aaaa11"));
        assert!(headers[1].since.to_string().contains("#bbbb22"));
    }

    #[test]
    fn session_tag_takes_six_chars_and_handles_short_ids() {
        assert_eq!(session_tag("abcdef123"), "#abcdef");
        assert_eq!(session_tag("ab"), "#ab");
        assert_eq!(session_tag(""), "#");
    }
}

#[cfg(test)]
mod client_expansion_tests {
    use super::{EXPANDED_SESSIONS, session_rows};

    fn info(session_id: &str) -> crate::mcp::McpSessionInfo {
        crate::mcp::McpSessionInfo {
            session_id: session_id.to_string(),
            client: "WorkBuddy Connector 5.9.3".to_string(),
            connected_wall: "10:22:31".to_string(),
            connected_secs: 12,
            granted: crate::mcp::Permissions::default(),
            peer_ip: "local".to_string(),
            client_id: Some("WorkBuddy Connector 5.9.3".to_string()),
            trusted: false,
        }
    }

    fn reset() {
        EXPANDED_SESSIONS.with(|set| set.borrow_mut().clear());
    }

    /// 折叠靠渲染侧的 `if`（不是 `visible`），模型行数必须恒定 ——
    /// 行数一变，1 Hz 刷新就会退化成整表替换，开关的重建会打断动画。
    #[test]
    fn row_count_is_constant_regardless_of_expansion() {
        reset();
        let sessions = vec![info("aaaa1111")];
        let collapsed = session_rows(false, &sessions);
        EXPANDED_SESSIONS.with(|set| set.borrow_mut().insert("aaaa1111".to_string()));
        let expanded = session_rows(false, &sessions);
        assert_eq!(collapsed.len(), expanded.len());
        assert_eq!(collapsed.len(), 1 + crate::mcp::PERMISSION_BITS.len());
        assert!(collapsed.iter().all(|row| !row.expanded));
        assert!(expanded.iter().all(|row| row.expanded));
        reset();
    }

    /// 展开态按会话独立：两个同名客户端只展开其中一个。
    #[test]
    fn expansion_is_per_session() {
        reset();
        let sessions = vec![info("aaaa1111"), info("bbbb2222")];
        EXPANDED_SESSIONS.with(|set| set.borrow_mut().insert("aaaa1111".to_string()));
        let rows = session_rows(false, &sessions);
        let first = rows
            .iter()
            .find(|row| row.session_id == "aaaa1111" && row.kind == "header");
        let second = rows
            .iter()
            .find(|row| row.session_id == "bbbb2222" && row.kind == "header");
        assert!(first.expect("第一张卡").expanded);
        assert!(!second.expect("第二张卡").expanded);
        reset();
    }
}
