//! 进程内共享状态：租约表、配置热更新、审计封装。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use rmcp::RoleServer;
use rmcp::service::Peer;

use crate::resource::TabStatuses;
use crate::terminal::TermBuffers;

use super::audit_types::session_close_record;
use super::config::{GrantUpdate, McpConfigSnapshot, McpStatus};
use super::labels::CloseReason;
use super::lease::{ClientLease, ConsentState, McpSessionInfo, find_grant, grant_perms, session8};
use super::perms::Permissions;

/// 界面侧工具的共享句柄。四个都是 `Arc`，MCP 线程可以直接读，
/// 不必绕回 UI 线程 —— 读屏与状态查询因此不受 UI 事件循环节流影响。
#[derive(Clone)]
pub(crate) struct AppHandles {
    pub(crate) bufs: TermBuffers,
    pub(crate) tab_statuses: TabStatuses,
    /// 每标签页的 SFTP 通道。文件读写不需要经过 UI 线程。
    pub(crate) sftp_handles: crate::sftp::SftpHandles,
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
    /// `get_host_info` 的现场采样器。懒创建：只有快照陈旧时才用得上。
    ///
    /// 不读 `app.local_snap` 就够的原因见 `host_info()`：窗口隐藏时采样整拍跳过，
    /// 读到的可能是几分钟前的数字。
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
                crate::mcp::record_event(session_close_record(lease, reason, None));
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
                session8(&lease.session_id),
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
            Some(grant) => grant_perms(grant),
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
        crate::mcp::record_event(session_close_record(lease, reason, tool));
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

