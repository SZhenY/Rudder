//! `rmcp::ServerHandler` 实现：工具列表、连接授权与调用入口。
//!
//! 权限只活在 `ClientLease` 上：连接时弹窗授权，之后在 AI 页的客户端卡上开关。
//! 工具调用本身不再弹窗，缺位直接 `permission_denied`。

use axum::http::request::Parts;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, Extensions, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::{NotificationContext, RequestContext},
};
use serde_json::Value;

use super::calls;
use super::tools;
use super::types::{ClientLease, CloseReason, ConsentState, SharedState};

#[derive(Clone)]
pub(super) struct McpHandler {
    state: SharedState,
}

impl McpHandler {
    pub(super) fn new(state: SharedState) -> Self {
        Self { state }
    }
}

/// 从 rmcp 注入的 `http::request::Parts` 取 `mcp-session-id`。
///
/// Streamable HTTP 的每个后续请求 / 通知都带这个头；rmcp 在 tower 层把
/// `Parts` 注入了 `extensions`（`streamable_http_server/tower.rs:1087-1093`）。
fn session_id_of(extensions: &Extensions) -> Option<String> {
    let parts = extensions.get::<Parts>()?;
    parts
        .headers
        .get("mcp-session-id")?
        .to_str()
        .ok()
        .map(str::to_owned)
}

impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new("rudder", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "Operates a running Rudder desktop client: list saved hosts, open or close \
             sessions, read terminal screens, type into the PTY, run one-shot commands, \
             and change settings. Permissions are granted per client when it connects \
             and can be toggled in Rudder → Settings → AI. Call get_server_info to see \
             which ones this session has.",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let all: Vec<Tool> = serde_json::from_value(tools::definitions())
            .map_err(|_| ErrorData::internal_error("invalid tool definitions", None))?;

        // 没有会话 id（协议异常）不过滤；有租约时按本次会话的授权过滤。
        // Pending 阶段用该客户端记住的授权兜底，避免握手时列表为空；
        // 授权完成后 approve() 会发 tools/list_changed 让客户端重取。
        let filter = session_id_of(&context.extensions).map(|session_id| {
            match self.state.lease(&session_id) {
                Some(lease) if lease.verdict == ConsentState::Granted => lease.granted,
                Some(lease) => lease
                    .client_id()
                    .map(|client_id| self.state.remembered_grant(&client_id))
                    .unwrap_or_default(),
                None => crate::mcp::Permissions::default(),
            }
        });

        let tools = match filter {
            Some(perms) => all
                .into_iter()
                .filter(|tool| tools::listable(&perms, tool.name.as_ref()))
                .collect(),
            None => all,
        };
        Ok(ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        let Some(session_id) = session_id_of(&context.extensions) else {
            tracing::warn!("mcp: initialized without mcp-session-id; cannot bind permissions");
            return;
        };
        let (name, version) = context
            .peer
            .peer_info()
            .map(|info| (info.client_info.name.clone(), info.client_info.version.clone()))
            .unwrap_or_default();
        self.state
            .store_client(&session_id, name, version, context.peer.clone());

        if self.state.config().consent == crate::mcp::ConsentPolicy::Auto {
            // 开发令牌：直接授予全部位，不弹窗。
            let all = crate::mcp::Permissions::from_keys(
                &crate::mcp::PERMISSION_BITS
                    .iter()
                    .map(|bit| bit.key)
                    .collect::<Vec<_>>(),
            );
            self.state.approve(&session_id, all);
            return;
        }

        // 不能在这里等用户点击：这是一次通知请求，客户端不会等它的响应。
        let state = self.state.clone();
        tokio::spawn(request_session_consent(state, session_id));
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let name = request.name.to_string();
        let args = Value::Object(request.arguments.unwrap_or_default());
        let session_id = session_id_of(&context.extensions);

        let needed = match self.authorize(&name, &args, session_id.as_deref()).await {
            Ok(needed) => needed,
            Err(payload) => {
                self.audit_denied(&name, &args, session_id.as_deref(), &payload);
                // 权限不足是**工具结果**而不是协议错误：客户端要能看到原因。
                return Ok(CallToolResult::structured_error(payload));
            }
        };

        // 兜底总超时：`run_command` 自身上限 300s，再留 10s 余量。没有这一层，
        // 任何一个卡住的工具都会让 HTTP 请求永远挂着（客户端只能等自己超时）。
        let started = std::time::Instant::now();
        let session = session_id.clone().unwrap_or_default();
        let (outcome, ui_ms) = tokio::time::timeout(
            std::time::Duration::from_secs(310),
            crate::mcp::with_session(
                session,
                crate::mcp::with_ui_wait(calls::call(&self.state, &name, &args)),
            ),
        )
        .await
        .unwrap_or((Err(crate::mcp::McpError::TimedOut), 0));
        let elapsed_ms = started.elapsed().as_millis() as u64;

        self.audit(
            &name,
            &args,
            &needed,
            session_id.as_deref(),
            &outcome,
            (elapsed_ms, ui_ms),
        );
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

impl McpHandler {
    /// 授权门：只看本次会话的 `granted`。没有全局上限，也不在这里弹窗。
    async fn authorize(
        &self,
        name: &str,
        args: &Value,
        session_id: Option<&str>,
    ) -> Result<crate::mcp::Permissions, Value> {
        // 存活探测 / 能力查询永远放行：客户端启动就会调，挡了它整个连接就没法用。
        if name == "get_server_info" {
            return Ok(crate::mcp::Permissions::default());
        }
        let Some(session_id) = session_id else {
            return Err(session_error(
                "closed",
                "missing MCP session id; initialize again",
            ));
        };
        let Some(lease) = self.state.lease(session_id) else {
            return Err(session_error(
                "closed",
                "this session was closed; initialize again",
            ));
        };
        let needed = match tools::required_perms(&self.state.app.tab_statuses, name, args) {
            Ok(needed) => needed,
            Err(tools::PermLookup::UnknownTool) => {
                return Err(denied_missing(
                    &crate::mcp::Permissions::default(),
                    &crate::mcp::Permissions::default(),
                    "unknown tool",
                ));
            }
            Err(tools::PermLookup::BadArgument(message)) => {
                return Err(serde_json::json!({
                    "error": "invalid_argument",
                    "message": message,
                }));
            }
        };
        match lease.verdict {
            ConsentState::Pending => {
                return Err(session_error(
                    "consent_pending",
                    "waiting for the user to approve this connection in Rudder",
                ));
            }
            ConsentState::Granted => {}
        }
        if needed.covered_by(&lease.granted) {
            Ok(needed)
        } else {
            Err(denied_missing(
                &lease.granted,
                &needed,
                "enable it in Rudder → Settings → AI → this client",
            ))
        }
    }

    /// 落一条 `tool_call` 审计。写盘失败只降级，不影响返回值。
    fn audit(
        &self,
        name: &str,
        args: &Value,
        needed: &crate::mcp::Permissions,
        session_id: Option<&str>,
        outcome: &Result<Value, crate::mcp::McpError>,
        timing: (u64, u64),
    ) {
        let cfg = self.state.config();
        if !cfg.audit.enabled {
            return;
        }
        let (elapsed_ms, ui_ms) = timing;
        let (session8, client, granted, epoch) = self.session_meta(session_id);
        let (args_view, args_bytes, args_truncated) = crate::mcp::redact_args(name, args);
        let (ok, code, error) = match outcome {
            Ok(_) => (true, String::new(), String::new()),
            Err(err) => (false, err.code().to_string(), err.message()),
        };
        let keys = needed.keys();
        let record = serde_json::json!({
            "ts": chrono::Local::now().to_rfc3339(),
            "event": "tool_call",
            "session": session8,
            "client": client,
            "tool": name,
            "perm": keys.first().copied().unwrap_or(""),
            "permissions": keys,
            "ok": ok,
            "code": code,
            "error": error,
            "ms": elapsed_ms,
            "ui_ms": ui_ms,
            "args": args_view,
            "args_bytes": args_bytes,
            "args_truncated": args_truncated,
            "granted": granted.describe(),
            "epoch": epoch,
        });
        let entry = crate::mcp::AuditEntry {
            at: chrono::Local::now().format("%H:%M:%S").to_string(),
            tool: name.to_string(),
            ok,
            code: code.clone(),
            ms: elapsed_ms,
            client: client.clone(),
        };
        crate::mcp::record_audit(record, entry);
    }

    /// 被权限门挡下的调用单独记一条，便于回答"为什么它一直失败"。
    fn audit_denied(
        &self,
        name: &str,
        args: &Value,
        session_id: Option<&str>,
        payload: &Value,
    ) {
        if !self.state.config().audit.enabled {
            return;
        }
        let (session8, client, granted, epoch) = self.session_meta(session_id);
        let (args_view, _, _) = crate::mcp::redact_args(name, args);
        crate::mcp::record_event(serde_json::json!({
            "ts": chrono::Local::now().to_rfc3339(),
            "event": "tool_denied",
            "session": session8,
            "client": client,
            "tool": name,
            "permission": payload.get("permission").cloned().unwrap_or(Value::Null),
            "code": payload.get("code").cloned().unwrap_or(Value::Null),
            "hint": payload.get("hint").cloned().unwrap_or(Value::Null),
            "args": args_view,
            "granted": granted.describe(),
            "epoch": epoch,
        }));
    }

    fn session_meta(
        &self,
        session_id: Option<&str>,
    ) -> (String, String, crate::mcp::Permissions, u64) {
        match session_id.and_then(|id| self.state.lease(id)) {
            Some(lease) => (
                lease.session_id.chars().take(8).collect(),
                lease.client_label(),
                lease.granted,
                lease.epoch,
            ),
            None => (
                String::new(),
                String::new(),
                crate::mcp::Permissions::default(),
                0,
            ),
        }
    }
}

/// 连接建立后弹一次授权框。允许 → 写入授权并记住；拒绝 / 超时 → 移除租约。
pub(super) async fn request_session_consent(state: SharedState, session_id: String) {
    if state.config().consent != crate::mcp::ConsentPolicy::Session {
        return;
    }
    // 同一时间只允许一个询问；多个客户端同时接入时逐个弹。
    let _gate = state.consent_gate.lock().await;
    let Some(lease) = state.lease(&session_id) else {
        return;
    };
    if lease.verdict != ConsentState::Pending {
        return;
    }
    let base = lease
        .client_id()
        .map(|client_id| state.remembered_grant(&client_id))
        .unwrap_or_default();

    let request = crate::app::mcp_ui::ConsentRequest {
        client: lease.client_label(),
        base,
    };
    let reply =
        crate::app::mcp_ui::ask_consent(request, ClientLease::CONSENT_TIMEOUT).await;
    match reply {
        Ok(granted) => {
            if let Some(update) = state.approve(&session_id, granted) {
                crate::app::mcp_ui::remember_client_grant(update).await;
            }
        }
        Err(crate::mcp::McpError::Revoked) => {
            state.close_session(&session_id, CloseReason::Denied);
        }
        Err(_) => {
            // 超时 / 界面不可用：视为未接入，客户端必须重连再弹。
            state.close_session(&session_id, CloseReason::Timeout);
        }
    }
}

fn denied_missing(
    have: &crate::mcp::Permissions,
    needed: &crate::mcp::Permissions,
    hint: &str,
) -> Value {
    let missing = needed.minus(have);
    let keys = missing.keys();
    serde_json::json!({
        "error": "permission_denied",
        // 兼容旧客户端：单数字段是第一个缺的位。
        "permission": keys.first().copied().unwrap_or(""),
        "permissions": keys,
        "hint": hint,
    })
}

fn session_error(code: &str, message: &str) -> Value {
    serde_json::json!({
        "error": message,
        "code": code,
    })
}
