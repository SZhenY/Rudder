use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use rmcp::RoleServer;
use rmcp::service::Peer;

use crate::config::settings::McpClientGrant;
use crate::config::ConfigStore;
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
    pub(crate) error: Option<String>,
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

/// 一个已接入客户端的可展示形态（AI 页的客户端卡）。
#[derive(Debug, Clone)]
pub(crate) struct McpSessionInfo {
    pub(crate) session_id: String,
    pub(crate) client: String,
    pub(crate) connected_wall: String,
    pub(crate) connected_secs: u64,
    pub(crate) granted: Permissions,
}

/// 一个客户端会话。多个客户端各自一份，权限只活在这里。
#[derive(Debug, Clone)]
pub(crate) struct ClientLease {
    pub(crate) session_id: String,
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
        }
    }
}

impl ClientLease {
    /// 空闲这么久就自动释放，避免客户端崩溃后端口永久占着。
    pub(crate) const IDLE: Duration = Duration::from_secs(15 * 60);

    /// 弹窗后等用户决定的时长。超时 = 未接入，租约会被移除。
    pub(crate) const CONSENT_TIMEOUT: Duration = Duration::from_secs(45);

    pub(crate) fn new(session_id: String, epoch: u64) -> Self {
        Self {
            session_id,
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

    /// 用于记忆授权的身份；未上报名字时返回 `None`（不持久化 unknown client）。
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
        let cleaned: String = raw.chars().filter(|ch| !ch.is_control()).take(128).collect();
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned)
        }
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
    pub(crate) cfg: RwLock<McpConfigSnapshot>,
    pub(crate) status: Mutex<McpStatus>,
    /// 多客户端：key 是 `mcp-session-id`。权限只活在这里，配置里没有全局位。
    pub(crate) leases: Mutex<HashMap<String, ClientLease>>,
    pub(crate) requests: tokio::sync::Semaphore,
    /// 串行化连接授权弹窗：多个客户端同时接入时逐个弹。
    pub(crate) consent_gate: tokio::sync::Mutex<()>,
    /// 租约 epoch 的全局序号。跨租约单调，在途弹窗不会写到重连后的新会话上。
    pub(crate) epoch_seq: AtomicU64,
    /// UI 线程不能 await；授权变化后的 `list_changed` 用这个 handle 派发。
    rt: tokio::runtime::Handle,
    pub(crate) app: AppHandles,
}

impl McpState {
    /// 同时接入的客户端上限。每条租约都很轻，但工具调用共享同一个 UI 线程。
    pub(crate) const MAX_CLIENTS: usize = 8;

    pub(crate) fn new(
        cfg: McpConfigSnapshot,
        app: AppHandles,
        rt: tokio::runtime::Handle,
    ) -> Self {
        Self {
            cfg: RwLock::new(cfg),
            status: Mutex::new(McpStatus::default()),
            leases: Mutex::new(HashMap::new()),
            // 一次工具调用可能同时压着读屏与状态采样。
            requests: tokio::sync::Semaphore::new(8),
            consent_gate: tokio::sync::Mutex::new(()),
            epoch_seq: AtomicU64::new(1),
            rt,
            app,
        }
    }

    pub(crate) fn next_epoch(&self) -> u64 {
        self.epoch_seq.fetch_add(1, Ordering::Relaxed)
    }

    fn lease_map(&self) -> std::sync::MutexGuard<'_, HashMap<String, ClientLease>> {
        self.leases
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }

    /// 新会话建租约。超过 `MAX_CLIENTS` 返回 false（调用方回 429）。
    pub(crate) fn insert_lease(&self, session_id: String) -> bool {
        let mut leases = self.lease_map();
        if leases.len() >= Self::MAX_CLIENTS {
            return false;
        }
        let epoch = self.next_epoch();
        leases.insert(session_id.clone(), ClientLease::new(session_id, epoch));
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
        for lease in drained {
            self.audit_close(&lease, reason, None);
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
                    granted: lease.granted.keys().into_iter().map(str::to_string).collect(),
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
        items
            .into_iter()
            .map(|lease| McpSessionInfo {
                session_id: lease.session_id.clone(),
                client: lease.client_label(),
                connected_wall: lease.connected_wall.clone(),
                connected_secs: lease.connected_at.elapsed().as_secs(),
                granted: lease.granted,
            })
            .collect()
    }

    fn audit_close(&self, lease: &ClientLease, reason: CloseReason, tool: Option<&str>) {
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

    pub(crate) fn config(&self) -> McpConfigSnapshot {
        self.cfg
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    pub(crate) fn set_status(&self, status: McpStatus) {
        *self
            .status
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = status;
    }

    pub(crate) fn status(&self) -> McpStatus {
        self.status
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }
}

pub(crate) type SharedState = Arc<McpState>;
