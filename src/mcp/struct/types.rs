use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use rmcp::RoleServer;
use rmcp::service::Peer;

use crate::config::ConfigStore;
use crate::config::settings::McpClientGrant;
use crate::resource::{LocalSnap, TabStatuses};
use crate::terminal::TermBuffers;

use super::auth;
use super::perms::Permissions;

/// 服务配置快照。令牌 / 策略的改动只换这一份（热更新），不重绑监听；
/// 只有 `same_listeners` 判为不同才需要重建 listener。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpConfigSnapshot {
    pub(crate) enabled: bool,
    pub(crate) port: u16,
    pub(crate) loopback: bool,
    pub(crate) lan_enabled: bool,
    pub(crate) lan_ifaces: Vec<String>,
    pub(crate) token: String,
    /// 按客户端记住的授权，只在连接时用来预勾弹窗。
    pub(crate) client_grants: Vec<McpClientGrant>,
    pub(crate) consent: ConsentPolicy,
    /// `run_command` 走交互式 PTY（命令可见）而不是一次性 exec 通道。
    pub(crate) pty_commands: bool,
    pub(crate) audit: AuditConfig,
}

/// 授权策略：默认每个客户端会话单独确认；`Auto` 是旧的"全局即全部"行为，
/// 只给脚本化测试与 `RUDDER_MCP_DEV_TOKEN` 用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ConsentPolicy {
    #[default]
    Session,
    Auto,
}

/// 审计落盘参数。`retention_days == 0` 表示永久保留。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuditConfig {
    pub(crate) enabled: bool,
    pub(crate) retention_days: u16,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_days: 14,
        }
    }
}

impl McpConfigSnapshot {
    /// 从配置取一份快照。服务已启用但还没有令牌时**当场生成并落盘** ——
    /// 空令牌会让鉴权退化成"任何串都能过"，所以这个值不允许为空。
    pub(crate) fn from_store(store: &mut ConfigStore) -> Self {
        let audit = AuditConfig {
            enabled: store.mcp_audit_enabled(),
            retention_days: store.mcp_audit_retention_days(),
        };
        let consent = if store.mcp_consent() == "auto" {
            ConsentPolicy::Auto
        } else {
            ConsentPolicy::Session
        };
        let client_grants = store.mcp_client_grants().to_vec();

        // 开发口子：仅 debug 构建生效，供 curl / 端到端测试免改配置。
        // `Auto` 会直接授予全部 12 位（见 handler::on_initialized），不再有全局权限位。
        #[cfg(debug_assertions)]
        if let Ok(token) = std::env::var("RUDDER_MCP_DEV_TOKEN")
            && !token.is_empty()
        {
            return Self {
                enabled: true,
                port: store.mcp_port(),
                loopback: true,
                lan_enabled: false,
                lan_ifaces: Vec::new(),
                token,
                client_grants,
                consent: ConsentPolicy::Auto,
                pty_commands: store.mcp_pty_commands(),
                audit: AuditConfig {
                    enabled: true,
                    retention_days: audit.retention_days,
                },
            };
        }

        let enabled = store.mcp_enabled();
        let mut token = store.mcp_token().to_string();
        if enabled && token.is_empty() {
            token = auth::new_token();
            store.set_mcp_token(token.clone());
            store.save_logging();
        }
        Self {
            enabled,
            port: store.mcp_port(),
            loopback: store.mcp_bind_loopback(),
            lan_enabled: store.mcp_lan_enabled(),
            lan_ifaces: store.mcp_lan_ifaces().to_vec(),
            token,
            client_grants,
            consent,
            pty_commands: store.mcp_pty_commands(),
            audit,
        }
    }

    /// 监听面是否一致。端口 / 回环 / 局域网选择任一变化都要重建 listener。
    pub(crate) fn same_listeners(&self, other: &Self) -> bool {
        self.enabled == other.enabled
            && self.port == other.port
            && self.loopback == other.loopback
            && self.lan_enabled == other.lan_enabled
            && self.lan_ifaces == other.lan_ifaces
    }
}

/// 运行状态，UI 轮询读取。
#[derive(Debug, Clone, Default)]
pub(crate) struct McpStatus {
    pub(crate) running: bool,
    pub(crate) listeners: Vec<SocketAddr>,
    /// 失败细节（地址 / 系统错误）。**不要在这里拼中文文案** —— 界面按语言自己拼标题，
    /// 否则英文界面里会冒出中文。
    pub(crate) error: Option<String>,
    /// 失败类别，决定界面显示哪句标题。
    pub(crate) error_kind: Option<McpErrorKind>,
}

/// 服务启动失败的原因类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpErrorKind {
    /// 没有可监听的地址：回环关了、又没勾任何网口。
    NoAddress,
    /// 某个地址绑定失败（端口被占、权限不足等）。
    BindFailed,
}

impl McpStatus {
    /// 可直接粘贴给 AI agent 的地址清单。
    pub(crate) fn urls(&self) -> Vec<String> {
        self.listeners
            .iter()
            .map(|addr| format!("http://{addr}/mcp"))
            .collect()
    }
}

/// 在记忆授权里找这个客户端的记录。
///
/// 精确匹配优先；找不到时退回 `name version`（升级前的格式没有 `@ip` 后缀），
/// 让老信任还能用一轮 —— 否则升级后所有客户端都要重新授权。
fn find_grant<'a>(
    grants: &'a [crate::config::settings::McpClientGrant],
    client_id: &str,
) -> Option<&'a crate::config::settings::McpClientGrant> {
    if let Some(found) = grants.iter().find(|grant| grant.id == client_id) {
        return Some(found);
    }
    let legacy = client_id.split('@').next().unwrap_or(client_id);
    grants.iter().find(|grant| grant.id == legacy)
}

/// 已记住的授权能否**直接放行（不弹窗）**。
///
/// 三个条件里任一满足即可：用户勾了「永久信任」；或上次授权距现在不足
/// `window_secs`（一个租约空闲期，15 分钟）。权限集合为空时一律重新弹窗 ——
/// 那是"用户上次一位都没给"，不该被当成"已授权"。
pub(crate) fn grant_allows_silent(
    grant: &crate::config::settings::McpClientGrant,
    now_unix: u64,
    window_secs: u64,
) -> bool {
    // 「永久信任」先判：弹窗允许一位都不勾（那样只能调 get_server_info），
    // 这种客户端勾了永久信任后不该被反复打扰，否则设置页的开关等于摆设。
    if grant.trusted {
        return true;
    }
    if grant.permissions.is_empty() {
        return false;
    }
    grant.last_auth_unix > 0 && now_unix.saturating_sub(grant.last_auth_unix) < window_secs
}

/// 一个已接入客户端的可展示形态（AI 页的客户端卡）。
#[derive(Debug, Clone)]
pub(crate) struct McpSessionInfo {
    pub(crate) session_id: String,
    pub(crate) client: String,
    pub(crate) connected_wall: String,
    pub(crate) connected_secs: u64,
    pub(crate) granted: Permissions,
    /// 来源：`local` 或 IP；空串表示拿不到。用于在卡片上区分"同一台设备"与"不同设备"。
    pub(crate) peer_ip: String,
    /// 该客户端的记忆标识；`None` = 客户端没上报名字，无法记忆/信任。
    pub(crate) client_id: Option<String>,
    /// 「永久信任」当前状态（来自配置里的 grant），驱动卡片开关。
    pub(crate) trusted: bool,
}

/// 一个客户端会话。多个客户端各自一份，权限只活在这里。
#[derive(Debug, Clone)]
pub(crate) struct ClientLease {
    pub(crate) session_id: String,
    /// 连接来源 IP（initialize 那一刻记录）。用于区分"不同设备上的同名客户端"。
    pub(crate) peer_ip: Option<std::net::IpAddr>,
    pub(crate) last_seen: Instant,
    /// 建立时刻（单调，用于算"已连接多久"）。
    pub(crate) connected_at: Instant,
    /// 展示用的壁钟（`HH:MM:SS`）。
    pub(crate) connected_wall: String,
    /// 客户端自称的名字 / 版本，来自 `initialize` 的 `clientInfo`。
    pub(crate) client_name: String,
    pub(crate) client_version: String,
    /// 授权状态机。
    pub(crate) verdict: ConsentState,
    /// 本会话的授权位。
    pub(crate) granted: Permissions,
    /// 重连 / 授权变化时 +1，用来打断在途调用。
    pub(crate) epoch: u64,
    /// 用来在授权变化后发 `notifications/tools/list_changed`。
    pub(crate) peer: Option<Peer<RoleServer>>,
}

/// 会话授权状态。拒绝 / 超时会把租约整条移除，所以活着的只有"待确认"与"已授权"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsentState {
    Pending,
    Granted,
}

/// 移除租约的原因。审计用 `reason` 字段区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseReason {
    /// 用户在连接弹窗里点了拒绝。
    Denied,
    /// 弹窗超时：视为未接入。
    Timeout,
    /// 用户在 AI 页点了断开。
    Disconnected,
    /// 令牌轮换，旧客户端全部失效。
    TokenRotated,
    /// 空闲超过 `IDLE`。
    Idle,
    /// 客户端主动 DELETE。
    Delete,
    /// rmcp 已不认这个会话。
    NotFound,
    /// 服务被停止 / 重启（换端口、换网口、关掉开关）。这条路径原来完全无痕。
    Stopped,
}

impl CloseReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Timeout => "timeout",
            Self::Disconnected => "disconnected",
            Self::TokenRotated => "token_rotated",
            Self::Idle => "idle",
            Self::Delete => "delete",
            Self::NotFound => "not_found",
            Self::Stopped => "stopped",
        }
    }

    /// AI 页「最近断开」里的一行短句。
    pub(crate) fn label(self, en: bool) -> &'static str {
        match self {
            Self::Denied => {
                if en {
                    "you denied it"
                } else {
                    "你在弹窗里拒绝了"
                }
            }
            Self::Timeout => {
                if en {
                    "authorisation timed out"
                } else {
                    "授权弹窗超时（45 秒未确认）"
                }
            }
            Self::Disconnected => {
                if en {
                    "you disconnected it"
                } else {
                    "你在设置里断开了"
                }
            }
            Self::TokenRotated => {
                if en {
                    "access token changed"
                } else {
                    "访问令牌已更换"
                }
            }
            Self::Idle => {
                if en {
                    "idle for 15 minutes"
                } else {
                    "闲置 15 分钟被回收"
                }
            }
            Self::Delete => {
                if en {
                    "closed by the client"
                } else {
                    "客户端主动关闭"
                }
            }
            Self::NotFound => {
                if en {
                    "session expired in the MCP runtime"
                } else {
                    "会话在 MCP 运行时已失效"
                }
            }
            Self::Stopped => {
                if en {
                    "MCP service stopped"
                } else {
                    "MCP 服务已停止"
                }
            }
        }
    }

    /// 是否需要用户注意（界面上用告警色）。客户端自己关闭 / 闲置回收不算异常。
    pub(crate) fn alarm(self) -> bool {
        matches!(
            self,
            Self::Denied | Self::Timeout | Self::TokenRotated | Self::Stopped
        )
    }
}

impl ClientLease {
    /// 空闲这么久就自动释放，避免客户端崩溃后端口永久占着。
    pub(crate) const IDLE: Duration = Duration::from_secs(15 * 60);

    /// 弹窗后等用户决定的时长。超时 = 未接入，租约会被移除。
    pub(crate) const CONSENT_TIMEOUT: Duration = Duration::from_secs(45);

    pub(crate) fn new(
        session_id: String,
        epoch: u64,
        peer_ip: Option<std::net::IpAddr>,
    ) -> Self {
        Self {
            session_id,
            peer_ip,
            last_seen: Instant::now(),
            connected_at: Instant::now(),
            connected_wall: chrono::Local::now().format("%H:%M:%S").to_string(),
            client_name: String::new(),
            client_version: String::new(),
            verdict: ConsentState::Pending,
            granted: Permissions::default(),
            epoch,
            peer: None,
        }
    }

    pub(crate) fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_seen) > Self::IDLE
    }

    /// `"Claude Desktop 0.7.2"`；未上报时给个占位。
    pub(crate) fn client_label(&self) -> String {
        let name = if self.client_name.is_empty() {
            "unknown client"
        } else {
            self.client_name.as_str()
        };
        if self.client_version.is_empty() {
            name.to_string()
        } else {
            format!("{name} {}", self.client_version)
        }
    }

    /// 来源的展示形式：本机回环统一显示 `local`，其余显示 IP；拿不到时为空串。
    pub(crate) fn peer_label(&self) -> String {
        match self.peer_ip {
            Some(ip) if ip.is_loopback() => "local".to_string(),
            Some(ip) => ip.to_string(),
            None => String::new(),
        }
    }

    /// 用于记忆授权的身份；未上报名字时返回 `None`（不持久化 unknown client）。
    ///
    /// 末尾附带来源（`@local` / `@192.168.1.5`）：同一台设备的多个连接共享一条记忆授权，
    /// 不同设备各自独立 —— 否则一个 token 多设备时会互相覆盖。
    pub(crate) fn client_id(&self) -> Option<String> {
        let name = self.client_name.trim();
        if name.is_empty() {
            return None;
        }
        let version = self.client_version.trim();
        let raw = if version.is_empty() {
            name.to_string()
        } else {
            format!("{name} {version}")
        };
        let cleaned: String = raw
            .chars()
            .filter(|ch| !ch.is_control())
            .take(128)
            .collect();
        if cleaned.is_empty() {
            return None;
        }
        Some(match self.peer_ip {
            // 回环统一归一化：127.0.0.1 与 ::1 是同一台本机。
            Some(ip) if ip.is_loopback() => format!("{cleaned}@local"),
            Some(ip) => format!("{cleaned}@{ip}"),
            None => cleaned,
        })
    }
}

/// 界面侧工具的共享句柄。三个都是 `Arc`，MCP 线程可以直接读，
/// 不必绕回 UI 线程 —— 读屏与状态查询因此不受 UI 事件循环节流影响。
#[derive(Clone)]
pub(crate) struct AppHandles {
    pub(crate) bufs: TermBuffers,
    pub(crate) tab_statuses: TabStatuses,
    pub(crate) local_snap: LocalSnap,
    /// 每标签页的 SFTP 通道。文件读写不需要经过 UI 线程。
    pub(crate) sftp_handles: crate::sftp::SftpHandles,
}

/// 工具层错误。区分"界面没了""超时""参数不对""这一传输不支持"，客户端据此
/// 决定重试还是换用法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpError {
    UiUnavailable,
    UiTimeout,
    Reentrant,
    Invalid(String),
    NotSupported(&'static str),
    NotConnected(&'static str),
    TransferFailed(String),
    TimedOut,
    /// 弹窗超时（用户没在 45 秒内决定）。
    ConsentTimeout,
    /// 用户撤销了本次会话的授权。
    Revoked,
    /// 内部故障（采样线程崩了之类），不该让调用方按"用法错误"处理。
    Internal(String),
}

impl McpError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::UiUnavailable => "ui_unavailable",
            Self::UiTimeout => "ui_timeout",
            Self::Reentrant => "reentrant",
            Self::Invalid(_) => "invalid_argument",
            Self::NotSupported(_) => "unsupported",
            Self::NotConnected(_) => "not_connected",
            Self::TransferFailed(_) => "transfer_failed",
            Self::TimedOut => "timeout",
            Self::ConsentTimeout => "consent_timeout",
            Self::Revoked => "revoked",
            Self::Internal(_) => "internal",
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::UiUnavailable => "the Rudder window is not available".to_string(),
            Self::UiTimeout => "the UI thread did not answer in time".to_string(),
            Self::Reentrant => "bridge call issued from the UI thread".to_string(),
            Self::Invalid(message) => message.clone(),
            Self::NotSupported(message) => (*message).to_string(),
            Self::NotConnected(message) => (*message).to_string(),
            Self::TransferFailed(message) => message.clone(),
            Self::TimedOut => "the tool call exceeded its time budget".to_string(),
            Self::ConsentTimeout => "no answer from the Rudder window in time".to_string(),
            Self::Revoked => "the user revoked this session".to_string(),
            Self::Internal(message) => message.clone(),
        }
    }
}

/// 一次授权变更的结果，供 UI 线程写回"按客户端记住"的配置。
#[derive(Debug, Clone, Default)]
pub(crate) struct GrantUpdate {
    pub(crate) client_id: Option<String>,
    pub(crate) granted: Vec<String>,
}

/// 处理请求时共享的状态。全部字段可跨线程，`McpHandler` 只持有它。
pub(crate) struct McpState {
    /// 配置快照。用 `Arc` 装：`config()` 每次返回引用计数 +1，而不是把整份
    /// （含 `client_grants` 列表与所有字符串）深拷贝一遍 —— 一次工具调用会取 2–3 次。
    pub(crate) cfg: RwLock<std::sync::Arc<McpConfigSnapshot>>,
    pub(crate) status: Mutex<McpStatus>,
    /// 多客户端：key 是 `mcp-session-id`。权限只活在这里，配置里没有全局位。
    pub(crate) leases: Mutex<HashMap<String, ClientLease>>,
    pub(crate) requests: tokio::sync::Semaphore,
    /// 串行化连接授权弹窗：多个客户端同时接入时逐个弹。
    pub(crate) consent_gate: tokio::sync::Mutex<()>,
    /// 租约 epoch 的全局序号。跨租约单调，在途弹窗不会写到重连后的新会话上。
    pub(crate) epoch_seq: AtomicU64,
    /// `get_host_info` 的现场采样器。懒创建：只有在 UI 侧栏采样器停摆时才用得上。
    ///
    /// 不读 `app.local_snap` 就够的原因见 `host_info()`：侧栏折叠时采样整拍跳过，
    /// 读到的可能是几分钟前的数字。
    pub(crate) local_sampler: std::sync::Arc<Mutex<Option<crate::resource::SystemSampler>>>,
    /// UI 线程不能 await；授权变化后的 `list_changed` 用这个 handle 派发。
    rt: tokio::runtime::Handle,
    pub(crate) app: AppHandles,
}

impl McpState {
    /// 同时接入的客户端上限。每条租约都很轻，但工具调用共享同一个 UI 线程。
    pub(crate) const MAX_CLIENTS: usize = 8;

    pub(crate) fn new(cfg: McpConfigSnapshot, app: AppHandles, rt: tokio::runtime::Handle) -> Self {
        Self {
            cfg: RwLock::new(std::sync::Arc::new(cfg)),
            status: Mutex::new(McpStatus::default()),
            leases: Mutex::new(HashMap::new()),
            // 一次工具调用可能同时压着读屏与状态采样。
            requests: tokio::sync::Semaphore::new(8),
            consent_gate: tokio::sync::Mutex::new(()),
            epoch_seq: AtomicU64::new(1),
            local_sampler: std::sync::Arc::new(Mutex::new(None)),
            rt,
            app,
        }
    }

    pub(crate) fn next_epoch(&self) -> u64 {
        self.epoch_seq.fetch_add(1, Ordering::Relaxed)
    }

    fn lease_map(&self) -> std::sync::MutexGuard<'_, HashMap<String, ClientLease>> {
        self.leases.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// 新会话建租约。超过 `MAX_CLIENTS` 返回 false（调用方回 429）。
    pub(crate) fn insert_lease(
        &self,
        session_id: String,
        peer_ip: Option<std::net::IpAddr>,
    ) -> bool {
        let mut leases = self.lease_map();
        if leases.len() >= Self::MAX_CLIENTS {
            return false;
        }
        let epoch = self.next_epoch();
        leases.insert(
            session_id.clone(),
            ClientLease::new(session_id, epoch, peer_ip),
        );
        true
    }

    pub(crate) fn lease(&self, session_id: &str) -> Option<ClientLease> {
        self.lease_map().get(session_id).cloned()
    }

    pub(crate) fn lease_count(&self) -> usize {
        self.lease_map().len()
    }

    pub(crate) fn at_capacity(&self) -> bool {
        self.lease_count() >= Self::MAX_CLIENTS
    }

    /// 清掉空闲过期的租约；`sessions_info` 与 `http::guard` 都调它。
    pub(crate) fn prune_expired(&self, now: Instant) {
        let expired: Vec<ClientLease> = {
            let mut leases = self.lease_map();
            let ids: Vec<String> = leases
                .iter()
                .filter(|(_, lease)| lease.expired(now))
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| leases.remove(&id))
                .collect()
        };
        for lease in &expired {
            self.audit_close(lease, CloseReason::Idle, None);
        }
    }

    pub(crate) fn touch(&self, session_id: &str, now: Instant) -> bool {
        match self.lease_map().get_mut(session_id) {
            Some(lease) => {
                lease.last_seen = now;
                true
            }
            None => false,
        }
    }

    pub(crate) fn store_client(
        &self,
        session_id: &str,
        name: String,
        version: String,
        peer: Peer<RoleServer>,
    ) {
        if let Some(lease) = self.lease_map().get_mut(session_id) {
            if lease.client_name.is_empty() {
                lease.client_name = name;
                lease.client_version = version;
            }
            lease.peer = Some(peer);
        }
    }

    /// 允许接入：写入授权、记住该客户端、通知工具列表变化。
    pub(crate) fn approve(&self, session_id: &str, granted: Permissions) -> Option<GrantUpdate> {
        let update = {
            let mut leases = self.lease_map();
            let lease = leases.get_mut(session_id)?;
            lease.verdict = ConsentState::Granted;
            lease.granted = granted;
            lease.epoch = lease.epoch.wrapping_add(1);
            GrantUpdate {
                client_id: lease.client_id(),
                granted: granted.keys().into_iter().map(str::to_string).collect(),
            }
        };
        self.notify_tools_changed(session_id);
        Some(update)
    }

    /// 拒绝 / 超时 / 断开：整条租约移除，客户端下一次请求拿 404。
    pub(crate) fn close_session(&self, session_id: &str, reason: CloseReason) {
        let lease = self.lease_map().remove(session_id);
        if let Some(lease) = lease {
            self.audit_close(&lease, reason, None);
        }
    }

    /// 令牌轮换 / 停止服务：清空全部客户端。
    pub(crate) fn close_all(&self, reason: CloseReason) {
        let drained: Vec<ClientLease> = {
            let mut leases = self.lease_map();
            let out = leases.values().cloned().collect();
            leases.clear();
            out
        };
        // 还在等授权（Pending）的租约从没接入过，记进"最近断开"只会挤掉真正有用的
        // Denied / Timeout。
        let granted: Vec<&ClientLease> = drained
            .iter()
            .filter(|lease| lease.verdict == ConsentState::Granted)
            .collect();
        // 服务停止多半是"改端口 / 换网口"的副作用，一次关掉多个会话时只在内存环里留
        // 一条汇总 —— 否则 16 条容量会被刷屏。
        if reason == CloseReason::Stopped && granted.len() > 1 {
            super::audit::record_close(super::audit_types::SessionCloseEntry::from_summary(
                granted.len(),
                reason,
            ));
        } else {
            for lease in &granted {
                self.audit_close(lease, reason, None);
            }
        }
        // 审计文件仍逐条记（那是给事后排查用的，不该被合并）。
        if self.config().audit.enabled {
            for lease in &granted {
                crate::mcp::record_event(serde_json::json!({
                    "ts": chrono::Local::now().to_rfc3339(),
                    "event": "session_close",
                    "reason": reason.as_str(),
                    "session": lease.session_id.chars().take(8).collect::<String>(),
                    "client": lease.client_label(),
                    "tool": "",
                }));
            }
        }
    }

    /// 客户端卡上的一个开关。返回需要写回配置的记忆授权。
    pub(crate) fn set_permission(
        &self,
        session_id: &str,
        key: &str,
        enabled: bool,
    ) -> Option<GrantUpdate> {
        let (update, client_label, epoch, session8) = {
            let mut leases = self.lease_map();
            let lease = leases.get_mut(session_id)?;
            if lease.verdict != ConsentState::Granted || !lease.granted.set(key, enabled) {
                return None;
            }
            lease.epoch = lease.epoch.wrapping_add(1);
            (
                GrantUpdate {
                    client_id: lease.client_id(),
                    granted: lease
                        .granted
                        .keys()
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                },
                lease.client_label(),
                lease.epoch,
                lease.session_id.chars().take(8).collect::<String>(),
            )
        };
        if self.config().audit.enabled {
            crate::mcp::record_event(serde_json::json!({
                "ts": chrono::Local::now().to_rfc3339(),
                "event": "permission_change",
                "session": session8,
                "client": client_label,
                "key": key,
                "enabled": enabled,
                "epoch": epoch,
            }));
        }
        self.notify_tools_changed(session_id);
        Some(update)
    }

    /// 连接时取该客户端上次记住的授权；没有则空集。
    pub(crate) fn remembered_grant(&self, client_id: &str) -> Permissions {
        let cfg = self.config();
        match cfg.client_grants.iter().find(|grant| grant.id == client_id) {
            Some(grant) => {
                let keys: Vec<&str> = grant.permissions.iter().map(String::as_str).collect();
                Permissions::from_keys(&keys)
            }
            None => Permissions::default(),
        }
    }

    /// 连接时取该客户端上次记住的整条记录（权限 + 是否永久信任 + 上次授权时间）。
    pub(crate) fn remembered_grant_record(
        &self,
        client_id: &str,
    ) -> Option<crate::config::settings::McpClientGrant> {
        let cfg = self.config();
        find_grant(&cfg.client_grants, client_id).cloned()
    }

    /// 授权变化后让客户端重新 `tools/list`。UI 线程调用，派发到 tokio。
    pub(crate) fn notify_tools_changed(&self, session_id: &str) {
        let peer = self
            .leases
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(session_id)
            .and_then(|lease| lease.peer.clone());
        if let Some(peer) = peer {
            self.rt.spawn(async move {
                let _ = peer.notify_tool_list_changed().await;
            });
        }
    }

    /// AI 页的客户端列表。顺带清理空闲过期的租约。
    pub(crate) fn sessions_info(&self) -> Vec<McpSessionInfo> {
        let now = Instant::now();
        self.prune_expired(now);
        let leases = self.lease_map();
        let mut items: Vec<&ClientLease> = leases
            .values()
            .filter(|lease| lease.verdict == ConsentState::Granted)
            .collect();
        items.sort_by_key(|lease| lease.connected_at);
        // `config()` 返回的是 Arc 快照（不是 guard），这里再取不会和租约锁打架。
        let cfg = self.config();
        let grants = &cfg.client_grants;
        items
            .into_iter()
            .map(|lease| {
                let client_id = lease.client_id();
                let trusted = client_id
                    .as_ref()
                    .and_then(|id| grants.iter().find(|grant| &grant.id == id))
                    .map(|grant| grant.trusted)
                    .unwrap_or(false);
                McpSessionInfo {
                    session_id: lease.session_id.clone(),
                    client: lease.client_label(),
                    connected_wall: lease.connected_wall.clone(),
                    connected_secs: lease.connected_at.elapsed().as_secs(),
                    granted: lease.granted,
                    peer_ip: lease.peer_label(),
                    client_id,
                    trusted,
                }
            })
            .collect()
    }

    fn audit_close(&self, lease: &ClientLease, reason: CloseReason, tool: Option<&str>) {
        // 先无条件进内存环：界面上"会话为什么没了"不该依赖"记录工具调用"这个开关。
        super::audit::record_close(super::audit_types::SessionCloseEntry::from_lease(
            lease,
            reason,
            lease.connected_at.elapsed().as_secs(),
        ));
        if !self.config().audit.enabled {
            return;
        }
        crate::mcp::record_event(serde_json::json!({
            "ts": chrono::Local::now().to_rfc3339(),
            "event": "session_close",
            "reason": reason.as_str(),
            "session": lease.session_id.chars().take(8).collect::<String>(),
            "client": lease.client_label(),
            "tool": tool.unwrap_or(""),
            "epoch": lease.epoch,
        }));
    }

    pub(crate) fn config(&self) -> std::sync::Arc<McpConfigSnapshot> {
        self.cfg
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    pub(crate) fn set_status(&self, status: McpStatus) {
        *self.status.lock().unwrap_or_else(|err| err.into_inner()) = status;
    }

    pub(crate) fn status(&self) -> McpStatus {
        self.status
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }
}

pub(crate) type SharedState = Arc<McpState>;

#[cfg(test)]
mod grant_tests {
    use super::grant_allows_silent;
    use crate::config::settings::McpClientGrant;

    fn grant(trusted: bool, last_auth_unix: u64, perms: &[&str]) -> McpClientGrant {
        McpClientGrant {
            id: "Claude Desktop 0.7.2".to_string(),
            permissions: perms.iter().map(|key| (*key).to_string()).collect(),
            last_auth_unix,
            trusted,
        }
    }

    const WINDOW: u64 = 15 * 60;

    /// 永久信任：无论多久没来都直接放行。
    #[test]
    fn trusted_client_never_prompts() {
        assert!(grant_allows_silent(
            &grant(true, 0, &["read"]),
            1_000_000,
            WINDOW
        ));
    }

    /// 一个租约空闲期内来过的客户端不再被弹窗 —— 这正是"每次操作都弹"要解决的问题。
    #[test]
    fn recent_grant_within_the_window_is_silent() {
        let now = 1_000_000;
        assert!(grant_allows_silent(
            &grant(false, now - 60, &["read"]),
            now,
            WINDOW
        ));
        assert!(grant_allows_silent(
            &grant(false, now - WINDOW + 1, &["read"]),
            now,
            WINDOW
        ));
    }

    /// 超过一个租约空闲期就要重新授权。
    #[test]
    fn stale_grant_requires_a_new_prompt() {
        let now = 1_000_000;
        assert!(!grant_allows_silent(
            &grant(false, now - WINDOW, &["read"]),
            now,
            WINDOW
        ));
        assert!(!grant_allows_silent(
            &grant(false, now - 3600, &["read"]),
            now,
            WINDOW
        ));
    }

    /// 上次一位权限都没给：不算"已授权"，要重新问 —— 但**勾了永久信任的例外**，
    /// 否则设置页里那个开关对这类客户端等于摆设。
    #[test]
    fn empty_permissions_prompt_unless_trusted() {
        assert!(!grant_allows_silent(
            &grant(false, 1_000_000, &[]),
            1_000_000,
            WINDOW
        ));
        assert!(grant_allows_silent(
            &grant(true, 1_000_000, &[]),
            1_000_000,
            WINDOW
        ));
    }

    /// 从没授权过（时间戳 0）的老配置：必须走弹窗，不能被算进免弹窗口。
    #[test]
    fn never_authorised_grant_prompts() {
        assert!(!grant_allows_silent(
            &grant(false, 0, &["read"]),
            1_000_000,
            WINDOW
        ));
    }
}

#[cfg(test)]
mod close_reason_tests {
    use super::CloseReason;

    const ALL: [CloseReason; 8] = [
        CloseReason::Denied,
        CloseReason::Timeout,
        CloseReason::Disconnected,
        CloseReason::TokenRotated,
        CloseReason::Idle,
        CloseReason::Delete,
        CloseReason::NotFound,
        CloseReason::Stopped,
    ];

    /// 每个变体都要有中英两套文案，且非空 —— 漏一个界面上就是空白。
    #[test]
    fn every_reason_has_a_non_empty_label_in_both_languages() {
        for reason in ALL {
            assert!(!reason.label(false).is_empty(), "{reason:?} 缺中文文案");
            assert!(!reason.label(true).is_empty(), "{reason:?} 缺英文文案");
            assert_ne!(reason.label(false), reason.label(true));
        }
    }

    /// 告警色只给"用户需要看一眼"的原因；客户端自己关、闲置回收都不该报警。
    #[test]
    fn alarm_covers_only_user_actionable_reasons() {
        for reason in [
            CloseReason::Denied,
            CloseReason::Timeout,
            CloseReason::TokenRotated,
            CloseReason::Stopped,
        ] {
            assert!(reason.alarm(), "{reason:?} 应该告警");
        }
        for reason in [
            CloseReason::Disconnected,
            CloseReason::Idle,
            CloseReason::Delete,
            CloseReason::NotFound,
        ] {
            assert!(!reason.alarm(), "{reason:?} 不该告警");
        }
    }

    /// `as_str` 是审计字段，必须与变体一一对应且不重复。
    #[test]
    fn as_str_values_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for reason in ALL {
            assert!(seen.insert(reason.as_str()), "{} 重复", reason.as_str());
        }
    }
}

#[cfg(test)]
mod client_identity_tests {
    use super::{ClientLease, find_grant};
    use crate::config::settings::McpClientGrant;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn lease(name: &str, version: &str, peer: Option<IpAddr>) -> ClientLease {
        let mut lease = ClientLease::new("session-1".to_string(), 1, peer);
        lease.client_name = name.to_string();
        lease.client_version = version.to_string();
        lease
    }

    /// 不同设备上的同名客户端必须得到不同身份 —— 这是"一个 token 多设备"能分开管的前提。
    #[test]
    fn same_name_on_different_devices_gets_distinct_ids() {
        let a = lease(
            "rudder",
            "1.0.0",
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5))),
        );
        let b = lease(
            "rudder",
            "1.0.0",
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 9))),
        );
        assert_eq!(a.client_id().as_deref(), Some("rudder 1.0.0@192.168.1.5"));
        assert_eq!(b.client_id().as_deref(), Some("rudder 1.0.0@192.168.1.9"));
        assert_ne!(a.client_id(), b.client_id());
    }

    /// 同一台设备上的两个连接共享身份（`@local`），这与"同一台机器本来就该共享信任"一致。
    #[test]
    fn same_device_shares_the_id() {
        let first = lease("rudder", "1.0.0", Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        let second = lease("rudder", "1.0.0", Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_eq!(first.client_id(), second.client_id());
    }

    /// 回环地址归一化：127.0.0.1 与 ::1 是同一台本机，不该被当成两台设备。
    #[test]
    fn loopback_is_normalised_to_local() {
        let v4 = lease("rudder", "1.0.0", Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        let v6 = lease("rudder", "1.0.0", Some(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert_eq!(v4.client_id().as_deref(), Some("rudder 1.0.0@local"));
        assert_eq!(v4.client_id(), v6.client_id());
    }

    /// 拿不到来源时退回旧格式（测试环境与老路径仍要能工作）。
    #[test]
    fn missing_peer_falls_back_to_legacy_id() {
        assert_eq!(
            lease("rudder", "1.0.0", None).client_id().as_deref(),
            Some("rudder 1.0.0")
        );
    }

    /// 没上报名字的客户端不记忆（返回 None）。
    #[test]
    fn unnamed_client_has_no_id() {
        assert_eq!(lease("", "1.0.0", None).client_id(), None);
    }

    /// 升级前的老记录没有 `@ip` 后缀：要能被回退命中，否则升级后全员重授权。
    #[test]
    fn legacy_grant_is_matched_before_the_suffix() {
        let legacy = McpClientGrant {
            id: "rudder 1.0.0".to_string(),
            permissions: vec!["read".to_string()],
            last_auth_unix: 1,
            trusted: false,
        };
        let fresh = McpClientGrant {
            id: "rudder 1.0.0@192.168.1.5".to_string(),
            permissions: vec!["read".to_string(), "exec".to_string()],
            last_auth_unix: 2,
            trusted: true,
        };
        let grants = vec![legacy.clone(), fresh.clone()];

        // 精确命中优先：新格式存在时不会被老记录抢走。
        let hit = find_grant(&grants, "rudder 1.0.0@192.168.1.5").expect("应命中新记录");
        assert_eq!(hit.id, fresh.id);
        assert!(hit.trusted);

        // 另一台设备只能命中老记录（过渡期行为）。
        let fallback = find_grant(&grants, "rudder 1.0.0@192.168.1.9").expect("应回退命中老记录");
        assert_eq!(fallback.id, legacy.id);
        assert!(!fallback.trusted);
    }
}
