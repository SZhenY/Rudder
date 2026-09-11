//! Outbound proxy support for SSH / SFTP connections (issue #7).
//!
//! Establishes the TCP stream to the target host **through a proxy**, then the
//! caller hands that stream to `russh::client::connect_stream`.  Both proxy
//! kinds end up as a transparent `TcpStream`:
//!
//! * **SOCKS5** (`socks5://` / `socks5h://`) via `tokio-socks`; after the
//!   handshake we unwrap to the inner `TcpStream`.
//! * **HTTP / HTTPS CONNECT** (`http://` / `https://`): we issue an HTTP
//!   `CONNECT host:port` and reuse the same socket as the tunnel.
//!
//! The proxy is taken from the per-session setting, falling back to the standard
//! `ALL_PROXY` / `all_proxy` environment variable.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::Secret;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ProxyKind {
    Socks5,
    Http,
    /// `https://` — TLS *to the proxy itself*, which this module does not
    /// implement. Parsed into its own kind instead of being rejected in
    /// [`parse`] on purpose: `parse` returning `None` makes [`resolve`] report
    /// "no proxy", which would silently connect **directly** and ignore the
    /// proxy the user configured (a real possibility with `ALL_PROXY`). Failing
    /// loudly at connect time is the honest option; `http://` stays available
    /// for anyone who accepts a plaintext CONNECT.
    HttpsUnsupported,
}

#[derive(Clone)]
pub struct ProxyConfig {
    kind: ProxyKind,
    host: String,
    port: u16,
    // (user, password). The password is wrapped in `Secret` so it is zeroed on
    // drop and never printed; see the manual `Debug` below for the user part.
    auth: Option<(String, Secret)>,
}

// Manual `Debug` so proxy credentials can never leak via `{:?}` / tracing
// (issue #32). Host/port/kind stay visible for diagnostics; auth is redacted.
impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("auth", &self.auth.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// Resolve the proxy for a session: the explicit `session_proxy` string if set,
/// otherwise the `ALL_PROXY` / `all_proxy` environment variable.  Returns `None`
/// for a direct connection.
pub fn resolve(session_proxy: &str) -> Option<ProxyConfig> {
    let s = session_proxy.trim();
    if !s.is_empty() {
        return parse(s);
    }
    for var in ["ALL_PROXY", "all_proxy"] {
        if let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
        {
            return parse(v.trim());
        }
    }
    None
}

/// Parse a proxy URL: `scheme://[user:pass@]host:port`.
fn parse(url: &str) -> Option<ProxyConfig> {
    let (scheme, rest) = url.split_once("://").unwrap_or(("socks5", url));
    let kind = match scheme.to_ascii_lowercase().as_str() {
        "socks5" | "socks5h" | "socks" => ProxyKind::Socks5,
        "http" => ProxyKind::Http,
        "https" => ProxyKind::HttpsUnsupported,
        _ => return None,
    };
    // Optional userinfo before '@'.
    let (auth, hostport) = match rest.rsplit_once('@') {
        Some((userinfo, hp)) => {
            let (u, p) = userinfo.split_once(':').unwrap_or((userinfo, ""));
            (Some((u.to_string(), Secret::new(p))), hp)
        }
        None => (None, rest),
    };
    let hostport = hostport.trim_end_matches('/');
    let (host, port) = hostport.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some(ProxyConfig {
        kind,
        host: host.to_string(),
        port,
        auth,
    })
}

/// Human-readable description of where we're connecting (for status messages).
pub fn describe(cfg: &ProxyConfig) -> String {
    let scheme = match cfg.kind {
        ProxyKind::Socks5 => "socks5",
        ProxyKind::Http => "http",
        ProxyKind::HttpsUnsupported => "https",
    };
    format!("{}://{}:{}", scheme, cfg.host, cfg.port)
}

/// Open a TCP stream to `target_host:target_port` through the proxy.
pub async fn connect(cfg: &ProxyConfig, target_host: &str, target_port: u16) -> Result<TcpStream> {
    match cfg.kind {
        ProxyKind::Socks5 => connect_socks5(cfg, target_host, target_port).await,
        ProxyKind::Http => connect_http(cfg, target_host, target_port).await,
        // Deliberately a hard error rather than a silent plaintext fallback:
        // `connect_http` sends `Proxy-Authorization: Basic <user:pass>` in the
        // clear, so honouring an `https://` request that way would put the
        // proxy password on the wire unencrypted. The marker phrase is what
        // `classify_connect_failure` matches on to build the user-facing text.
        ProxyKind::HttpsUnsupported => bail!(
            "https proxy requires TLS — not supported (proxy {}:{})",
            cfg.host,
            cfg.port
        ),
    }
}

async fn connect_socks5(cfg: &ProxyConfig, host: &str, port: u16) -> Result<TcpStream> {
    use tokio_socks::tcp::Socks5Stream;
    let proxy = (cfg.host.as_str(), cfg.port);
    let target = (host, port);
    let stream = match &cfg.auth {
        Some((u, p)) => Socks5Stream::connect_with_password(proxy, target, u, p.as_str())
            .await
            .context("SOCKS5 proxy connect failed")?,
        None => Socks5Stream::connect(proxy, target)
            .await
            .context("SOCKS5 proxy connect failed")?,
    };
    // After the handshake the underlying socket is a transparent tunnel.
    Ok(stream.into_inner())
}

async fn connect_http(cfg: &ProxyConfig, host: &str, port: u16) -> Result<TcpStream> {
    let mut s = TcpStream::connect((cfg.host.as_str(), cfg.port))
        .await
        .with_context(|| format!("connect to HTTP proxy {}:{} failed", cfg.host, cfg.port))?;

    let mut req = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some((u, p)) = &cfg.auth {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{u}:{}", p.as_str()));
        req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    req.push_str("Proxy-Connection: keep-alive\r\n\r\n");
    s.write_all(req.as_bytes())
        .await
        .context("write CONNECT to proxy")?;

    // Read response headers up to the blank line, bounded.
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = s.read(&mut byte).await.context("read proxy response")?;
        if n == 0 {
            bail!("proxy closed the connection during CONNECT");
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            bail!("proxy CONNECT response too large");
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let status_line = head.lines().next().unwrap_or("");
    // Expect "HTTP/1.x 200 ...".
    let ok = status_line
        .split_whitespace()
        .nth(1)
        .map(|c| c == "200")
        .unwrap_or(false);
    if !ok {
        return Err(anyhow!("proxy CONNECT rejected: {}", status_line.trim()));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socks5_schemes_parse_with_credentials() {
        for scheme in ["socks5", "socks5h", "socks"] {
            let cfg = parse(&format!("{scheme}://user:pa%40ss@proxy.local:1080")).unwrap();
            assert!(matches!(cfg.kind, ProxyKind::Socks5));
            assert_eq!(cfg.host, "proxy.local");
            assert_eq!(cfg.port, 1080);
            let (user, pass) = cfg.auth.as_ref().unwrap();
            assert_eq!(user, "user");
            assert_eq!(pass.as_str(), "pa%40ss");
        }
    }

    #[test]
    fn http_and_https_are_not_the_same_kind() {
        let http = parse("http://proxy.local:3128").unwrap();
        assert!(matches!(http.kind, ProxyKind::Http));
        assert_eq!(http.host, "proxy.local");
        assert_eq!(http.port, 3128);

        // Regression guard: `https://` used to collapse into `ProxyKind::Http`,
        // which sent `Proxy-Authorization: Basic` over a plaintext socket.
        let https = parse("https://proxy.local:3128").unwrap();
        assert!(matches!(https.kind, ProxyKind::HttpsUnsupported));
        assert_eq!(describe(&https), "https://proxy.local:3128");
    }

    #[tokio::test]
    async fn https_proxy_errors_instead_of_connecting_in_the_clear() {
        let cfg = parse("https://user:pass@proxy.local:3128").unwrap();
        // Fails before any I/O — no network access needed for this test.
        let err = connect(&cfg, "target.local", 22).await.unwrap_err();
        // Lowercased to mirror exactly what `classify_connect_failure` matches.
        let msg = format!("{err:#}").to_lowercase();
        assert!(msg.contains("https proxy requires tls"), "got: {msg}");
    }

    #[test]
    fn scheme_is_optional_and_defaults_to_socks5() {
        let cfg = parse("proxy.local:1080").unwrap();
        assert!(matches!(cfg.kind, ProxyKind::Socks5));
    }

    #[test]
    fn unknown_scheme_and_malformed_inputs_are_rejected() {
        assert!(parse("ftp://proxy.local:21").is_none());
        assert!(parse("proxy.local").is_none(), "port is required");
        assert!(parse("http://:3128").is_none(), "empty host");
        assert!(parse("http://proxy.local:notaport").is_none());
    }

    #[test]
    fn debug_output_redacts_credentials_but_keeps_host() {
        let cfg = parse("socks5://bob:supersecret@proxy.local:1080").unwrap();
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("supersecret"), "leaked: {rendered}");
        assert!(rendered.contains("[redacted]"));
        assert!(rendered.contains("proxy.local"));
    }
}
