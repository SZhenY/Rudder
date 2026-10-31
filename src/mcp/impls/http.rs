//! axum 边界：鉴权、单客户端租约、请求体上限。
//!
//! 协议解析与 SSE 传输交给 rmcp；这一层只负责"谁能进来"。
//! 顺序：并发许可 → Origin 拒绝 → Bearer 校验 → 方法白名单 → 租约 → body 包装。

use std::sync::Arc;
use std::time::Instant;

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use http_body_util::Limited;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde_json::json;

use super::auth;
use super::handler::McpHandler;
use super::types::{ClientLease, CloseReason, SharedState};

/// 请求体上限。工具调用的参数都是短 JSON，1 MiB 已经远超实际需要。
const MAX_BODY: usize = 1024 * 1024;

pub(super) fn router(state: SharedState, config: StreamableHttpServerConfig) -> Router {
    let factory = state.clone();
    let service = StreamableHttpService::new(
        move || Ok(McpHandler::new(factory.clone())),
        session_manager(),
        config,
    );
    Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(state, guard))
}

/// rmcp 默认在 5 分钟没有任何会话事件后销毁会话。桌面端的 AI 客户端经常隔几分
/// 钟才继续调用，会话一没，客户端拿到 404，而单客户端租约还占着位置 —— 得再等
/// 15 分钟空闲才能重新 initialize。把会话寿命对齐租约，两边同时过期。
fn session_manager() -> Arc<LocalSessionManager> {
    let mut manager = LocalSessionManager::default();
    manager.session_config.keep_alive = Some(ClientLease::IDLE);
    Arc::new(manager)
}

async fn guard(State(state): State<SharedState>, request: Request, next: Next) -> Response {
    // 并发闸门：单客户端也限流，避免一次批量调用把 UI 线程灌满。
    let Ok(_permit) = state.requests.try_acquire() else {
        return failure(StatusCode::TOO_MANY_REQUESTS, "server busy");
    };

    // MCP 客户端不是浏览器。任何带 Origin 的请求都拒掉，封死 DNS rebinding
    // 与恶意网页直接调用本机端口这条路（rmcp 的 allowed_hosts 再兜一层）。
    if request.headers().contains_key(header::ORIGIN) {
        return failure(StatusCode::FORBIDDEN, "origin is not allowed");
    }

    let Some(token) = bearer_token(&request) else {
        return unauthorized();
    };
    let cfg = state.config();
    if !auth::token_matches(&cfg.token, &token) {
        return unauthorized();
    }

    let method = request.method().clone();
    // 本服务不主动推消息，独立事件流按 MCP 规范可以回 405。
    if method == Method::GET || method == Method::HEAD {
        return method_not_allowed();
    }
    if method != Method::POST && method != Method::DELETE {
        return method_not_allowed();
    }

    // 先按 Content-Length 挡掉超大请求体：`Limited` 抛错时 axum 只会回 500，
    // 客户端看不出是自己发太大了。
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|len| len > MAX_BODY as u64)
    {
        return failure(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large");
    }

    let session = match request.headers().get("mcp-session-id") {
        Some(value) => match value.to_str() {
            Ok(value) if !value.is_empty() && value.len() <= 128 => Some(value.to_owned()),
            _ => return failure(StatusCode::BAD_REQUEST, "invalid session id"),
        },
        None => None,
    };
    if request.headers().get_all("mcp-session-id").iter().count() > 1 {
        return failure(StatusCode::BAD_REQUEST, "ambiguous session id");
    }

    // 多客户端：每个 session id 对应一份租约；不带 id 的 POST 只可能是 initialize。
    let now = Instant::now();
    state.prune_expired(now);
    if let Some(requested) = &session {
        // 租约已不在（空闲过期 / 拒绝 / 断开 / 令牌轮换）：rmcp 可能仍认识这个会话，
        // 放行等于"清租约不断权"，必须挡回去逼客户端重新 initialize。
        // DELETE 例外：那是客户端主动收尾。
        if !state.touch(requested, now) && method != Method::DELETE {
            return failure(
                StatusCode::NOT_FOUND,
                "MCP session expired; initialize again",
            );
        }
    } else if state.at_capacity() {
        return failure(StatusCode::TOO_MANY_REQUESTS, "too many MCP clients");
    }

    let (mut parts, body) = request.into_parts();
    // 不要把 Bearer 令牌带进 SDK 的请求上下文 / debug 视图。
    parts.headers.remove(header::AUTHORIZATION);
    let request = Request::from_parts(parts, Body::new(Limited::new(body, MAX_BODY)));
    let mut response = next.run(request).await;

    if session.is_none() && response.status().is_success() {
        if let Some(id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            let id = id.to_owned();
            if state.insert_lease(id.clone()) {
                audit_session(&state, "session_open", "", &id);
            } else {
                // 并发下超出上限：这次会话没有租约，客户端下一次请求会拿 404。
                tracing::warn!("mcp: client limit reached; refusing session {}", &id[..id.len().min(8)]);
            }
        }
    } else if method == Method::DELETE && response.status().is_success() {
        if let Some(id) = session.as_deref() {
            state.close_session(id, CloseReason::Delete);
        }
    } else if response.status() == StatusCode::NOT_FOUND {
        // rmcp 已经不认这个会话（空闲超时被回收）。
        if let Some(id) = session.as_deref() {
            state.close_session(id, CloseReason::NotFound);
        }
    }

    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// 会话建立 / 关闭只进审计文件，不进 AI 页的"最近调用"列表。
fn audit_session(state: &SharedState, event: &str, reason: &str, session: &str) {
    if !state.config().audit.enabled {
        return;
    }
    crate::mcp::record_event(serde_json::json!({
        "ts": chrono::Local::now().to_rfc3339(),
        "event": event,
        "session": session.chars().take(8).collect::<String>(),
        "reason": reason,
    }));
}

/// 只接受单个 `Authorization: Bearer <token>`。多值或换 scheme 一律拒绝。
fn bearer_token(request: &Request) -> Option<String> {
    let mut values = request.headers().get_all(header::AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, token) = value.to_str().ok()?.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    Some(token.to_owned())
}

fn failure(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({ "error": message }))).into_response()
}

fn unauthorized() -> Response {
    let mut response = failure(StatusCode::UNAUTHORIZED, "invalid or missing access token");
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"rudder-mcp\""),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn method_not_allowed() -> Response {
    let mut response = failure(
        StatusCode::METHOD_NOT_ALLOWED,
        "standalone event stream is not supported",
    );
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
    response
}

#[cfg(test)]
mod tests {
    //! 边界行为测试：不需要真实端口，也不需要 GUI —— 直接驱动 axum 的 Router。
    //! 覆盖鉴权、多客户端租约、按会话授权与工具列表过滤。

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
    use tower::ServiceExt;

    use super::super::types::{
        AppHandles, CloseReason, McpConfigSnapshot, McpState, SharedState,
    };
    use super::MAX_BODY;
    use crate::mcp::Permissions;

    const TOKEN: &str = "rk_test";

    const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;

    const CALL_INFO: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_server_info","arguments":{}}}"#;

    /// 只需要 `file_upload`，不依赖 tab 类型，适合验证授权门。
    const CALL_UPLOAD: &str = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"sftp_upload","arguments":{"tab_id":"t1","local_path":"/tmp/x","remote_path":"/tmp/y"}}}"#;

    const LIST_TOOLS: &str = r#"{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}"#;

    fn state(consent: crate::mcp::ConsentPolicy) -> SharedState {
        Arc::new(McpState::new(
            McpConfigSnapshot {
                enabled: true,
                port: 8765,
                loopback: true,
                lan_enabled: false,
                lan_ifaces: Vec::new(),
                token: TOKEN.to_string(),
                client_grants: Vec::new(),
                consent,
                audit: super::super::types::AuditConfig {
                    enabled: false,
                    retention_days: 0,
                },
            },
            AppHandles {
                bufs: Arc::new(Mutex::new(HashMap::new())),
                tab_statuses: Arc::new(Mutex::new(HashMap::new())),
                local_snap: Arc::new(Mutex::new(Default::default())),
                sftp_handles: Arc::new(Mutex::new(HashMap::new())),
            },
            tokio::runtime::Handle::current(),
        ))
    }

    fn router_for(state: SharedState) -> Router {
        super::router(state, StreamableHttpServerConfig::default())
    }

    fn post(body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/mcp")
            // 真实服务端会由 HTTP 层补上 Host；直接驱动 Router 时要自己给，
            // 否则 rmcp 的 allowed_hosts 校验会把请求判成 400。
            .header(header::HOST, "127.0.0.1:8765")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn post_with_session(body: &str, session: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "127.0.0.1:8765")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header("mcp-session-id", session)
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn open_session(app: &Router) -> String {
        let response = app.clone().oneshot(post(INITIALIZE)).await.unwrap();
        let status = response.status();
        let session = response
            .headers()
            .get("mcp-session-id")
            .map(|value| value.to_str().unwrap().to_string());
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "unexpected body: {}",
            String::from_utf8_lossy(&body)
        );
        session.expect("initialize must issue a session id")
    }

    async fn call(app: &Router, session: &str, body: &str) -> (StatusCode, String) {
        let response = app
            .clone()
            .oneshot(post_with_session(body, session))
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn rejects_missing_or_wrong_token() {
        let app = router_for(state(crate::mcp::ConsentPolicy::Auto));
        for token in [None, Some("rk_wrong")] {
            let mut builder = Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::HOST, "127.0.0.1:8765")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream");
            if let Some(token) = token {
                builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            let request = builder.body(Body::from(INITIALIZE)).unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
        }
    }

    #[tokio::test]
    async fn rejects_browser_origin() {
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::ORIGIN, "https://evil.example")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::from(INITIALIZE))
            .unwrap();
        let response = router_for(state(crate::mcp::ConsentPolicy::Auto))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// 多客户端：第二个 initialize 不再 409，各自拿到独立 session id。
    #[tokio::test]
    async fn multiple_clients_get_independent_sessions() {
        let state = state(crate::mcp::ConsentPolicy::Auto);
        let app = router_for(state.clone());
        let first = open_session(&app).await;
        let second = open_session(&app).await;
        assert_ne!(first, second);
        assert_eq!(state.lease_count(), 2);
    }

    /// 超过上限时 initialize 被挡在 429，不再创建第 9 个租约。
    #[tokio::test]
    async fn client_limit_is_enforced() {
        let state = state(crate::mcp::ConsentPolicy::Auto);
        let app = router_for(state.clone());
        for _ in 0..McpState::MAX_CLIENTS {
            let _ = open_session(&app).await;
        }
        let response = app.clone().oneshot(post(INITIALIZE)).await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    /// 未知 / 已断开的 session id 必须 404，不能放行到 rmcp。
    #[tokio::test]
    async fn unknown_or_closed_session_is_rejected() {
        let state = state(crate::mcp::ConsentPolicy::Auto);
        let app = router_for(state.clone());
        let session = open_session(&app).await;

        state.close_session(&session, CloseReason::Disconnected);
        let (status, _) = call(&app, &session, CALL_INFO).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // 席位已释放：新客户端可以立刻 initialize。
        let fresh = open_session(&app).await;
        assert!(!fresh.is_empty());

        let (status, _) = call(&app, "not-a-session", CALL_INFO).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn oversized_body_is_rejected_with_413() {
        let app = router_for(state(crate::mcp::ConsentPolicy::Auto));
        let session = open_session(&app).await;
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "127.0.0.1:8765")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, (MAX_BODY + 1).to_string())
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header("mcp-session-id", &session)
            .body(Body::from("a".repeat(MAX_BODY + 1)))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn tools_call_reaches_the_handler() {
        let app = router_for(state(crate::mcp::ConsentPolicy::Auto));
        let session = open_session(&app).await;
        let (status, text) = call(&app, &session, CALL_INFO).await;
        assert_eq!(status, StatusCode::OK);
        assert!(text.contains("rudder"), "unexpected body: {text}");
    }

    /// 未批准的会话只能拿到 consent_pending；批准后缺位才是 permission_denied。
    #[tokio::test]
    async fn pending_session_is_not_allowed_to_call_tools() {
        let state = state(crate::mcp::ConsentPolicy::Session);
        let app = router_for(state.clone());
        let session = open_session(&app).await;

        let (status, text) = call(&app, &session, CALL_UPLOAD).await;
        assert_eq!(status, StatusCode::OK);
        assert!(text.contains("consent_pending"), "unexpected body: {text}");

        state.approve(&session, Permissions::from_keys(&["read"]));
        let (_, text) = call(&app, &session, CALL_UPLOAD).await;
        assert!(text.contains("permission_denied"), "unexpected body: {text}");
        assert!(text.contains("file_upload"), "unexpected body: {text}");
    }

    /// `list_tools` 按本次会话的授权过滤；另一个客户端看不到同样的集合。
    #[tokio::test]
    async fn tool_list_follows_each_session_grant() {
        let state = state(crate::mcp::ConsentPolicy::Session);
        let app = router_for(state.clone());
        let reader = open_session(&app).await;
        let executor = open_session(&app).await;
        state.approve(&reader, Permissions::from_keys(&["read"]));
        state.approve(&executor, Permissions::from_keys(&["exec"]));

        let (_, read_list) = call(&app, &reader, LIST_TOOLS).await;
        assert!(read_list.contains("read_terminal"), "{read_list}");
        assert!(!read_list.contains("sftp_upload"), "{read_list}");

        let (_, exec_list) = call(&app, &executor, LIST_TOOLS).await;
        assert!(exec_list.contains("run_command"), "{exec_list}");
        assert!(!exec_list.contains("read_terminal"), "{exec_list}");
    }
}

