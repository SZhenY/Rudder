use super::*;

/// The canonical ZMODEM abort sequence: eight CAN (0x18) then eight BS (0x08).
/// Sending this makes the remote `sz`/`rz` give up so the session recovers (#76).
pub(crate) const ZMODEM_CANCEL: [u8; 16] = [
    0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08,
];

pub(crate) const PROMPT_SETUP_PREFIX: &str = "test -z \"$FISH_VERSION\"";
pub(crate) const PROMPT_SETUP_SUFFIX: &str = "__ms7'";
#[cfg(test)]
pub(crate) const PROMPT_SETUP_HISTORY_MARKER: &str = "__MEATSHELL_INTERNAL_SETUP_1";
pub(crate) const PROMPT_BODY: &str = "test -z \"$FISH_VERSION\" && eval '__msc(){ __c=\"$(fc -ln -1 2>/dev/null)\"; [ -n \"$__c\" ] && [ \"$__c\" != \"$__cl\" ] && { __cl=\"$__c\"; printf \"\\033]697;%s\\007\" \"$__c\"; }; }; __ms7(){ printf \"\\033]7;file://%s%s\\007\" \"$HOSTNAME\" \"$PWD\"; __msc; }; if [ -n \"$ZSH_VERSION\" ]; then autoload -Uz add-zsh-hook 2>/dev/null; add-zsh-hook precmd __ms7; else PROMPT_COMMAND=\"__ms7${PROMPT_COMMAND:+;$PROMPT_COMMAND}\"; fi; : __MEATSHELL_INTERNAL_SETUP_1; if [ -n \"$BASH_VERSION\" ]; then __md=\"$(history 2>/dev/null | { __md=\"\"; while read -r __mn __mr; do case \"$__mr\" in *\"__ms7()\"*\"PROMPT_COMMAND=\"*) __mn=\"${__mn%\\*}\"; __md=\"$__mn $__md\";; esac; done; printf \"%s\" \"$__md\"; })\"; for __mn in $__md; do history -d \"$__mn\" 2>/dev/null; done; unset __md __mn __mr; fi; __cl=\"$(fc -ln -1 2>/dev/null)\"; __ms7'";
pub(crate) const PROMPT_SHELL_PROBE: &[u8] = b"if [ -n \"$BASH_VERSION\" ]; then printf '__MEATSHELL_SHELL__:bash\\n'; elif [ -n \"$ZSH_VERSION\" ]; then printf '__MEATSHELL_SHELL__:zsh\\n'; else printf '__MEATSHELL_SHELL__:other\\n'; fi";

/// Auxiliary (non-PTY) channel kinds, opened strictly in sequence after the
/// terminal is usable. Opening them from detached tasks while the primary
/// PTY's `channel.wait()` was being polled made strict SSH servers drop the
/// transport exactly when the first monitor started (#264 follow-up).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuxiliaryChannelKind {
    Resources,
    Processes,
    SystemInfo,
}

impl AuxiliaryChannelKind {
    pub(crate) fn delay(self) -> std::time::Duration {
        match self {
            Self::Resources => std::time::Duration::from_millis(750),
            Self::Processes => std::time::Duration::from_millis(1500),
            Self::SystemInfo => std::time::Duration::from_millis(2500),
        }
    }

    pub(crate) fn next(self) -> Option<Self> {
        match self {
            Self::Resources => Some(Self::Processes),
            Self::Processes => Some(Self::SystemInfo),
            Self::SystemInfo => None,
        }
    }
}

pub(crate) struct AuxiliaryStartup {
    pub(crate) pending: Option<AuxiliaryChannelKind>,
    pub(crate) in_progress: bool,
}

impl AuxiliaryStartup {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            pending: enabled.then_some(AuxiliaryChannelKind::Resources),
            in_progress: false,
        }
    }

    pub(crate) fn pending(&self) -> Option<AuxiliaryChannelKind> {
        if self.in_progress { None } else { self.pending }
    }

    pub(crate) fn begin(&mut self) -> Option<AuxiliaryChannelKind> {
        let kind = self.pending()?;
        if self.in_progress {
            return None;
        }
        self.in_progress = true;
        Some(kind)
    }

    pub(crate) fn complete(&mut self) {
        if !self.in_progress {
            return;
        }
        self.pending = self.pending.and_then(AuxiliaryChannelKind::next);
        self.in_progress = false;
    }
}

pub(crate) const AUXILIARY_CHANNEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 辅助通道**整批**的看门狗。开一条通道本身有 3s 超时、一批最多两条，10s 留足余量；
/// 超时说明负责开通道的任务出了意外，此时必须强制复位在飞标志，否则主 PTY 的
/// `channel.wait()` 会一直挂着（会话假死）。
pub(crate) const AUX_OPEN_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(10);

/// 会话收尾（`eof` / `disconnect`）的等待上限，网络半死时不再一直挂着。
pub(crate) const CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

pub(crate) async fn auxiliary_timeout<F, T>(
    timeout: std::time::Duration,
    future: F,
) -> Result<T, tokio::time::error::Elapsed>
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(timeout, future).await
}

pub(crate) async fn open_auxiliary_channel(
    handle: &Handle<ClientHandler>,
    command: &[u8],
    label: &'static str,
) -> Option<Channel<Msg>> {
    let operation = async {
        let channel = handle
            .channel_open_session()
            .await
            .with_context(|| format!("{label} channel open"))?;
        channel
            .exec(true, command)
            .await
            .with_context(|| format!("{label} exec"))?;
        Ok::<Channel<Msg>, anyhow::Error>(channel)
    };

    match auxiliary_timeout(AUXILIARY_CHANNEL_TIMEOUT, operation).await {
        Ok(Ok(channel)) => Some(channel),
        Ok(Err(error)) => {
            tracing::warn!("{label} startup failed: {error:#}");
            None
        }
        Err(_) => {
            tracing::warn!(
                "{label} startup timed out after {} ms",
                AUXILIARY_CHANNEL_TIMEOUT.as_millis()
            );
            None
        }
    }
}

/// 在**后台任务**里打开辅助通道，结果经 `tx` 送回泵 —— 于是泵在开通道期间
/// 仍能处理输入 / resize，不再被阻塞（早先在这里就地 await 会把输出、输入、
/// resize 一起冻住整整一个超时窗口）。
///
/// ⚠️ 调用方必须同时保证：**任务在飞时不要轮询主 PTY 的 `channel.wait()`**。
/// 一边轮询 `wait()` 一边开通道，严格的 SSH 服务端会直接断开传输（#264）。
/// 泵里对应地有 `aux_open_in_flight` 把 `wait()` 分支挂起。
/// 后台任务打开辅助通道后的**整批**结果。
///
/// ⚠️ 必须是"一批"而不是"一条"：一批里可能有多条通道（重开监控是两条），
/// 泵只在**整批到达**后才清 `aux_open_in_flight`。若开一条就发一条，第一条
/// 到达时泵就会恢复轮询 `channel.wait()`，而此时第二条还在开 —— 那正是 #264
/// 禁止的情形（轮询主 PTY 的同时开通道 → 严格的服务端断开传输）。
pub(crate) type AuxOpenBatch = Vec<(AuxiliaryChannelKind, Option<Channel<Msg>>)>;

pub(crate) fn spawn_auxiliary_open(
    handle: Arc<Handle<ClientHandler>>,
    kind: AuxiliaryChannelKind,
    command: Vec<u8>,
    label: &'static str,
    tx: tokio::sync::mpsc::UnboundedSender<AuxOpenBatch>,
) {
    tokio::spawn(async move {
        let channel = open_auxiliary_channel(&handle, &command, label).await;
        let _ = tx.send(vec![(kind, channel)]);
    });
}

/// 重开两条监控通道（Resources → Processes），**串行**放在同一个后台任务里 ——
/// 于是任意时刻最多只有一个开通道的任务在飞，泵那边的
/// `aux_open_in_flight` 也就只有一个为真，天然满足 #264 的约束。
///
/// 泵已退出时 `tx.send` 会失败，直接收手；已开出的那条通道随连接一起回收。
pub(crate) fn spawn_monitoring_channels(
    handle: Arc<Handle<ClientHandler>>,
    mon_cmd: Vec<u8>,
    proc_cmd: Vec<u8>,
    tx: tokio::sync::mpsc::UnboundedSender<AuxOpenBatch>,
) {
    tokio::spawn(async move {
        // 两条都开完再**整批**发回：期间泵一直挂着 `channel.wait()`，
        // 绝不出现在"开第二条"时已在轮询主 PTY 的情形（#264）。
        let mut batch = AuxOpenBatch::new();
        for (kind, command, label) in [
            (AuxiliaryChannelKind::Resources, mon_cmd.as_slice(), "monitor"),
            (AuxiliaryChannelKind::Processes, proc_cmd.as_slice(), "process monitor"),
        ] {
            let channel = open_auxiliary_channel(&handle, command, label).await;
            batch.push((kind, channel));
        }
        let _ = tx.send(batch);
    });
}

/// 排队中的一次性命令。
pub(crate) struct ExecRequest {
    pub(crate) command: String,
    pub(crate) timeout: std::time::Duration,
    pub(crate) max_output_bytes: usize,
    pub(crate) reply: tokio::sync::oneshot::Sender<ExecResult>,
}

/// 从队列取一条命令并启动。
///
/// 与监控通道共用 `aux_open_in_flight`：开通道期间泵会挂起主 PTY 的
/// `channel.wait()`（#264）。开完立刻经 `done` 通知泵恢复，读取阶段不再占用互斥。
pub(crate) fn start_next_exec(
    pending: &mut std::collections::VecDeque<ExecRequest>,
    in_flight: &mut bool,
    started: &mut Option<tokio::time::Instant>,
    handle: &Arc<Handle<ClientHandler>>,
    done: &tokio::sync::mpsc::UnboundedSender<()>,
) {
    if *in_flight || pending.is_empty() {
        return;
    }
    let request = pending.pop_front().expect("checked non-empty");
    *in_flight = true;
    *started = Some(tokio::time::Instant::now());
    let handle = handle.clone();
    let done = done.clone();
    tokio::spawn(async move { run_exec(handle, request, done).await });
}

pub(crate) async fn run_exec(
    handle: Arc<Handle<ClientHandler>>,
    request: ExecRequest,
    done: tokio::sync::mpsc::UnboundedSender<()>,
) {
    let started = std::time::Instant::now();
    let mut result = ExecResult {
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        truncated: false,
        timed_out: false,
        duration: std::time::Duration::ZERO,
        error: None,
    };

    let opened = auxiliary_timeout(AUXILIARY_CHANNEL_TIMEOUT, async {
        let channel = handle.channel_open_session().await?;
        channel.exec(true, request.command.as_str()).await?;
        Ok::<_, russh::Error>(channel)
    })
    .await;
    // 开通道阶段结束：无论成败都立刻让泵恢复主 PTY 轮询。
    let _ = done.send(());

    let mut channel = match opened {
        Ok(Ok(channel)) => channel,
        Ok(Err(err)) => {
            result.error = Some(format!("exec channel: {err}"));
            result.duration = started.elapsed();
            let _ = request.reply.send(result);
            return;
        }
        Err(_) => {
            result.error = Some("exec channel open timed out".to_string());
            result.duration = started.elapsed();
            let _ = request.reply.send(result);
            return;
        }
    };

    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    // 多数服务端把 exit-status 放在 EOF **之后**再发。原来遇到 EOF 直接 break，
    // 退出码永远是 None；这里给 EOF 后留一小段窗口取回状态，拿不到就退出 ——
    // 个别实现不发 CLOSE，不能一直等到整体超时。
    const EXIT_STATUS_GRACE: std::time::Duration = std::time::Duration::from_millis(500);
    let mut saw_eof = false;
    let read = async {
        loop {
            let message = if saw_eof {
                match tokio::time::timeout(EXIT_STATUS_GRACE, channel.wait()).await {
                    Ok(message) => message,
                    Err(_) => break,
                }
            } else {
                channel.wait().await
            };
            match message {
                Some(ChannelMsg::Data { data }) => {
                    let room = request.max_output_bytes.saturating_sub(stdout.len());
                    if data.len() > room {
                        result.truncated = true;
                    }
                    stdout.extend_from_slice(&data[..data.len().min(room)]);
                }
                Some(ChannelMsg::ExtendedData { data, ext }) => {
                    if ext != 1 {
                        continue;
                    }
                    let room = request.max_output_bytes.saturating_sub(stderr.len());
                    if data.len() > room {
                        result.truncated = true;
                    }
                    stderr.extend_from_slice(&data[..data.len().min(room)]);
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    result.exit_code = Some(exit_status as i32);
                }
                Some(ChannelMsg::Eof) => saw_eof = true,
                Some(ChannelMsg::Close) | None => break,
                _ => {}
            }
        }
    };

    if tokio::time::timeout(request.timeout, read).await.is_err() {
        result.timed_out = true;
        let _ = channel.close().await;
    }
    result.duration = started.elapsed();
    // 远端可能是 GBK 等非 UTF-8 编码；这里按 UTF-8 有损解码，与读屏口径一致。
    result.stdout = String::from_utf8_lossy(&stdout).into_owned();
    result.stderr = String::from_utf8_lossy(&stderr).into_owned();
    let _ = request.reply.send(result);
}

pub(crate) fn prompt_setup_supported(probe_output: &str) -> Option<bool> {
    if probe_output.contains("__MEATSHELL_SHELL__:bash")
        || probe_output.contains("__MEATSHELL_SHELL__:zsh")
    {
        Some(true)
    } else if probe_output.contains("__MEATSHELL_SHELL__:other") {
        Some(false)
    } else {
        None
    }
}

/// Probe the login shell through a separate exec channel so unsupported shells
/// never see the long interactive prompt-integration command. In particular,
/// BusyBox ash (used by OpenWrt) ignores `PROMPT_COMMAND`; injecting into its
/// line editor only risks a visible partial command or continuation prompt.
pub(crate) async fn remote_supports_prompt_setup(handle: &Handle<ClientHandler>) -> bool {
    let probe = async {
        let mut channel = handle.channel_open_session().await.ok()?;
        channel.exec(true, PROMPT_SHELL_PROBE).await.ok()?;
        let _ = channel.eof().await;

        let mut output = String::new();
        let mut supported: Option<bool> = None;
        // Drain the channel fully, including the server's CHANNEL_CLOSE. Do not
        // return as soon as the marker is seen (that drops the Channel
        // mid-flight) and do not just send `channel.close()` without awaiting
        // the peer's confirmation: some servers (Dropbear in particular) reuse
        // channel IDs aggressively and tear down the *next* channel — the
        // interactive shell — if it is opened while this one is still being
        // torn down, which surfaces as an instant disconnect right after login.
        // Draining to Close serializes the teardown and avoids that race; for
        // OpenSSH it is a no-op.
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                    if supported.is_none() {
                        output.push_str(&String::from_utf8_lossy(&data));
                        if let Some(s) = prompt_setup_supported(&output) {
                            supported = Some(s);
                        } else if output.len() > 256 {
                            supported = Some(false);
                        }
                    }
                }
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        supported
    };

    tokio::time::timeout(std::time::Duration::from_millis(1000), probe)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// Which side of a ZMODEM transfer the remote is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZmodemDirection {
    /// Remote `sz` sends files; Rudder receives them into Downloads.
    Download,
    /// Remote `rz` receives files; Rudder sends the locally picked selection.
    Upload,
}

/// Identify the first hex ZMODEM handshake by its actual frame type. Remote
/// `sz` starts with ZRQINIT (`00`), while remote `rz` starts with ZRINIT (`01`).
/// Treating both as a receive operation made `rz` deadlock because each side
/// waited for the other to start sending (#308).
pub(crate) fn zmodem_direction(data: &[u8]) -> Option<ZmodemDirection> {
    data.windows(4).find_map(|window| {
        if window[0] != 0x18 || window[1] != b'B' {
            return None;
        }
        let high = zmodem_hex_nibble(window[2])?;
        let low = zmodem_hex_nibble(window[3])?;
        match (high << 4) | low {
            0 => Some(ZmodemDirection::Download),
            1 => Some(ZmodemDirection::Upload),
            _ => None,
        }
    })
}

pub(crate) fn zmodem_hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn line_start_before(text: &str, pos: usize) -> usize {
    text[..pos].rfind(['\r', '\n']).map(|i| i + 1).unwrap_or(0)
}

pub(crate) fn include_following_line_break(text: &str, mut pos: usize) -> usize {
    let bytes = text.as_bytes();
    if pos < bytes.len() && bytes[pos] == b'\r' {
        pos += 1;
        if pos < bytes.len() && bytes[pos] == b'\n' {
            pos += 1;
        }
    } else if pos < bytes.len() && bytes[pos] == b'\n' {
        pos += 1;
        if pos < bytes.len() && bytes[pos] == b'\r' {
            pos += 1;
        }
    }
    pos
}

/// End of the echoed setup command.  The suffix (`__ms7'`) is the command's
/// final characters, so a match means the whole — possibly wrapped — line has
/// landed.  Returns `None` while the line is still incomplete: callers must
/// keep waiting or, past the late-echo window, discard.  They must NEVER fall
/// back to a line end: the ~1.4 KiB command wraps across many rows, so cutting
/// at the first newline leaks every wrapped row after it (#289 follow-up).
pub(crate) fn prompt_setup_echo_end(text: &str, prefix_pos: usize) -> Option<usize> {
    text[prefix_pos..]
        .find(PROMPT_SETUP_SUFFIX)
        .map(|rel| prefix_pos + rel + PROMPT_SETUP_SUFFIX.len())
}

/// Discard everything from the setup line's start to the end of the buffer.
///
/// Used once the late-echo window has expired or the buffer cap has blown
/// while the wrapped echo is still incomplete.  At that point the buffer holds
/// little but the setup echo — the session began moments ago, so no user
/// output has accumulated yet — and cutting at the first newline would leak
/// the remaining wrapped rows of the command.
pub(crate) fn discard_prompt_setup_tail(text: &mut String, prefix_pos: usize) {
    let start = line_start_before(text, prefix_pos);
    text.replace_range(start.., "\r\x1b[2K");
}

/// Release the late-echo accumulator once its window has expired.  A setup
/// echo that only now finishes arriving must not leak: if the prefix has
/// landed, discard the whole remainder first, then release.
pub(crate) fn release_after_window_expiry(buf: &mut String, chunk: &str) -> String {
    let mut held = std::mem::take(buf);
    held.push_str(chunk);
    if let Some(p) = held.find(PROMPT_SETUP_PREFIX) {
        discard_prompt_setup_tail(&mut held, p);
    }
    held
}

/// Final safety net: strip a fully-landed setup echo out of any output block
/// about to be rendered, regardless of the suppress state machine's state.
///
/// The user's requirement is absolute — the setup command must never show, on
/// any session, under any timing.  The suppression window / accumulator only
/// cover the "normal" races; if the state machine ends up disabled for any
/// reason while a wrapped echo still lands, this guard still catches it.
///
/// Matching is anchored on the command's unique prefix (`test -z "$FISH_VERSION"`)
/// and terminated by the command's final `__ms7'` or the OSC 7 that follows it
/// (or the block end).  False positives would require the user to type a
/// command literally starting with `test -z "$FISH_VERSION"` — essentially
/// impossible, and exactly the case #289 wanted stripped anyway.
pub(crate) fn final_setup_guard(text: &mut String) -> bool {
    let Some(p) = text.find(PROMPT_SETUP_PREFIX) else {
        return false;
    };
    // Terminate at the suffix (command tail), the trailing OSC 7, or the end
    // of the block — never at a bare newline: the command wraps across rows.
    let end = text[p..]
        .find(PROMPT_SETUP_SUFFIX)
        .map(|rel| p + rel + PROMPT_SETUP_SUFFIX.len())
        .or_else(|| extract_osc7_end(&text[p..]).map(|(_, rel)| p + rel))
        .unwrap_or(text.len());
    strip_prompt_setup_echo(text, p, end);
    true
}

pub(crate) fn strip_prompt_setup_echo(text: &mut String, prefix_pos: usize, end_pos: usize) {
    let start = line_start_before(text, prefix_pos);
    let end = include_following_line_break(text, end_pos.min(text.len()));
    // The remote PTY has already echoed the hidden setup command and advanced
    // its cursor through that line. Removing the bytes outright leaves our
    // local vt100 parser at the old prompt column, so readline's later relative
    // backspaces repaint history commands beside one another (#289). Reset and
    // clear the current local row before feeding the final prompt that follows.
    text.replace_range(start..end, "\r\x1b[2K");
}

/// Remove a late-echoed prompt setup command when it arrives after the initial
/// suppression window. Some shells echo a long injected command only after the
/// first prompt has already been delivered, so the normal buffered path cannot
/// catch it (#266).
pub(crate) fn strip_late_prompt_setup_echo(text: &mut String) -> bool {
    let Some(prefix_pos) = text.find(PROMPT_SETUP_PREFIX) else {
        return false;
    };
    let Some(rel_end) = text[prefix_pos..].find(PROMPT_SETUP_SUFFIX) else {
        return false;
    };
    let end = prefix_pos + rel_end + PROMPT_SETUP_SUFFIX.len();
    strip_prompt_setup_echo(text, prefix_pos, end);
    true
}

/// Accumulate one output chunk while a late-echoed prompt setup command may
/// still be in flight (the suppression window closed before the echo landed).
///
/// The echoed command is a single long line; a slow PTY and TCP segmentation
/// can deliver it across several chunks. Stripping per-chunk leaks the whole
/// command whenever `PROMPT_SETUP_PREFIX` and `PROMPT_SETUP_SUFFIX` land in
/// different chunks: the prefix-bearing chunk is already released to the
/// screen, so no later chunk can ever match again.
///
/// Returns `(release, done)`:
/// - `release`: text safe to render right now (empty while we keep holding);
/// - `done`: the late-echo state can end (complete strip, or cap fallback).
pub(crate) fn accumulate_late_echo(buf: &mut String, chunk: &str) -> (String, bool) {
    const ECHO_BUF_CAP: usize = 1 << 14; // 16 KiB
    buf.push_str(chunk);
    // Fast path: the complete echoed line (prefix … suffix) has arrived.
    if strip_late_prompt_setup_echo(buf) {
        return (std::mem::take(buf), true);
    }
    if buf.contains(PROMPT_SETUP_PREFIX) {
        // Prefix landed but the line is still incomplete: hold it back. A cap
        // fallback keeps an unresponsive shell from freezing the terminal —
        // and discards the wrapped remainder whole, never cutting at a line
        // end (which would leak the other rows of the echo).
        if buf.len() >= ECHO_BUF_CAP
            && let Some(p) = buf.find(PROMPT_SETUP_PREFIX)
        {
            discard_prompt_setup_tail(buf, p);
            return (std::mem::take(buf), true);
        }
        return (String::new(), false);
    }
    // Unrelated output: release it, but keep a trailing overlap window so a
    // prefix split across chunks still matches once the next chunk arrives.
    let overlap = PROMPT_SETUP_PREFIX.len() - 1;
    if buf.len() > overlap {
        let split = buf.floor_char_boundary(buf.len() - overlap);
        let tail = buf.split_off(split);
        let head = std::mem::take(buf);
        buf.push_str(&tail);
        (head, false)
    } else {
        (String::new(), false)
    }
}

/// Extract the remote path from an OSC 7 sequence embedded in `text`.
///
/// Find the next complete OSC sequence (`ESC ] … BEL | ST`) at or after `from`.
///
/// Returns `(seq_start, content_range, end_after_terminator)`。An unterminated
/// tail (no BEL / ST yet) yields `None` — the caller leaves it for the next
/// chunk, which is what vt100 / the PTY pump expect.
///
/// OSC 7（cwd 跟踪）与 OSC 697（命令捕获）原先各自手写一遍这段扫描，
/// 逐字相同 —— 收在这里，避免两份规则各自漂移。
pub(crate) fn next_osc(
    bytes: &[u8],
    from: usize,
) -> Option<(usize, std::ops::Range<usize>, usize)> {
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] != 0x1b || bytes[i + 1] != b']' {
            i += 1;
            continue;
        }
        let seq_start = i;
        let osc_start = i + 2;
        // Scan for BEL (0x07) or ST (ESC \)
        let mut end = osc_start;
        let mut term_len = 0;
        while end < bytes.len() {
            if bytes[end] == 0x07 {
                term_len = 1;
                break;
            } else if bytes[end] == 0x1b && end + 1 < bytes.len() && bytes[end + 1] == b'\\' {
                term_len = 2;
                break;
            }
            end += 1;
        }
        if end >= bytes.len() {
            return None; // incomplete — leave it for the next chunk
        }
        return Some((seq_start, osc_start..end, end + term_len));
    }
    None
}

/// Format: `ESC ] 7 ; file://hostname/path BEL`
/// Returns the decoded absolute path component (without hostname).
pub fn extract_osc7_path(text: &str) -> Option<String> {
    extract_osc7_end(text).map(|(path, _)| path)
}

/// Like [`extract_osc7_path`] but also returns the byte index just past the OSC
/// sequence's terminator, so the caller can cut everything up to and including
/// it — used to discard the echoed setup line (which may wrap) at connect (#98).
pub(crate) fn extract_osc7_end(text: &str) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some((_seq_start, content, after)) = next_osc(bytes, from) {
        if let Ok(content) = std::str::from_utf8(&bytes[content])
            && let Some(rest) = content.strip_prefix("7;file://")
        {
            // rest = "hostname/path" or "/path" (empty hostname)
            let path = if rest.starts_with('/') {
                rest.to_string()
            } else if let Some(slash) = rest.find('/') {
                rest[slash..].to_string()
            } else {
                "/".to_string()
            };
            return Some((url_decode(&path), after));
        }
        from = after;
    }
    None
}

/// Find a Rudder command-capture sequence (`ESC ] 697 ; <command> BEL|ST`)
/// emitted by the shell hook (#113). Returns the command text and the byte
/// range of the whole escape sequence, so the caller can strip it before the
/// text is rendered. An incomplete sequence (terminator not yet received)
/// yields `None` — vt100 buffers it and the next chunk completes it.
pub fn extract_osc_command(text: &str) -> Option<(String, std::ops::Range<usize>)> {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some((seq_start, content, after)) = next_osc(bytes, from) {
        if let Ok(content) = std::str::from_utf8(&bytes[content])
            && let Some(cmd) = content.strip_prefix("697;")
        {
            return Some((cmd.to_string(), seq_start..after));
        }
        from = after;
    }
    None
}

/// Percent-decode a URL path segment (e.g. `%20` → space).
pub(crate) fn url_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next();
            let h2 = chars.next();
            match (h1, h2) {
                (Some(a), Some(b)) => {
                    let hex = format!("{a}{b}");
                    if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                        result.push(byte as char);
                    } else {
                        result.push('%');
                        result.push(a);
                        result.push(b);
                    }
                }
                (Some(a), None) => {
                    result.push('%');
                    result.push(a);
                }
                _ => result.push('%'),
            }
        } else {
            result.push(c);
        }
    }
    result
}
