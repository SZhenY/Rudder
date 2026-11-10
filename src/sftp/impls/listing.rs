use super::*;

pub(crate) async fn stage_remote_for_copy(
    sftp: &SftpSession,
    handle: &client::Handle<SftpClientHandler>,
    remote: &str,
    events: &UnboundedSender<SessionEvent>,
) -> Result<(PathBuf, PathBuf)> {
    let cleanup_root =
        std::env::temp_dir().join(format!("meatshell-remote-copy-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&cleanup_root)
        .await
        .with_context(|| format!("failed to create temp dir {}", cleanup_root.display()))?;

    let name = sanitize_filename(&base_name(remote));
    let local_path = cleanup_root.join(&name);
    let local_parent = cleanup_root.to_string_lossy().to_string();
    let is_dir = sftp
        .metadata(remote)
        .await
        .ok()
        .map(|m| (m.permissions.unwrap_or(0) & 0o170_000) == 0o040_000)
        .unwrap_or(false);
    let no_cancel = Arc::new(AtomicBool::new(false));
    let id = Uuid::new_v4().to_string();

    if is_dir {
        tokio::fs::create_dir_all(&local_path)
            .await
            .with_context(|| format!("failed to create temp dir {}", local_path.display()))?;
        let empty = list_dir_impl(sftp, remote)
            .await
            .map(|entries| entries.is_empty())
            .unwrap_or(false);
        if !empty {
            // Editor fetch: not exposed in the transfer panel, so a throwaway
            // never-set cancel flag is fine here.
            let editor_cancel = Arc::new(AtomicBool::new(false));
            download_dir(
                sftp,
                handle,
                remote,
                &local_parent,
                events,
                &Uuid::new_v4().to_string(),
                &editor_cancel,
            )
            .await?;
        }
    } else {
        let local = local_path.to_string_lossy().to_string();
        download_impl(handle, remote, &local, &name, &id, events, &no_cancel).await?;
    }

    Ok((local_path, cleanup_root))
}

/// A friendlier message for a failed directory listing, calling out the common
/// permission-denied case explicitly rather than dumping the raw error (#112).
pub(crate) fn list_error_msg(path: &str, e: &impl std::fmt::Display) -> String {
    let raw = e.to_string();
    let low = raw.to_lowercase();
    if low.contains("permission") || low.contains("denied") {
        format!("{}: {}", t("权限不足,无法访问", "Permission denied"), path)
    } else {
        format!("{} {}: {}", t("无法访问", "Cannot open"), path, raw)
    }
}

/// List `path` for the file panel: emit the "Loading…" status, then either the
/// entries + the final status, or a friendly error message.
///
/// `SftpCommand::ListDir`（点开目录）与 `SftpCommand::RefreshDir`（手动刷新）在面板
/// 这一段逐字相同 —— 收在一处，免得两边平行维护。唯一的差别是 `path` 的所有权：
/// 刷新那条之后还要拿 `path` 去重建目录树，所以调用处各自 clone 即可。
pub(crate) async fn emit_panel_dir(
    sftp: &SftpSession,
    events: &UnboundedSender<SessionEvent>,
    path: &str,
) {
    let _ = events.send(SessionEvent::SftpStatus(format!(
        "{} {}...",
        t("加载", "Loading"),
        path
    )));
    match list_dir_impl(sftp, path).await {
        Ok(entries) => {
            let _ = events.send(SessionEvent::SftpEntries {
                path: path.to_string(),
                entries,
            });
            let _ = events.send(SessionEvent::SftpStatus(path.to_string()));
        }
        Err(e) => {
            let _ = events.send(SessionEvent::SftpError(list_error_msg(path, &e)));
        }
    }
}

pub(crate) async fn list_dir_impl(sftp: &SftpSession, path: &str) -> Result<Vec<RemoteEntry>> {
    let raw = sftp
        .read_dir(path)
        .await
        .with_context(|| format!("read_dir {path} failed"))?;

    // 目录条数上限（B1.4）：10 万文件的目录会一次性建出 8 MB+ 的条目表，而且树视图
    // 那份（`list_dirs_only_impl`）是**另一次**同样量级的构建。这里截断到 5000 条
    // ——列表随后按"目录优先 + 名称"排序，因此截断的是**未排序的前 5000 条**。
    // `ReadDir` 是迭代器（没有 len），所以多取一条来判定"是否被截断"。
    const MAX_SFTP_ENTRIES: usize = 5_000;

    let mut entries: Vec<RemoteEntry> = raw
        .into_iter()
        .filter(|e| {
            let n = e.file_name();
            n != "." && n != ".."
        })
        .take(MAX_SFTP_ENTRIES + 1)
        .map(|e| {
            let name = e.file_name().to_string();
            let full_path = format!("{}/{}", path.trim_end_matches('/'), name);
            let meta = e.metadata();
            // Determine if entry is a directory via Unix permission bits.
            let permissions = meta.permissions.unwrap_or(0);
            let is_dir = (permissions & 0o170_000) == 0o040_000;
            let size = meta.size.unwrap_or(0);
            let modified = meta.mtime.unwrap_or(0);
            RemoteEntry {
                name,
                full_path,
                is_dir,
                size,
                modified,
                mode: permissions & 0o7777,
                // 类型位也带上：SFTP 列表的「类型 / 权限」两列要用（上游 ed7f231）。
                mode_full: permissions & 0o177_777,
            }
        })
        .collect();

    if entries.len() > MAX_SFTP_ENTRIES {
        entries.truncate(MAX_SFTP_ENTRIES);
        tracing::warn!(path, kept = MAX_SFTP_ENTRIES, "SFTP listing truncated");
    }

    // Sort: directories first, then files; both groups alphabetically.
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    Ok(entries)
}

/// List only the subdirectories of `path` (no files). Used to build the tree.
pub(crate) async fn list_dirs_only_impl(
    sftp: &SftpSession,
    path: &str,
) -> Result<Vec<(String, String)>> {
    let entries = list_dir_impl(sftp, path).await?;
    Ok(entries
        .into_iter()
        .filter(|e| e.is_dir)
        .map(|e| (e.name, e.full_path))
        .collect())
}

/// Bundled arguments for `emit_transfer` (clippy::too_many_arguments).
pub(crate) struct TransferEmit<'a> {
    pub(crate) events: &'a UnboundedSender<SessionEvent>,
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) is_upload: bool,
    pub(crate) transferred: u64,
    pub(crate) total: u64,
    pub(crate) state: u8,
    pub(crate) msg: &'a str,
}

/// Emit a transfer-progress event.
pub(crate) fn emit_transfer(t: TransferEmit) {
    let TransferEmit {
        events,
        id,
        name,
        is_upload,
        transferred,
        total,
        state,
        msg,
    } = t;
    let _ = events.send(SessionEvent::SftpTransfer {
        id: id.to_string(),
        name: name.to_string(),
        is_upload,
        transferred,
        total,
        state,
        msg: msg.to_string(),
    });
}
