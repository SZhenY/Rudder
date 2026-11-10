//! 工具实现。
//!
//! 约定：返回值直接序列化成 MCP 的 `structuredContent`；错误用 `McpError`，
//! 由 handler 包装成 `isError: true` 的结构化结果（**不是** JSON-RPC 协议错误）——
//! 协议错误客户端只会看到 "Tool result missing due to internal error"，看不到原因。
//!
//! 数据来源分两类：
//! * 只读的终端缓冲与标签页状态是 `Arc`，直接读，不经过 UI 线程；
//! * 需要 `ConfigStore` / `SessionHandle` 的操作经 `crate::app::mcp_ui` 回 UI 线程。

use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Value, json};

use super::args::{bool_arg, int_arg, opt_str_arg, raw_str_arg, str_arg};
use super::labels::state_name;
use super::pty_cmd;
use super::text::{tail_bytes, truncate_chars};
use super::types::{McpError, SharedState};
use crate::config::SessionKind;
use crate::terminal::TextSnapshot;

pub(super) async fn call(
    state: &SharedState,
    name: &str,
    args: &Value,
) -> Result<Value, McpError> {
    match name {
        "get_server_info" => Ok(server_info(state)),
        "list_sessions" => crate::app::mcp_ui::list_sessions(opt_str_arg(args, "group")).await,
        "get_session" => crate::app::mcp_ui::get_session(str_arg(args, "session_id")?).await,
        "list_tabs" => Ok(list_tabs(state)),
        "get_tab_status" => tab_status(state, args).await,
        "get_host_info" => host_info(state).await,
        "kill_process" => kill_process(state, args).await,
        "get_update_status" => crate::app::mcp_ui::update_status().await,
        "sftp_mkdir" => sftp_mkdir(state, args).await,
        "sftp_remove" => sftp_remove(state, args).await,
        "sftp_rename" => sftp_rename(state, args).await,
        "sftp_chmod" => sftp_chmod(state, args).await,
        "read_terminal" => read_terminal(state, args),
        "wait_for_output" => wait_for_output(state, args).await,
        "open_session" => {
            crate::app::mcp_ui::open_session(
                str_arg(args, "session_id")?,
                bool_arg(args, "trust_unknown_host_key", false),
            )
            .await
        }
        "close_session" => crate::app::mcp_ui::close_session(str_arg(args, "tab_id")?).await,
        "send_input" => {
            crate::app::mcp_ui::send_input(
                str_arg(args, "tab_id")?,
                raw_str_arg(args, "text"),
                bool_arg(args, "enter", true),
            )
            .await
        }
        "run_command" => run_command(state, args).await,
        "get_settings" => {
            let keys = args
                .get("keys")
                .and_then(Value::as_array)
                .map(|keys| {
                    keys.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            crate::app::mcp_ui::get_settings(
                keys,
                session_perms(state).allows("settings_high"),
            )
            .await
        }
        "set_settings" => {
            let values = args
                .get("values")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if values.is_empty() {
                return Err(McpError::Invalid(
                    "`values` must be a non-empty object".to_string(),
                ));
            }
            crate::app::mcp_ui::set_settings(
                values,
                session_perms(state).allows("settings_high"),
            )
            .await
        }
        "upsert_host" => crate::app::mcp_ui::upsert_host(args.clone()).await,
        "delete_host" => {
            crate::app::mcp_ui::delete_host(
                str_arg(args, "session_id")?,
                bool_arg(args, "close_open_tabs", false),
            )
            .await
        }
        "sftp_list" => sftp_list(state, args).await,
        "sftp_download" => sftp_download(state, args).await,
        "sftp_upload" => sftp_upload(state, args).await,
        "run_action" => crate::app::mcp_ui::run_action(str_arg(args, "action")?).await,
        other => Err(McpError::Invalid(format!("unknown tool: {other}"))),
    }
}

#[path = "calls_status.rs"] mod status;
pub(crate) use status::*;
#[path = "calls_terminal.rs"] mod terminal;
pub(crate) use terminal::*;
#[path = "calls_exec.rs"] mod exec;
pub(crate) use exec::*;
#[path = "calls_sftp.rs"] mod sftp;
pub(crate) use sftp::*;

#[cfg(test)]
#[path = "calls_tests.rs"]
mod calls_tests_aggregate;
