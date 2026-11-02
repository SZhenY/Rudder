//! 配置快照与运行状态：启动参数、审计容量、对外状态。

use std::net::SocketAddr;
use crate::config::ConfigStore;
use crate::config::settings::McpClientGrant;
use super::auth;
use super::error::McpErrorKind;

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
            // 连接一律走确认框；只有开发令牌那条口子会置成 Auto。
            consent: ConsentPolicy::Session,
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

impl McpStatus {
    /// 可直接粘贴给 AI agent 的地址清单。
    pub(crate) fn urls(&self) -> Vec<String> {
        self.listeners
            .iter()
            .map(|addr| format!("http://{addr}/mcp"))
            .collect()
    }
}

/// 一次授权变更的结果，供 UI 线程写回"按客户端记住"的配置。
#[derive(Debug, Clone, Default)]
pub(crate) struct GrantUpdate {
    pub(crate) client_id: Option<String>,
    pub(crate) granted: Vec<String>,
}

