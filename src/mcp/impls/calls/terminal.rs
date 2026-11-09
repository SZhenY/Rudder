use super::*;

pub(crate) fn read_terminal(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let source = opt_str_arg(args, "source").unwrap_or_else(|| "screen".to_string());
    if source != "screen" && source != "scrollback" {
        return Err(McpError::Invalid(
            "`source` must be `screen` or `scrollback`".to_string(),
        ));
    }
    let max_rows = int_arg(args, "max_rows", 200, 1, 400) as usize;
    let snapshot = snapshot(state, &tab_id, max_rows, source == "scrollback")?;
    Ok(json!({
        "tab_id": tab_id,
        "source": source,
        "rows": snapshot.rows,
        "cols": snapshot.cols,
        "cursor": { "row": snapshot.cursor_row, "col": snapshot.cursor_col },
        "text": snapshot.text,
        "truncated": snapshot.truncated,
        "output_seq": snapshot.output_seq,
    }))
}

pub(crate) async fn wait_for_output(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let max_rows = int_arg(args, "max_rows", 400, 1, 400) as usize;
    let timeout_ms = int_arg(args, "timeout_ms", 15_000, 100, 120_000);
    let quiet_ms = int_arg(args, "quiet_ms", 300, 0, 5_000);
    let pattern = opt_str_arg(args, "pattern");
    let regex = match pattern.as_deref() {
        Some(pattern) => Some(
            Regex::new(pattern)
                .map_err(|err| McpError::Invalid(format!("invalid `pattern`: {err}")))?,
        ),
        None => None,
    };

    let started = Instant::now();
    let deadline = started + Duration::from_millis(timeout_ms);
    let mut seen_seq = match args.get("since_seq").and_then(Value::as_u64) {
        Some(seq) => seq,
        None => snapshot(state, &tab_id, max_rows, true)?.output_seq,
    };
    let mut last_change = started;
    // "已经看到过新输出"必须单独记：`seen_seq` 在检测到变化时就被推进了，
    // 若直接拿 `output_seq != seen_seq` 当作"有新输出"，下一次轮询它永远为假，
    // 静默模式会一路等到超时。
    let mut saw_output = false;

    // 两次整屏快照的最小间隔。序号仍然每 40 ms 看一次（读一个 u64），但 400 行文本
    // 的重建要占住终端缓冲锁（与 SSH ingest、UI 渲染共用），输出持续时按 25 次/秒
    // 重建整屏没有意义：正则匹配晚 200 ms 也不会漏掉那段时间的输出。
    const SNAPSHOT_MIN_INTERVAL: Duration = Duration::from_millis(200);

    let mut first = true;
    let mut last_snapshot_at = started - SNAPSHOT_MIN_INTERVAL;
    loop {
        let seq_now = output_seq_of(state, &tab_id)?;
        let changed = seq_now != seen_seq;
        if changed {
            seen_seq = seq_now;
            last_change = Instant::now();
            saw_output = true;
        }

        let quiet_due = regex.is_none()
            && saw_output
            && last_change.elapsed() >= Duration::from_millis(quiet_ms);
        let timed_out = Instant::now() >= deadline;
        // 文本只在要判断的时候生成：首次（拿基线）、正则匹配（按间隔节流）、
        // 或即将返回（静默 / 超时）。静默模式在有输出时完全不生成文本。
        let need_text = first
            || quiet_due
            || timed_out
            || (regex.is_some() && changed && last_snapshot_at.elapsed() >= SNAPSHOT_MIN_INTERVAL);

        if need_text {
            let snapshot = snapshot(state, &tab_id, max_rows, true)?;
            last_snapshot_at = Instant::now();
            // 快照里的序号可能比刚才读到的更新，以它为准。
            if snapshot.output_seq != seen_seq {
                seen_seq = snapshot.output_seq;
                last_change = Instant::now();
                saw_output = true;
            }
            first = false;
            let hit = match &regex {
                Some(regex) => regex.is_match(&snapshot.text),
                None => quiet_due,
            };
            if hit || timed_out {
                let mut out = json!({
                    "matched": hit,
                    "text": tail_bytes(&snapshot.text, 4096),
                    "output_seq": snapshot.output_seq,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                });
                // `timed_out` 只在真的超时出现：静默命中时带上它会让调用方误判成失败。
                if !hit {
                    out["timed_out"] = json!(true);
                }
                return Ok(out);
            }
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

/// 只读输出序号，不生成文本。
///
/// 等待类工具每 40–60 ms 轮询一次；直接调 `snapshot()` 的话每次都要逐行渲染 400 行，
/// 还占着缓冲锁（与 SSH ingest、UI 渲染共用）。先比序号能把这笔开销降到一次读 u64。
pub(crate) fn output_seq_of(state: &SharedState, tab_id: &str) -> Result<u64, McpError> {
    let handle = {
        let bufs = state.app.bufs.lock().unwrap_or_else(|err| err.into_inner());
        bufs.get(tab_id)
            .cloned()
            .ok_or_else(|| McpError::Invalid(format!("unknown tab_id: {tab_id}")))?
    };
    let buffer = handle.lock().unwrap_or_else(|err| err.into_inner());
    Ok(buffer.output_seq())
}

/// 取一块终端缓冲的只读快照。先克隆 `Arc` 再单独锁缓冲，避免外层表与缓冲同时持锁。
pub(crate) fn snapshot(
    state: &SharedState,
    tab_id: &str,
    max_rows: usize,
    include_scrollback: bool,
) -> Result<TextSnapshot, McpError> {
    let handle = {
        let bufs = state.app.bufs.lock().unwrap_or_else(|err| err.into_inner());
        bufs.get(tab_id)
            .cloned()
            .ok_or_else(|| McpError::Invalid(format!("unknown tab_id: {tab_id}")))?
    };
    let buffer = handle.lock().unwrap_or_else(|err| err.into_inner());
    Ok(buffer.snapshot_text(max_rows, include_scrollback))
}

/// 一次性执行命令。SSH 走会话连接上的 exec 通道（不进终端窗口），
/// 本地会话直接起进程；Telnet / 串口没有 exec 通道。
pub(crate) async fn run_command(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let command = str_arg(args, "command")?;
    let timeout_ms = int_arg(args, "timeout_ms", 30_000, 1_000, 300_000);
    let max_output_bytes = int_arg(args, "max_output_bytes", 1_048_576, 1024, 4_194_304) as usize;
    let timeout = Duration::from_millis(timeout_ms);

    // `host` 决定本地标签用哪套 marker 语法（cmd / wsl / PowerShell）；
    // `session_id` 供全屏保护时另开一个标签页。
    let (kind, host, session_id) = {
        let statuses = state
            .app
            .tab_statuses
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        match statuses.get(&tab_id) {
            Some(status) => (status.kind, status.host.clone(), status.session_id.clone()),
            None => return Err(McpError::Invalid(format!("unknown tab_id: {tab_id}"))),
        }
    };

    // 执行通道开关：开 → 走交互式 PTY（命令与输出对用户可见）；
    // 默认关 = 下面的 exec 通道（不碰终端）。
    if state
        .cfg
        .read()
        .map(|cfg| cfg.pty_commands)
        .unwrap_or(false)
    {
        // 用户正在 vim / htop 这类全屏程序里时，往同一个 PTY 写会把屏幕打乱。
        // 另开一个标签页执行，命令与输出在那边可见，当前屏幕一个字都不动。
        if is_alt_screen(state, &tab_id) {
            let opened = crate::app::mcp_ui::open_session(session_id.clone(), false).await?;
            let new_tab = opened
                .get("tab_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if new_tab.is_empty() {
                return Err(McpError::Invalid(
                    "could not open a tab to run the command in".to_string(),
                ));
            }
            // 新标签页返回时还是 connecting，PTY 没就绪 —— 等它画出提示符再发命令。
            wait_tab_ready(state, &new_tab, timeout.min(Duration::from_secs(20))).await?;
            let mut out = pty_run_command(
                state,
                new_tab.clone(),
                kind,
                &host,
                command,
                timeout,
                max_output_bytes,
            )
            .await?;
            if let Some(obj) = out.as_object_mut() {
                obj.insert("reason".to_string(), json!("alt_screen"));
                obj.insert("opened_tab".to_string(), json!(new_tab));
                obj.insert("executed_tab".to_string(), json!(new_tab));
            }
            return Ok(out);
        }
        return pty_run_command(
            state,
            tab_id,
            kind,
            &host,
            command,
            timeout,
            max_output_bytes,
        )
        .await;
    }

    let result = match kind {
        SessionKind::Ssh => {
            let receiver =
                crate::app::mcp_ui::ssh_exec(tab_id.clone(), command, timeout, max_output_bytes)
                    .await?;
            receiver.await.map_err(|_| {
                McpError::Invalid("session closed before the command finished".to_string())
            })?
        }
        // 装箱：这条分支带管道读取的状态机，不装箱会把它的体积并进 `call()` 的 future，
        // 进而抬高所有工具调用（包括不相关的 http 测试）的栈占用。
        SessionKind::Local => Box::pin(local_exec(command, timeout, max_output_bytes)).await,
        _ => {
            return Err(McpError::NotSupported(
                "exec is unavailable for telnet and serial sessions; use send_input + read_terminal",
            ));
        }
    };

    if let Some(error) = result.error {
        return Err(McpError::Invalid(error));
    }
    Ok(json!({
        "tab_id": tab_id,
        "exit_code": result.exit_code,
        "stdout": result.stdout,
        "stderr": result.stderr,
        "truncated": result.truncated,
        "timed_out": result.timed_out,
        "duration_ms": result.duration.as_millis() as u64,
    }))
}

/// 这个标签页当前是否处于 alt-screen（vim / htop / less 这类全屏程序）。
pub(crate) fn is_alt_screen(state: &SharedState, tab_id: &str) -> bool {
    let handle = {
        let bufs = state.app.bufs.lock().unwrap_or_else(|err| err.into_inner());
        match bufs.get(tab_id).cloned() {
            Some(handle) => handle,
            None => return false,
        }
    };
    let buffer = handle.lock().unwrap_or_else(|err| err.into_inner());
    crate::terminal::is_alt(&buffer.term)
}

/// 等一个新标签页的 PTY 就绪：它返回时还是 `connecting`，此刻写进去的字节会丢。
/// 判据是"终端缓冲已经有输出"（登录提示符 / banner），而不是查状态字段 ——
/// 状态转到 connected 之后提示符可能还没画出来。
pub(crate) async fn wait_tab_ready(
    state: &SharedState,
    tab_id: &str,
    timeout: Duration,
) -> Result<(), McpError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(snap) = snapshot(state, tab_id, 40, false)
            && snap.output_seq > 0
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(McpError::Invalid(format!(
                "the tab opened for this command did not become ready in time: {tab_id}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}
