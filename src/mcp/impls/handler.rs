//! `rmcp::ServerHandler` 实现：工具列表与调用入口。
//!
//! 这一层只做三件事：报能力、列工具、按权限门分发。真正的操作在 `calls`。

use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
};
use serde_json::Value;

use super::calls;
use super::tools;
use super::types::SharedState;

#[derive(Clone)]
pub(super) struct McpHandler {
    state: SharedState,
}

impl McpHandler {
    pub(super) fn new(state: SharedState) -> Self {
        Self { state }
    }
}

impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rudder", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Operates a running Rudder desktop client: list saved hosts, open or close \
                 sessions, read terminal screens, type into the PTY, run one-shot commands, \
                 and change settings. Every write tool has its own permission; call \
                 get_server_info to see which ones are granted.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = serde_json::from_value(tools::definitions())
            .map_err(|_| ErrorData::internal_error("invalid tool definitions", None))?;
        Ok(ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let name = request.name.to_string();
        let cfg = self.state.config();
        if let Some(permission) = tools::required_perm(&name)
            && !cfg.perms.allows(permission)
        {
            // 权限不足是**工具结果**而不是协议错误：客户端要能看到原因。
            return Ok(CallToolResult::structured_error(serde_json::json!({
                "error": "permission_denied",
                "permission": permission,
                "hint": "grant it in Rudder → Settings → AI",
            })));
        }
        let args = Value::Object(request.arguments.unwrap_or_default());
        // 兜底总超时：`run_command` 自身上限 300s，再留 10s 余量。没有这一层，
        // 任何一个卡住的工具都会让 HTTP 请求永远挂着（客户端只能等自己超时）。
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(310),
            calls::call(&self.state, &name, &args),
        )
        .await
        .unwrap_or(Err(crate::mcp::McpError::TimedOut));
        Ok(match outcome {
            Ok(value) => CallToolResult::structured(value),
            Err(err) => CallToolResult::structured_error(serde_json::json!({
                "error": err.message(),
                "code": err.code(),
                "tool": name,
            })),
        })
    }
}
