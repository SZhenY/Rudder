use super::*;

/// 取该标签页的 SFTP 通道发一条命令。`SftpHandle` 不是 `Clone`，所以闭包里发完
/// 立刻释放表锁，再在锁外等结果 —— 不能把 UI 线程要用的表锁按住等传输。
pub(crate) async fn with_sftp<T>(
    state: &SharedState,
    tab_id: &str,
    send: impl FnOnce(&crate::sftp::SftpHandle) -> tokio::sync::oneshot::Receiver<T>,
) -> Result<tokio::sync::oneshot::Receiver<T>, McpError> {
    // 懒启动的会话此时还没有第二条 SSH 连接：先登记需求，再等通道出现（最长 2s）。
    crate::sftp::want_sftp(tab_id, None);
    let mut send = Some(send);
    for _ in 0..40 {
        {
            let handles = state
                .app
                .sftp_handles
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            if let Some(handle) = handles.get(tab_id)
                && let Some(send) = send.take()
            {
                return Ok(send(handle));
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(McpError::NotSupported(
        "SFTP channel is still connecting; retry shortly",
    ))
}

/// 列目录超时：目录很大时 30 秒仍不够属于异常，交给客户端重试。
pub(crate) const SFTP_LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// 上传/下载超时：与 `await_sftp_mutation` 的量级一致。
pub(crate) const SFTP_TRANSFER_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) fn sftp_closed() -> McpError {
    McpError::NotConnected("the SFTP channel closed before the request finished")
}

/// SFTP 变更类的共用尾部：等回执并套超时。
///
/// SFTP worker 是串行的，大传输在飞时这些命令要排队 —— 没有超时的话，一个卡住的
/// 请求会占着 1/8 的并发许可，直到 handler 的 310 秒上限。
pub(crate) async fn await_sftp_mutation(
    receiver: tokio::sync::oneshot::Receiver<Result<(), String>>,
) -> Result<(), McpError> {
    tokio::time::timeout(std::time::Duration::from_secs(30), receiver)
        .await
        .map_err(|_| McpError::TimedOut)?
        .map_err(|_| sftp_closed())?
        .map_err(McpError::TransferFailed)
}

/// 八进制权限位。抽成纯函数是为了能单测（`"999"` 这种必须在进 worker 前就拒掉）。
pub(crate) fn parse_mode(raw: &str) -> Result<u32, McpError> {
    let trimmed = raw.trim();
    u32::from_str_radix(trimmed, 8)
        .ok()
        // 3 或 4 位：`"75"` 虽然能解析成八进制 61，但不是一个有效的权限位写法。
        .filter(|mode| *mode <= 0o7777 && (3..=4).contains(&trimmed.len()))
        .ok_or_else(|| {
            McpError::Invalid("`mode` must be octal digits such as \"755\" or \"0644\"".to_string())
        })
}

pub(crate) async fn sftp_mkdir(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let path = str_arg(args, "path")?;
    let receiver = with_sftp(state, &tab_id, |handle| handle.mcp_mkdir(path.clone())).await?;
    await_sftp_mutation(receiver).await?;
    Ok(json!({ "tab_id": tab_id, "path": path, "created": true }))
}

pub(crate) async fn sftp_remove(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let path = str_arg(args, "path")?;
    let recursive = bool_arg(args, "recursive", false);
    let receiver = with_sftp(state, &tab_id, |handle| {
        handle.mcp_remove(path.clone(), recursive)
    })
    .await?;
    await_sftp_mutation(receiver).await?;
    Ok(json!({
        "tab_id": tab_id,
        "path": path,
        "recursive": recursive,
        "removed": true,
    }))
}

pub(crate) async fn sftp_rename(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let from = str_arg(args, "from")?;
    let to = str_arg(args, "to")?;
    let receiver = with_sftp(state, &tab_id, |handle| {
        handle.mcp_rename(from.clone(), to.clone())
    })
    .await?;
    await_sftp_mutation(receiver).await?;
    Ok(json!({ "tab_id": tab_id, "from": from, "to": to, "renamed": true }))
}

pub(crate) async fn sftp_chmod(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let path = str_arg(args, "path")?;
    let mode = parse_mode(&str_arg(args, "mode")?)?;
    let receiver = with_sftp(state, &tab_id, |handle| {
        handle.mcp_chmod(path.clone(), mode)
    })
    .await?;
    await_sftp_mutation(receiver).await?;
    Ok(json!({
        "tab_id": tab_id,
        "path": path,
        "mode": format!("{mode:o}"),
        "changed": true,
    }))
}

pub(crate) async fn sftp_list(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let path = opt_str_arg(args, "path").unwrap_or_else(|| "/".to_string());
    let receiver = with_sftp(state, &tab_id, |handle| handle.mcp_list(path.clone())).await?;
    // 与变更类一致：SFTP worker 是串行的，一个卡住不回的列目录会占着并发许可
    // 直到调用方的总超时。
    let mut entries = tokio::time::timeout(SFTP_LIST_TIMEOUT, receiver)
        .await
        .map_err(|_| McpError::TransferFailed("列目录超时（30 秒）".to_string()))?
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

pub(crate) async fn sftp_download(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let remote = str_arg(args, "remote_path")?;
    // 固定写到传输管理配置的下载目录；不再接受任意 local_path。
    let dir = crate::app::mcp_ui::download_dir().await?;
    let receiver = with_sftp(state, &tab_id, |handle| {
        handle.mcp_download_auto(remote.clone(), dir)
    })
    .await?;
    let (local_path, renamed, bytes) = tokio::time::timeout(SFTP_TRANSFER_TIMEOUT, receiver)
        .await
        .map_err(|_| McpError::TransferFailed("下载超时（300 秒）".to_string()))?
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

pub(crate) async fn sftp_upload(state: &SharedState, args: &Value) -> Result<Value, McpError> {
    let tab_id = str_arg(args, "tab_id")?;
    let local = str_arg(args, "local_path")?;
    let remote = str_arg(args, "remote_path")?;
    let local_path = std::path::PathBuf::from(&local);
    let receiver = with_sftp(state, &tab_id, |handle| {
        handle.mcp_upload(local_path, remote.clone())
    })
    .await?;
    let bytes = tokio::time::timeout(SFTP_TRANSFER_TIMEOUT, receiver)
        .await
        .map_err(|_| McpError::TransferFailed("上传超时（300 秒）".to_string()))?
        .map_err(|_| sftp_closed())?
        .map_err(McpError::TransferFailed)?;
    Ok(json!({ "tab_id": tab_id, "local_path": local, "remote_path": remote, "bytes": bytes }))
}
