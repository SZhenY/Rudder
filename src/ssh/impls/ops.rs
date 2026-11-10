use super::*;

pub(crate) async fn kill_remote_process(
    handle: Arc<Handle<ClientHandler>>,
    pid: u32,
    root_password: Option<crate::config::Secret>,
) -> ProcessKillResult {
    use zeroize::Zeroize as _;

    let privileged = root_password.is_some();
    // 口令下面会被 move 进闭包并 zeroize，这里留一份**只用于日志脱敏**的副本。
    // 它只活到这个函数返回；而 `result.message` 本身就含同一段明文（远端回显），
    // 所以这里没有新增暴露面 —— 目的只是别让明文落进磁盘上的 error.log。
    let redact_key = root_password
        .as_ref()
        .map(|secret| secret.as_str().to_string());
    let stage = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let operation_stage = stage.clone();
    let operation = async move {
        let started = std::time::Instant::now();
        tracing::warn!("[PROC_KILL] pid={pid} privileged={privileged} stage=open-channel begin");
        let mut channel = handle
            .channel_open_session()
            .await
            .context("open process-control channel")?;
        operation_stage.store(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            "[PROC_KILL] pid={pid} stage=open-channel ok elapsed_ms={}",
            started.elapsed().as_millis()
        );
        if privileged {
            // `sudo` authentication is commonly configured by PAM to require a
            // controlling terminal. Disable echo at the SSH PTY level so the
            // password can never be reflected into channel output or logs.
            channel
                .request_pty(true, "xterm", 80, 24, 0, 0, &[(russh::Pty::ECHO, 0)])
                .await
                .context("request process-control terminal")?;
            operation_stage.store(2, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                "[PROC_KILL] pid={pid} stage=request-pty ok echo=off elapsed_ms={}",
                started.elapsed().as_millis()
            );
        }
        let command = process_kill_command(pid, privileged);
        channel
            .exec(true, command.as_bytes())
            .await
            .context("execute process-control command")?;
        operation_stage.store(3, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            "[PROC_KILL] pid={pid} stage=exec-sudo ok elapsed_ms={} waiting_for_password_prompt={privileged}",
            started.elapsed().as_millis()
        );
        if !privileged {
            channel
                .eof()
                .await
                .context("finish process-control input")?;
        }

        let mut response = String::new();
        let mut password_sent = !privileged;
        let prompt_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let exit_status = loop {
            let msg = if !password_sent {
                match tokio::time::timeout_at(prompt_deadline, channel.wait()).await {
                    Ok(msg) => msg,
                    Err(_) => {
                        tracing::warn!(
                            "[PROC_KILL] pid={pid} stage=wait-password-prompt timeout; sending password fallback"
                        );
                        if let Some(password) = root_password.as_ref() {
                            let mut input = password.as_str().as_bytes().to_vec();
                            input.push(b'\r');
                            let sent = channel.data(&input[..]).await;
                            input.zeroize();
                            sent.context("write root password after prompt timeout")?;
                        }
                        password_sent = true;
                        operation_stage.store(5, std::sync::atomic::Ordering::Relaxed);
                        continue;
                    }
                }
            } else {
                channel.wait().await
            };
            let Some(msg) = msg else { break 1 };
            match msg {
                // ExitStatus is the authoritative completion result. Some SSH
                // servers keep a PTY channel open and never promptly follow it
                // with Close, so waiting beyond this point causes a false timeout.
                ChannelMsg::ExitStatus {
                    exit_status: status,
                } => {
                    operation_stage.store(6, std::sync::atomic::Ordering::Relaxed);
                    tracing::warn!(
                        "[PROC_KILL] pid={pid} stage=exit-status status={status} elapsed_ms={}",
                        started.elapsed().as_millis()
                    );
                    break status;
                }
                ChannelMsg::Close => {
                    tracing::warn!(
                        "[PROC_KILL] pid={pid} stage=channel-close without-exit-status elapsed_ms={}",
                        started.elapsed().as_millis()
                    );
                    break 1;
                }
                ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                    let text = String::from_utf8_lossy(&data);
                    let safe = process_control_log_text(
                        &text,
                        root_password.as_ref().map(|secret| secret.as_str()),
                    );
                    if !safe.is_empty() {
                        tracing::warn!("[PROC_KILL] pid={pid} stage=remote-output text={safe:?}");
                    }
                    if response.len() < 1024 {
                        response.push_str(&text);
                        response.truncate(response.len().min(1024));
                    }
                    if !password_sent && looks_like_sudo_password_prompt(&text) {
                        tracing::warn!(
                            "[PROC_KILL] pid={pid} stage=password-prompt detected; submitting secret"
                        );
                        if let Some(password) = root_password.as_ref() {
                            let mut input = password.as_str().as_bytes().to_vec();
                            input.push(b'\r');
                            let sent = channel.data(&input[..]).await;
                            input.zeroize();
                            sent.context("write root password after prompt")?;
                        }
                        password_sent = true;
                        operation_stage.store(5, std::sync::atomic::Ordering::Relaxed);
                        tracing::warn!(
                            "[PROC_KILL] pid={pid} stage=password-submitted elapsed_ms={}",
                            started.elapsed().as_millis()
                        );
                    }
                }
                _ => {}
            }
        };
        anyhow::Ok((exit_status, response))
    };

    let result = match tokio::time::timeout(std::time::Duration::from_secs(15), operation).await {
        Ok(Ok((0, _))) => ProcessKillResult {
            success: true,
            message: format!("{} PID {pid}", t("已发送 SIGTERM：", "SIGTERM sent to")),
        },
        Ok(Ok((_, response))) if privileged => ProcessKillResult {
            success: false,
            message: process_kill_failure_message(&response, true),
        },
        Ok(Ok((_, response))) => ProcessKillResult {
            success: false,
            message: process_kill_failure_message(&response, false),
        },
        Ok(Err(err)) => ProcessKillResult {
            success: false,
            message: format!(
                "{}: {err}",
                t("结束进程失败", "Failed to terminate process")
            ),
        },
        Err(_) => {
            let stage =
                process_control_stage_name(stage.load(std::sync::atomic::Ordering::Relaxed));
            tracing::warn!("[PROC_KILL] pid={pid} result=timeout stage={stage}");
            ProcessKillResult {
                success: false,
                message: format!(
                    "{} ({stage})",
                    t(
                        "结束进程超时，诊断已写入 error.log",
                        "Timed out; diagnostics were written to error.log"
                    )
                ),
            }
        }
    };
    // 日志一律过脱敏：message 里嵌着最多 1 KiB 的**原始远端输出**，而远端可能把
    // 刚输入的 sudo 口令回显出来 —— 这条是 WARN，默认会落到磁盘上的 error.log。
    tracing::warn!(
        "[PROC_KILL] pid={pid} result={} message={:?}",
        if result.success { "success" } else { "failure" },
        process_control_log_text(&result.message, redact_key.as_deref())
    );
    result
}

pub(crate) fn process_control_stage_name(stage: u8) -> &'static str {
    match stage {
        0 => "open-channel",
        1 => "request-pty",
        2 => "exec-sudo",
        3 => "wait-password-prompt",
        5 => "wait-exit-status",
        6 => "completed",
        _ => "unknown",
    }
}

pub(crate) fn looks_like_sudo_password_prompt(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("password") || lower.contains("密码")
}

pub(crate) fn process_control_log_text(text: &str, password: Option<&str>) -> String {
    let mut safe = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>();
    if let Some(password) = password.filter(|value| !value.is_empty()) {
        safe = safe.replace(password, "[REDACTED]");
    }
    safe.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(512)
        .collect()
}

pub(crate) fn process_kill_failure_message(response: &str, privileged: bool) -> String {
    let detail = response
        .replace(['\r', '\n'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !detail.is_empty() {
        return format!("{}: {detail}", t("结束失败", "Failed to terminate process"));
    }
    if privileged {
        t(
            "结束失败：服务器未返回具体的 sudo/PAM 错误",
            "Failed: the server returned no specific sudo/PAM error",
        )
        .to_string()
    } else {
        t(
            "结束失败：进程已退出或无权操作",
            "Failed: the process exited or permission was denied",
        )
        .to_string()
    }
}

pub(crate) fn process_kill_command(pid: u32, privileged: bool) -> String {
    if privileged {
        // `sudo` authenticates the connected account, matching what users run
        // manually. `su root` instead asks for the root account password, which
        // is commonly locked even when the user is an authorised sudoer.
        format!("LC_ALL=C sudo -S -p 'Password:' -- kill -TERM {pid}")
    } else {
        format!("kill -TERM {pid}")
    }
}

/// Entry point: spawn a session on the shared tokio runtime.
///
/// `initial_cols` / `initial_rows` are the PTY dimensions to request when
/// opening the channel. Slint fires a `terminal-resize` callback very shortly
/// after the tab becomes active; passing the best-known size here avoids the
/// remote shell starting at a stale 80×24 and sending an extra SIGWINCH.
///
/// Returns a [`SessionHandle`] for the UI + an [`UnboundedReceiver`] the UI
/// should drain on the Slint event loop.
pub fn spawn_session(
    runtime: &tokio::runtime::Handle,
    session: Session,
    jump: Option<Session>,
    initial_cols: u32,
    initial_rows: u32,
    trust_unknown_host_key: bool,
) -> (SessionHandle, UnboundedReceiver<SessionEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<SessionEvent>();

    let evt_tx_for_task = evt_tx.clone();
    // 丢弃 `JoinHandle` = detach（tokio 语义）：会话任务照常运行。
    runtime.spawn(async move {
        if let Err(err) = run_session(
            session,
            jump,
            cmd_rx,
            evt_tx_for_task.clone(),
            initial_cols,
            initial_rows,
            trust_unknown_host_key,
        )
        .await
        {
            tracing::warn!("ssh session ended with error: {err:#}");
            let _ = evt_tx_for_task.send(SessionEvent::Closed(classify_connect_failure(&err)));
        }
    });

    (SessionHandle { commands: cmd_tx }, evt_rx)
}

pub(crate) fn normalized_bind_addr(f: &PortForward) -> String {
    let bind = f.bind_addr.trim();
    if bind.is_empty() {
        "127.0.0.1".to_string()
    } else {
        bind.to_string()
    }
}

pub(crate) struct RuntimeForward {
    pub(crate) info: RuntimeTunnelInfo,
    pub(crate) task: Option<JoinHandle<()>>,
}

pub(crate) fn tunnel_label(f: &PortForward) -> String {
    if !f.name.trim().is_empty() {
        return f.name.trim().to_string();
    }
    match f.kind.as_str() {
        "local" => format!("-L {}:{}", normalized_bind_addr(f), f.bind_port),
        "remote" => format!("-R {}:{}", normalized_bind_addr(f), f.bind_port),
        "dynamic" => format!("-D {}:{}", normalized_bind_addr(f), f.bind_port),
        _ => format!("{} {}:{}", f.kind, normalized_bind_addr(f), f.bind_port),
    }
}

pub(crate) fn tunnel_info(
    id: String,
    f: &PortForward,
    active: bool,
    status: &str,
) -> RuntimeTunnelInfo {
    RuntimeTunnelInfo {
        id,
        name: tunnel_label(f),
        kind: f.kind.clone(),
        bind_addr: normalized_bind_addr(f),
        bind_port: f.bind_port,
        host: f.host.trim().to_string(),
        host_port: f.host_port,
        active,
        status: status.to_string(),
    }
}

pub(crate) fn emit_tunnel_update(
    forwards: &std::collections::HashMap<String, RuntimeForward>,
    events: &UnboundedSender<SessionEvent>,
) {
    let mut rows: Vec<RuntimeTunnelInfo> = forwards.values().map(|f| f.info.clone()).collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    let _ = events.send(SessionEvent::TunnelUpdate(rows));
}

pub(crate) fn start_runtime_forward(
    handle: Arc<Handle<ClientHandler>>,
    id: String,
    forward: PortForward,
    events: &UnboundedSender<SessionEvent>,
) -> RuntimeForward {
    let info = tunnel_info(id, &forward, true, t("运行中", "running"));
    let task = match forward.kind.as_str() {
        "local" => Some(crate::tunnel::spawn_local(
            handle,
            info.bind_addr.clone(),
            info.bind_port,
            info.host.clone(),
            info.host_port,
            events.clone(),
        )),
        "dynamic" => Some(crate::tunnel::spawn_dynamic(
            handle,
            info.bind_addr.clone(),
            info.bind_port,
            events.clone(),
        )),
        _ => None,
    };
    RuntimeForward { info, task }
}

/// Open an SSH transport to the session's host (directly or via a SOCKS5 / HTTP
/// proxy) and return the russh handle, ready for authentication. Factored out so
/// the keyboard-interactive fallback can reconnect on a *fresh* handle — russh
/// hangs if a second auth method is attempted on a handle whose first attempt
/// already failed (#86).
pub(crate) async fn connect_ssh(
    session: &Session,
    jump: Option<&Session>,
    config: Arc<client::Config>,
    events: &UnboundedSender<SessionEvent>,
    trust_unknown_host_key: bool,
) -> Result<(Handle<ClientHandler>, Option<Handle<ClientHandler>>)> {
    // Remote (-R) forwards are serviced inside the handler when the server opens
    // channels back, so it needs the bind-port → local-target map up front (the
    // handler is moved into `connect`) (#56).
    let remote_forwards: std::collections::HashMap<u32, (String, u16)> = session
        .forwards
        .iter()
        .filter(|f| f.kind == "remote")
        .map(|f| (f.bind_port as u32, (f.host.clone(), f.host_port)))
        .collect();
    let handler = ClientHandler {
        host: session.host.clone(),
        port: session.port,
        remote_forwards,
        events: events.clone(),
        trust_unknown_host_key,
    };
    let addr = format!("{}:{}", session.host, session.port);

    // SSH jump host (bastion): connect + authenticate the jump session, then open
    // a direct-tcpip channel through it to this host and run the SSH handshake
    // over that tunnel. The returned jump handle must be kept alive for the whole
    // session (the tunnel lives on it) (#211).
    if let Some(j) = jump {
        let _ = events.send(SessionEvent::Status(format!(
            "{} {}@{} → {}",
            t("经跳板机连接", "via jump host"),
            j.user,
            j.host,
            addr
        )));
        let (handle, jump_handle) = connect_target_via_jump(
            j,
            &session.host,
            session.port,
            config,
            handler,
            events,
            trust_unknown_host_key,
        )
        .await
        .with_context(|| format!("connect {} via jump failed", addr))?;
        return Ok((handle, Some(jump_handle)));
    }

    // Connect directly, or tunnel through a SOCKS5 / HTTP proxy (issue #7).
    let handle = match crate::ssh::proxy::resolve_session(session) {
        Some(p) => {
            let _ = events.send(SessionEvent::Status(format!(
                "{} {} → {}",
                t("经代理连接", "via proxy"),
                crate::ssh::proxy::describe(&p),
                addr
            )));
            let stream = crate::ssh::proxy::connect(&p, &session.host, session.port)
                .await
                .with_context(|| format!("proxy connect to {} failed", addr))?;
            client::connect_stream(config, stream, handler)
                .await
                .with_context(|| format!("connect {} failed", addr))?
        }
        None => client::connect(config, addr.as_str(), handler)
            .await
            .with_context(|| format!("connect {} failed", addr))?,
    };
    Ok((handle, None))
}

/// Outcome of authenticating an SSH session, so callers can distinguish a user
/// cancel from a credential rejection and word the status line accordingly.
pub(crate) enum AuthResult {
    Success,
    Cancelled,
    Failed,
}

/// Authenticate an already-connected SSH handle using the session's method,
/// prompting for missing credentials and supporting explicit / fallback
/// `keyboard-interactive` auth (#86, #249). Shared by the shell, SFTP and
/// jump-host paths. On the keyboard-interactive fallback it reconnects, updating
/// both `handle` and `jump_handle` in place so the caller keeps the live tunnel.
pub(crate) async fn authenticate_session(
    handle: &mut Handle<ClientHandler>,
    jump_handle: &mut Option<Handle<ClientHandler>>,
    session: &Session,
    jump: Option<&Session>,
    config: Arc<client::Config>,
    events: &UnboundedSender<SessionEvent>,
    trust_unknown_host_key: bool,
) -> Result<AuthResult> {
    let (user, password) = match resolve_credentials(session, events).await {
        Some(c) => c,
        None => return Ok(AuthResult::Cancelled),
    };

    let authed = match session.auth {
        AuthMethod::Password => {
            let mut ok = handle
                .authenticate_password(&user, password.as_str())
                .await
                .context("password auth failed")?
                .success();
            if !ok {
                // russh can't switch auth methods on a handle whose first attempt
                // already failed (it hangs), so reconnect on a fresh handle before
                // trying keyboard-interactive (#86).
                let _ = handle.disconnect(Disconnect::ByApplication, "", "").await;
                let (h, jh) = Box::pin(connect_ssh(
                    session,
                    jump,
                    config.clone(),
                    events,
                    trust_unknown_host_key,
                ))
                .await?;
                *handle = h;
                *jump_handle = jh;
                ok = keyboard_interactive_auth(
                    handle,
                    &user,
                    password.as_str(),
                    &session.id,
                    &session.host,
                    events,
                )
                .await
                .context("keyboard-interactive auth failed")?;
            }
            ok
        }
        AuthMethod::KeyboardInteractive => keyboard_interactive_auth(
            handle,
            &user,
            password.as_str(),
            &session.id,
            &session.host,
            events,
        )
        .await
        .context("keyboard-interactive auth failed")?,
        AuthMethod::Key => {
            // An encrypted private key needs its passphrase; we reuse the
            // session's password field for it (empty = unencrypted key) (#90).
            let pass = password.as_str();
            let keypair = load_session_private_key(session, pass)?;
            // RSA keys must be signed with an explicit SHA-2 hash; every other
            // key type carries its own algorithm, so no override is needed.
            let hash = keypair.algorithm().is_rsa().then_some(HashAlg::Sha256);
            let key_with_hash = PrivateKeyWithHashAlg::new(Arc::new(keypair), hash);
            handle
                .authenticate_publickey(&user, key_with_hash)
                .await
                .context("publickey auth failed")?
                .success()
        }
    };

    if authed {
        Ok(AuthResult::Success)
    } else {
        Ok(AuthResult::Failed)
    }
}

/// Connect + authenticate a jump/bastion session, open a `direct-tcpip` channel
/// to `target_host:target_port`, and run the target's SSH handshake over it.
/// Returns the target handle plus the jump handle, which the caller MUST keep
/// alive for as long as the target session lives (the tunnel rides on it) (#211).
pub(crate) async fn connect_target_via_jump<H>(
    jump: &Session,
    target_host: &str,
    target_port: u16,
    config: Arc<client::Config>,
    handler: H,
    events: &UnboundedSender<SessionEvent>,
    trust_unknown_host_key: bool,
) -> Result<(Handle<H>, Handle<ClientHandler>)>
where
    H: client::Handler + 'static,
    H::Error: std::error::Error + Send + Sync + 'static,
{
    // Single hop: the jump session itself never goes through another jump.
    // `Box::pin` breaks the async recursion (connect_ssh → jump → connect_ssh).
    let (mut jhandle, mut no_nested) = Box::pin(connect_ssh(
        jump,
        None,
        config.clone(),
        events,
        trust_unknown_host_key,
    ))
    .await
    .with_context(|| format!("connect jump host {}:{} failed", jump.host, jump.port))?;
    match authenticate_session(
        &mut jhandle,
        &mut no_nested,
        jump,
        None,
        config.clone(),
        events,
        trust_unknown_host_key,
    )
    .await?
    {
        AuthResult::Success => {}
        AuthResult::Cancelled => {
            return Err(anyhow!(t("跳板机登录已取消", "jump host login cancelled")));
        }
        AuthResult::Failed => {
            return Err(anyhow!(t(
                "跳板机认证失败",
                "jump host authentication failed"
            )));
        }
    }
    let channel = jhandle
        .channel_open_direct_tcpip(
            target_host.to_string(),
            target_port as u32,
            "127.0.0.1".to_string(),
            0,
        )
        .await
        .with_context(|| format!("open jump tunnel to {target_host}:{target_port}"))?;
    let handle = client::connect_stream(config, channel.into_stream(), handler)
        .await
        .with_context(|| format!("SSH handshake to {target_host}:{target_port} via jump"))?;
    Ok((handle, jhandle))
}

// Key-exchange algorithms offered to the server, strongest first. This is the
// russh default set PLUS the ecdh-sha2-nistp* curves and the legacy
// diffie-hellman-group{14,1}-sha1 exchanges appended as last-resort fallbacks, so
// we can still reach old servers / network gear that only speak SHA-1 KEX and
// otherwise fail with "No common algorithm" (#172). Modern servers still pick a
// strong algorithm because the client's order decides and SHA-1 is last.
pub(crate) const COMPAT_KEX: &[russh::kex::Name] = &[
    russh::kex::CURVE25519,
    russh::kex::CURVE25519_PRE_RFC_8731,
    russh::kex::DH_G16_SHA512,
    russh::kex::DH_G14_SHA256,
    russh::kex::ECDH_SHA2_NISTP256,
    russh::kex::ECDH_SHA2_NISTP384,
    russh::kex::ECDH_SHA2_NISTP521,
    russh::kex::DH_G14_SHA1, // legacy fallback
    russh::kex::DH_G1_SHA1,  // legacy fallback
    // Keep the OpenSSH ext-info / strict-kex markers so modern servers still
    // negotiate ext-info and strict kex (mirrors russh's default tail).
    russh::kex::EXTENSION_SUPPORT_AS_CLIENT,
    russh::kex::EXTENSION_SUPPORT_AS_SERVER,
    russh::kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT,
    russh::kex::EXTENSION_OPENSSH_STRICT_KEX_AS_SERVER,
];

// Ciphers offered to the server, strongest first: russh's AEAD/CTR defaults plus
// the legacy CBC ciphers appended for old servers that only support CBC (#172).
pub(crate) const COMPAT_CIPHER: &[russh::cipher::Name] = &[
    russh::cipher::CHACHA20_POLY1305,
    russh::cipher::AES_256_GCM,
    russh::cipher::AES_256_CTR,
    russh::cipher::AES_192_CTR,
    russh::cipher::AES_128_CTR,
    russh::cipher::AES_256_CBC,    // legacy fallback
    russh::cipher::AES_192_CBC,    // legacy fallback
    russh::cipher::AES_128_CBC,    // legacy fallback
    russh::cipher::TRIPLE_DES_CBC, // legacy fallback
];

// MACs offered to the server. russh 0.65 dropped the SHA-1 variants from its
// defaults (0.49 still had them), so spell the old default list out and keep the
// legacy tail last: old servers that only speak hmac-sha1 still connect, modern
// ones never get there (#172).
pub(crate) const COMPAT_MAC: &[russh::mac::Name] = &[
    russh::mac::HMAC_SHA512_ETM,
    russh::mac::HMAC_SHA256_ETM,
    russh::mac::HMAC_SHA512,
    russh::mac::HMAC_SHA256,
    russh::mac::HMAC_SHA1_ETM, // legacy fallback
    russh::mac::HMAC_SHA1,     // legacy fallback
];

/// SSH 握手参数：保活间隔与算法优先级。终端、跳板机、SFTP 共用同一份，
/// 避免某条连接漏掉兼容回退（#160 保活、#172 老服务器算法）。
pub(crate) fn ssh_client_config() -> Arc<client::Config> {
    Arc::new(client::Config {
        // Keep idle connections alive (#160). The terminal usually has the
        // resource-monitor channel streaming every 2 s, but with shell
        // integration disabled (#140) it can go idle and be dropped by
        // NAT / firewall / server timeouts.
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        // Match the normal terminal connection exactly, including compatibility
        // fallbacks for older servers and network equipment (#172).
        preferred: russh::Preferred {
            kex: std::borrow::Cow::Borrowed(COMPAT_KEX),
            cipher: std::borrow::Cow::Borrowed(COMPAT_CIPHER),
            mac: std::borrow::Cow::Borrowed(COMPAT_MAC),
            ..russh::Preferred::DEFAULT
        },
        ..<_>::default()
    })
}

/// Perform the same SSH handshake and authentication as a real terminal
/// connection, but disconnect immediately after authentication succeeds.
/// Prompt events are returned through `events` so the session dialog can reuse
/// the normal host-key, missing-credential, and MFA UI (#276).
pub async fn test_session_auth(
    session: Session,
    jump: Option<Session>,
    events: UnboundedSender<SessionEvent>,
) -> Result<()> {
    let config = ssh_client_config();
    let (mut handle, mut jump_handle) =
        connect_ssh(&session, jump.as_ref(), config.clone(), &events, false).await?;

    let auth = authenticate_session(
        &mut handle,
        &mut jump_handle,
        &session,
        jump.as_ref(),
        config,
        &events,
        false,
    )
    .await?;

    let result = match auth {
        AuthResult::Success => Ok(()),
        AuthResult::Cancelled => Err(anyhow!("login cancelled")),
        AuthResult::Failed => {
            // The (accepted-but-wrong) credentials cached for this session are
            // now known bad — drop them so the next attempt shows the dialog
            // again instead of silently reusing them (#cred-cache-invalidate).
            crate::app::auth_dialogs::invalidate_cred_cache(&session.id);
            Err(anyhow!("authentication failed"))
        }
    };

    let _ = handle
        .disconnect(Disconnect::ByApplication, "connection test complete", "")
        .await;
    if let Some(jump_handle) = jump_handle {
        let _ = jump_handle
            .disconnect(Disconnect::ByApplication, "connection test complete", "")
            .await;
    }
    result
}

/// Turn a connection-establishment failure into a user-readable, actionable
/// reason. Matches keywords across the whole anyhow chain (inner io::Errors
/// carry the precise kind: timeout / refused / unreachable / dns …). The
/// technical error stays in the tracing log; the user gets the verdict plus
/// what to check.
pub(crate) fn classify_connect_failure(err: &anyhow::Error) -> String {
    let joined: String = err
        .chain()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" | ")
        .to_lowercase();

    // 算法协商失败用变体匹配：russh 0.65 的 Display 会把双方算法列表整段打出来，
    // 直接透给用户就是一大串专有名词。这里只保留"哪一类算法"与"服务器提供什么"。
    if let Some(russh::Error::NoCommonAlgo { kind, theirs, .. }) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<russh::Error>())
    {
        let title = match kind {
            russh::AlgorithmKind::Kex => t(
                "没有双方都支持的密钥交换算法",
                "No common key-exchange algorithm",
            ),
            russh::AlgorithmKind::Key => t(
                "没有双方都支持的主机密钥算法",
                "No common host-key algorithm",
            ),
            russh::AlgorithmKind::Cipher => t("没有双方都支持的加密算法", "No common cipher"),
            russh::AlgorithmKind::Mac => t("没有双方都支持的消息认证算法", "No common MAC"),
            russh::AlgorithmKind::Compression => {
                t("没有双方都支持的压缩算法", "No common compression")
            }
        };
        let offered = if theirs.is_empty() {
            t("服务器未提供任何候选", "none").to_string()
        } else {
            theirs.join(", ")
        };
        format!("{title}: {} {offered}", t("服务器提供", "server offers"))
    } else if joined.contains("https proxy requires tls") {
        // Raised by `proxy::connect` for an `https://` proxy. Surfaced here so
        // the user gets the two working alternatives instead of a raw error.
        t(
            "HTTPS 代理不受支持：无法与代理建立 TLS 连接。请改用 socks5://（加密传输）或 http://（明文 CONNECT）",
            "HTTPS proxy is not supported: Rudder cannot speak TLS to a proxy. Use socks5:// (encrypted) or http:// (plaintext CONNECT)",
        )
        .to_string()
    } else if joined.contains("authentication failed") || joined.contains("auth failed") {
        t(
            "认证失败：用户名、密码或密钥被服务器拒绝，请核对凭据后按 Enter 重试",
            "Authentication failed: the username, password or key was rejected — check your credentials and press Enter to retry",
        )
        .to_string()
    } else if joined.contains("timed out") || joined.contains("timeout") {
        t(
            "连接超时：服务器无响应，请检查网络连通性与防火墙",
            "Timed out: the server did not respond — check network reachability and firewall",
        )
        .to_string()
    } else if joined.contains("refused") {
        t(
            "连接被拒绝：目标端口可能未开放，请确认 IP 与端口",
            "Connection refused: the port is likely closed — verify host and port",
        )
        .to_string()
    } else if joined.contains("no route") || joined.contains("unreachable") {
        t(
            "网络不可达：无法路由到目标主机，请检查本机网络",
            "Network unreachable: no route to the host — check local connectivity",
        )
        .to_string()
    } else if joined.contains("nodename")
        || joined.contains("name or service")
        || joined.contains("resolve")
        || joined.contains("getaddrinfo")
        || joined.contains("failed to lookup")
    {
        t(
            "域名解析失败：主机名无法解析，请检查拼写与 DNS",
            "DNS lookup failed: the hostname could not be resolved — check spelling and DNS",
        )
        .to_string()
    } else if joined.contains("host key") || joined.contains("known_hosts") {
        t(
            "主机密钥校验失败：服务器指纹与已知记录不符（可能重装或更换主机，需更新已知主机）",
            "Host key verification failed: the fingerprint differs from the known record (server reinstalled or replaced?)",
        )
        .to_string()
    } else if joined.contains("reset") || joined.contains("broken pipe") || joined.contains("eof") {
        t(
            "连接被远端重置：服务器在握手过程中断开了连接",
            "Connection reset by the remote host during handshake",
        )
        .to_string()
    } else {
        format!("{}: {err:#}", t("连接失败", "Connection failed"))
    }
}
