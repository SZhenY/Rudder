use super::*;

pub(crate) async fn run_session(
    session: Session,
    jump: Option<Session>,
    mut commands: UnboundedReceiver<SessionCommand>,
    events: UnboundedSender<SessionEvent>,
    initial_cols: u32,
    initial_rows: u32,
    trust_unknown_host_key: bool,
) -> Result<()> {
    let session_started = std::time::Instant::now();
    let _ = events.send(SessionEvent::Status(format!(
        "{} {}@{}:{} ...",
        t("连接中", "Connecting"),
        session.user,
        session.host,
        session.port
    )));

    let config = ssh_client_config();

    let (mut handle, mut jump_handle) = connect_ssh(
        &session,
        jump.as_ref(),
        config.clone(),
        &events,
        trust_unknown_host_key,
    )
    .await?;
    tracing::info!(
        "[SESSION_START] id={} stage=transport-ready elapsed_ms={}",
        session.id,
        session_started.elapsed().as_millis()
    );

    // --- Auth (shared with SFTP + jump-host paths) ---------------------
    // Try plain `password` first, then `keyboard-interactive` on a fresh handle —
    // many bastions (JumpServer) disable `password` (#86). Missing credentials
    // are prompted for (#110).
    match authenticate_session(
        &mut handle,
        &mut jump_handle,
        &session,
        jump.as_ref(),
        config.clone(),
        &events,
        trust_unknown_host_key,
    )
    .await?
    {
        AuthResult::Success => {}
        AuthResult::Cancelled => {
            let _ = events.send(SessionEvent::Closed(
                t("已取消登录", "login cancelled").into(),
            ));
            let _ = handle
                .disconnect(Disconnect::ByApplication, "cancelled", "")
                .await;
            return Ok(());
        }
        AuthResult::Failed => {
            tracing::warn!(
                "ssh authentication failed for {}@{}",
                session.user,
                session.host
            );
            let _ = events.send(SessionEvent::Closed(
                t(
                    "认证失败：用户名、密码或密钥被服务器拒绝，请核对凭据后按 Enter 重试",
                    "Authentication failed: the username, password or key was rejected — check your credentials and press Enter to retry",
                )
                .into(),
            ));
            let _ = handle
                .disconnect(Disconnect::ByApplication, "auth failed", "")
                .await;
            return Ok(());
        }
    };
    tracing::info!(
        "[SESSION_START] id={} stage=authenticated elapsed_ms={}",
        session.id,
        session_started.elapsed().as_millis()
    );

    // Keep the jump-host connection alive for the whole session — the direct-tcpip
    // tunnel that carries this session rides on it (#211).
    let _jump_keepalive = jump_handle;

    // The integration body is Bash/Zsh-specific. Probe out-of-band before the
    // interactive channel exists, so ash/dash/fish/unknown shells never receive
    // (and therefore can never display or get stuck parsing) the setup command.
    let prompt_setup_supported =
        !session.disable_shell_integration && remote_supports_prompt_setup(&handle).await;
    tracing::info!(
        "[SETUP] id={} probe result: shell-integration-supported={} (disable={})",
        session.id,
        prompt_setup_supported,
        session.disable_shell_integration
    );

    // --- Shell channel --------------------------------------------------
    let mut channel = handle
        .channel_open_session()
        .await
        .context("open session channel")?;

    channel
        .request_pty(
            true,
            "xterm-256color",
            initial_cols,
            initial_rows,
            0,
            0,
            &[],
        )
        .await
        .context("request PTY")?;
    channel.request_shell(true).await.context("request shell")?;

    tracing::info!(
        "[SESSION_START] id={} stage=terminal-ready elapsed_ms={}",
        session.id,
        session_started.elapsed().as_millis()
    );

    let _ = events.send(SessionEvent::Connected);
    let _ = events.send(SessionEvent::Status(format!(
        "{} {}@{}",
        t("已连接", "Connected"),
        session.user,
        session.host
    )));

    // Whether we have already injected the PROMPT_COMMAND setup.
    // We wait for the first non-empty data chunk (the initial shell prompt)
    // before sending so the command doesn't interleave with banner text.
    let mut prompt_injected = false;
    // True from injecting PROMPT_SETUP until the echoed setup line has been
    // received and stripped; output is buffered (not shown) during that window.
    let mut suppress_echo = false;
    // Hard deadline for the suppression window. A non-POSIX shell (Windows
    // pwsh/cmd) never runs our hook and so never echoes the OSC 7 we wait for —
    // without this, output stayed hidden until a 16 KiB cap, leaving the terminal
    // blank/"unusable" on Windows servers (#140-1). When the deadline passes we
    // stop suppressing and show whatever arrived.
    let mut suppress_deadline: Option<tokio::time::Instant> = None;
    // Buffers output while `suppress_echo` so the (long) echoed setup line can be
    // stripped even when it splits across reads (#98).
    let mut echo_buf = String::new();
    // `strip_late_prompt_setup_echo` must only run while an initial setup echo
    // can genuinely still be in flight. Leaving it enabled for the whole SSH
    // session makes recalling an accidentally saved setup command clear normal
    // terminal rows (#289).
    let mut late_prompt_echo_pending = false;
    // Cross-chunk accumulator for a setup echo that arrives after the
    // suppression window closed. Without it, a TCP/PTY-segmented echo leaks
    // the whole injected command (prefix and suffix in different chunks can
    // never match again). See `accumulate_late_echo`.
    let mut late_echo_buf = String::new();
    // Hard lifetime for the late-echo stripping state, counted from injection.
    // If the echoed setup line never completes (wrapped prefix, truncated by
    // the suppression cap, a shell that silently dropped the echo), pending
    // must not survive forever: a later command that happens to contain
    // PROMPT_SETUP_PREFIX (e.g. recalling the setup line from shell history on
    // a *second* session, where the server's history file already holds it)
    // would otherwise be swallowed by the accumulator (#289 follow-up).
    let mut late_echo_window: Option<tokio::time::Instant> = None;
    // Cross-chunk overlap for the final safety net (`final_setup_guard`):
    // keeps at most PREFIX.len()-1 bytes of the previous output block so a
    // setup echo split across chunk boundaries is still caught even if the
    // suppression state machine never engaged.
    let mut guard_tail = String::new();
    // After a ZMODEM transfer finishes we briefly ignore ZMODEM detection so the
    // sender's lingering close frames can't spawn a spurious second receive (#76).
    let mut zmodem_done_at: Option<std::time::Instant> = None;

    // Cwd-notification (OSC 7) setup, injected once after the first prompt so
    // the SFTP panel can follow `cd` (#91). It must work across shells:
    //   • bash/sh  → PROMPT_COMMAND runs `__ms7` before every prompt.
    //   • zsh      → bash's PROMPT_COMMAND is IGNORED by zsh, so we register a
    //                `precmd` hook via `add-zsh-hook` instead (non-destructive —
    //                it preserves oh-my-zsh / p10k hooks, unlike `precmd(){…}`).
    //   • fish     → guarded out (fish 3.1+ emits OSC 7 itself).
    // `__ms7` is called once at the end so the initial cwd arrives immediately.
    //
    // The whole shell-specific body lives inside `eval '…'`: fish can't parse
    // bash/zsh function & `if` syntax, but it CAN parse `eval '<opaque string>'`,
    // and the `test -z "$FISH_VERSION" &&` guard short-circuits before the eval
    // ever runs under fish (#71). The body uses only double quotes inside so the
    // outer single-quoted string needs no escaping; printf turns \033/\007 into
    // ESC/BEL at prompt time. No array syntax → safe to *parse* in dash/ash too.
    //
    // The leading space keeps the line out of shell history (HISTCONTROL=
    // ignorespace, the default on most distros); its echo is stripped locally
    // (the needle below) so the bookkeeping command never shows up.
    //
    // Besides OSC 7 (cwd), the hook also captures the command the user just ran
    // and reports it via a private `OSC 697 ; <cmd> BEL` so it can join the
    // command-box history (#113) — terminal-typed commands aren't otherwise
    // recorded. `__msc` reads the last history entry with `fc -ln -1`; this only
    // ever sees real executed commands, never password prompts (those use
    // `read -s` and aren't shell commands). `__cl` remembers the last reported
    // command so a redrawn prompt (e.g. Enter on an empty line) doesn't re-emit
    // it, and is primed once up front so the pre-session history isn't replayed.
    //
    // The echoed setup line is discarded by anchoring on the OSC 7 it produces
    // (see the suppress block below), so it doesn't matter that the long line
    // wraps — we never substring-match it.
    let prompt_setup = format!(" {}\r", PROMPT_BODY);
    // --- Remote resource monitor (separate exec channel) ----------------
    // A tiny remote loop streams /proc/stat + /proc/meminfo every 2s; we parse
    // it into CPU% / mem / swap for the sidebar.  Best-effort: if the channel
    // or exec fails (e.g. a non-Linux host without /proc), monitoring is
    // silently skipped and the interactive shell is unaffected.
    // Reset PATH to the standard system directories first (#27): the monitor
    // runs over an exec channel, so a server with a hijacked PATH (or a
    // BASH_ENV pointing at a malicious file) could otherwise shadow awk/cat/df/
    // sleep with arbitrary binaries. A fixed PATH covering /usr/bin and /bin is
    // more portable than hardcoding one absolute path per tool (their location
    // differs across distros). Monitoring is best-effort, so even if this shell
    // is unusual and the reset finds nothing, only the sidebar stats are lost.
    // The `ps` section feeds the process monitor (#23): top-40 by CPU, columns
    // pid/user/pcpu/pmem/args, each line clipped to 200 chars so a giant command
    // line can't bloat the stream. A host whose `ps` lacks `--sort`/`-o` simply
    // yields nothing (2>/dev/null), degrading to an empty process list.
    const MON_CMD: &[u8] = b"PATH=/usr/bin:/bin:/usr/sbin:/sbin; export PATH; while :; do awk '/^cpu /{print}' /proc/stat; awk '/^(MemTotal|MemAvailable|SwapTotal|SwapFree|Buffers|Cached):/{print}' /proc/meminfo; cat /proc/net/dev; echo __DF__; df -kP 2>/dev/null; echo __MSTICK__; sleep 2; done\n";
    // Detailed system information is intentionally one-shot and last priority.
    // It includes commands such as lspci/hostname that may be slow on some hosts
    // and must never delay either the terminal or the lightweight sidebar sample.
    const SYS_CMD: &[u8] = b"PATH=/usr/bin:/bin:/usr/sbin:/sbin; export PATH; awk '/^cpu /{print}' /proc/stat; awk '/^(MemTotal|MemAvailable|SwapTotal|SwapFree|Buffers|Cached):/{print}' /proc/meminfo; cat /proc/net/dev; echo __DF__; df -kP 2>/dev/null; echo __SYS__; { . /etc/os-release 2>/dev/null; echo OS=${PRETTY_NAME:-$(uname -o 2>/dev/null)}; }; echo KERNEL=$(uname -s 2>/dev/null); echo KERNEL_RELEASE=$(uname -r 2>/dev/null); echo ARCH=$(uname -m 2>/dev/null); echo HOSTNAME=$(hostname 2>/dev/null); echo IPS=$(hostname -I 2>/dev/null); echo UPTIME=$(uptime -p 2>/dev/null); echo LOAD=$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null); awk -F: '/model name|Hardware/{gsub(/^[ \\t]+/,\"\",$2); print \"CPU_MODEL=\"$2; exit}' /proc/cpuinfo 2>/dev/null; echo CPU_CORES=$(grep -c '^processor' /proc/cpuinfo 2>/dev/null); awk -F: '/cache size/{gsub(/^[ \\t]+/,\"\",$2); print \"CPU_CACHE=\"$2; exit}' /proc/cpuinfo 2>/dev/null; awk -F: '/bogomips/{gsub(/^[ \\t]+/,\"\",$2); print \"CPU_BOGO=\"$2; exit}' /proc/cpuinfo 2>/dev/null; lspci 2>/dev/null | awk -F': ' '/VGA|3D|Display/{print \"GPU=\" $2; exit}'; echo __MSTICK__\n";
    // Skip the resource monitor entirely when shell integration is off (a
    // non-POSIX / Windows server) — the /proc-based loop only spews errors there
    // (#140).
    let mut mon_channel: Option<Channel<Msg>> = None;
    let mut mon_buf = String::new();
    // 暂停期间**照常解析**，只是不发事件，始终留着最新一份快照；恢复时立刻推出去 ——
    // 于是展开侧边栏即刻有数据（不必等远端下一拍，也不会先显示 0% 再跳）。
    let mut last_resource: Option<SessionEvent> = None;
    let mut last_process: Option<SessionEvent> = None;
    let mut sys_buf = String::new();
    let mut prev_cpu: Option<(u64, u64)> = None; // (total jiffies, idle jiffies)
    let mut prev_net: std::collections::HashMap<String, (u64, u64)> =
        std::collections::HashMap::new(); // iface -> (rx_bytes, tx_bytes)
    let mut prev_net_at = std::time::Instant::now();

    // Process sampling has its own channel. The broader resource command above
    // includes probes such as `df` which can block indefinitely on a stale NFS
    // mount; that must not leave dead PIDs frozen in the process window.
    const PROC_CMD: &[u8] = b"PATH=/usr/bin:/bin:/usr/sbin:/sbin; export PATH; while :; do echo __ME__; id -un 2>/dev/null; echo __PS__; ps -eo pid,user:32,pcpu,pmem,args --sort=-pcpu 2>/dev/null | head -n 41 | cut -c -200; echo __PSTICK__; sleep 2; done\n";
    let mut proc_channel: Option<Channel<Msg>> = None;
    let mut sys_channel: Option<Channel<Msg>> = None;
    let mut proc_buf = String::new();

    // --- Port forwarding / tunnels (#56) --------------------------------
    // Remote (-R) first, while we still hold `handle` mutably (tcpip_forward
    // takes &mut self); the server then opens channels back, serviced in the
    // handler. Then wrap the handle in an Arc so the local/dynamic listener
    // tasks can share it (russh's Handle isn't Clone, but its methods are &self).
    let mut runtime_forwards: std::collections::HashMap<String, RuntimeForward> =
        std::collections::HashMap::new();
    for (idx, f) in session
        .forwards
        .iter()
        .enumerate()
        .filter(|(_, f)| f.kind == "remote")
    {
        let bind = normalized_bind_addr(f);
        let id = format!("config-{idx}");
        match handle.tcpip_forward(bind.clone(), f.bind_port as u32).await {
            Ok(_) => {
                let _ = crate::output_budget::emit_output(&events, format!(
                    "\r\n[rudder] -R {bind}:{} → {}:{}\r\n",
                    f.bind_port, f.host, f.host_port
                ));
                runtime_forwards.insert(
                    id.clone(),
                    RuntimeForward {
                        info: tunnel_info(id, f, true, t("运行中", "running")),
                        task: None,
                    },
                );
            }
            Err(e) => {
                let _ = crate::output_budget::emit_output(&events, format!(
                    "\r\n[rudder] -R {bind}:{} 请求失败 / request failed: {e}\r\n",
                    f.bind_port
                ));
                runtime_forwards.insert(
                    id.clone(),
                    RuntimeForward {
                        info: tunnel_info(id, f, false, t("启动失败", "failed")),
                        task: None,
                    },
                );
            }
        }
    }
    let handle = Arc::new(handle);

    // Delay auxiliary channels until after the terminal is usable, but open
    // them from this task (serially, with per-channel timeouts) instead of in
    // detached tasks: opening channels while `channel.wait()` was being polled
    // made strict SSH servers close the primary PTY (#264 follow-up).
    let auxiliary_started_at = tokio::time::Instant::now();
    let mut auxiliary_startup = AuxiliaryStartup::new(!session.disable_shell_integration);
    // 辅助通道全部在后台任务里打开、结果送回泵；任务在飞期间**不轮询**主 PTY
    // 的 `channel.wait()`（#264）。`aux_open_in_flight` 用来挂起那个分支。
    let (aux_open_tx, mut aux_open_rx) = tokio::sync::mpsc::unbounded_channel::<AuxOpenBatch>();
    let mut aux_open_in_flight = false;
    let mut aux_open_started: Option<tokio::time::Instant> = None;
    // 在飞的那批属于**启动序列**（true）还是**重开监控**（false）—— 只有前者
    // 在结果回来后推进 `auxiliary_startup`。
    let mut aux_open_for_startup = false;
    // MCP 的一次性命令队列。与监控通道共用同一把"开通道"互斥（#264）：
    // 同一时刻只允许一个开通道任务在飞。
    let mut pending_execs: std::collections::VecDeque<ExecRequest> =
        std::collections::VecDeque::new();
    let (exec_done_tx, mut exec_done_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut auxiliary_deadline = auxiliary_startup
        .pending()
        .map(|kind| auxiliary_started_at + kind.delay());
    let mut resource_monitoring = true;
    let mut first_terminal_output = true;
    // Local (-L) and dynamic (-D) listen client-side; their tasks are aborted
    // on session exit.
    for (idx, f) in session.forwards.iter().enumerate() {
        match f.kind.as_str() {
            "local" | "dynamic" => {
                let id = format!("config-{idx}");
                runtime_forwards.insert(
                    id.clone(),
                    start_runtime_forward(handle.clone(), id, f.clone(), &events),
                );
            }
            _ => {}
        }
    }
    emit_tunnel_update(&runtime_forwards, &events);

    // Per-session charset codecs (#338): the decoders must live across packets
    // because a multibyte character can be split between two data messages.
    let mut terminal_decoder = crate::terminal::TerminalEncoding::new(&session.encoding);
    let mut extended_decoder = crate::terminal::TerminalEncoding::new(&session.encoding);
    let terminal_encoder = crate::terminal::TerminalEncoding::new(&session.encoding);
    // Expect/send login triggers (#212). Rules are matched against the
    // decoded terminal stream; empty rules are filtered out up front.
    let mut trigger_engine = TriggerEngine::new(&session.triggers);

    // --- Main pump ------------------------------------------------------
    loop {
        // Stop pulling from the PTY while the UI is behind, so a `yes`/`cat`
        // firehose cannot grow the event queue without bound.
        crate::output_budget::wait_for_drain().await;
        tokio::select! {
            _ = async {
                match auxiliary_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                // 看门狗：批次迟迟不回（开通道的任务 panic、或长时间没被调度）就强制
                // 复位。否则 `aux_open_in_flight` 永远为真 → 主 PTY 的 `channel.wait()`
                // 被永久挂起 → 输入还能发、输出永不显示，会话假死且无法自愈。
                if aux_open_in_flight
                    && aux_open_started.is_some_and(|t| t.elapsed() > AUX_OPEN_WATCHDOG)
                {
                    tracing::warn!(
                        "[AUX] channel batch timed out after {:?}; resuming PTY polling",
                        AUX_OPEN_WATCHDOG
                    );
                    aux_open_in_flight = false;
                    aux_open_started = None;
                    auxiliary_startup.complete();
                    // 看门狗复位后也把排队的一次性命令接上，否则它们会一直等着。
                    start_next_exec(
                        &mut pending_execs,
                        &mut aux_open_in_flight,
                        &mut aux_open_started,
                        &handle,
                        &exec_done_tx,
                    );
                }
                // ⚠️ 必须先判"有批次在飞"再 `begin()`：否则 `begin()` 已把
                // in_progress 置上，随后又因为让路而不开通道，序列就再也推进不了。
                if aux_open_in_flight {
                    // 已有批次在飞（例如刚重开监控）—— 不并发开通道，稍后再来。
                    // 这里同时充当看门狗的轮询点（上面那段按 `aux_open_started` 判超时）。
                    auxiliary_deadline = Some(
                        tokio::time::Instant::now() + std::time::Duration::from_millis(500),
                    );
                } else if let Some(kind) = auxiliary_startup.begin() {
                    let (command, label) = match kind {
                        AuxiliaryChannelKind::Resources => (MON_CMD, "monitor"),
                        AuxiliaryChannelKind::Processes => (PROC_CMD, "process monitor"),
                        AuxiliaryChannelKind::SystemInfo => (SYS_CMD, "system-info"),
                    };
                    // 监控已被暂停（侧边栏收起）时，监控通道根本不用开。
                    let wanted = match kind {
                        AuxiliaryChannelKind::Resources | AuxiliaryChannelKind::Processes => {
                            resource_monitoring
                        }
                        AuxiliaryChannelKind::SystemInfo => true,
                    };
                    if wanted {
                        // 交给后台任务：泵继续处理输入 / resize，不被开通道阻塞。
                        // 同时 `aux_open_in_flight` 会把 `channel.wait()` 分支挂起 ——
                        // #264 的硬性前提：不能在轮询主 PTY 的同时开通道。
                        aux_open_in_flight = true;
                        aux_open_for_startup = true;
                        aux_open_started = Some(tokio::time::Instant::now());
                        spawn_auxiliary_open(
                            handle.clone(),
                            kind,
                            command,
                            label,
                            aux_open_tx.clone(),
                        );
                        // 结果由 `aux_open_rx` 分支回收；这里的 deadline 只用来驱动看门狗。
                        auxiliary_deadline =
                            Some(tokio::time::Instant::now() + AUX_OPEN_WATCHDOG);
                    } else {
                        // 监控处于暂停态：这条监控通道不开，直接推进到下一条
                        // （首次连接序列里的 SystemInfo 不受影响）。
                        let _ = (command, label);
                        auxiliary_startup.complete();
                        auxiliary_deadline = auxiliary_startup
                            .pending()
                            .map(|next| tokio::time::Instant::now() + next.delay());
                    }
                } else {
                    auxiliary_deadline = None;
                }
            }
            // 后台开通道的结果**整批**回来了（一批可能有多条）—— 这里才落库、
            // 才清 `aux_open_in_flight`、才推进启动序列。
            Some(batch) = aux_open_rx.recv() => {
                for (kind, opened) in batch {
                    // 暂停期间到达的通道**照常保留**（暂停不再关通道）：
                    // 解析在下面的数据分支里按 `resource_monitoring` 跳过即可。
                    let auxiliary_available = opened.is_some();

                    match kind {
                        AuxiliaryChannelKind::Resources => {
                            // Close the old channel before overwriting — russh
                            // 0.49 Channel has no Drop, so a silent replacement
                            // leaves the server-side monitor loop running.
                            if let Some(old) = mon_channel.take() {
                                let _ = old.close().await;
                            }
                            mon_channel = opened;
                        }
                        AuxiliaryChannelKind::Processes => {
                            if let Some(old) = proc_channel.take() {
                                let _ = old.close().await;
                            }
                            proc_channel = opened;
                        }
                        AuxiliaryChannelKind::SystemInfo => sys_channel = opened,
                    }
                    tracing::debug!(
                        "[SESSION_START] id={} stage={kind:?}-startup-finished available={} elapsed_ms={}",
                        session.id,
                        auxiliary_available,
                        session_started.elapsed().as_millis()
                    );
                }
                // 整批处理完才恢复轮询主 PTY（#264）。
                aux_open_in_flight = false;
                aux_open_started = None;
                // 只有**启动序列**那批才推进序列；重开监控那批不算
                // （否则会一路推进到 SystemInfo，白白多开一条通道）。
                if aux_open_for_startup {
                    aux_open_for_startup = false;
                    auxiliary_startup.complete();
                    auxiliary_deadline = auxiliary_startup
                        .pending()
                        .map(|next| tokio::time::Instant::now() + next.delay());
                }
                // 启动批次结束后，把排队中的一次性命令接上。
                start_next_exec(
                    &mut pending_execs,
                    &mut aux_open_in_flight,
                    &mut aux_open_started,
                    &handle,
                    &exec_done_tx,
                );
            }
            // 一次性命令的通道已经开完（或开失败）：恢复主 PTY 轮询，并接上下一条。
            Some(()) = exec_done_rx.recv() => {
                // 若此刻在飞的其实是启动序列的批次，别动它的标志位。
                if !aux_open_for_startup {
                    aux_open_in_flight = false;
                    aux_open_started = None;
                }
                start_next_exec(
                    &mut pending_execs,
                    &mut aux_open_in_flight,
                    &mut aux_open_started,
                    &handle,
                    &exec_done_tx,
                );
            }
            cmd = commands.recv() => {
                match cmd {
                    Some(SessionCommand::RawInput(bytes)) => {
                        // Only log the byte count — never the bytes themselves,
                        // which are raw keystrokes and may contain passwords (#15).
                        tracing::debug!("ssh channel.data len={} bytes", bytes.len());
                        let bytes = terminal_encoder.encode(&bytes);
                        if let Err(err) = channel.data(&bytes[..]).await {
                            let _ = events.send(SessionEvent::Closed(format!("{}: {err}", t("写入失败", "write failed"))));
                            break;
                        }
                    }
                    Some(SessionCommand::Resize(cols, rows)) => {
                        let _ = channel.window_change(cols, rows, 0, 0).await;
                    }
                    Some(SessionCommand::SetResourceMonitoring(enabled)) => {
                        if enabled == resource_monitoring || session.disable_shell_integration {
                            continue;
                        }
                        resource_monitoring = enabled;
                        if !enabled {
                            // Pause：**不关通道**（关掉再重开 exec 通道正是掉线的元凶），
                            // 也**不丢数据** —— 解析继续，只是不发事件，最新一份留在
                            // `last_*`。通道数据照常被读走，服务端写窗口不会填满。
                        } else {
                            // Resume：先把暂停期间攒的最新快照**立刻**推出去，
                            // 侧边栏展开即刻显示（不必等远端下一拍）。
                            if let Some(ev) = last_resource.take() {
                                let _ = events.send(ev);
                            }
                            if let Some(ev) = last_process.take() {
                                let _ = events.send(ev);
                            }
                            // 通道一直开着 → 通常无需重开；只有**任一条**没了才重开
                            // （用 `||` 而不是 `&&`：曾经出现"刚恢复时启动序列正开着
                            // 别的通道 → 恢复被跳过 → 进程监控永久缺失"的情形）。
                            // 此时基线已失效，需要重置（走后台任务，期间挂起
                            // `channel.wait()` —— #264）。
                            if (mon_channel.is_none() || proc_channel.is_none())
                                && !aux_open_in_flight
                            {
                                prev_cpu = None;
                                prev_net.clear();
                                prev_net_at = std::time::Instant::now();
                                aux_open_in_flight = true;
                                aux_open_for_startup = false;
                                aux_open_started = Some(tokio::time::Instant::now());
                                spawn_monitoring_channels(
                                    handle.clone(),
                                    MON_CMD,
                                    PROC_CMD,
                                    aux_open_tx.clone(),
                                );
                                auxiliary_deadline =
                                    Some(tokio::time::Instant::now() + AUX_OPEN_WATCHDOG);
                            }
                        }
                    }
                    Some(SessionCommand::AddTunnel { id, forward }) => {
                        if forward.kind == "local" || forward.kind == "dynamic" {
                            runtime_forwards.insert(
                                id.clone(),
                                start_runtime_forward(handle.clone(), id, forward, &events),
                            );
                            emit_tunnel_update(&runtime_forwards, &events);
                        } else {
                            let _ = crate::output_budget::emit_output(&events, format!(
                                "\r\n[rudder] {}\r\n",
                                t("运行时暂不支持新增远程转发 -R", "runtime remote forwarding (-R) is not supported yet")
                            ));
                        }
                    }
                    Some(SessionCommand::StopTunnel(id)) => {
                        if let Some(f) = runtime_forwards.remove(&id) {
                            if let Some(task) = f.task {
                                task.abort();
                            }
                            emit_tunnel_update(&runtime_forwards, &events);
                        }
                    }
                    Some(SessionCommand::KillProcess { pid, root_password, reply }) => {
                        let exec_handle = handle.clone();
                        tokio::spawn(async move {
                            let result = kill_remote_process(exec_handle, pid, root_password).await;
                            let _ = reply.send(result);
                        });
                    }
                    Some(SessionCommand::Exec { command, timeout, max_output_bytes, reply }) => {
                        pending_execs.push_back(ExecRequest {
                            command,
                            timeout,
                            max_output_bytes,
                            reply,
                        });
                        start_next_exec(
                            &mut pending_execs,
                            &mut aux_open_in_flight,
                            &mut aux_open_started,
                            &handle,
                            &exec_done_tx,
                        );
                    }
                    Some(SessionCommand::Close) | None => {
                        let _ = tokio::time::timeout(CLOSE_GRACE, channel.eof()).await;
                        break;
                    }
                }
            }
            // Suppression safety net: if the injected hook hasn't echoed its OSC 7
            // by the deadline, the remote shell isn't the POSIX one we injected for
            // (e.g. Windows pwsh/cmd). Stop hiding output so the terminal is usable
            // again; best-effort drop just the echoed setup line (#140-1).
            _ = async {
                match suppress_deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            }, if suppress_echo => {
                suppress_echo = false;
                suppress_deadline = None;
                let mut buf = std::mem::take(&mut echo_buf);
                if let Some(p) = buf.find(PROMPT_SETUP_PREFIX) {
                    // The whole echoed line has landed: strip it normally.
                    // find() replaces a contains()+unwrap() pair — same
                    // behaviour, but the Option makes the invariant explicit
                    // instead of leaning on a guard two lines above.
                    if let Some(end) = prompt_setup_echo_end(&buf, p) {
                        strip_prompt_setup_echo(&mut buf, p, end);
                        late_prompt_echo_pending = false;
                    } else if late_echo_window.is_none_or(|w| tokio::time::Instant::now() < w) {
                        // The prefix arrived but the line is still in flight
                        // (TCP/PTY segmentation). Hand it to the late-echo
                        // accumulator instead of cutting mid-line — that would
                        // leak the command tail once the remaining bytes land.
                        late_prompt_echo_pending = true;
                        let split = buf.floor_char_boundary(line_start_before(&buf, p));
                        late_echo_buf.push_str(&buf[split..]);
                        buf.truncate(split);
                    } else {
                        // Window expired while the wrapped echo was still
                        // incomplete. Discard the whole remainder — cutting at
                        // the first newline would leak the other rows of the
                        // wrapped command (#289 follow-up).
                        discard_prompt_setup_tail(&mut buf, p);
                        late_prompt_echo_pending = false;
                    }
                } else if late_echo_window.is_none_or(|w| tokio::time::Instant::now() < w) {
                    // Nothing identifiable arrived before the deadline. Allow
                    // one later setup echo to be removed, then permanently
                    // disable the special-case stripping for this session.
                    late_prompt_echo_pending = true;
                    // Keep a trailing overlap window so a setup echo whose
                    // prefix is still splitting in can still match once the
                    // remaining bytes land (otherwise the tail leaks).
                    let overlap = PROMPT_SETUP_PREFIX.len().saturating_sub(1);
                    if buf.len() > overlap {
                        let split = buf.floor_char_boundary(buf.len() - overlap);
                        late_echo_buf.push_str(&buf[split..]);
                        buf.truncate(split);
                    }
                }
                if !buf.is_empty() {
                    let _ = crate::output_budget::emit_output(&events, buf);
                }
            }
            // ⚠️ #264：后台任务正在开 exec 通道时**不能**轮询主 PTY 的
            // `channel.wait()` —— 严格的服务端会据此断开传输（侧边栏反复收展
            // 就会反复触发这条路径，连接于是"自己断开"）。
            // 挂起该分支即可：数据由 russh 缓冲在通道里，恢复后照常送达，不丢。
            msg = async {
                if aux_open_in_flight {
                    std::future::pending::<Option<ChannelMsg>>().await
                } else {
                    channel.wait().await
                }
            } => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        // Route the two ZMODEM handshakes in opposite directions:
                        // remote `sz` downloads into Downloads (#76); remote `rz`
                        // opens a local multi-file picker and uploads (#308).
                        // On any protocol error, cancel so the session recovers.
                        let zmodem_cooldown = zmodem_done_at
                            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(2));
                        if let Some(direction) = (!zmodem_cooldown)
                            .then(|| zmodem_direction(&data))
                            .flatten()
                        {
                            let result = match direction {
                                ZmodemDirection::Download => {
                                    crate::terminal::zmodem::receive(&mut channel, &data, &events)
                                        .await
                                }
                                ZmodemDirection::Upload => {
                                    let files = tokio::task::spawn_blocking(|| {
                                        rfd::FileDialog::new()
                                            .set_title(t(
                                                "选择要通过 rz 上传的文件",
                                                "Choose files to upload via rz",
                                            ))
                                            .pick_files()
                                            .unwrap_or_default()
                                    })
                                    .await
                                    .unwrap_or_default();
                                    if files.is_empty() {
                                        let _ = channel.data(&ZMODEM_CANCEL[..]).await;
                                        let _ = crate::output_budget::emit_output(&events, format!(
                                            "\r\n[rudder] {}\r\n",
                                            t("已取消 rz 上传", "rz upload cancelled")
                                        ));
                                        zmodem_done_at = Some(std::time::Instant::now());
                                        continue;
                                    }
                                    let _ = crate::output_budget::emit_output(&events, format!(
                                        "\r\n[rudder] {} {}...\r\n",
                                        t("开始上传", "Uploading"),
                                        files
                                            .iter()
                                            .filter_map(|path| path.file_name())
                                            .map(|name| name.to_string_lossy())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    ));
                                    crate::terminal::zmodem::send(&mut channel, &data, &files, &events)
                                        .await
                                }
                            };
                            zmodem_done_at = Some(std::time::Instant::now());
                            match result {
                                Ok(leftover) => {
                                    // Bytes after the transfer (the shell prompt):
                                    // run them through the normal output path so
                                    // the prompt shows and the cwd updates.
                                    if !leftover.is_empty() {
                                        let text = terminal_decoder.decode(&leftover);
                                        if let Some(cwd) = extract_osc7_path(&text) {
                                            let _ =
                                                events.send(SessionEvent::CwdChanged(cwd));
                                        }
                                        let _ = crate::output_budget::emit_output(&events, text);
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("zmodem {direction:?} failed: {e:#}");
                                    let _ = channel.data(&ZMODEM_CANCEL[..]).await;
                                    let _ = crate::output_budget::emit_output(&events, format!(
                                        "\r\n[rudder] {}: {e:#}\r\n",
                                        match direction {
                                            ZmodemDirection::Download => t(
                                                "ZMODEM 接收失败,已取消",
                                                "ZMODEM receive failed; cancelled",
                                            ),
                                            ZmodemDirection::Upload => t(
                                                "ZMODEM 上传失败,已取消",
                                                "ZMODEM upload failed; cancelled",
                                            ),
                                        }
                                    ));
                                }
                            }
                            continue;
                        }

                        let chunk = terminal_decoder.decode(&data);

                        if first_terminal_output {
                            first_terminal_output = false;
                            tracing::info!(
                                "[SESSION_START] id={} stage=first-terminal-output elapsed_ms={}",
                                session.id,
                                session_started.elapsed().as_millis()
                            );
                        }

                        // Inject PROMPT_COMMAND after the first real shell output,
                        // unless shell integration is disabled for this session
                        // (e.g. a Windows pwsh/cmd server) (#140).
                        if !prompt_injected
                            && !chunk.trim().is_empty()
                            && prompt_setup_supported
                        {
                            prompt_injected = true;
                            suppress_echo = true;
                            tracing::info!(
                                "[SETUP] id={} injected prompt_setup bytes={} suppress_echo=true",
                                session.id,
                                prompt_setup.len()
                            );
                            // Late-echo stripping may only run for a bounded
                            // window after injection; see `late_echo_window`.
                            late_echo_window = Some(
                                tokio::time::Instant::now()
                                    + std::time::Duration::from_secs(10),
                            );
                            // Give the hook ~2 s to echo its OSC 7; past that we
                            // assume a non-POSIX shell and stop hiding output (#140-1).
                            // 1.2 s was too tight for slow PTY/SSH servers — the echo
                            // + OSC 7 landed after the deadline, so the injected setup
                            // line leaked through (#176). The cost of the larger window
                            // is only a slightly longer blank on a non-POSIX shell that
                            // wasn't already flagged disable_shell_integration.
                            suppress_deadline = Some(
                                tokio::time::Instant::now()
                                    + std::time::Duration::from_millis(2000),
                            );
                            // Paint the banner/prompt immediately so the first
                            // usable terminal frame no longer waits for shell
                            // integration (later output carrying the injected
                            // setup command is still buffered and stripped).
                            // On hosts without a login banner this frame IS the
                            // shell prompt, and the shell prints an identical
                            // one once the setup command returns — rendering it
                            // twice. Drop the trailing prompt line here so only
                            // the post-setup prompt (sent through the normal
                            // path below) is shown; banner text above it is
                            // preserved.
                            let mut painted = chunk;
                            let prompt_cut = painted
                                .rsplit('\n')
                                .next()
                                .filter(|line| line.trim_end().ends_with(['#', '$', '%', '>']))
                                .and_then(|line| painted.rfind(line));
                            if let Some(pos) = prompt_cut {
                                painted.truncate(pos);
                            }
                            let _ = crate::output_budget::emit_output(&events, painted);
                            let _ = channel.data(prompt_setup.as_bytes()).await;
                            continue;
                        }

                        // While suppressing, buffer output until our echoed setup
                        // command AND the OSC 7 that the injected __ms7 prints right
                        // after it have both arrived. Then delete just that span —
                        // from the start of the command's line through the OSC 7 —
                        // which removes the echoed command (even if it WRAPPED across
                        // the terminal width, since we cut by byte range) and the
                        // now-redundant first prompt, while PRESERVING any MOTD/banner
                        // printed before it (#98). The command line is located by a
                        // short, un-wrappable prefix of the injected command. A size
                        // cap is the safety valve for a shell that never reports back
                        // (e.g. dash without PROMPT_COMMAND).
                        let text = if suppress_echo {
                            echo_buf.push_str(&chunk);
                            const ECHO_BUF_CAP: usize = 1 << 14; // 16 KiB
                            // The command echo + its trailing OSC 7 (the one after
                            // our command, not any earlier prompt OSC 7).
                            let landed = echo_buf.find(PROMPT_SETUP_PREFIX).and_then(|p| {
                                extract_osc7_end(&echo_buf[p..])
                                    .map(|(cwd, rel)| (p, p + rel, cwd))
                            });
                            if let Some((cmd_pos, osc_end, cwd)) = landed {
                                suppress_echo = false;
                                suppress_deadline = None;
                                late_prompt_echo_pending = false;
                                tracing::debug!("OSC7 cwd={:?}", cwd);
                                let _ = events.send(SessionEvent::CwdChanged(cwd));
                                let mut buf = std::mem::take(&mut echo_buf);
                                strip_prompt_setup_echo(&mut buf, cmd_pos, osc_end);
                                buf
                            } else if echo_buf.len() >= ECHO_BUF_CAP {
                                suppress_echo = false;
                                suppress_deadline = None;
                                let mut buf = std::mem::take(&mut echo_buf);
                                if let Some(p) = buf.find(PROMPT_SETUP_PREFIX) {
                                    // find() replaces contains()+unwrap(): the
                                    // Option states the invariant explicitly.
                                    if let Some(end) = prompt_setup_echo_end(&buf, p) {
                                        strip_prompt_setup_echo(&mut buf, p, end);
                                        late_prompt_echo_pending = false;
                                    } else if late_echo_window
                                        .is_none_or(|w| tokio::time::Instant::now() < w)
                                    {
                                        // Incomplete line: defer to the
                                        // late-echo accumulator.
                                        late_prompt_echo_pending = true;
                                        let split =
                                            buf.floor_char_boundary(line_start_before(&buf, p));
                                        late_echo_buf.push_str(&buf[split..]);
                                        buf.truncate(split);
                                    } else {
                                        // Window expired: discard the whole
                                        // wrapped remainder (see deadline path).
                                        discard_prompt_setup_tail(&mut buf, p);
                                        late_prompt_echo_pending = false;
                                    }
                                } else if late_echo_window
                                    .is_none_or(|w| tokio::time::Instant::now() < w)
                                {
                                    late_prompt_echo_pending = true;
                                    // Keep a trailing overlap window so a setup
                                    // echo still splitting in can match later.
                                    let overlap = PROMPT_SETUP_PREFIX.len().saturating_sub(1);
                                    if buf.len() > overlap {
                                        let split =
                                            buf.floor_char_boundary(buf.len() - overlap);
                                        late_echo_buf.push_str(&buf[split..]);
                                        buf.truncate(split);
                                    }
                                }
                                buf
                            } else {
                                continue; // keep buffering; show nothing yet
                            }
                        } else {
                            // Late-echo handling: the setup command's echo can
                            // still be in flight after the suppression window
                            // closed. Accumulate across chunks so a split echo
                            // is stripped in one piece instead of leaking.
                            let clean = if late_prompt_echo_pending {
                                // Bounded lifetime: past the window the echo
                                // can no longer be in flight; stop stripping
                                // and release everything (a later command that
                                // merely looks like the setup line must not be
                                // swallowed — e.g. recalling it from shell
                                // history on a second session).
                                let window_expired = late_echo_window
                                    .is_none_or(|w| tokio::time::Instant::now() >= w);
                                if window_expired {
                                    late_prompt_echo_pending = false;
                                    release_after_window_expiry(&mut late_echo_buf, &chunk)
                                } else {
                                    let (release, done) =
                                        accumulate_late_echo(&mut late_echo_buf, &chunk);
                                    if done {
                                        late_prompt_echo_pending = false;
                                    }
                                    if release.is_empty() {
                                        continue; // echo incomplete — hold back
                                    }
                                    release
                                }
                            } else {
                                chunk
                            };
                            // Scan for the OSC 7 CWD notification (cd-follow).
                            if let Some(cwd) = extract_osc7_path(&clean) {
                                tracing::debug!("OSC7 cwd={:?}", cwd);
                                let _ = events.send(SessionEvent::CwdChanged(cwd));
                            }
                            clean
                        };

                        // Final safety net, applied to EVERY rendered block
                        // regardless of which path produced it: a fully
                        // landed setup echo must never display, no matter the
                        // session count or timing. The tail overlap carries
                        // the block boundary so a split echo still matches.
                        guard_tail.push_str(&text);
                        let mut guarded = std::mem::take(&mut guard_tail);
                        if final_setup_guard(&mut guarded) {
                            tracing::info!(
                                "[SETUP] id={} final guard stripped echoed setup ({} bytes)",
                                session.id,
                                text.len()
                            );
                        }
                        // Only hold back a tail that is literally a prefix of
                        // the setup command (a split echo arriving next block);
                        // anything else is rendered immediately, so normal
                        // output is never delayed or dropped.
                        let keep = PROMPT_SETUP_PREFIX.len().saturating_sub(1).min(guarded.len());
                        let split = guarded.floor_char_boundary(guarded.len() - keep);
                        if PROMPT_SETUP_PREFIX.starts_with(&guarded[split..]) {
                            guard_tail = guarded.split_off(split);
                        }
                        let text = guarded;

                        // Capture commands run in the terminal via our OSC 697
                        // hook, and strip the sequence so it never reaches the
                        // renderer (#113). Skip our own injected setup line in the
                        // rare case HISTCONTROL=ignorespace isn't in effect.
                        // 单次扫描 + 一次构建。原来每命中一条就 `replace_range` 一次，
                        // 每次都要搬动整段已剥离的字符串 → 命中 k 条就是 O(k·n)，
                        // `cat` 一大段带 OSC 697 的输出会明显变慢（C2.2）。
                        // 语义等价：`extract_osc_command` 每次都返回**第一个**匹配，
                        // 而删除处之前的内容从不改变，所以按序推进与反复重扫等价。
                        let mut commands = Vec::new();
                        let mut stripped = String::with_capacity(text.len());
                        let mut cursor = 0usize;
                        while let Some((cmd, range)) = extract_osc_command(&text[cursor..]) {
                            let start = cursor + range.start;
                            stripped.push_str(&text[cursor..start]);
                            cursor += range.end;
                            commands.push(cmd);
                        }
                        stripped.push_str(&text[cursor..]);
                        for cmd in &commands {
                            let cmd = cmd.trim();
                            if !cmd.is_empty() && !cmd.contains("__ms7") {
                                let _ = events.send(SessionEvent::CommandRan(cmd.to_string()));
                            }
                        }
                        let text = stripped;

                        // Expect/send login triggers (#212): match against the
                        // text the user actually sees (OSC hooks are already
                        // stripped above) and reply on the same channel.
                        for (response, append_enter) in trigger_engine.feed(&text) {
                            let mut bytes = response.as_str().as_bytes().to_vec();
                            if append_enter {
                                bytes.push(b'\r');
                            }
                            let encoded = terminal_encoder.encode(&bytes);
                            if let Err(error) = channel.data(&encoded[..]).await {
                                tracing::warn!("session trigger response failed: {error}");
                            }
                        }

                        let _ = crate::output_budget::emit_output(&events, text);
                    }
                    Some(ChannelMsg::ExtendedData { data, ext: _ }) => {
                        let text = extended_decoder.decode(&data);
                        for (response, append_enter) in trigger_engine.feed(&text) {
                            let mut bytes = response.as_str().as_bytes().to_vec();
                            if append_enter {
                                bytes.push(b'\r');
                            }
                            let encoded = terminal_encoder.encode(&bytes);
                            if let Err(error) = channel.data(&encoded[..]).await {
                                tracing::warn!("session trigger response failed: {error}");
                            }
                        }
                        let _ = crate::output_budget::emit_output(&events, text);
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        let _ = events.send(SessionEvent::Status(
                            format!("{} (code {exit_status})", t("远程进程退出", "remote process exited")),
                        ));
                    }
                    Some(ChannelMsg::Close) | None => {
                        break;
                    }
                    _ => {}
                }
            }
            // Remote resource monitor channel.  The `async { ... }` lets us poll
            // an Option<Channel>: once the monitor channel closes we replace it
            // with `pending()` so this arm simply never fires again.
            mon = async {
                match mon_channel.as_mut() {
                    Some(ch) => ch.wait().await,
                    None => std::future::pending().await,
                }
            } => {
                match mon {
                    Some(ChannelMsg::Data { data }) => {
                        mon_buf.push_str(&String::from_utf8_lossy(&data));
                        // Process every complete sample terminated by the marker.
                        while let Some(idx) = mon_buf.find("__MSTICK__") {
                            let block = mon_buf[..idx].to_string();
                            let rest = mon_buf[idx + "__MSTICK__".len()..]
                                .trim_start_matches(['\r', '\n'])
                                .to_string();
                            mon_buf = rest;
                            if let Some(stats) = parse_monitor_block(
                                &block,
                                &mut prev_cpu,
                                &mut prev_net,
                                &mut prev_net_at,
                            ) {
                                // 暂停时**照样解析**（1 Hz，成本可忽略），只是不发事件；
                                // 始终留着最新一份，恢复时立刻推出去。
                                if resource_monitoring {
                                    let _ = events.send(stats.clone());
                                }
                                last_resource = Some(stats);
                            }
                        }
                        // Bound the leftover (incomplete) tail: a server that
                        // streams data but never emits the __MSTICK__ marker must
                        // not grow this buffer without limit (memory DoS, #27).
                        // A real sample is a few KiB; 1 MiB is a generous ceiling.
                        const MON_BUF_CAP: usize = 1 << 20;
                        if mon_buf.len() > MON_BUF_CAP {
                            mon_buf.clear();
                        }
                    }
                    Some(ChannelMsg::Close) | None => {
                        mon_channel = None;
                    }
                    _ => {}
                }
            }
            sys = async {
                match sys_channel.as_mut() {
                    Some(ch) => ch.wait().await,
                    None => std::future::pending().await,
                }
            } => {
                match sys {
                    Some(ChannelMsg::Data { data }) => {
                        sys_buf.push_str(&String::from_utf8_lossy(&data));
                        // 与上方 `mon_buf` 同一条理由（memory DoS, #27）：服务器持续发
                        // 数据却迟迟不给 `__MSTICK__` 标记时，这个缓冲区不能无上限增长。
                        // 真正的采样只有几 KiB，1 MiB 是宽松的上界（B1.2）。
                        const SYS_BUF_CAP: usize = 1 << 20;
                        if sys_buf.len() > SYS_BUF_CAP {
                            sys_buf.clear();
                        }
                        if let Some(idx) = sys_buf.find("__MSTICK__") {
                            let block = sys_buf[..idx].to_string();
                            let mut detail_cpu = None;
                            let mut detail_net = std::collections::HashMap::new();
                            let mut detail_at = std::time::Instant::now();
                            if let Some(details) = parse_monitor_block(
                                &block,
                                &mut detail_cpu,
                                &mut detail_net,
                                &mut detail_at,
                            ) {
                                let _ = events.send(details);
                            }
                            sys_buf.clear();
                            sys_channel = None;
                        }
                    }
                    Some(ChannelMsg::Close) | None => {
                        sys_channel = None;
                    }
                    _ => {}
                }
            }
            proc_msg = async {
                match proc_channel.as_mut() {
                    Some(ch) => ch.wait().await,
                    None => std::future::pending().await,
                }
            } => {
                match proc_msg {
                    Some(ChannelMsg::Data { data }) => {
                        proc_buf.push_str(&String::from_utf8_lossy(&data));
                        while let Some(idx) = proc_buf.find("__PSTICK__") {
                            let block = proc_buf[..idx].to_string();
                            proc_buf = proc_buf[idx + "__PSTICK__".len()..]
                                .trim_start_matches(['\r', '\n'])
                                .to_string();
                            let (current_user, procs) = parse_process_block(&block);
                            let ev = SessionEvent::ProcessStats {
                                current_user,
                                procs,
                            };
                            // 同上：暂停时仍解析，只留最新一份。
                            if resource_monitoring {
                                let _ = events.send(ev.clone());
                            }
                            last_process = Some(ev);
                        }
                        const PROC_BUF_CAP: usize = 1 << 18;
                        if proc_buf.len() > PROC_BUF_CAP {
                            proc_buf.clear();
                        }
                    }
                    Some(ChannelMsg::Close) | None => proc_channel = None,
                    _ => {}
                }
            }
        }
    }

    // Tear down any port-forward listeners (#56); -R forwards die with the
    // session's disconnect below.
    for f in runtime_forwards.into_values() {
        if let Some(task) = f.task {
            task.abort();
        }
    }
    let _ = events.send(SessionEvent::TunnelUpdate(Vec::new()));

    // 网络半死时 `disconnect` 等不到确认，超时直接放弃。
    if tokio::time::timeout(
        CLOSE_GRACE,
        handle.disconnect(Disconnect::ByApplication, "bye", ""),
    )
    .await
    .is_err()
    {
        tracing::warn!(
            "ssh disconnect timed out ({}@{})",
            session.user,
            session.host
        );
    }
    // The shell pump loop only exits when the channel closes / EOFs (incl. a
    // peer/bastion-initiated disconnect), so record it for #86 diagnostics.
    tracing::warn!("ssh connection closed ({}@{})", session.user, session.host);
    let _ = events.send(SessionEvent::Closed(
        t("连接已关闭", "connection closed").into(),
    ));
    Ok(())
}
