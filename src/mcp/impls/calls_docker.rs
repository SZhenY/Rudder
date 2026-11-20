//! Docker MCP 工具：参数 → `crate::docker` 命令 → 现有 SSH exec → 解析。
//!
//! 命令与解析全部复用 UI 控制器同一份 `crate::docker`，这里只做 MCP 参数校验与
//! host/tab 解析，避免两套实现漂移。

use std::time::Duration;

use serde_json::{Value, json};

use super::args::{bool_arg, int_arg, opt_str_arg, str_arg};
use super::text::truncate_chars;
use super::types::{McpError, SharedState};
use crate::config::SessionKind;
use crate::docker::classify::classify as classify_docker;
use crate::docker::{command, parse, DockerHostKey};

const READ_TIMEOUT: Duration = Duration::from_secs(20);

pub(super) async fn call(state: &SharedState, name: &str, args: &Value) -> Result<Value, McpError> {
    let (tab_id, host) = resolve_session(state, args)?;
    match name {
        "docker_list_containers" => {
            let all = bool_arg(args, "all", true);
            let res = exec(state, &tab_id, command::ps(all), READ_TIMEOUT, 512 * 1024).await?;
            let parsed = parse::parse_ps(&res.stdout);
            let running = parsed
                .rows
                .iter()
                .filter(|c| c.state == crate::docker::ContainerState::Running)
                .count();
            Ok(json!({
                "host": host,
                "tab_id": tab_id,
                "total": parsed.rows.len(),
                "running": running,
                "bad_lines": parsed.bad_lines,
                "containers": parsed.rows.iter().map(|c| json!({
                    "id": c.id,
                    "name": c.name,
                    "image": c.image,
                    "state": c.state.key(),
                    "status": c.status,
                    "ports": c.ports,
                    "created": c.created,
                })).collect::<Vec<_>>(),
            }))
        }
        "docker_container_logs" => {
            let container = str_arg(args, "container_id")?;
            let tail = int_arg(args, "tail", 200, 1, 2000) as u32;
            let timestamps = bool_arg(args, "timestamps", false);
            let cmd = command::logs(&container, tail, timestamps)
                .map_err(|_| McpError::Invalid("invalid container_id".into()))?;
            let res = exec(state, &tab_id, cmd, READ_TIMEOUT, 512 * 1024).await?;
            let (lines, truncated) = parse::parse_logs(&res.stdout, &res.stderr, tail as usize);
            Ok(json!({
                "host": host,
                "tab_id": tab_id,
                "container_id": container,
                "lines": lines.iter().map(|l| truncate_chars(l.clone(), 2000).0).collect::<Vec<_>>(),
                "truncated": truncated,
            }))
        }
        _ => {
            let container = str_arg(args, "container_id")?;
            let timeout_ms = int_arg(args, "timeout_ms", 30_000, 5_000, 120_000);
            let force = bool_arg(args, "force", false);
            let (cmd, action) = match name {
                "docker_start_container" => (
                    command::start(&container).map_err(|_| McpError::Invalid("invalid container_id".into()))?,
                    "start",
                ),
                "docker_stop_container" => (
                    command::stop(&container).map_err(|_| McpError::Invalid("invalid container_id".into()))?,
                    "stop",
                ),
                "docker_restart_container" => (
                    command::restart(&container).map_err(|_| McpError::Invalid("invalid container_id".into()))?,
                    "restart",
                ),
                "docker_remove_container" if force => (
                    command::remove(&container, true).map_err(|_| McpError::Invalid("invalid container_id".into()))?,
                    "remove",
                ),
                "docker_remove_container" => (
                    command::remove_sequence(&container).map_err(|_| McpError::Invalid("invalid container_id".into()))?,
                    "remove",
                ),
                _ => return Err(McpError::Invalid(format!("unknown tool: {name}"))),
            };
            let res = exec(
                state,
                &tab_id,
                cmd,
                Duration::from_millis(timeout_ms),
                64 * 1024,
            )
            .await?;
            let ok = !res.timed_out && res.error.is_none() && res.exit_code.unwrap_or(1) == 0;
            if !ok {
                return Err(McpError::TransferFailed(format!(
                    "docker {action} failed: {}",
                    if res.stderr.trim().is_empty() {
                        format!("exit code {:?}", res.exit_code)
                    } else {
                        truncate_chars(res.stderr.trim().to_string(), 400).0
                    }
                )));
            }
            Ok(json!({
                "host": host,
                "tab_id": tab_id,
                "container_id": container,
                "action": action,
                "ok": true,
            }))
        }
    }
}

fn resolve_session(state: &SharedState, args: &Value) -> Result<(String, String), McpError> {
    let statuses = state
        .app
        .tab_statuses
        .lock()
        .map_err(|_| McpError::UiUnavailable)?;

    if let Some(tab_id) = opt_str_arg(args, "tab_id") {
        let st = statuses
            .get(&tab_id)
            .ok_or_else(|| McpError::Invalid(format!("unknown tab_id: {tab_id}")))?;
        if st.kind != SessionKind::Ssh || st.is_local {
            return Err(McpError::NotSupported("Docker requires an SSH session"));
        }
        if st.state != 1 {
            return Err(McpError::NotConnected("the SSH session is not connected"));
        }
        let host = DockerHostKey::parse(&st.host, &st.user)
            .ok_or_else(|| McpError::Invalid("session has no SSH host".into()))?;
        return Ok((tab_id, host.as_str().to_string()));
    }

    let host = str_arg(args, "host")?;
    let key = DockerHostKey::parse(&host, "")
        .ok_or_else(|| McpError::Invalid(format!("invalid host: {host}")))?;
    let mut available = Vec::new();
    let mut candidate = None;
    for (tab_id, st) in statuses.iter() {
        if st.kind != SessionKind::Ssh || st.is_local {
            continue;
        }
        let Some(k) = DockerHostKey::parse(&st.host, &st.user) else {
            continue;
        };
        if k.as_str() == key.as_str() {
            if st.state == 1 {
                candidate = Some(tab_id.clone());
                break;
            }
        } else if st.state == 1 {
            available.push(k.as_str().to_string());
        }
    }
    let tab_id = candidate.ok_or_else(|| {
        if available.is_empty() {
            McpError::NotConnected("no live SSH host matches `host`")
        } else {
            McpError::Invalid(format!(
                "no live SSH host matches `{host}`; available: {}",
                available.join(", ")
            ))
        }
    })?;
    Ok((tab_id, key.as_str().to_string()))
}

async fn exec(
    _state: &SharedState,
    tab_id: &str,
    command: String,
    timeout: Duration,
    max_output: usize,
) -> Result<crate::ssh::ExecResult, McpError> {
    let rx = crate::app::mcp_ui::ssh_exec(tab_id.to_string(), command, timeout, max_output).await?;
    let res = tokio::time::timeout(timeout + Duration::from_secs(2), rx)
        .await
        .map_err(|_| McpError::TimedOut)?
        .map_err(|_| McpError::NotConnected("the SSH session closed"))?;
    if res.timed_out {
        return Err(McpError::TimedOut);
    }
    if res.error.is_some() {
        return Err(McpError::NotSupported("docker exec failed"));
    }
    if res.exit_code.unwrap_or(1) != 0 {
        let failure = classify_docker(&res.stderr, res.exit_code, false);
        return Err(match failure {
            crate::docker::DockerFailure::NoPermission => {
                McpError::Invalid("no Docker permission on the remote host".into())
            }
            crate::docker::DockerFailure::NoCli => {
                McpError::Invalid("docker CLI is not installed on the remote host".into())
            }
            crate::docker::DockerFailure::DaemonDown => {
                McpError::NotConnected("remote Docker daemon is not reachable")
            }
            _ => McpError::TransferFailed(truncate_chars(res.stderr.trim().to_string(), 400).0),
        });
    }
    Ok(res)
}
