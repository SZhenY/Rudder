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
use super::types::{ClientLease, SharedState};

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

    // 单客户端：不带会话 id 的 POST 只可能是 initialize；已有活跃租约就 409。
    let now = Instant::now();
    {
        let mut lease = state.lease.lock().await;
        if lease
            .as_ref()
            .is_some_and(|current: &ClientLease| current.expired(now))
        {
            *lease = None;
        }
        match (&session, lease.as_mut()) {
            (None, Some(_)) => {
                return failure(
                    StatusCode::CONFLICT,
                    "another MCP client is already connected",
                );
            }
            (Some(requested), Some(current)) if requested != &current.session_id => {
                return failure(
                    StatusCode::NOT_FOUND,
                    "MCP session not found; initialize again",
                );
            }
            (Some(_), Some(current)) => current.last_seen = now,
            _ => {}
        }
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
            *state.lease.lock().await = Some(ClientLease {
                session_id: id.to_owned(),
                last_seen: Instant::now(),
            });
        }
    } else if method == Method::DELETE && response.status().is_success() {
        let mut lease = state.lease.lock().await;
        if lease.as_ref().map(|current| &current.session_id) == session.as_ref() {
            *lease = None;
        }
    } else if response.status() == StatusCode::NOT_FOUND {
        // rmcp 已经不认这个会话（空闲超时被回收）。租约留着只会让客户端在下次
        // initialize 时吃 409，白等一个空闲周期 —— 立刻释放，让它重连。
        let mut lease = state.lease.lock().await;
        if lease.as_ref().map(|current| &current.session_id) == session.as_ref() {
            *lease = None;
        }
    }

    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
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
    //! 覆盖鉴权、Origin 拒绝、单客户端租约与工具调用链。

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
    use tower::ServiceExt;

    use super::super::types::{
        AppHandles, McpConfigSnapshot, McpState, Permissions, SharedState,
    };
    use super::MAX_BODY;

    const TOKEN: &str = "rk_test";

    const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;

    const CALL_INFO: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_server_info","arguments":{}}}"#;

    fn state() -> SharedState {
        Arc::new(McpState::new(
            McpConfigSnapshot {
                enabled: true,
                port: 8765,
                loopback: true,
                lan_enabled: false,
                lan_ifaces: Vec::new(),
                token: TOKEN.to_string(),
                perms: Permissions::default(),
            },
            AppHandles {
                bufs: Arc::new(Mutex::new(HashMap::new())),
                tab_statuses: Arc::new(Mutex::new(HashMap::new())),
                local_snap: Arc::new(Mutex::new(Default::default())),
                sftp_handles: Arc::new(Mutex::new(HashMap::new())),
            },
        ))
    }

    fn app(state: SharedState) -> Router {
        super::router(state, StreamableHttpServerConfig::default())
    }

    fn post(body: &str, token: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            // 真实服务端会由 HTTP 层补上 Host；直接驱动 Router 时要自己给，
            // 否则 rmcp 的 allowed_hosts 校验会把请求判成 400。
            .header(header::HOST, "127.0.0.1:8765")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    async fn open_session(app: &Router) -> String {
        let response = app
            .clone()
            .oneshot(post(INITIALIZE, Some(TOKEN)))
            .await
            .unwrap();
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

    #[tokio::test]
    async fn rejects_missing_or_wrong_token() {
        let app = app(state());
        for token in [None, Some("rk_wrong")] {
            let response = app
                .clone()
                .oneshot(post(INITIALIZE, token))
                .await
                .unwrap();
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
        let response = app(state()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn initialize_issues_a_session_and_a_second_client_is_refused() {
        let app = app(state());
        let session = open_session(&app).await;
        assert!(!session.is_empty());

        let second = app
            .clone()
            .oneshot(post(INITIALIZE, Some(TOKEN)))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::CONFLICT);
    }

    /// 超过上限的请求体要在进 rmcp 之前挡掉：`Limited` 抛错时只会变成 500。
    #[tokio::test]
    async fn oversized_body_is_rejected_with_413() {
        let app = app(state());
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

    /// 会话被 rmcp 回收后必须释放租约：否则客户端下一次 initialize 只会拿到
    /// 409，白白空等一个空闲周期。
    #[tokio::test]
    async fn unknown_session_releases_the_lease() {
        let state = state();
        // 第一个路由的会话管理器认识新会话；第二个相当于"会话已被回收"的服务端。
        let first = super::router(state.clone(), StreamableHttpServerConfig::default());
        let stale = open_session(&first).await;
        let second = super::router(state, StreamableHttpServerConfig::default());

        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "127.0.0.1:8765")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header("mcp-session-id", &stale)
            .body(Body::from(CALL_INFO))
            .unwrap();
        let response = second.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // 租约已释放 → 重新 initialize 能成功，而不是 409。
        let fresh = open_session(&second).await;
        assert!(!fresh.is_empty());
    }

    #[tokio::test]
    async fn tools_call_reaches_the_handler() {
        let app = app(state());
        let session = open_session(&app).await;

        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::HOST, "127.0.0.1:8765")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header("mcp-session-id", &session)
            .body(Body::from(CALL_INFO))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("rudder"), "unexpected body: {text}");
    }
}
