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

use super::pty_cmd;
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
        "get_host_info" => host_info(state).await,
        "kill_process" => kill_process(state, args).await,
        "get_update_status" => crate::app::mcp_ui::update_status().await,
        "sftp_mkdir" => sftp_mkdir(state, args).await,
        "sftp_remove" => sftp_remove(state, args).await,
        "sftp_rename" => sftp_rename(state, args).await,
        "sftp_chmod" => sftp_chmod(state, args).await,
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
            crate::app::mcp_ui::get_settings(
                keys,
                session_perms(state).allows("settings_high"),
            )
            .await
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
            crate::app::mcp_ui::set_settings(
                values,
                session_perms(state).allows("settings_high"),
            )
            .await
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
        "run_action" => crate::app::mcp_ui::run_action(str_arg(args, "action")?).await,
        other => Err(McpError::Invalid(format!("unknown tool: {other}"))),
    }
}

fn server_info(state: &SharedState) -> Value {
    let status = state.status();
    let granted = session_perms(state);
    let cfg = state.config();
    json!({
        "name": "rudder",
        "version": env!("CARGO_PKG_VERSION"),
        "running": status.running,
        "endpoints": status.urls(),
        // 权限只在会话上；`session` 与 `effective` 现在同义，保留两个键是为了
        // 老客户端不用改解析。
        "permissions": {
            "session": granted.describe(),
            "effective": granted.describe(),
        },
        "permissions_rev": crate::mcp::PERMISSIONS_REV,
        "consent": match cfg.consent {
            crate::mcp::ConsentPolicy::Session => "session",
            crate::mcp::ConsentPolicy::Auto => "auto",
        },
        "limits": {
            "max_concurrent_calls": 8,
            "max_request_bytes": 1024 * 1024,
        },
        "error": status.error,
    })
}

/// 当前调用的会话授权。供 `get_server_info` 与设置键脱敏使用。
fn session_perms(state: &SharedState) -> crate::mcp::Permissions {
    crate::mcp::current_session()
        .and_then(|session_id| state.lease(&session_id))
        .map(|lease| lease.granted)
        .unwrap_or_default()
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

/// 本机（运行 Rudder 的这台机器）的资源快照，不需要 `tab_id`。
///
/// 快照在 2 秒内就直接用 UI 采样器的结果（零成本）；否则用 MCP 自己持有的采样器现采。
/// 不能只读 `local_snap`：侧栏折叠 / 窗口隐藏时采样会整拍跳过（`sampler.rs`），
/// 读到的可能是几分钟前的数字。
async fn host_info(state: &SharedState) -> Result<Value, McpError> {
    let fresh = {
        let snap = state
            .app
            .local_snap
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        snap.sampled_at
            .map(|at| at.elapsed() < std::time::Duration::from_secs(2))
            .unwrap_or(false)
            .then_some(snap)
    };
    let (snapshot, source) = match fresh {
        Some(snapshot) => (snapshot, "snapshot"),
        None => (sample_local(state).await?, "on_demand"),
    };
    Ok(json!({
        "scope": "local",
        "source": source,
        "sampled_ms_ago": snapshot.sampled_at.map(|at| at.elapsed().as_millis() as u64),
        "cpu_percent": snapshot.cpu_percent,
        "memory": {
            "used_mib": snapshot.mem_used_mib,
            "total_mib": snapshot.mem_total_mib,
            "percent": snapshot.mem_percent,
        },
        "swap": {
            "used_mib": snapshot.swap_used_mib,
            "total_mib": snapshot.swap_total_mib,
            "percent": snapshot.swap_percent,
        },
        "network": {
            "rx_bytes_per_sec": snapshot.net_rx_per_sec,
            "tx_bytes_per_sec": snapshot.net_tx_per_sec,
        },
        "filesystems": snapshot
            .disks
            .iter()
            .map(|(mount, available, total)| json!({
                "mount": mount,
                "available_bytes": available,
                "total_bytes": total,
            }))
            .collect::<Vec<_>>(),
    }))
}

/// 现场采一次。
///
/// 放 `spawn_blocking`：磁盘枚举在 macOS 上约 27 ms（见 `DISK_REFRESH_EVERY` 的注释），
/// 占着 tokio worker 会拖慢同一 runtime 上的其它请求。
async fn sample_local(state: &SharedState) -> Result<crate::resource::SystemSnapshot, McpError> {
    let sampler = state.local_sampler.clone();
    tokio::task::spawn_blocking(move || {
        let mut guard = sampler.lock().unwrap_or_else(|err| err.into_inner());
        let first = guard.is_none();
        if first {
            *guard = Some(crate::resource::SystemSampler::new());
            // 新建的 System 第一次 `refresh_cpu_usage` 没有基线，CPU 必为 0 —— 等一拍再采。
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        guard.as_mut().map(crate::resource::SystemSampler::sample)
    })
    .await
    .map_err(|err| McpError::Internal(format!("local sampler failed: {err}")))?
    .ok_or_else(|| McpError::Internal("local sampler unavailable".to_string()))
}

/// 结束远端进程。
///
/// **失败也返回 `Ok`**：`ProcessKillResult.message` 是远端原始输出，sudo 可能把口令
/// 回显在里面；走 `Err` 会被 `handler` 原样写进审计的 `error` 字段，等于把口令落盘。
async fn kill_process(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    // 不用 `int_arg`：它会把缺失的 pid clamp 成 1，那就变成"杀 init"了。
    let pid = args
        .get("pid")
        .and_then(Value::as_u64)
        .filter(|pid| *pid > 0 && *pid <= u32::MAX as u64)
        .ok_or_else(|| McpError::Invalid("missing or invalid `pid`".to_string()))?
        as u32;
    let root_password = opt_str_arg(args, "root_password");

    // 只有 SSH 标签有远端进程控制。
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
    if kind != crate::config::SessionKind::Ssh {
        return Err(McpError::NotSupported(
            "only SSH tabs can kill a remote process; use send_input for local, telnet and serial tabs",
        ));
    }

    let receiver = crate::app::mcp_ui::ssh_kill_process(tab_id.clone(), pid, root_password).await?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), receiver)
        .await
        .map_err(|_| McpError::TimedOut)?
        .map_err(|_| McpError::NotConnected("the session closed before the kill finished"))?;
    Ok(json!({
        "tab_id": tab_id,
        "pid": pid,
        "success": result.success,
        "message": result.message,
    }))
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
            "swap_percent": local.swap_percent,
            "swap_used_mib": local.swap_used_mib,
            "swap_total_mib": local.swap_total_mib,
            "net_rx_per_sec": local.net_rx_per_sec,
            "net_tx_per_sec": local.net_tx_per_sec,
            // 本机磁盘之前被整段丢掉，其实快照里一直有。
            "disks": local
                .disks
                .iter()
                .map(|(mount, avail, total)| {
                    json!({ "mount": mount, "avail": avail, "total": total })
                })
                .collect::<Vec<_>>(),
        });
        // 与远端共用同一套陈旧判定：采样器在侧栏折叠 / 窗口隐藏时整拍跳过。
        let age_ms = local.sampled_at.map(|at| at.elapsed().as_millis() as u64);
        value["last_sample_ms_ago"] = json!(age_ms);
        value["stale"] = json!(age_ms.map(|ms| ms > 5_000).unwrap_or(true));
        return Ok(value);
    }

    let mut value = status_json(state, &tab_id);
    // `get_tab_status` 是只读工具：不再临时打开远端采样（那会写会话运行态，
    // 且恢复判断依赖"侧栏是否折叠"，可能永久留在开启）。数据陈旧就如实标记。
    let stale = value
        .get("last_sample_ms_ago")
        .and_then(Value::as_u64)
        .map(|ms| ms > 5_000)
        .unwrap_or(true);
    value["stale"] = json!(stale);
    if bool_arg(args, "refresh", false) {
        value["refresh_ignored"] = json!(true);
        value["hint"] = json!("open the resource panel in Rudder to get fresh samples");
    }
    Ok(value)
}

/// `SystemDetails` 的 7 节。
///
/// 6 个 label/value 节输出成对象（沿用既有的 `pairs`），两个 5 元组输出成具名数组：
/// `networks` 的第 4/5 位、`filesystems` 的第 5 位是采集脚本里的占位符，不输出。
/// 全空时返回 `null` —— 给 6 个空对象会让调用方以为"这台机器没有内存"。
fn sys_json(sys: &crate::ssh::SystemDetails) -> Value {
    let mut out = serde_json::Map::new();
    for (key, rows) in [
        ("overview", &sys.overview),
        ("cpu_info", &sys.cpu_info),
        ("gpu_info", &sys.gpu_info),
        ("cpu_usage", &sys.cpu_usage),
        ("memory", &sys.memory),
        ("swap", &sys.swap),
    ] {
        if !rows.is_empty() {
            out.insert(key.to_string(), pairs(rows));
        }
    }
    if !sys.networks.is_empty() {
        out.insert(
            "networks".to_string(),
            Value::Array(
                sys.networks
                    .iter()
                    // 元组是 (iface, tx, rx, "-", "-")。
                    .map(|(iface, tx, rx, _, _)| {
                        json!({ "iface": iface, "tx_total": tx, "rx_total": rx })
                    })
                    .collect(),
            ),
        );
    }
    if !sys.filesystems.is_empty() {
        out.insert(
            "filesystems".to_string(),
            Value::Array(
                sys.filesystems
                    .iter()
                    // 元组是 (mount, total, used_percent, avail, mount重复)。
                    .map(|(mount, total, used_percent, available, _)| {
                        json!({
                            "mount": mount,
                            "total": total,
                            "used_percent": used_percent,
                            "available": available,
                        })
                    })
                    .collect(),
            ),
        );
    }
    if out.is_empty() {
        Value::Null
    } else {
        Value::Object(out)
    }
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
        "sys": sys_json(&status.sys),
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

    let mut first = true;
    loop {
        // 先只读序号：文本没变时 regex 的结果也不会变，没必要生成整屏文本。
        let changed = output_seq_of(state, &tab_id)? != seen_seq;
        if first || changed {
            first = false;
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
        } else {
            // 没有新输出：静默模式等够 quiet 就返回，其余情况只检查超时（都不需要文本，
            // 真要返回时才取一次）。
            let quiet = regex.is_none()
                && saw_output
                && last_change.elapsed() >= Duration::from_millis(quiet_ms);
            if quiet || Instant::now() >= deadline {
                let snapshot = snapshot(state, &tab_id, max_rows, true)?;
                let mut out = json!({
                    "matched": quiet,
                    "text": tail(&snapshot.text, 4096),
                    "output_seq": snapshot.output_seq,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                });
                // `timed_out` 只在真的超时出现：静默命中时带上它会让调用方误判成失败。
                if !quiet {
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
fn output_seq_of(state: &SharedState, tab_id: &str) -> Result<u64, McpError> {
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
    Ok(buffer.output_seq())
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
            let opened =
                crate::app::mcp_ui::open_session(session_id.clone(), false).await?;
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
        return pty_run_command(state, tab_id, kind, &host, command, timeout, max_output_bytes)
            .await;
    }

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

/// 这个标签页当前是否处于 alt-screen（vim / htop / less 这类全屏程序）。
fn is_alt_screen(state: &SharedState, tab_id: &str) -> bool {
    let handle = {
        let bufs = state
            .app
            .bufs
            .lock()
            .unwrap_or_else(|err| err.into_inner());
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
async fn wait_tab_ready(
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

/// 终端里那条 shell 的家族 —— 决定 marker 语法。
///
/// 判据跟着 `terminal/impls/local.rs` 的 `local_program`：`host == "cmd"` 才起
/// cmd.exe，`"wsl"` 起的是发行版里的 POSIX 登录 shell，其余 Windows 本地标签是
/// PowerShell；SSH / Telnet / 串口一律按 POSIX 处理。
fn shell_family(kind: SessionKind, host: &str) -> pty_cmd::ShellFamily {
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
async fn pty_run_command(
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
                let (text, truncated) = clamp_chars(out.text, max_output_bytes);
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
                "stdout": tail(&snap.text, max_output_bytes.min(4096)),
                "stderr": "",
                "truncated": true,
                "timed_out": true,
                "duration_ms": started.elapsed().as_millis() as u64,
            }));
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
}

/// 按字符数截断（字节下标切多字节字符会 panic）。
fn clamp_chars(text: String, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    (text.chars().take(max_bytes).collect(), true)
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

/// SFTP 变更类的共用尾部：等回执并套超时。
///
/// SFTP worker 是串行的，大传输在飞时这些命令要排队 —— 没有超时的话，一个卡住的
/// 请求会占着 1/8 的并发许可，直到 handler 的 310 秒上限。
async fn await_sftp_mutation(
    receiver: tokio::sync::oneshot::Receiver<Result<(), String>>,
) -> Result<(), McpError> {
    tokio::time::timeout(std::time::Duration::from_secs(30), receiver)
        .await
        .map_err(|_| McpError::TimedOut)?
        .map_err(|_| sftp_closed())?
        .map_err(McpError::TransferFailed)
}

/// 八进制权限位。抽成纯函数是为了能单测（`"999"` 这种必须在进 worker 前就拒掉）。
fn parse_mode(raw: &str) -> Result<u32, McpError> {
    let trimmed = raw.trim();
    u32::from_str_radix(trimmed, 8)
        .ok()
        // 3 或 4 位：`"75"` 虽然能解析成八进制 61，但不是一个有效的权限位写法。
        .filter(|mode| *mode <= 0o7777 && (3..=4).contains(&trimmed.len()))
        .ok_or_else(|| {
            McpError::Invalid("`mode` must be octal digits such as \"755\" or \"0644\"".to_string())
        })
}

async fn sftp_mkdir(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let path = str_arg(args, "path")?;
    let receiver = with_sftp(state, &tab_id, |handle| handle.mcp_mkdir(path.clone()))?;
    await_sftp_mutation(receiver).await?;
    Ok(json!({ "tab_id": tab_id, "path": path, "created": true }))
}

async fn sftp_remove(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let path = str_arg(args, "path")?;
    let recursive = bool_arg(args, "recursive", false);
    let receiver = with_sftp(state, &tab_id, |handle| {
        handle.mcp_remove(path.clone(), recursive)
    })?;
    await_sftp_mutation(receiver).await?;
    Ok(json!({
        "tab_id": tab_id,
        "path": path,
        "recursive": recursive,
        "removed": true,
    }))
}

async fn sftp_rename(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let from = str_arg(args, "from")?;
    let to = str_arg(args, "to")?;
    let receiver = with_sftp(state, &tab_id, |handle| {
        handle.mcp_rename(from.clone(), to.clone())
    })?;
    await_sftp_mutation(receiver).await?;
    Ok(json!({ "tab_id": tab_id, "from": from, "to": to, "renamed": true }))
}

async fn sftp_chmod(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let path = str_arg(args, "path")?;
    let mode = parse_mode(&str_arg(args, "mode")?)?;
    let receiver = with_sftp(state, &tab_id, |handle| handle.mcp_chmod(path.clone(), mode))?;
    await_sftp_mutation(receiver).await?;
    Ok(json!({
        "tab_id": tab_id,
        "path": path,
        "mode": format!("{mode:o}"),
        "changed": true,
    }))
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
    // 固定写到传输管理配置的下载目录；不再接受任意 local_path。
    let dir = crate::app::mcp_ui::download_dir().await?;
    let receiver =
        with_sftp(state, &tab_id, |handle| handle.mcp_download_auto(remote.clone(), dir))?;
    let (local_path, renamed, bytes) = receiver
        .await
        .map_err(|_| sftp_closed())?
        .map_err(McpError::TransferFailed)?;
    Ok(json!({
        "tab_id": tab_id,
        "remote_path": remote,
        "local_path": local_path.to_string_lossy(),
        "renamed": renamed,
        "bytes": bytes,
    }))
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

    use super::super::types::{AppHandles, McpConfigSnapshot, McpState};

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
                client_grants: Vec::new(),
                consent: crate::mcp::ConsentPolicy::Auto,
                pty_commands: false,
                audit: super::super::types::AuditConfig::default(),
            },
            AppHandles {
                bufs: Arc::new(Mutex::new(bufs)),
                tab_statuses: Arc::new(Mutex::new(HashMap::new())),
                local_snap: Arc::new(Mutex::new(Default::default())),
                sftp_handles: Arc::new(Mutex::new(HashMap::new())),
            },
            tokio::runtime::Handle::current(),
        ))
    }

    /// 快照新鲜时直接复用 UI 采样器的结果，不额外采样。
    #[tokio::test]
    async fn host_info_reuses_a_fresh_snapshot() {
        let state = state_with_buffer();
        {
            let mut snap = state
                .app
                .local_snap
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            snap.cpu_percent = 0.25;
            snap.mem_total_mib = 16384;
            snap.sampled_at = Some(std::time::Instant::now());
        }
        let value = super::call(&state, "get_host_info", &json!({}))
            .await
            .unwrap();
        assert_eq!(value["scope"], "local");
        assert_eq!(value["source"], "snapshot");
        assert_eq!(value["memory"]["total_mib"], 16384);
    }

    /// 快照陈旧（侧栏折叠时采样器会整拍跳过）→ 现场采一次，数字必须是真的。
    #[tokio::test]
    async fn host_info_samples_on_demand_when_the_snapshot_is_stale() {
        let state = state_with_buffer();
        {
            let mut snap = state
                .app
                .local_snap
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            snap.sampled_at = None;
        }
        // 首次采样要预热 200ms（新建的 System 没有 CPU 基线）。
        let value = super::call(&state, "get_host_info", &json!({}))
            .await
            .unwrap();
        assert_eq!(value["source"], "on_demand");
        assert!(
            value["memory"]["total_mib"].as_u64().unwrap_or(0) > 0,
            "本机总内存不该是 0"
        );
        assert!(value["filesystems"].is_array());
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

#[cfg(test)]
mod sys_json_tests {
    use super::sys_json;
    use crate::ssh::SystemDetails;

    /// 全空时给 `null`，不给 6 个空对象 —— 否则调用方会以为"这台机器没有内存"。
    #[test]
    fn empty_details_serialize_to_null() {
        assert!(sys_json(&SystemDetails::default()).is_null());
    }

    /// 两个 5 元组节要输出成具名对象，并丢掉采集脚本的占位列。
    #[test]
    fn tuple_sections_drop_placeholder_columns() {
        let sys = SystemDetails {
            overview: vec![("系统".to_string(), "Linux".to_string())],
            networks: vec![(
                "eth0".to_string(),
                "1 GB".to_string(),
                "2 GB".to_string(),
                "-".to_string(),
                "-".to_string(),
            )],
            filesystems: vec![(
                "/".to_string(),
                "120G".to_string(),
                "61%".to_string(),
                "46G".to_string(),
                "/".to_string(),
            )],
            ..Default::default()
        };

        let value = sys_json(&sys);
        assert_eq!(value["overview"]["系统"], "Linux");
        let net = value["networks"][0].as_object().expect("networks 是对象");
        assert_eq!(net.len(), 3, "占位列不该输出：{net:?}");
        assert_eq!(net["iface"], "eth0");
        let fs = value["filesystems"][0]
            .as_object()
            .expect("filesystems 是对象");
        assert_eq!(fs.len(), 4, "重复的 mount 列不该输出：{fs:?}");
        assert_eq!(fs["used_percent"], "61%");
    }

    /// 只有部分节有值时，缺的节不出现在对象里（而不是给空对象）。
    #[test]
    fn missing_sections_are_omitted() {
        let sys = SystemDetails {
            memory: vec![("总计".to_string(), "16 GB".to_string())],
            ..Default::default()
        };
        let value = sys_json(&sys);
        assert!(value.get("overview").is_none());
        assert!(value.get("networks").is_none());
        assert_eq!(value["memory"]["总计"], "16 GB");
    }
}

#[cfg(test)]
mod mode_tests {
    use super::parse_mode;

    /// 八进制是唯一接受的写法：`"755"` 与 `"0755"` 等价，`"999"` 这种必须在进 worker 前拒掉。
    #[test]
    fn mode_is_parsed_as_octal() {
        assert_eq!(parse_mode("755").unwrap(), 0o755);
        assert_eq!(parse_mode("0755").unwrap(), 0o755);
        assert_eq!(parse_mode("0644").unwrap(), 0o644);
        assert_eq!(parse_mode("  777 ").unwrap(), 0o777);
    }

    #[test]
    fn bad_modes_are_rejected() {
        for raw in ["999", "75", "", "abc", "8888", "-1"] {
            assert!(parse_mode(raw).is_err(), "{raw} 应该被拒");
        }
    }
}
