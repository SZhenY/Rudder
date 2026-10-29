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

#[path = "impls/auth.rs"]
mod auth;
#[path = "impls/calls.rs"]
mod calls;
#[path = "impls/handler.rs"]
mod handler;
#[path = "impls/http.rs"]
mod http;
#[path = "impls/server.rs"]
mod server;
#[path = "impls/tools.rs"]
mod tools;
#[path = "struct/types.rs"]
mod types;

pub(crate) use auth::new_token;
pub(crate) use server::{reload, set_context, start, status, stop};
pub(crate) use types::{AppHandles, McpConfigSnapshot, McpError};
