use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::config::ConfigStore;
use crate::resource::{LocalSnap, TabStatuses};
use crate::terminal::TermBuffers;

use super::auth;

/// 服务配置快照。权限位与令牌的改动只换这一份（热更新），不重绑监听；
/// 只有 `same_listeners` 判为不同才需要重建 listener。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpConfigSnapshot {
    pub(crate) enabled: bool,
    pub(crate) port: u16,
    pub(crate) loopback: bool,
    pub(crate) lan_enabled: bool,
    pub(crate) lan_ifaces: Vec<String>,
    pub(crate) token: String,
    pub(crate) perms: Permissions,
}

impl McpConfigSnapshot {
    /// 从配置取一份快照。服务已启用但还没有令牌时**当场生成并落盘** ——
    /// 空令牌会让鉴权退化成"任何串都能过"，所以这个值不允许为空。
    pub(crate) fn from_store(store: &mut ConfigStore) -> Self {
        // 开发口子：仅 debug 构建生效，供 curl / 端到端测试免改配置。
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
                perms: Permissions {
                    terminal_input: true,
                    exec: true,
                    settings_write: true,
                    host_write: true,
                    session_open_close: true,
                    file_transfer: true,
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
            perms: Permissions {
                terminal_input: store.mcp_allow_terminal_input(),
                exec: store.mcp_allow_exec(),
                settings_write: store.mcp_allow_settings_write(),
                host_write: store.mcp_allow_host_write(),
                session_open_close: store.mcp_allow_session_open_close(),
                file_transfer: store.mcp_allow_file_transfer(),
            },
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

/// 五个授权位：出厂全关，设置页逐项开启。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Permissions {
    pub(crate) terminal_input: bool,
    pub(crate) exec: bool,
    pub(crate) settings_write: bool,
    pub(crate) host_write: bool,
    pub(crate) session_open_close: bool,
    pub(crate) file_transfer: bool,
}

impl Permissions {
    /// 未知权限名一律拒绝（fail closed）—— 新增工具忘了登记权限时宁可不可用。
    pub(crate) fn allows(&self, key: &str) -> bool {
        match key {
            "terminal_input" => self.terminal_input,
            "exec" => self.exec,
            "settings_write" => self.settings_write,
            "host_write" => self.host_write,
            "session_open_close" => self.session_open_close,
            "file_transfer" => self.file_transfer,
            _ => false,
        }
    }

    pub(crate) fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "terminal_input": self.terminal_input,
            "exec": self.exec,
            "settings_write": self.settings_write,
            "host_write": self.host_write,
            "session_open_close": self.session_open_close,
            "file_transfer": self.file_transfer,
        })
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

/// 当前客户端租约：单客户端模式下第二个 `initialize` 会被 409 挡回。
#[derive(Debug, Clone)]
pub(crate) struct ClientLease {
    pub(crate) session_id: String,
    pub(crate) last_seen: Instant,
}

impl ClientLease {
    /// 空闲这么久就自动释放，避免客户端崩溃后端口永久占着。
    pub(crate) const IDLE: Duration = Duration::from_secs(15 * 60);

    pub(crate) fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_seen) > Self::IDLE
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
        }
    }
}

/// 处理请求时共享的状态。全部字段可跨线程，`McpHandler` 只持有它。
pub(crate) struct McpState {
    pub(crate) cfg: RwLock<McpConfigSnapshot>,
    pub(crate) status: Mutex<McpStatus>,
    pub(crate) lease: tokio::sync::Mutex<Option<ClientLease>>,
    pub(crate) requests: tokio::sync::Semaphore,
    pub(crate) app: AppHandles,
}

impl McpState {
    pub(crate) fn new(cfg: McpConfigSnapshot, app: AppHandles) -> Self {
        Self {
            cfg: RwLock::new(cfg),
            status: Mutex::new(McpStatus::default()),
            lease: tokio::sync::Mutex::new(None),
            // 单客户端也留出余量：一次工具调用可能同时压着读屏与状态采样。
            requests: tokio::sync::Semaphore::new(8),
            app,
        }
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
