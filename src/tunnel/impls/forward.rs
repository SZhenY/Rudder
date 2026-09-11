//! SSH port forwarding / tunnels (#56).
//!
//! Local (-L) and dynamic (-D / SOCKS5) forwards run client-side: we listen on
//! a local TCP port and, per inbound connection, open a `direct-tcpip` channel
//! on the SSH session, then splice the two streams together. Remote (-R)
//! forwards are requested with `tcpip_forward` and serviced in the session
//! handler when the server opens channels back (see `ssh.rs`).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use russh::Channel;
use russh::client::{Handle, Msg};
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::i18n::t;
use crate::ssh::{ClientHandler, SessionEvent};

/// Emit a one-line notice into the terminal output stream.
fn notice(events: &UnboundedSender<SessionEvent>, msg: String) {
    let _ = events.send(SessionEvent::Output(format!("\r\n[rudder] {msg}\r\n")));
}

fn bind_target(bind_addr: &str, bind_port: u16) -> String {
    let addr = if bind_addr.trim().is_empty() {
        "127.0.0.1"
    } else {
        bind_addr.trim()
    };
    // IPv6 literals must be bracketed for TcpListener::bind ("[::1]:8080");
    // an already-bracketed address is left as-is (#109).
    if addr.contains(':') && !addr.starts_with('[') {
        format!("[{addr}]:{bind_port}")
    } else {
        format!("{addr}:{bind_port}")
    }
}

/// Whether a bind address only accepts connections from this machine.
///
/// A `-D` listener has no authentication of its own, so anything beyond
/// loopback turns this host into an open proxy for every machine that can reach
/// it. Unparseable input (a hostname we cannot classify) is reported as
/// *exposed* so the warning errs on the side of caution.
fn is_loopback_bind(bind_addr: &str) -> bool {
    let addr = bind_addr
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    if addr.is_empty() {
        return true; // empty → 127.0.0.1, see `bind_target`
    }
    if addr.eq_ignore_ascii_case("localhost") {
        return true;
    }
    addr.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Open a `direct-tcpip` channel to `host:port`, recording the originating peer
/// (some servers log / ACL on it).
async fn open_direct(
    handle: &Arc<Handle<ClientHandler>>,
    host: &str,
    port: u16,
    peer: SocketAddr,
) -> Result<Channel<Msg>, russh::Error> {
    handle
        .channel_open_direct_tcpip(
            host.to_string(),
            port as u32,
            peer.ip().to_string(),
            peer.port() as u32,
        )
        .await
}

/// Local forward (-L): listen locally and tunnel each connection to
/// `target_host:target_port` reached from the SSH server's side.
pub fn spawn_local(
    handle: Arc<Handle<ClientHandler>>,
    bind_addr: String,
    bind_port: u16,
    target_host: String,
    target_port: u16,
    events: UnboundedSender<SessionEvent>,
) -> JoinHandle<()> {
    let bind = bind_target(&bind_addr, bind_port);
    tokio::spawn(async move {
        let listener = match TcpListener::bind(&bind).await {
            Ok(l) => l,
            Err(e) => {
                notice(&events, format!("-L {bind} {}: {e}", t("监听失败", "bind failed")));
                return;
            }
        };
        notice(&events, format!("-L {bind} → {target_host}:{target_port}"));
        loop {
            let (mut inbound, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let handle = handle.clone();
            let host = target_host.clone();
            let ev = events.clone();
            tokio::spawn(async move {
                match open_direct(&handle, &host, target_port, peer).await {
                    Ok(ch) => {
                        let mut stream = ch.into_stream();
                        let _ = copy_bidirectional(&mut inbound, &mut stream).await;
                    }
                    Err(e) => notice(
                        &ev,
                        format!("-L {host}:{target_port} {}: {e}", t("连接失败", "open failed")),
                    ),
                }
            });
        }
    })
}

/// Dynamic forward (-D): a minimal SOCKS5 proxy. Each accepted connection
/// negotiates SOCKS5 (no auth, CONNECT only), then we open a `direct-tcpip`
/// channel to the requested destination and splice.
pub fn spawn_dynamic(
    handle: Arc<Handle<ClientHandler>>,
    bind_addr: String,
    bind_port: u16,
    events: UnboundedSender<SessionEvent>,
) -> JoinHandle<()> {
    let bind = bind_target(&bind_addr, bind_port);
    tokio::spawn(async move {
        let listener = match TcpListener::bind(&bind).await {
            Ok(l) => l,
            Err(e) => {
                notice(&events, format!("-D {bind} {}: {e}", t("监听失败", "bind failed")));
                return;
            }
        };
        notice(&events, format!("-D {bind} (SOCKS5)"));
        if !is_loopback_bind(&bind_addr) {
            notice(
                &events,
                t(
                    "警告：-D 绑定在非本机地址上，且 SOCKS5 不校验密码——任何能访问该地址的设备都可借道这台机器访问远端网络。",
                    "Warning: -D is bound to a non-loopback address and SOCKS5 does not authenticate — anything that can reach this address can use this machine as a proxy into the remote network.",
                )
                .to_string(),
            );
        }
        loop {
            let (inbound, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let handle = handle.clone();
            let ev = events.clone();
            tokio::spawn(async move {
                if let Err(e) = socks5_serve(&handle, inbound, peer).await {
                    tracing::debug!("socks5 conn ended: {e}");
                    let _ = ev; // notices for SOCKS are too noisy; keep to trace
                }
            });
        }
    })
}

/// Negotiate the SOCKS5 greeting (`VER, NMETHODS, METHODS[NMETHODS]`).
///
/// `Ok(true)` means the client offered "no authentication" (0x00) and we
/// accepted, so the caller may continue with the request. `Ok(false)` means the
/// connection must be dropped; the reply, if one is owed, has already been
/// written. Pulled out of [`socks5_serve`] so the negotiation can be exercised
/// without a live SSH `Handle`.
async fn socks5_greeting<S>(stream: &mut S) -> std::io::Result<bool>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await?;
    if head[0] != 0x05 {
        return Ok(false); // not SOCKS5
    }
    let nmethods = head[1] as usize;
    let mut methods = vec![0u8; nmethods];
    stream.read_exact(&mut methods).await?;
    // Only "no authentication" is implemented. Refuse the handshake when the
    // client did not offer it, instead of forcing our own choice on it — a
    // strict client would read that as a protocol error. An empty method list
    // lands in this branch too.
    if !methods.contains(&0x00) {
        stream.write_all(&[0x05, 0xFF]).await?; // no acceptable methods
        return Ok(false);
    }
    stream.write_all(&[0x05, 0x00]).await?; // VER=5, METHOD=0
    Ok(true)
}

/// Handle one SOCKS5 client connection end-to-end.
async fn socks5_serve(
    handle: &Arc<Handle<ClientHandler>>,
    mut inbound: TcpStream,
    peer: SocketAddr,
) -> std::io::Result<()> {
    if !socks5_greeting(&mut inbound).await? {
        return Ok(());
    }

    // Request: VER, CMD, RSV, ATYP, DST.ADDR, DST.PORT.
    let mut req = [0u8; 4];
    inbound.read_exact(&mut req).await?;
    if req[0] != 0x05 {
        return Ok(());
    }
    if req[1] != 0x01 {
        // Only CONNECT is supported → reply "command not supported".
        let _ = inbound.write_all(&socks_reply(0x07)).await;
        return Ok(());
    }
    let host = match req[3] {
        0x01 => {
            let mut a = [0u8; 4];
            inbound.read_exact(&mut a).await?;
            Ipv4Addr::from(a).to_string()
        }
        0x04 => {
            let mut a = [0u8; 16];
            inbound.read_exact(&mut a).await?;
            Ipv6Addr::from(a).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            inbound.read_exact(&mut len).await?;
            let mut d = vec![0u8; len[0] as usize];
            inbound.read_exact(&mut d).await?;
            String::from_utf8_lossy(&d).into_owned()
        }
        _ => {
            let _ = inbound.write_all(&socks_reply(0x08)).await; // addr type unsupported
            return Ok(());
        }
    };
    let mut port = [0u8; 2];
    inbound.read_exact(&mut port).await?;
    let port = u16::from_be_bytes(port);

    match open_direct(handle, &host, port, peer).await {
        Ok(ch) => {
            inbound.write_all(&socks_reply(0x00)).await?; // succeeded
            let mut stream = ch.into_stream();
            let _ = copy_bidirectional(&mut inbound, &mut stream).await;
        }
        Err(_) => {
            let _ = inbound.write_all(&socks_reply(0x05)).await; // connection refused
        }
    }
    Ok(())
}

/// A SOCKS5 reply with the given reply code and a zeroed bound address
/// (`0.0.0.0:0`) — clients don't need the real bound address for CONNECT.
fn socks_reply(code: u8) -> [u8; 10] {
    [0x05, code, 0x00, 0x01, 0, 0, 0, 0, 0, 0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_target_brackets_ipv6_literals_exactly_once() {
        assert_eq!(bind_target("", 8080), "127.0.0.1:8080");
        assert_eq!(bind_target("0.0.0.0", 8080), "0.0.0.0:8080");
        assert_eq!(bind_target("::1", 8080), "[::1]:8080");
        // An already-bracketed address must not be double-wrapped (#109).
        assert_eq!(bind_target("[::1]", 8080), "[::1]:8080");
    }

    #[test]
    fn loopback_detection_treats_unclassifiable_hosts_as_exposed() {
        assert!(is_loopback_bind(""), "empty defaults to 127.0.0.1");
        assert!(is_loopback_bind("127.0.0.1"));
        assert!(is_loopback_bind("::1"));
        assert!(is_loopback_bind("[::1]"));
        assert!(is_loopback_bind("localhost"));
        assert!(is_loopback_bind("  LOCALHOST  "));

        assert!(!is_loopback_bind("0.0.0.0"));
        assert!(!is_loopback_bind("192.168.1.10"));
        // Not an IP literal we can classify → warn rather than stay silent.
        assert!(!is_loopback_bind("build.internal"));
    }

    #[tokio::test]
    async fn socks5_greeting_accepts_no_auth_and_refuses_anything_else() {
        use tokio::io::duplex;

        // Client offers 0x00 → we agree (05 00) and report "continue".
        let (mut client, mut server) = duplex(64);
        let peer = tokio::spawn(async move {
            let ok = socks5_greeting(&mut server).await.unwrap();
            assert!(ok, "no-auth offer must be accepted");
        });
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0x00]);
        peer.await.unwrap();

        // Client offers only username/password (0x02) → refuse with 05 FF.
        let (mut client, mut server) = duplex(64);
        let peer = tokio::spawn(async move {
            let ok = socks5_greeting(&mut server).await.unwrap();
            assert!(!ok, "an offer without 0x00 must be refused");
        });
        client.write_all(&[0x05, 0x01, 0x02]).await.unwrap();
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0xFF]);
        peer.await.unwrap();

        // Empty method list must not be treated as "offered no auth".
        let (mut client, mut server) = duplex(64);
        let peer = tokio::spawn(async move {
            let ok = socks5_greeting(&mut server).await.unwrap();
            assert!(!ok, "an empty method list must be refused");
        });
        client.write_all(&[0x05, 0x00]).await.unwrap();
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0xFF]);
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn socks5_greeting_ignores_non_socks5_traffic() {
        use tokio::io::duplex;

        // A wrong protocol version must not produce a reply at all.
        let (mut client, mut server) = duplex(64);
        let peer = tokio::spawn(async move {
            let ok = socks5_greeting(&mut server).await.unwrap();
            assert!(!ok);
        });
        client.write_all(&[0x04, 0x01, 0x00]).await.unwrap();
        peer.await.unwrap();
    }
}
