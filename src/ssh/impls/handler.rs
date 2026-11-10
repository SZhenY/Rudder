use super::*;

/// True if a keyboard-interactive prompt is asking for a second factor (an MFA /
/// OTP / verification code) rather than the account password. We answer password
/// challenges automatically with the stored password but must ask the user for
/// these (#86-MFA). Heuristic over the common English/Chinese wordings used by
/// JumpServer, Google Authenticator (PAM), Duo, etc.
pub(crate) fn looks_like_mfa(prompt: &str) -> bool {
    let t = prompt.to_lowercase();
    t.contains("code")
        || t.contains("otp")
        || t.contains("mfa")
        || t.contains("2fa")
        || t.contains("factor") // two-factor / second factor
        || t.contains("duo")
        || t.contains("verification")
        || t.contains("verify")
        || t.contains("token")
        || t.contains("authenticator")
        || t.contains("passcode")
        || t.contains("one-time")
        || t.contains("one time")
        || t.contains("验证码")
        || t.contains("动态")
        || t.contains("令牌")
}

/// Authenticate via `keyboard-interactive`. The stored password answers the
/// first password challenge automatically (the JumpServer-style bastions that
/// disable the plain `password` method, #86); any *other* challenge — an MFA /
/// verification-code prompt — is shown to the user, whose typed answer is sent
/// back. This is what makes MFA-enabled bastions (JumpServer with MFA forced on)
/// work (#86-MFA).
pub(crate) async fn keyboard_interactive_auth<H>(
    handle: &mut Handle<H>,
    user: &str,
    password: &str,
    session_id: &str,
    host: &str,
    events: &UnboundedSender<SessionEvent>,
) -> Result<bool>
where
    H: Handler + 'static,
    H::Error: std::error::Error + Send + Sync + 'static,
{
    use russh::client::KeyboardInteractiveAuthResponse as Kb;
    let mut res = handle
        .authenticate_keyboard_interactive_start(user.to_string(), None)
        .await?;
    let mut password_used = false;
    // Bound the exchange so a misbehaving server can't loop us forever.
    for _ in 0..16 {
        match res {
            Kb::Success => return Ok(true),
            Kb::Failure { .. } => return Ok(false),
            Kb::InfoRequest { prompts, .. } => {
                let mut responses = Vec::with_capacity(prompts.len());
                for p in &prompts {
                    // Use the stored password for the first password-like
                    // challenge; ask the user for everything else (MFA codes).
                    if !password_used && !password.is_empty() && !looks_like_mfa(&p.prompt) {
                        responses.push(password.to_string());
                        password_used = true;
                    } else {
                        match ask_mfa_prompt(session_id, host, &p.prompt, p.echo, events).await {
                            Some(answer) => responses.push(answer),
                            None => return Ok(false), // user cancelled
                        }
                    }
                }
                res = handle
                    .authenticate_keyboard_interactive_respond(responses)
                    .await?;
            }
        }
    }
    Ok(false)
}

/// Ask the UI for a single keyboard-interactive answer (an MFA / verification
/// code), blocking until the user responds. `None` = cancelled or no UI (#86-MFA).
pub(crate) async fn ask_mfa_prompt(
    session_id: &str,
    host: &str,
    prompt: &str,
    echo: bool,
    events: &UnboundedSender<SessionEvent>,
) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let sent = events.send(SessionEvent::MfaPrompt {
        session_id: session_id.to_string(),
        host: host.to_string(),
        prompt: prompt.to_string(),
        echo,
        responder: MfaResponder::new(tx),
    });
    if sent.is_err() {
        return None; // no UI to ask
    }
    rx.await.ok().flatten()
}

/// Client handler. Verifies the server host key against the known_hosts store,
/// prompting the user on first contact / on a changed key (#109-5).
///
/// Carries the remote-forward (-R) map so we can service channels the server
/// opens back to us: server bind-port → local `(host, port)` target (#56).
pub(crate) struct ClientHandler {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) remote_forwards: std::collections::HashMap<u32, (String, u16)>,
    pub(crate) events: UnboundedSender<SessionEvent>,
    /// 首次连接未知主机时直接信任并记录（MCP 显式授权）。密钥**变更**不看它。
    pub(crate) trust_unknown_host_key: bool,
}

/// Shared host-key check used by both the shell and SFTP connections: trust a
/// matching stored key silently; otherwise ask the UI (via `events`) and, on
/// acceptance, remember the key. A dropped/closed reply channel (UI gone)
/// counts as a rejection so we never connect to an unverified host.
///
/// `trust_unknown` 只对**未知**主机生效（MCP 显式授权首次连接）：直接记录并放行，
/// 不弹窗。**密钥变更永远弹窗** —— 那可能是中间人，必须由人判断。
pub(crate) async fn verify_host_key(
    host: &str,
    port: u16,
    key: &PublicKey,
    events: &UnboundedSender<SessionEvent>,
    trust_unknown: bool,
) -> bool {
    use crate::ssh::known_hosts::HostKeyStatus;
    match crate::ssh::known_hosts::verify(host, port, key) {
        HostKeyStatus::Match => true,
        status => {
            let changed = status == HostKeyStatus::Changed;
            if trust_unknown && !changed {
                if let Err(e) = crate::ssh::known_hosts::remember(host, port, key) {
                    tracing::warn!("could not save host key for {host}:{port}: {e:#}");
                }
                return true;
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            let sent = events.send(SessionEvent::HostKeyPrompt {
                host: host.to_string(),
                port,
                key_type: key.algorithm().to_string(),
                fingerprint: crate::ssh::known_hosts::fingerprint(key),
                changed,
                responder: HostKeyResponder::new(tx),
            });
            if sent.is_err() {
                return false; // no UI to ask
            }
            match rx.await {
                Ok(true) => {
                    if let Err(e) = crate::ssh::known_hosts::remember(host, port, key) {
                        tracing::warn!("could not save host key for {host}:{port}: {e:#}");
                    }
                    true
                }
                _ => false,
            }
        }
    }
}

/// Resolve a session's username/password, prompting the UI for whatever is
/// missing (#110). Returns the effective `(user, password)`, or `None` if the
/// user cancelled. Both the shell and SFTP connections call this; the UI
/// de-duplicates by session id so a single dialog serves both. A dropped reply
/// channel (no UI) falls through with the stored values so auth fails normally.
pub(crate) async fn resolve_credentials(
    session: &Session,
    events: &UnboundedSender<SessionEvent>,
) -> Option<(String, String)> {
    let mut user = session.user.trim().to_string();
    let mut password = session.password.as_str().to_string();
    let need_user = user.is_empty();
    let need_password = matches!(session.auth, AuthMethod::Password) && password.is_empty();
    if !(need_user || need_password) {
        return Some((user, password));
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let sent = events.send(SessionEvent::CredentialPrompt {
        session_id: session.id.clone(),
        host: session.host.clone(),
        user: user.clone(),
        need_user,
        need_password,
        responder: CredentialResponder::new(tx),
    });
    if sent.is_err() {
        return Some((user, password));
    }
    match rx.await {
        Ok(Some((u, p, _remember))) => {
            if need_user {
                user = u.trim().to_string();
            }
            if need_password {
                password = p;
            }
            Some((user, password))
        }
        _ => None,
    }
}

impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // 0.65 把参数换成"公钥或主机证书"。我们不配置 CA 信任库，证书按它认证的
        // 那把 key 校验（与 OpenSSH 无 CA 时的行为一致）。
        let key = server_public_key.public_key();
        Ok(verify_host_key(
            &self.host,
            self.port,
            &key,
            &self.events,
            self.trust_unknown_host_key,
        )
        .await)
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        _data: &[u8],
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Remote forward (-R): the server opened a channel for a connection that
    /// arrived on a port we asked it to listen on. Connect to the configured
    /// local target and splice the two together (#56).
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<Msg>,
        connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: russh::client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let target = self.remote_forwards.get(&connected_port).cloned();
        let Some((host, port)) = target else {
            tracing::warn!(
                "forwarded-tcpip on {connected_address}:{connected_port} with no mapping"
            );
            reply
                .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        };
        // 0.65 起，入站通道要显式表态：`ChannelOpenHandle` 被 drop 时会自动回
        // AdministrativelyProhibited。必须在 spawn 之前 accept，否则 -R 静默失效。
        reply.accept().await;
        let events = self.events.clone();
        tokio::spawn(async move {
            match tokio::net::TcpStream::connect((host.as_str(), port)).await {
                Ok(mut tcp) => {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
                }
                Err(e) => {
                    let _ = events.send(SessionEvent::Output(format!(
                        "\r\n[rudder] -R {host}:{port} 连接失败 / connect failed: {e}\r\n"
                    )));
                }
            }
        });
        Ok(())
    }
}

// Compile-time assertion: `Handle<ClientHandler>` must stay `Send` — external code
// (SSH forwards) holds it behind an `Arc` across threads. If the bound ever breaks,
// this const fails to compile.
const _: fn() = || {
    fn takes<T: Send>() {}
    takes::<Handle<ClientHandler>>();
};
