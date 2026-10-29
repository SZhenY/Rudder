//! 工具实现。
//!
//! 约定：返回值直接序列化成 MCP 的 `structuredContent`；错误用 `McpError`，
//! 由 handler 包装成 `isError: true` 的结构化结果（**不是** JSON-RPC 协议错误）——
//! 协议错误客户端只会看到 "Tool result missing due to internal error"，看不到原因。
//!
//! 数据来源分两类：
//! * 只读的终端缓冲与标签页状态是 `Arc`，直接读，不经过 UI 线程；
//! * 需要 `ConfigStore` / `SessionHandle` 的操作经 `crate::app::mcp_ui` 回 UI 线程。

use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Value, json};

use super::types::{McpError, SharedState};
use crate::config::SessionKind;
use crate::terminal::TextSnapshot;

pub(super) async fn call(
    state: &SharedState,
    name: &str,
    args: &Value,
) -> Result<Value, McpError> {
    match name {
        "get_server_info" => Ok(server_info(state)),
        "list_sessions" => crate::app::mcp_ui::list_sessions(opt_str_arg(args, "group")).await,
        "get_session" => crate::app::mcp_ui::get_session(str_arg(args, "session_id")?).await,
        "list_tabs" => Ok(list_tabs(state)),
        "get_tab_status" => tab_status(state, args).await,
        "read_terminal" => read_terminal(state, args),
        "wait_for_output" => wait_for_output(state, args).await,
        "open_session" => {
            crate::app::mcp_ui::open_session(
                str_arg(args, "session_id")?,
                bool_arg(args, "trust_unknown_host_key", false),
            )
            .await
        }
        "close_session" => crate::app::mcp_ui::close_session(str_arg(args, "tab_id")?).await,
        "send_input" => {
            crate::app::mcp_ui::send_input(
                str_arg(args, "tab_id")?,
                raw_str_arg(args, "text"),
                bool_arg(args, "enter", true),
            )
            .await
        }
        "run_command" => run_command(state, args).await,
        "get_settings" => {
            let keys = args
                .get("keys")
                .and_then(Value::as_array)
                .map(|keys| {
                    keys.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            crate::app::mcp_ui::get_settings(keys).await
        }
        "set_settings" => {
            let values = args
                .get("values")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if values.is_empty() {
                return Err(McpError::Invalid(
                    "`values` must be a non-empty object".to_string(),
                ));
            }
            crate::app::mcp_ui::set_settings(values).await
        }
        "upsert_host" => crate::app::mcp_ui::upsert_host(args.clone()).await,
        "delete_host" => {
            crate::app::mcp_ui::delete_host(
                str_arg(args, "session_id")?,
                bool_arg(args, "close_open_tabs", false),
            )
            .await
        }
        "sftp_list" => sftp_list(state, args).await,
        "sftp_download" => sftp_download(state, args).await,
        "sftp_upload" => sftp_upload(state, args).await,
        other => Err(McpError::Invalid(format!("unknown tool: {other}"))),
    }
}

fn server_info(state: &SharedState) -> Value {
    let cfg = state.config();
    let status = state.status();
    json!({
        "name": "rudder",
        "version": env!("CARGO_PKG_VERSION"),
        "running": status.running,
        "endpoints": status.urls(),
        "permissions": cfg.perms.describe(),
        "limits": {
            "max_concurrent_calls": 8,
            "max_request_bytes": 1024 * 1024,
        },
        "error": status.error,
    })
}

fn list_tabs(state: &SharedState) -> Value {
    let statuses = state
        .app
        .tab_statuses
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let mut tabs: Vec<Value> = statuses
        .iter()
        .map(|(tab_id, status)| {
            json!({
                "tab_id": tab_id,
                "host": status.host,
                "user": status.user,
                "session_id": status.session_id,
                "state": state_name(status.state),
                "is_local": status.is_local,
            })
        })
        .collect();
    tabs.sort_by(|a, b| {
        a["tab_id"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["tab_id"].as_str().unwrap_or_default())
    });
    json!({ "count": tabs.len(), "tabs": tabs })
}

async fn tab_status(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let is_local = {
        let statuses = state
            .app
            .tab_statuses
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        match statuses.get(&tab_id) {
            Some(status) => status.is_local,
            None => return Err(McpError::Invalid(format!("unknown tab_id: {tab_id}"))),
        }
    };

    // 本机会话没有远端样本：直接给本机快照，不折腾远端监控。
    if is_local {
        let local = state
            .app
            .local_snap
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        let mut value = status_json(state, &tab_id);
        value["local"] = json!({
            "cpu_percent": local.cpu_percent,
            "mem_percent": local.mem_percent,
            "mem_used_mib": local.mem_used_mib,
            "mem_total_mib": local.mem_total_mib,
            "net_rx_per_sec": local.net_rx_per_sec,
            "net_tx_per_sec": local.net_tx_per_sec,
        });
        value["stale"] = json!(false);
        return Ok(value);
    }

    let refresh = bool_arg(args, "refresh", true);
    let fresh = if refresh {
        refresh_stats(state, &tab_id).await?
    } else {
        false
    };
    let mut value = status_json(state, &tab_id);
    value["stale"] = json!(!fresh);
    Ok(value)
}

/// 侧栏折叠时远端采样是停着的。这里临时开一下、等一个新样本、再恢复原状。
/// 返回数据是否已经新鲜。
async fn refresh_stats(state: &SharedState, tab_id: &str) -> Result<bool, McpError> {
    if sample_age(state, tab_id).is_some_and(|age| age < Duration::from_secs(5)) {
        return Ok(true);
    }
    let was_enabled = match crate::app::mcp_ui::sidebar_collapsed().await {
        Ok(collapsed) => !collapsed,
        // 界面已关：拿不到采样开关，就用现成数据并标记 stale，不要整个工具失败。
        Err(_) => return Ok(false),
    };
    if crate::app::mcp_ui::set_resource_monitoring(tab_id.to_string(), true)
        .await
        .is_err()
    {
        return Ok(false);
    }

    let deadline = Instant::now() + Duration::from_millis(1600);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if sample_age(state, tab_id).is_some_and(|age| age < Duration::from_millis(400)) {
            break;
        }
    }
    if !was_enabled {
        let _ = crate::app::mcp_ui::set_resource_monitoring(tab_id.to_string(), false).await;
    }
    Ok(sample_age(state, tab_id).is_some_and(|age| age < Duration::from_secs(5)))
}

fn sample_age(state: &SharedState, tab_id: &str) -> Option<Duration> {
    let statuses = state
        .app
        .tab_statuses
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    statuses
        .get(tab_id)
        .and_then(|status| status.last_stats_at)
        .map(|at| at.elapsed())
}

fn status_json(state: &SharedState, tab_id: &str) -> Value {
    let statuses = state
        .app
        .tab_statuses
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let Some(status) = statuses.get(tab_id) else {
        return json!({ "tab_id": tab_id, "state": "unknown" });
    };
    json!({
        "tab_id": tab_id,
        "host": status.host,
        "user": status.user,
        "session_id": status.session_id,
        "state": state_name(status.state),
        "is_local": status.is_local,
        "cpu_percent": status.cpu,
        "mem": { "used_kib": status.mem_used_kib, "total_kib": status.mem_total_kib },
        "swap": { "used_kib": status.swap_used_kib, "total_kib": status.swap_total_kib },
        "net": status
            .net
            .iter()
            .map(|(iface, rx, tx)| json!({ "iface": iface, "rx_total": rx, "tx_total": tx }))
            .collect::<Vec<_>>(),
        "disks": status
            .disks
            .iter()
            .map(|(mount, avail, total)| json!({ "mount": mount, "avail": avail, "total": total }))
            .collect::<Vec<_>>(),
        "top_procs": status
            .procs
            .iter()
            .take(10)
            .map(|proc| json!({
                "pid": proc.pid,
                "user": proc.user,
                "cpu": proc.cpu,
                "mem": proc.mem,
                "command": proc.command,
            }))
            .collect::<Vec<_>>(),
        "sys": { "overview": pairs(&status.sys.overview) },
        "last_sample_ms_ago": status
            .last_stats_at
            .map(|at| at.elapsed().as_millis() as u64),
    })
}

fn pairs(rows: &[(String, String)]) -> Value {
    Value::Object(
        rows.iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect(),
    )
}

fn state_name(state: u8) -> &'static str {
    match state {
        0 => "connecting",
        1 => "connected",
        2 => "closed",
        _ => "unknown",
    }
}

fn read_terminal(state: &SharedState, args: &Value) -> Result<Value, McpError> {
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

async fn wait_for_output(state: &SharedState, args: &Value) -> Result<Value, McpError> {
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

    loop {
        let snapshot = snapshot(state, &tab_id, max_rows, true)?;
        if snapshot.output_seq != seen_seq {
            seen_seq = snapshot.output_seq;
            last_change = Instant::now();
            saw_output = true;
        }
        let hit = match &regex {
            Some(regex) => regex.is_match(&snapshot.text),
            None => saw_output && last_change.elapsed() >= Duration::from_millis(quiet_ms),
        };
        if hit {
            return Ok(json!({
                "matched": true,
                "text": tail(&snapshot.text, 4096),
                "output_seq": snapshot.output_seq,
                "elapsed_ms": started.elapsed().as_millis() as u64,
            }));
        }
        if Instant::now() >= deadline {
            return Ok(json!({
                "matched": false,
                "timed_out": true,
                "text": tail(&snapshot.text, 4096),
                "output_seq": snapshot.output_seq,
                "elapsed_ms": started.elapsed().as_millis() as u64,
            }));
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

/// 取一块终端缓冲的只读快照。先克隆 `Arc` 再单独锁缓冲，避免外层表与缓冲同时持锁。
fn snapshot(
    state: &SharedState,
    tab_id: &str,
    max_rows: usize,
    include_scrollback: bool,
) -> Result<TextSnapshot, McpError> {
    let handle = {
        let bufs = state
            .app
            .bufs
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        bufs.get(tab_id)
            .cloned()
            .ok_or_else(|| McpError::Invalid(format!("unknown tab_id: {tab_id}")))?
    };
    let buffer = handle.lock().unwrap_or_else(|err| err.into_inner());
    Ok(buffer.snapshot_text(max_rows, include_scrollback))
}

/// 一次性执行命令。SSH 走会话连接上的 exec 通道（不进终端窗口），
/// 本地会话直接起进程；Telnet / 串口没有 exec 通道。
async fn run_command(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let command = str_arg(args, "command")?;
    let timeout_ms = int_arg(args, "timeout_ms", 30_000, 1_000, 300_000);
    let max_output_bytes = int_arg(args, "max_output_bytes", 1_048_576, 1024, 4_194_304) as usize;
    let timeout = Duration::from_millis(timeout_ms);

    let kind = {
        let statuses = state
            .app
            .tab_statuses
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        match statuses.get(&tab_id) {
            Some(status) => status.kind,
            None => return Err(McpError::Invalid(format!("unknown tab_id: {tab_id}"))),
        }
    };

    let result = match kind {
        SessionKind::Ssh => {
            let receiver = crate::app::mcp_ui::ssh_exec(
                tab_id.clone(),
                command,
                timeout,
                max_output_bytes,
            )
            .await?;
            receiver.await.map_err(|_| {
                McpError::Invalid("session closed before the command finished".to_string())
            })?
        }
        SessionKind::Local => local_exec(command, timeout, max_output_bytes).await,
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

/// 本地会话：直接起一个子进程，不进 PTY（PTY 里跑命令会污染用户屏幕）。
async fn local_exec(
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

    let child = match child_cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return crate::ssh::ExecResult::unsupported(format!("failed to start shell: {err}"));
        }
    };
    // 超时后 future 被丢弃 → `kill_on_drop` 负责杀掉子进程。
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let (stdout, stdout_cut) = truncate_bytes(&output.stdout, max_output_bytes);
            let (stderr, stderr_cut) = truncate_bytes(&output.stderr, max_output_bytes);
            crate::ssh::ExecResult {
                exit_code: output.status.code(),
                stdout,
                stderr,
                truncated: stdout_cut || stderr_cut,
                timed_out: false,
                duration: started.elapsed(),
                error: None,
            }
        }
        Ok(Err(err)) => crate::ssh::ExecResult::unsupported(format!("shell wait failed: {err}")),
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

fn truncate_bytes(bytes: &[u8], max: usize) -> (String, bool) {
    let cut = bytes.len() > max;
    let slice = &bytes[..bytes.len().min(max)];
    (String::from_utf8_lossy(slice).into_owned(), cut)
}

/// 取该标签页的 SFTP 通道发一条命令。`SftpHandle` 不是 `Clone`，所以闭包里发完
/// 立刻释放表锁，再在锁外等结果 —— 不能把 UI 线程要用的表锁按住等传输。
fn with_sftp<T>(
    state: &SharedState,
    tab_id: &str,
    send: impl FnOnce(&crate::sftp::SftpHandle) -> tokio::sync::oneshot::Receiver<T>,
) -> Result<tokio::sync::oneshot::Receiver<T>, McpError> {
    let handles = state
        .app
        .sftp_handles
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let Some(handle) = handles.get(tab_id) else {
        return Err(McpError::NotSupported(
            "this tab has no SFTP channel; only SSH sessions have one",
        ));
    };
    Ok(send(handle))
}

fn sftp_closed() -> McpError {
    McpError::NotConnected("the SFTP channel closed before the request finished")
}

async fn sftp_list(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let path = opt_str_arg(args, "path").unwrap_or_else(|| "/".to_string());
    let receiver = with_sftp(state, &tab_id, |handle| handle.mcp_list(path.clone()))?;
    let mut entries = receiver
        .await
        .map_err(|_| sftp_closed())?
        .map_err(McpError::TransferFailed)?;
    // 目录优先、再按名字：与界面上的 SFTP 面板同一顺序。
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(json!({
        "tab_id": tab_id,
        "path": path,
        "count": entries.len(),
        "entries": entries
            .iter()
            .map(|entry| json!({
                "name": entry.name,
                "path": entry.full_path,
                "is_dir": entry.is_dir,
                "size": entry.size,
                "modified": entry.modified,
                "mode": entry.mode,
            }))
            .collect::<Vec<_>>(),
    }))
}

async fn sftp_download(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let remote = str_arg(args, "remote_path")?;
    let local = str_arg(args, "local_path")?;
    let local_path = std::path::PathBuf::from(&local);
    let receiver = with_sftp(state, &tab_id, |handle| {
        handle.mcp_download(remote.clone(), local_path)
    })?;
    let bytes = receiver
        .await
        .map_err(|_| sftp_closed())?
        .map_err(McpError::TransferFailed)?;
    Ok(json!({ "tab_id": tab_id, "remote_path": remote, "local_path": local, "bytes": bytes }))
}

async fn sftp_upload(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let local = str_arg(args, "local_path")?;
    let remote = str_arg(args, "remote_path")?;
    let local_path = std::path::PathBuf::from(&local);
    let receiver =
        with_sftp(state, &tab_id, |handle| handle.mcp_upload(local_path, remote.clone()))?;
    let bytes = receiver
        .await
        .map_err(|_| sftp_closed())?
        .map_err(McpError::TransferFailed)?;
    Ok(json!({ "tab_id": tab_id, "local_path": local, "remote_path": remote, "bytes": bytes }))
}

fn tail(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

fn str_arg(args: &Value, key: &str) -> Result<String, McpError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| McpError::Invalid(format!("missing string argument `{key}`")))
}

fn opt_str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
}

/// 允许空串（`send_input` 只按回车时 `text` 就是空的）。
fn raw_str_arg(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn int_arg(args: &Value, key: &str, default: u64, min: u64, max: u64) -> u64 {
    args.get(key)
        .and_then(Value::as_u64)
        .unwrap_or(default)
        .clamp(min, max)
}

fn bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    //! 只读路径不依赖 UI 线程：直接准备一块终端缓冲就能验证。

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::super::types::{AppHandles, McpConfigSnapshot, McpState, Permissions};

    fn state_with_buffer() -> super::SharedState {
        let mut buffer = crate::terminal::TermBuffer::new(24, 80, 1000);
        buffer.ingest(b"hello from pty\r\n");
        let mut bufs = HashMap::new();
        bufs.insert("term-1".to_string(), Arc::new(Mutex::new(buffer)));
        Arc::new(McpState::new(
            McpConfigSnapshot {
                enabled: true,
                port: 8765,
                loopback: true,
                lan_enabled: false,
                lan_ifaces: Vec::new(),
                token: "rk_test".to_string(),
                perms: Permissions::default(),
            },
            AppHandles {
                bufs: Arc::new(Mutex::new(bufs)),
                tab_statuses: Arc::new(Mutex::new(HashMap::new())),
                local_snap: Arc::new(Mutex::new(Default::default())),
                sftp_handles: Arc::new(Mutex::new(HashMap::new())),
            },
        ))
    }

    #[tokio::test]
    async fn read_terminal_returns_screen_text() {
        let state = state_with_buffer();
        let value = super::call(&state, "read_terminal", &json!({ "tab_id": "term-1" }))
            .await
            .unwrap();
        let text = value["text"].as_str().unwrap();
        assert!(text.contains("hello from pty"), "{text}");
        assert_eq!(value["output_seq"], 16);
    }

    #[tokio::test]
    async fn unknown_tab_is_reported_as_invalid_argument() {
        let state = state_with_buffer();
        let err = super::call(&state, "read_terminal", &json!({ "tab_id": "nope" }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "invalid_argument");
    }

    #[tokio::test]
    async fn list_sessions_reports_ui_unavailable_without_a_window() {
        let state = state_with_buffer();
        let err = super::call(&state, "list_sessions", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "ui_unavailable");
    }

    /// 回归：静默模式曾经永远不命中 —— `seen_seq` 在检测到变化时就被推进，
    /// 再用它判断"有新输出"第二圈恒为假，于是一路等到超时。
    #[tokio::test]
    async fn wait_for_output_returns_once_output_goes_quiet() {
        let state = state_with_buffer();
        let value = super::call(
            &state,
            "wait_for_output",
            &json!({
                "tab_id": "term-1",
                "since_seq": 0,
                "quiet_ms": 50,
                "timeout_ms": 2000,
            }),
        )
        .await
        .unwrap();
        assert_eq!(value["matched"], true, "{value}");
        assert!(value.get("timed_out").is_none(), "{value}");
    }
}
