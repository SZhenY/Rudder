//! 进程内 MCP server：外部 AI agent 通过 HTTP 操作这个正在运行的 Rudder。
//!
//! 与上游 meatshell 的实现差异：这里的工具操作的是**本进程已有的界面状态**
//! （打开的标签页、终端缓冲、配置存储），而不是另开一条 SSH 连接。因此
//! `McpHandler` 不能持有任何 Slint 句柄（`Rc` 不满足 `Send + Sync`），需要界面
//! 状态的操作统一经 `crate::app::mcp_ui` 送回 UI 线程执行。
//!
//! 模块边界：
//! * `server` —— 生命周期（多地址绑定 / 停止 / 状态回填）
//! * `http`   —— axum 边界：鉴权、单客户端租约、请求体上限
//! * `handler`—— `rmcp::ServerHandler`：工具列表与调用入口
//! * `tools`  —— 工具的 JSON schema 与权限位映射
//! * `calls`  —— 工具实现
//! * `auth`   —— 令牌生成与常量时间比对

#[path = "impls/audit.rs"]
mod audit;
#[path = "struct/audit.rs"]
mod audit_types;
#[path = "impls/auth.rs"]
mod auth;
#[path = "impls/calls.rs"]
mod calls;
#[path = "impls/handler.rs"]
mod handler;
#[path = "impls/http.rs"]
mod http;
#[path = "struct/perms.rs"]
mod perms;
#[path = "impls/server.rs"]
mod server;
#[path = "impls/tools.rs"]
mod tools;
#[path = "struct/types.rs"]
mod types;

pub(crate) use auth::new_token;
pub(crate) use audit_types::{AuditEntry, redact_args};
pub(crate) use server::{
    audit_degraded, disconnect_session, recent_calls, reload, sessions_info,
    set_session_permission, set_context, start, status, stop,
};
pub(crate) use perms::{
    PERMISSION_BITS, PERMISSIONS_REV, Permissions, bits_with_heads, is_high_risk_setting,
};
#[cfg(test)]
pub(crate) use perms::HIGH_RISK_SETTINGS;
pub(crate) use types::{AppHandles, ConsentPolicy, GrantUpdate, McpConfigSnapshot, McpError};

tokio::task_local! {
    /// 本次工具调用花在"等 UI 线程"上的毫秒数。`mcp_ui::call` 累加，
    /// `handler` 收尾时读出来写进审计。没有作用域时静默忽略。
    static UI_WAIT_MS: std::cell::Cell<u64>;
    /// 本次调用的会话 id。`calls` 里少数需要知道"是哪个客户端"的工具
    /// （`get_server_info`、`set_settings`/`get_settings` 的高危键脱敏）读它。
    static SESSION_ID: String;
}

/// 在"当前会话"的作用域里跑一个 future。
pub(crate) async fn with_session<F>(session: String, future: F) -> F::Output
where
    F: std::future::Future,
{
    SESSION_ID.scope(session, future).await
}

pub(crate) fn current_session() -> Option<String> {
    SESSION_ID.try_with(|id| id.clone()).ok()
}

/// 记一条审计（写文件 + 进环形缓冲）。调用方不需要处理失败。
pub(crate) fn record_audit(record: serde_json::Value, entry: AuditEntry) {
    audit::record(record, entry);
}

/// 只进文件、不进"最近调用"列表的事件（会话建立 / 关闭）。
pub(crate) fn record_event(record: serde_json::Value) {
    audit::record_only(record);
}

pub(crate) fn add_ui_wait(ms: u64) {
    let _ = UI_WAIT_MS.try_with(|cell| cell.set(cell.get() + ms));
}

/// 在"累计 UI 等待"的作用域里跑一个 future，返回 (结果, ui_ms)。
pub(crate) async fn with_ui_wait<F>(future: F) -> (F::Output, u64)
where
    F: std::future::Future,
{
    UI_WAIT_MS
        .scope(std::cell::Cell::new(0), async move {
            let out = future.await;
            let ms = UI_WAIT_MS.with(|cell| cell.get());
            (out, ms)
        })
        .await
}
