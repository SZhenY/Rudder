use super::*;

/// Commands posted to the worker task by the UI.
#[derive(Debug)]
pub enum SessionCommand {
    /// Send raw bytes directly to the PTY (individual keystrokes, no modification).
    RawInput(Vec<u8>),
    /// Notify the remote PTY of a terminal resize.
    Resize(u32, u32),
    /// Start or pause periodic local/remote resource monitoring for this session.
    SetResourceMonitoring(bool),
    /// Start a runtime-only SSH tunnel for this connected session (#206).
    AddTunnel {
        id: String,
        forward: crate::config::PortForward,
    },
    /// Stop a runtime tunnel created for this connected session (#206).
    StopTunnel(String),
    /// Terminate one remote process on a short-lived exec channel. Supplying a
    /// password selects the privileged `sudo -S` path; the secret is never
    /// written to the interactive PTY or shell history.
    KillProcess {
        pid: u32,
        root_password: Option<crate::config::Secret>,
        reply: tokio::sync::oneshot::Sender<ProcessKillResult>,
    },
    /// Run one command on a fresh exec channel. Opening the channel goes through
    /// the same "do not poll the PTY while opening" gate as the monitor channels
    /// (#264); the read phase does not block the PTY.
    Exec {
        command: String,
        timeout: std::time::Duration,
        max_output_bytes: usize,
        reply: tokio::sync::oneshot::Sender<ExecResult>,
    },
    /// Gracefully disconnect and drop the session.
    Close,
}

#[derive(Debug)]
pub struct ProcessKillResult {
    pub success: bool,
    pub message: String,
}

/// 一次性 exec 通道的结果。
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// 输出超过 `max_output_bytes`，后续内容被丢弃。
    pub truncated: bool,
    /// 超时后通道被关闭（远端进程是否退出不做保证）。
    pub timed_out: bool,
    pub duration: std::time::Duration,
    /// 开通道 / 写 exec 失败时的原因；`Some` 时其余字段无意义。
    pub error: Option<String>,
}

impl ExecResult {
    /// 该传输没有 exec 通道（串口 / Telnet / 本地 PTY）。
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
            duration: std::time::Duration::ZERO,
            error: Some(message.into()),
        }
    }
}

/// Carries the user's answer to a host-key confirmation prompt back to the
/// blocked `check_server_key` handler. Wrapped in `Arc<Mutex<Option<…>>>` so the
/// enclosing [`SessionEvent`] stays `Clone` (a bare `oneshot::Sender` is not);
/// the first `respond` consumes the sender, later calls are no-ops.
#[derive(Clone)]
pub struct HostKeyResponder(Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>);

impl HostKeyResponder {
    pub fn new(tx: tokio::sync::oneshot::Sender<bool>) -> Self {
        Self(Arc::new(std::sync::Mutex::new(Some(tx))))
    }

    /// Deliver the user's decision (`true` = trust). Idempotent.
    pub fn respond(&self, accept: bool) {
        if let Ok(mut guard) = self.0.lock()
            && let Some(tx) = guard.take()
        {
            let _ = tx.send(accept);
        }
    }
}

impl std::fmt::Debug for HostKeyResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostKeyResponder")
    }
}

/// The user's answer to a connect-time credential prompt: `(username, password,
/// remember)`, or `None` if they cancelled.
pub type CredentialReply = (String, String, bool);

/// Carries the credential prompt's answer back to the blocked auth flow (#110).
/// `Arc<Mutex<Option<…>>>` so the enclosing [`SessionEvent`] stays `Clone`.
#[derive(Clone)]
pub struct CredentialResponder(
    Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Option<CredentialReply>>>>>,
);

impl CredentialResponder {
    pub fn new(tx: tokio::sync::oneshot::Sender<Option<CredentialReply>>) -> Self {
        Self(Arc::new(std::sync::Mutex::new(Some(tx))))
    }

    /// Deliver the user's answer (`None` = cancelled). Idempotent.
    pub fn respond(&self, reply: Option<CredentialReply>) {
        if let Ok(mut guard) = self.0.lock()
            && let Some(tx) = guard.take()
        {
            let _ = tx.send(reply);
        }
    }
}

impl std::fmt::Debug for CredentialResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CredentialResponder")
    }
}

/// Carries the answer to a keyboard-interactive (MFA / verification-code) prompt
/// back to the blocked auth flow (#86-MFA). `None` = the user cancelled.
/// `Arc<Mutex<Option<…>>>` so the enclosing [`SessionEvent`] stays `Clone`.
#[derive(Clone)]
pub struct MfaResponder(
    Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Option<String>>>>>,
);

impl MfaResponder {
    pub fn new(tx: tokio::sync::oneshot::Sender<Option<String>>) -> Self {
        Self(Arc::new(std::sync::Mutex::new(Some(tx))))
    }

    /// Deliver the user's answer (`None` = cancelled). Idempotent.
    pub fn respond(&self, reply: Option<String>) {
        if let Ok(mut guard) = self.0.lock()
            && let Some(tx) = guard.take()
        {
            let _ = tx.send(reply);
        }
    }
}

impl std::fmt::Debug for MfaResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MfaResponder")
    }
}

/// One process row sampled from the remote `ps` (#23). CPU/mem are percentages
/// as reported by `ps` (pcpu/pmem); `command` is the (width-truncated) args.
#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub user: String,
    pub cpu: f32,
    pub mem: f32,
    pub command: String,
}

#[derive(Debug, Clone, Default)]
pub struct SystemDetails {
    pub overview: Vec<(String, String)>,
    pub cpu_info: Vec<(String, String)>,
    pub gpu_info: Vec<(String, String)>,
    pub cpu_usage: Vec<(String, String)>,
    pub memory: Vec<(String, String)>,
    pub swap: Vec<(String, String)>,
    pub networks: Vec<(String, String, String, String, String)>,
    pub filesystems: Vec<(String, String, String, String, String)>,
}

/// One SSH tunnel row shown in the runtime tunnel panel (#206).
#[derive(Debug, Clone)]
pub struct RuntimeTunnelInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub bind_addr: String,
    pub bind_port: u16,
    pub host: String,
    pub host_port: u16,
    pub active: bool,
    pub status: String,
}

/// Events emitted back to the UI thread.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Free-form status text for the tab header / status line.
    Status(String),
    /// A chunk of stdout/stderr output from the remote shell.
    Output(String),
    /// Connection is up.
    Connected,
    /// Connection closed (either cleanly or after an error).
    Closed(String),
    /// The server presented a host key that is unknown or has changed; the UI
    /// must show a confirmation dialog and answer via `responder` (#109-5). The
    /// handler is blocked awaiting that answer.
    HostKeyPrompt {
        host: String,
        port: u16,
        key_type: String,
        fingerprint: String,
        /// True when a *different* key was previously stored (possible MITM).
        changed: bool,
        responder: HostKeyResponder,
    },
    /// The session is missing a username and/or password; the UI must prompt for
    /// them and answer via `responder`. The auth flow is blocked meanwhile (#110).
    CredentialPrompt {
        session_id: String,
        host: String,
        user: String,
        need_user: bool,
        need_password: bool,
        responder: CredentialResponder,
    },
    /// A keyboard-interactive challenge that isn't the account password —
    /// typically an MFA / OTP / verification-code prompt from a bastion such as
    /// JumpServer. The UI shows `prompt` and answers via `responder`; the auth
    /// flow is blocked meanwhile (#86-MFA).
    MfaPrompt {
        session_id: String,
        host: String,
        /// The server's prompt text, e.g. "MFA code: " / "Verification code:".
        prompt: String,
        /// Whether typed input should be visible (false = hide, like a password).
        echo: bool,
        responder: MfaResponder,
    },
    /// Remote machine resource sample (from the monitor channel).
    /// Memory/swap are in KiB (as reported by /proc/meminfo).
    ResourceStats {
        cpu_percent: f32,
        mem_used_kib: u64,
        mem_total_kib: u64,
        swap_used_kib: u64,
        swap_total_kib: u64,
        /// Per-interface (name, rx_bytes_per_sec, tx_bytes_per_sec).
        net: Vec<(String, u64, u64)>,
        /// Per-filesystem (mount_point, available_bytes, total_bytes).
        disks: Vec<(String, u64, u64)>,
        /// Detailed system information for the detached system-info window.
        /// Detailed data is present only for the separately delayed one-shot
        /// system-information probe; lightweight resource samples leave it None.
        /// Boxed to keep the enum's stack/heap footprint small
        /// (clippy::large_enum_variant).
        sys: Box<Option<SystemDetails>>,
    },

    /// Effective user and top-process snapshot from the dedicated lightweight
    /// process channel. Keeping this separate prevents a slow `df`, `lspci`, or
    /// other system-information probe from freezing the process window.
    ProcessStats {
        current_user: String,
        procs: Vec<ProcInfo>,
    },

    /// A command the user ran in the terminal, captured via the shell hook
    /// (OSC 697) so it can join the command-box history (#113).
    CommandRan(String),

    /// Runtime tunnel state changed (#206).
    TunnelUpdate(Vec<RuntimeTunnelInfo>),

    // --- SFTP events -------------------------------------------------------
    /// The shell's current working directory changed (parsed from OSC 7).
    CwdChanged(String),
    /// SFTP directory listing arrived.
    SftpEntries {
        path: String,
        entries: Vec<RemoteEntry>,
    },
    /// Free-form SFTP status message (progress, errors, etc.).
    SftpStatus(String),
    /// A directory listing failed (e.g. permission denied): show the message and
    /// stop the panel's loading spinner without disturbing the current view (#112).
    SftpError(String),
    /// Directory tree structure changed (full rebuild pushed on every toggle).
    SftpTreeUpdate(Vec<RemoteTreeNode>),
    /// File-transfer progress / completion (download or upload).
    SftpTransfer {
        id: String,
        name: String,
        is_upload: bool,
        transferred: u64,
        total: u64,
        state: u8, // 0 = active, 1 = done, 2 = error
        msg: String,
    },
    /// A remote text file loaded for the built-in viewer/editor (#70). On
    /// failure (too large, binary, non-UTF-8, I/O error) `error` is non-empty
    /// and `content` is empty.
    SftpFileText {
        path: String,
        name: String,
        content: String,
        edit: bool,
        error: String,
    },
}

/// Handle retained by the UI layer to talk to a running session.
///
/// 只有 `commands`：`tab_id` 与 `join` 两个字段全项目从未被读过 —— 句柄本就按 tab_id
/// 存进 `HashMap`（键即身份，字段冗余）；丢弃 `JoinHandle` 只是 detach（tokio 语义），
/// 会话任务照跑，要停它靠给 `commands` 发指令。
pub struct SessionHandle {
    pub commands: UnboundedSender<SessionCommand>,
}

impl SessionHandle {
    pub fn send_raw(&self, bytes: Vec<u8>) {
        let _ = self.commands.send(SessionCommand::RawInput(bytes));
    }

    pub fn resize(&self, cols: u32, rows: u32) {
        let _ = self.commands.send(SessionCommand::Resize(cols, rows));
    }

    pub fn set_resource_monitoring(&self, enabled: bool) {
        let _ = self
            .commands
            .send(SessionCommand::SetResourceMonitoring(enabled));
    }

    pub fn add_tunnel(&self, id: String, forward: PortForward) {
        let _ = self
            .commands
            .send(SessionCommand::AddTunnel { id, forward });
    }

    pub fn stop_tunnel(&self, id: String) {
        let _ = self.commands.send(SessionCommand::StopTunnel(id));
    }

    pub fn kill_process(
        &self,
        pid: u32,
        root_password: Option<crate::config::Secret>,
    ) -> tokio::sync::oneshot::Receiver<ProcessKillResult> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let _ = self.commands.send(SessionCommand::KillProcess {
            pid,
            root_password,
            reply,
        });
        rx
    }

    pub fn close(&self) {
        let _ = self.commands.send(SessionCommand::Close);
    }
}

impl SessionHandle {
    /// 在会话的连接上开一条一次性 exec 通道执行 `command`。
    /// 返回的 `oneshot` 在命令结束（或失败 / 超时）时给出结果。
    pub fn exec(
        &self,
        command: String,
        timeout: std::time::Duration,
        max_output_bytes: usize,
    ) -> tokio::sync::oneshot::Receiver<ExecResult> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let _ = self.commands.send(SessionCommand::Exec {
            command,
            timeout,
            max_output_bytes,
            reply,
        });
        rx
    }
}
