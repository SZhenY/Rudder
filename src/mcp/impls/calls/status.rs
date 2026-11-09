use super::*;

pub(crate) fn server_info(state: &SharedState) -> Value {
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
pub(crate) fn session_perms(state: &SharedState) -> crate::mcp::Permissions {
    crate::mcp::current_session()
        .and_then(|session_id| state.lease(&session_id))
        .map(|lease| lease.granted)
        .unwrap_or_default()
}

pub(crate) fn list_tabs(state: &SharedState) -> Value {
    // 锁内只拷标量：`tab_statuses` 由 UI 线程 1 Hz 写入，JSON 化（每个标签一份
    // Map + 字符串）不该占着这把锁。
    let mut rows: Vec<(String, String, String, String, &'static str, bool)> = {
        let statuses = state
            .app
            .tab_statuses
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        statuses
            .iter()
            .map(|(tab_id, status)| {
                (
                    tab_id.clone(),
                    status.host.clone(),
                    status.user.clone(),
                    status.session_id.clone(),
                    state_name(status.state),
                    status.is_local,
                )
            })
            .collect()
    };
    // 排序在锁外、按元组首项比，不再从 JSON 里往回取值。
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    let tabs: Vec<Value> = rows
        .into_iter()
        .map(|(tab_id, host, user, session_id, state, is_local)| {
            json!({
                "tab_id": tab_id,
                "host": host,
                "user": user,
                "session_id": session_id,
                "state": state,
                "is_local": is_local,
            })
        })
        .collect();
    json!({ "count": tabs.len(), "tabs": tabs })
}

/// 本机（运行 Rudder 的这台机器）的资源快照，不需要 `tab_id`。
///
/// 快照在 2 秒内就直接用 UI 采样器的结果（零成本）；否则用 MCP 自己持有的采样器现采。
/// 不能只读 `local_snap`：窗口隐藏时采样会整拍跳过（`sampler.rs`），
/// 读到的可能是几分钟前的数字。
pub(crate) async fn host_info(state: &SharedState) -> Result<Value, McpError> {
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
pub(crate) async fn sample_local(
    state: &SharedState,
) -> Result<crate::resource::SystemSnapshot, McpError> {
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
pub(crate) async fn kill_process(state: &SharedState, args: &Value) -> Result<Value, McpError> {
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

pub(crate) async fn tab_status(state: &SharedState, args: &Value) -> Result<Value, McpError> {
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
        // 与远端共用同一套陈旧判定：采样器在窗口隐藏时整拍跳过。
        let age_ms = local.sampled_at.map(|at| at.elapsed().as_millis() as u64);
        value["last_sample_ms_ago"] = json!(age_ms);
        value["stale"] = json!(age_ms.map(|ms| ms > 5_000).unwrap_or(true));
        return Ok(value);
    }

    let mut value = status_json(state, &tab_id);
    // `get_tab_status` 是只读工具：不再临时打开远端采样（那会写会话运行态，
    // 且恢复判断依赖已删除的侧栏折叠状态，可能永久留在开启）。数据陈旧就如实标记。
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
pub(crate) fn sys_json(sys: &crate::ssh::SystemDetails) -> Value {
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

pub(crate) fn status_json(state: &SharedState, tab_id: &str) -> Value {
    // 锁内只取一份副本：下面要生成 `procs` / `disks` / `sys` 几十个 Value，
    // 占着锁会把 UI 线程的 1 Hz 采样挡在后面。`TabStatus` 的 Clone 是几次
    // Vec/String 拷贝，代价远低于边持锁边分配。
    let status = {
        let statuses = state
            .app
            .tab_statuses
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        statuses.get(tab_id).cloned()
    };
    let Some(status) = status else {
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

pub(crate) fn pairs(rows: &[(String, String)]) -> Value {
    Value::Object(
        rows.iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect(),
    )
}
