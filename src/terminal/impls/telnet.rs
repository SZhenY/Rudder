//! Telnet session worker (issue #17).
//!
//! Like [`crate::terminal::serial`], this mirrors [`crate::ssh::spawn_session`]'s public
//! surface so the terminal UI is reused unchanged. Telnet is just a TCP byte
//! stream with in-band option negotiation (RFC 854/855), so the worker:
//!
//! * strips IAC command sequences out of the data stream before it reaches the
//!   terminal,
//! * answers option negotiation with a minimal, conservative policy (let the
//!   server echo and run in character mode; advertise our window size),
//! * doubles `0xFF` bytes in user input as the protocol requires,
//! * re-sends NAWS (window size) on every terminal resize.
//!
//! There is no SFTP and no resource monitor — a Telnet console is a raw pipe.

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::config::Session;
use crate::i18n::t;
use crate::ssh::{SessionCommand, SessionEvent, SessionHandle};

// Telnet protocol bytes (RFC 854).
const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;

// Options we care about.
const OPT_ECHO: u8 = 1;
const OPT_SGA: u8 = 3; // suppress go-ahead → character-at-a-time mode
const OPT_NAWS: u8 = 31; // negotiate about window size

pub fn spawn_telnet_session(
    runtime: &tokio::runtime::Handle,
    tab_id: String,
    session: Session,
    initial_cols: u32,
    initial_rows: u32,
) -> (SessionHandle, UnboundedReceiver<SessionEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<SessionEvent>();

    let evt_for_task = evt_tx.clone();
    let join = runtime.spawn(async move {
        if let Err(err) = run_telnet(
            session,
            cmd_rx,
            evt_for_task.clone(),
            initial_cols,
            initial_rows,
        )
        .await
        {
            let _ = evt_for_task.send(SessionEvent::Closed(format!("{err:#}")));
        }
    });

    (
        SessionHandle {
            tab_id,
            commands: cmd_tx,
            join,
        },
        evt_rx,
    )
}

/// Incoming-byte parser state for stripping IAC sequences.
enum TnState {
    Data,
    Iac,
    Opt(u8), // saw IAC <DO/DONT/WILL/WONT>, awaiting option byte
    Sub,     // inside subnegotiation, awaiting IAC
    SubIac,  // inside subnegotiation, saw IAC (awaiting SE)
}

/// Escape `0xFF` (IAC) in outbound user data so the receiver sees a literal
/// byte rather than the start of a command (RFC 854).
fn escape_iac(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        out.push(b);
        if b == IAC {
            out.push(IAC);
        }
    }
    out
}

fn naws_subneg(cols: u32, rows: u32) -> Vec<u8> {
    let w = (cols.clamp(1, u16::MAX as u32)) as u16;
    let h = (rows.clamp(1, u16::MAX as u32)) as u16;
    let mut v = vec![IAC, SB, OPT_NAWS];
    // Width / height are 16-bit; any 255 byte inside must be doubled.
    for b in [
        (w >> 8) as u8,
        (w & 0xff) as u8,
        (h >> 8) as u8,
        (h & 0xff) as u8,
    ] {
        v.push(b);
        if b == IAC {
            v.push(IAC);
        }
    }
    v.extend_from_slice(&[IAC, SE]);
    v
}

async fn run_telnet(
    session: Session,
    mut commands: UnboundedReceiver<SessionCommand>,
    events: UnboundedSender<SessionEvent>,
    initial_cols: u32,
    initial_rows: u32,
) -> Result<()> {
    let host = session.host.trim().to_string();
    let port = if session.port == 0 { 23 } else { session.port };
    let addr = format!("{host}:{port}");

    let _ = events.send(SessionEvent::Status(format!(
        "{} {} ...",
        t("Telnet 连接中", "Telnet connecting"),
        addr
    )));

    // Direct, or tunnel through a SOCKS5 / HTTP proxy (reuses issue #7 plumbing).
    let stream = match crate::ssh::proxy::resolve(&session.proxy) {
        Some(p) => {
            let _ = events.send(SessionEvent::Status(format!(
                "{} {} → {}",
                t("经代理连接", "via proxy"),
                crate::ssh::proxy::describe(&p),
                addr
            )));
            crate::ssh::proxy::connect(&p, &host, port)
                .await
                .with_context(|| format!("proxy connect to {addr} failed"))?
        }
        None => TcpStream::connect(&addr)
            .await
            .with_context(|| format!("connect {addr} failed"))?,
    };
    let _ = stream.set_nodelay(true);

    let _ = events.send(SessionEvent::Connected);
    let _ = events.send(SessionEvent::Status(format!(
        "{} {}",
        t("已连接", "Connected"),
        addr
    )));

    let (mut rd, mut wr) = tokio::io::split(stream);

    // Proactively advertise the options we support: we will suppress go-ahead
    // and report our window size. Most gear replies DO and starts echoing.
    let mut hello = vec![IAC, WILL, OPT_SGA, IAC, WILL, OPT_NAWS];
    hello.extend_from_slice(&naws_subneg(initial_cols, initial_rows));
    wr.write_all(&hello).await.context("telnet write hello")?;
    wr.flush().await.ok();

    let mut state = TnState::Data;
    let mut buf = [0u8; 4096];

    loop {
        tokio::select! {
            cmd = commands.recv() => {
                match cmd {
                    Some(SessionCommand::RawInput(bytes)) => {
                        // Never log keystroke bytes — they can be passwords (#15).
                        tracing::debug!("telnet write len={} bytes", bytes.len());
                        // Escape IAC (0xFF) in user data per RFC 854.
                        let out = escape_iac(&bytes);
                        if wr.write_all(&out).await.is_err() {
                            let _ = events.send(SessionEvent::Closed(
                                t("写入失败", "write failed").into()));
                            break;
                        }
                        let _ = wr.flush().await;
                    }
                    Some(SessionCommand::Resize(cols, rows)) => {
                        let _ = wr.write_all(&naws_subneg(cols, rows)).await;
                        let _ = wr.flush().await;
                    }
                    Some(SessionCommand::AddTunnel { .. })
                    | Some(SessionCommand::StopTunnel(_))
                    | Some(SessionCommand::SetResourceMonitoring(_)) => {}
                    Some(SessionCommand::KillProcess { reply, .. }) => {
                        let _ = reply.send(crate::ssh::ProcessKillResult {
                            success: false,
                            message: t("Telnet 不支持远程进程操作", "Remote process control is unavailable for Telnet sessions").into(),
                        });
                    }
                    Some(SessionCommand::Close) | None => break,
                }
            }
            r = rd.read(&mut buf) => {
                match r {
                    Ok(0) => break, // peer closed
                    Ok(n) => {
                        let mut data = Vec::with_capacity(n);
                        let mut replies: Vec<u8> = Vec::new();
                        process_incoming(&buf[..n], &mut state, &mut data, &mut replies);
                        if !replies.is_empty() {
                            let _ = wr.write_all(&replies).await;
                            let _ = wr.flush().await;
                        }
                        if !data.is_empty() {
                            let text = String::from_utf8_lossy(&data).into_owned();
                            let _ = events.send(SessionEvent::Output(text));
                        }
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::Closed(format!(
                            "{}: {e}", t("读取错误", "read error"))));
                        break;
                    }
                }
            }
        }
    }

    let _ = events.send(SessionEvent::Closed(
        t("连接已关闭", "connection closed").into(),
    ));
    Ok(())
}

/// Feed `input` through the IAC state machine: plain bytes accumulate into
/// `data`, and any negotiation responses we owe go into `replies`.
fn process_incoming(input: &[u8], state: &mut TnState, data: &mut Vec<u8>, replies: &mut Vec<u8>) {
    for &b in input {
        match *state {
            TnState::Data => {
                if b == IAC {
                    *state = TnState::Iac;
                } else {
                    data.push(b);
                }
            }
            TnState::Iac => match b {
                IAC => {
                    // Escaped 0xFF literal in the data stream.
                    data.push(IAC);
                    *state = TnState::Data;
                }
                DO | DONT | WILL | WONT => *state = TnState::Opt(b),
                SB => *state = TnState::Sub,
                // Standalone commands (GA, NOP, DM, …) — ignore.
                _ => *state = TnState::Data,
            },
            TnState::Opt(cmd) => {
                respond_negotiation(cmd, b, replies);
                *state = TnState::Data;
            }
            TnState::Sub => {
                // Skip subnegotiation payload until IAC SE.
                if b == IAC {
                    *state = TnState::SubIac;
                }
            }
            TnState::SubIac => {
                // IAC SE ends the block; IAC IAC is an escaped data byte we drop
                // (we don't interpret any subnegotiation payloads).
                if b == SE {
                    *state = TnState::Data;
                } else {
                    *state = TnState::Sub;
                }
            }
        }
    }
}

/// Conservative negotiation policy. Returns nothing; appends a 3-byte reply.
fn respond_negotiation(cmd: u8, opt: u8, replies: &mut Vec<u8>) {
    match cmd {
        // Server asks us to enable an option.
        DO => {
            let accept = matches!(opt, OPT_SGA | OPT_NAWS);
            replies.extend_from_slice(&[IAC, if accept { WILL } else { WONT }, opt]);
        }
        // Server tells us to disable — always agree.
        DONT => replies.extend_from_slice(&[IAC, WONT, opt]),
        // Server offers to enable an option on its side.
        WILL => {
            // Let the server echo and run in character mode; refuse the rest.
            let accept = matches!(opt, OPT_ECHO | OPT_SGA);
            replies.extend_from_slice(&[IAC, if accept { DO } else { DONT }, opt]);
        }
        // Server says it won't — acknowledge.
        WONT => replies.extend_from_slice(&[IAC, DONT, opt]),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive one chunk through a fresh parser and return `(data, replies)`.
    fn run(input: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut state = TnState::Data;
        let (mut data, mut replies) = (Vec::new(), Vec::new());
        process_incoming(input, &mut state, &mut data, &mut replies);
        (data, replies)
    }

    #[test]
    fn plain_data_passes_through_untouched() {
        let (data, replies) = run(b"hello\r\n");
        assert_eq!(data, b"hello\r\n");
        assert!(replies.is_empty());
    }

    #[test]
    fn escaped_iac_becomes_a_literal_0xff_in_the_data() {
        let (data, replies) = run(&[IAC, IAC, b'a']);
        assert_eq!(data, vec![IAC, b'a']);
        assert!(replies.is_empty(), "an escaped IAC owes no reply");
    }

    #[test]
    fn standalone_commands_are_ignored() {
        // IAC GA (249) has no option byte, so it must be swallowed whole.
        let (data, replies) = run(&[b'x', IAC, 249, b'y']);
        assert_eq!(data, b"xy");
        assert!(replies.is_empty());
    }

    #[test]
    fn do_sga_is_accepted_but_do_echo_is_refused() {
        // We suppress go-ahead ourselves, so we are willing to do it.
        let (_, replies) = run(&[IAC, DO, OPT_SGA]);
        assert_eq!(replies, vec![IAC, WILL, OPT_SGA]);
        // We never offer to echo on the server's behalf.
        let (_, replies) = run(&[IAC, DO, OPT_ECHO]);
        assert_eq!(replies, vec![IAC, WONT, OPT_ECHO]);
    }

    #[test]
    fn will_echo_and_sga_are_accepted_but_will_naws_is_refused() {
        // Let the server echo and run in character mode.
        let (_, replies) = run(&[IAC, WILL, OPT_ECHO]);
        assert_eq!(replies, vec![IAC, DO, OPT_ECHO]);
        let (_, replies) = run(&[IAC, WILL, OPT_SGA]);
        assert_eq!(replies, vec![IAC, DO, OPT_SGA]);
        // NAWS is ours to advertise; the server must not negotiate it.
        let (_, replies) = run(&[IAC, WILL, OPT_NAWS]);
        assert_eq!(replies, vec![IAC, DONT, OPT_NAWS]);
    }

    #[test]
    fn dont_and_wont_are_always_acknowledged() {
        let (_, replies) = run(&[IAC, DONT, OPT_ECHO]);
        assert_eq!(replies, vec![IAC, WONT, OPT_ECHO]);
        let (_, replies) = run(&[IAC, WONT, OPT_ECHO]);
        assert_eq!(replies, vec![IAC, DONT, OPT_ECHO]);
    }

    #[test]
    fn subnegotiation_payload_is_swallowed() {
        let (data, replies) = run(&[b'a', IAC, SB, OPT_NAWS, 0x00, 0x50, IAC, SE, b'b']);
        assert_eq!(data, b"ab", "the SB payload must not reach the terminal");
        assert!(replies.is_empty());
    }

    #[test]
    fn escaped_iac_inside_subnegotiation_does_not_end_the_block() {
        // IAC IAC inside SB is a literal 0xFF, not a way out of the block.
        let (data, _) = run(&[IAC, SB, 0x01, IAC, IAC, 0x02, IAC, SE, b'z']);
        assert_eq!(data, b"z");
    }

    /// TCP gives no framing guarantees, so a negotiation can be split anywhere.
    /// This is the reason the parser carries `TnState` across reads at all.
    #[test]
    fn parser_state_survives_chunk_boundaries() {
        let mut state = TnState::Data;
        let (mut data, mut replies) = (Vec::new(), Vec::new());

        for b in [b'h', b'i', IAC, DO, OPT_SGA, b'!'] {
            process_incoming(&[b], &mut state, &mut data, &mut replies);
        }

        assert_eq!(data, b"hi!", "data around the split sequence must survive");
        assert_eq!(replies, vec![IAC, WILL, OPT_SGA]);
    }

    #[test]
    fn subnegotiation_split_across_chunks_is_still_swallowed() {
        let mut state = TnState::Data;
        let (mut data, mut replies) = (Vec::new(), Vec::new());

        process_incoming(&[b'a', IAC, SB], &mut state, &mut data, &mut replies);
        process_incoming(&[0x00, 0x50], &mut state, &mut data, &mut replies);
        process_incoming(&[IAC], &mut state, &mut data, &mut replies);
        process_incoming(&[SE, b'b'], &mut state, &mut data, &mut replies);

        assert_eq!(data, b"ab");
        assert!(replies.is_empty());
    }

    #[test]
    fn naws_frame_carries_big_endian_dimensions() {
        assert_eq!(
            naws_subneg(80, 24),
            vec![IAC, SB, OPT_NAWS, 0x00, 0x50, 0x00, 0x18, IAC, SE]
        );
    }

    #[test]
    fn naws_doubles_0xff_and_clamps_to_at_least_one() {
        // 255 is IAC itself, so both dimensions must be doubled.
        assert_eq!(
            naws_subneg(255, 255),
            vec![IAC, SB, OPT_NAWS, 0x00, IAC, IAC, 0x00, IAC, IAC, IAC, SE]
        );
        // Zero is clamped up to 1 so the server never sees a 0x0 window.
        assert_eq!(
            naws_subneg(0, 0),
            vec![IAC, SB, OPT_NAWS, 0x00, 0x01, 0x00, 0x01, IAC, SE]
        );
    }

    #[test]
    fn outbound_0xff_bytes_are_doubled() {
        assert_eq!(escape_iac(&[b'a', IAC, b'b']), vec![b'a', IAC, IAC, b'b']);
        assert_eq!(escape_iac(b"plain"), b"plain");
        assert!(escape_iac(&[]).is_empty());
    }
}
