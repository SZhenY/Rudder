use super::*;

/// 终端里那条 shell 的家族 —— 决定 marker 语法。
///
/// 判据跟着 `terminal/impls/local.rs` 的 `local_program`：`host == "cmd"` 才起
/// cmd.exe，`"wsl"` 起的是发行版里的 POSIX 登录 shell，其余 Windows 本地标签是
/// PowerShell；SSH / Telnet / 串口一律按 POSIX 处理。
pub(crate) fn shell_family(kind: SessionKind, host: &str) -> pty_cmd::ShellFamily {
    if !matches!(kind, SessionKind::Local) || !cfg!(windows) {
        return pty_cmd::ShellFamily::Posix;
    }
    match host {
        "cmd" => pty_cmd::ShellFamily::Cmd,
        "wsl" => pty_cmd::ShellFamily::Posix,
        _ => pty_cmd::ShellFamily::PowerShell,
    }
}

/// PTY 通道执行：把命令包上 marker 发进**用户的交互式终端**，再从终端快照里切出输出。
///
/// 与 exec 通道的取舍：这条路径下命令与输出用户都看得见（这正是开关的目的），
/// 但拿不到独立的 stderr，退出码也只能靠 marker 协议约定；被用户并发输入打断时
/// marker 可能缺失，此时返回 `marker_found: false` 与快照尾部，让调用方知道结果不可靠。
pub(crate) async fn pty_run_command(
    state: &SharedState,
    tab_id: String,
    kind: SessionKind,
    host: &str,
    command: String,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<Value, McpError> {
    let started = Instant::now();
    let token = pty_cmd::new_token(state.next_epoch());
    let line = pty_cmd::wrap(shell_family(kind, host), &token, &command);

    // 复用 send_input 的路径：它经 UI 线程把字节写进 PTY。
    crate::app::mcp_ui::send_input(tab_id.clone(), line, true).await?;

    let deadline = started + timeout;
    let mut seen_seq = output_seq_of(state, &tab_id)?;
    let mut first = true;
    loop {
        // 没有新输出时不必生成整屏文本再扫 marker —— 60 ms 一次的轮询里那是主要开销，
        // 而且每次都要占住缓冲锁（与 ingest、渲染共用）。
        let changed = output_seq_of(state, &tab_id)? != seen_seq;
        if first || changed {
            first = false;
            let snap = snapshot(state, &tab_id, 400, true)?;
            seen_seq = snap.output_seq;
            let out = pty_cmd::extract(&token, &snap.text);
            if out.found {
                // 参数名是字节预算，这里按字符截断：中文输出最多 3 倍字节（上限 4 MiB → 12 MiB），
                // 换来的是不会把一个汉字劈成两半。
                let (text, truncated) = truncate_chars(out.text, max_output_bytes);
                return Ok(json!({
                    "tab_id": tab_id,
                    "channel": "pty",
                    "marker_found": true,
                    "exit_code": out.code,
                    "stdout": text,
                    "stderr": "",
                    "truncated": truncated,
                    "timed_out": false,
                    "duration_ms": started.elapsed().as_millis() as u64,
                }));
            }
        }
        if Instant::now() >= deadline {
            // 超时才取一次文本（用来告诉调用方卡在哪）。
            let snap = snapshot(state, &tab_id, 400, true)?;
            return Ok(json!({
                "tab_id": tab_id,
                "channel": "pty",
                "marker_found": false,
                "exit_code": null,
                "stdout": tail_bytes(&snap.text, max_output_bytes.min(4096)),
                "stderr": "",
                "truncated": true,
                "timed_out": true,
                "duration_ms": started.elapsed().as_millis() as u64,
            }));
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
}

/// 本地会话：直接起一个子进程，不进 PTY（PTY 里跑命令会污染用户屏幕）。
pub(crate) async fn local_exec(
    command: String,
    timeout: Duration,
    max_output_bytes: usize,
) -> crate::ssh::ExecResult {
    use std::process::Stdio;

    let started = Instant::now();
    #[cfg(windows)]
    let mut child_cmd = {
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.arg("/C").arg(&command);
        cmd
    };
    #[cfg(not(windows))]
    let mut child_cmd = {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(&command);
        cmd
    };
    child_cmd
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match child_cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return crate::ssh::ExecResult::unsupported(format!("failed to start shell: {err}"));
        }
    };
    // 边读边截断：`wait_with_output` 会把整份输出留在内存里，一个 `yes` 或
    // `journalctl -f` 就能顶到 GB 级（并发上限 8，等于 OOM）。两条管道必须并发读，
    // 否则子进程写满 stderr 时会和我们读 stdout 互相等死。
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let readers = async move {
        let out_fut = async {
            match out_pipe.as_mut() {
                Some(pipe) => read_capped(pipe, max_output_bytes).await,
                None => Ok((Vec::new(), false)),
            }
        };
        let err_fut = async {
            match err_pipe.as_mut() {
                Some(pipe) => read_capped(pipe, max_output_bytes).await,
                None => Ok((Vec::new(), false)),
            }
        };
        tokio::join!(out_fut, err_fut)
    };

    // 超时后 future 被丢弃 → `kill_on_drop` 负责杀掉子进程。
    match tokio::time::timeout(timeout, async move {
        let (stdout_res, stderr_res) = readers.await;
        let status = child.wait().await;
        (stdout_res, stderr_res, status)
    })
    .await
    {
        Ok((Ok((stdout, out_cut)), Ok((stderr, err_cut)), Ok(status))) => crate::ssh::ExecResult {
            exit_code: status.code(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            truncated: out_cut || err_cut,
            timed_out: false,
            duration: started.elapsed(),
            error: None,
        },
        Ok((Err(err), _, _)) | Ok((_, Err(err), _)) => {
            crate::ssh::ExecResult::unsupported(format!("read shell output: {err}"))
        }
        Ok((_, _, Err(err))) => {
            crate::ssh::ExecResult::unsupported(format!("shell wait failed: {err}"))
        }
        Err(_) => crate::ssh::ExecResult {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            timed_out: true,
            duration: started.elapsed(),
            error: None,
        },
    }
}

/// 读到 `cap` 字节为止，超出部分继续读但丢弃 —— 必须抽干管道，否则子进程会阻塞在写端。
/// 返回 (数据, 是否被截断)。
pub(crate) async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    use tokio::io::AsyncReadExt as _;

    let mut kept = Vec::with_capacity(cap.min(64 * 1024));
    // 放堆上：这个函数的状态机是 `call()` 那个巨型 future 的一部分，
    // 16 KB 栈缓冲会把整条调用链的栈占用一起抬高（实测能把 2 MB 线程顶爆）。
    let mut chunk = Box::new([0u8; 16 * 1024]);
    let mut cut = false;
    loop {
        let n = reader.read(&mut chunk[..]).await?;
        if n == 0 {
            return Ok((kept, cut));
        }
        let room = cap.saturating_sub(kept.len());
        if room == 0 {
            cut = true;
            continue;
        }
        let take = room.min(n);
        kept.extend_from_slice(&chunk[..take]);
        if take < n {
            cut = true;
        }
    }
}
