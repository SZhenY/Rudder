use super::*;

/// Make a remote-supplied file name safe to use as a *local* file name (for
/// both downloads and temp files): drops path separators (defence-in-depth
/// against traversal), replaces characters invalid on Windows or special to
/// shells with `_`, trims surrounding whitespace and Windows' trailing dots,
/// and neutralises reserved device names (CON, NUL, COM1…).  Normal names
/// (letters, digits, `.`, `-`, `_`, Unicode) pass through; Unix dotfiles keep
/// their leading dot.  Falls back to `file` when nothing usable remains.
pub(crate) fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' | '&' | '^' | '%' | '!' | '`'
            | '$' | '\'' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    // Drop leading whitespace and trailing dots/spaces (Windows strips the
    // latter silently). A leading dot is preserved so `.bashrc` survives.
    let trimmed = cleaned.trim_start_matches(' ').trim_end_matches([' ', '.']);
    if trimmed.is_empty() {
        return "file".to_string();
    }
    // Windows reserved device names are reserved case-insensitively and even
    // with an extension ("CON.txt" still opens the console). A download named
    // after one could read/write a device instead of a file, so prefix `_`.
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    let reserved = matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );
    if reserved {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// The local path a download would write to, applying the same sanitising the
/// transfer itself uses (#26). Shared with the UI thread so the conflict prompt
/// can check for an existing file *before* the transfer starts.
pub(crate) fn download_target_path(remote: &str, local_dir: &str) -> PathBuf {
    let filename = sanitize_filename(&base_name(remote));
    PathBuf::from(format!("{}/{}", local_dir.trim_end_matches('/'), filename))
}

/// First free `name (n).ext` sibling of `requested`, for "keep both" downloads.
/// Falls back to a bare `name (n)` when there is no extension.
pub(crate) fn available_download_path(requested: &std::path::Path) -> PathBuf {
    let parent = requested
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = requested
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    let extension = requested.extension().and_then(|value| value.to_str());
    // Bounded on purpose: an unbounded `for suffix in 1..` could only terminate
    // via unreachable!(), which aborts the whole process under panic="abort"
    // (Cargo.toml [profile.release]). A directory holding this many colliding
    // names is absurd but not impossible (a runaway script), so fall back to a
    // timestamped name rather than crashing the transfer — and the client.
    for suffix in 1..10_000usize {
        let name = match extension {
            Some(extension) if !extension.is_empty() => format!("{stem} ({suffix}).{extension}"),
            _ => format!("{stem} ({suffix})"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let name = match extension {
        Some(extension) if !extension.is_empty() => format!("{stem} ({stamp}).{extension}"),
        _ => format!("{stem} ({stamp})"),
    };
    parent.join(name)
}

/// Watch a downloaded temp file and re-upload it to the remote whenever it
/// changes on disk (the "edit" flow).  Re-upload is routed back through the
/// worker's own command channel.  Stops when the channel closes or after a
/// generous idle window.
pub(crate) fn spawn_edit_watcher(
    self_tx: UnboundedSender<SftpCommand>,
    local: String,
    remote: String,
) {
    tokio::spawn(async move {
        let mtime = |p: &str| std::fs::metadata(p).ok().and_then(|m| m.modified().ok());
        let mut last = mtime(&local);
        // ~40 min of 2s polls; also exits early once the worker is gone.
        for _ in 0..1200 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if self_tx.is_closed() {
                break;
            }
            let cur = mtime(&local);
            if cur.is_some() && cur != last {
                last = cur;
                let _ = self_tx.send(SftpCommand::UploadEdited {
                    local: PathBuf::from(&local),
                    remote: remote.clone(),
                });
            }
        }
    });
}

pub(crate) fn external_edit_local_name(host: &str, filename: &str, unique: &str) -> String {
    format!(
        "{}_{}_{}",
        sanitize_filename(host),
        sanitize_filename(unique),
        sanitize_filename(filename)
    )
}

// ---------------------------------------------------------------------------
// SFTP helpers
// ---------------------------------------------------------------------------

pub(crate) async fn cleanup_import_path(path: &Path) {
    let res = match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => tokio::fs::remove_dir_all(path).await,
        Ok(_) => tokio::fs::remove_file(path).await,
        Err(_) => Ok(()),
    };
    if let Err(e) = res {
        tracing::debug!("failed to clean temporary SFTP copy {:?}: {e}", path);
    }
}
