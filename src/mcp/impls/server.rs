//! 服务生命周期：解析监听地址、绑定、优雅停止、状态回填。
//!
//! 多网口 = 多个 listener，各绑各自 IP，**永不绑 `0.0.0.0`**：这样"实际暴露了
//! 哪些地址"与设置页勾选的内容一一对应，用户能核对。端口绑定本身即互斥体，
//! 不需要额外的单实例锁 —— 第二份 Rudder 会在这里拿到 EADDRINUSE。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};

use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
use sysinfo::Networks;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use super::http;
use super::types::{
    AppHandles, CloseReason, McpConfigSnapshot, McpSessionInfo, McpState, McpStatus, SharedState,
};

struct Active {
    cancel: CancellationToken,
    state: SharedState,
}

static ACTIVE: OnceLock<Mutex<Option<Active>>> = OnceLock::new();

/// 启动时记下的运行期依赖：设置页改完配置后要能在没有 `AppContext` 的地方重绑。
static CONTEXT: OnceLock<(Handle, AppHandles)> = OnceLock::new();

fn active() -> &'static Mutex<Option<Active>> {
    ACTIVE.get_or_init(|| Mutex::new(None))
}

/// 在 `app::run` 里调一次，记下 runtime 与只读句柄。
pub(crate) fn set_context(rt: &Handle, app: AppHandles) {
    let _ = CONTEXT.set((rt.clone(), app));
}

/// 配置变了之后重新应用（监听面不变时只换权限/令牌，不断连接）。
pub(crate) fn reload(snapshot: McpConfigSnapshot) {
    if let Some((rt, app)) = CONTEXT.get() {
        start(rt, snapshot, app.clone());
    }
}

/// 当前状态。没有活动服务时返回默认（stopped）。
pub(crate) fn status() -> McpStatus {
    let guard = active().lock().unwrap_or_else(|err| err.into_inner());
    match guard.as_ref() {
        Some(running) => running.state.status(),
        None => McpStatus::default(),
    }
}

/// 当前接入的客户端列表（AI 页的客户端卡读它）。
///
/// 顺带清理空闲过期的租约；状态锁都是短临界区，UI 线程不会卡住。
pub(crate) fn sessions_info() -> Vec<McpSessionInfo> {
    let state = {
        let guard = active().lock().unwrap_or_else(|err| err.into_inner());
        match guard.as_ref() {
            Some(running) => running.state.clone(),
            None => return Vec::new(),
        }
    };
    state.sessions_info()
}

/// 用户在 AI 页点"断开"：移除该客户端租约。它下一次请求拿 404。
pub(crate) fn disconnect_session(session_id: &str) {
    let state = {
        let guard = active().lock().unwrap_or_else(|err| err.into_inner());
        match guard.as_ref() {
            Some(running) => running.state.clone(),
            None => return,
        }
    };
    state.close_session(session_id, CloseReason::Disconnected);
}

/// 客户端卡上的权限开关。返回 null 表示 session 已不在或位未识别。
pub(crate) fn set_session_permission(
    session_id: &str,
    key: &str,
    enabled: bool,
) -> Option<super::types::GrantUpdate> {
    let state = {
        let guard = active().lock().unwrap_or_else(|err| err.into_inner());
        guard.as_ref()?.state.clone()
    };
    state.set_permission(session_id, key, enabled)
}

/// AI 页"最近调用"列表。
pub(crate) fn recent_calls() -> Vec<super::audit_types::AuditEntry> {
    super::audit::recent()
}

/// 审计写盘是否处于降级状态。
pub(crate) fn audit_degraded() -> bool {
    super::audit::degraded()
}

/// 按配置起服务。已有一份在跑就先停掉（改端口 / 改网口走同一条路径）。
pub(crate) fn start(rt: &Handle, snapshot: McpConfigSnapshot, app: AppHandles) {
    // 审计线程与保留天数：线程只起一次；天数放在热更新之前设，改完立刻生效，
    // 不用等到下次重绑监听。
    super::audit::set_retention(snapshot.audit.retention_days);
    super::audit::init();

    // 监听面没变就只换配置：权限位与令牌的改动不该掐断已经连上的客户端。
    {
        let guard = active().lock().unwrap_or_else(|err| err.into_inner());
        if let Some(running) = guard.as_ref()
            && snapshot.enabled
            && running.state.config().same_listeners(&snapshot)
        {
            let previous = running.state.config();
            *running
                .state
                .cfg
                .write()
                .unwrap_or_else(|err| err.into_inner()) = snapshot;
            // 令牌换了就释放唯一席位：旧客户端随后每个请求都是 401，若还占着租约，
            // 新客户端要等 15 分钟空闲才连得上。
            if previous.token != running.state.config().token {
                running.state.close_all(CloseReason::TokenRotated);
            }
            return;
        }
    }
    stop();
    if !snapshot.enabled {
        return;
    }
    let state: SharedState = Arc::new(McpState::new(snapshot, app, rt.clone()));
    let cancel = CancellationToken::new();
    if let Ok(mut slot) = active().lock() {
        *slot = Some(Active {
            cancel: cancel.clone(),
            state: state.clone(),
        });
    }
    rt.spawn(run(state, cancel));
}

/// 停止监听。返回后端口会释放（listener 随任务结束被 drop）。
pub(crate) fn stop() {
    let taken = active()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .take();
    if let Some(active) = taken {
        active.cancel.cancel();
    }
}

async fn run(state: SharedState, cancel: CancellationToken) {
    let cfg = state.config();
    let addrs = desired_addrs(&cfg);
    if addrs.is_empty() {
        state.set_status(McpStatus {
            running: false,
            listeners: Vec::new(),
            error: Some("没有可监听的地址".to_string()),
        });
        return;
    }

    let mut bound = Vec::new();
    for addr in &addrs {
        match bind_with_retry(*addr).await {
            Ok(listener) => bound.push((*addr, listener)),
            Err(err) => {
                // 任一地址失败就整体放弃：留着半套监听会让人误以为配置生效了。
                state.set_status(McpStatus {
                    running: false,
                    listeners: Vec::new(),
                    error: Some(format!("{addr} 绑定失败: {err}")),
                });
                return;
            }
        }
    }

    let listeners: Vec<SocketAddr> = bound.iter().map(|(addr, _)| *addr).collect();
    state.set_status(McpStatus {
        running: true,
        listeners: listeners.clone(),
        error: None,
    });

    let http_config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(allowed_hosts(&listeners))
        .with_cancellation_token(cancel.child_token());

    for (_, listener) in bound {
        let router = http::router(state.clone(), http_config.clone());
        let shutdown = cancel.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move { shutdown.cancelled().await })
                .await;
        });
    }

    cancel.cancelled().await;
    let mut status = state.status();
    status.running = false;
    state.set_status(status);
}

/// 绑端口。改端口 / 重开监听时旧 socket 可能还没被释放（`stop()` 只是发取消信号），
/// `AddrInUse` 短暂重试几次，避免"改个端口就报启动失败"。
async fn bind_with_retry(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let mut last = None;
    for _ in 0..10 {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                last = Some(err);
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            }
            Err(err) => return Err(err),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("address already in use")))
}

/// 实际要绑定的地址集合。
fn desired_addrs(cfg: &McpConfigSnapshot) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    if cfg.loopback {
        out.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), cfg.port));
    }
    if cfg.lan_enabled {
        for ip in lan_addresses(&cfg.lan_ifaces) {
            out.push(SocketAddr::new(ip, cfg.port));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 勾选网口的 IPv4 地址。只取可用的单播地址：跳过回环、链路本地（169.254）与未指定。
fn lan_addresses(ifaces: &[String]) -> Vec<IpAddr> {
    if ifaces.is_empty() {
        return Vec::new();
    }
    let networks = Networks::new_with_refreshed_list();
    let mut out = Vec::new();
    for (name, data) in networks.iter() {
        if !ifaces.iter().any(|wanted| wanted == name) {
            continue;
        }
        for network in data.ip_networks() {
            if let IpAddr::V4(v4) = network.addr
                && !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_unspecified()
            {
                out.push(network.addr);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// `Host` 头白名单：rmcp 默认只放行回环，局域网监听必须把实际地址加进去，
/// 否则客户端拿 `192.168.x.y:8765` 请求会被它自己挡掉。
fn allowed_hosts(listeners: &[SocketAddr]) -> Vec<String> {
    let mut hosts = vec!["localhost".to_string()];
    for addr in listeners {
        hosts.push(addr.ip().to_string());
        hosts.push(addr.to_string());
    }
    hosts.sort();
    hosts.dedup();
    hosts
}
