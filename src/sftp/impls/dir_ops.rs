use super::*;

/// Recursively download a remote directory tree under `local_parent` (#50).
///
/// Iterative (work-stack) rather than a boxed async recursion: each remote dir
/// is mirrored to a sanitized local name, then its files are downloaded with the
/// same per-file pipeline used for single downloads. Names are sanitized (#26)
/// so a hostile server can't escape the chosen folder.
pub(crate) async fn download_dir(
    sftp: &SftpSession,
    handle: &client::Handle<SftpClientHandler>,
    remote_root: &str,
    local_parent: &str,
    events: &UnboundedSender<SessionEvent>,
    file_id: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    let root_name = sanitize_filename(&base_name(remote_root));
    let root_local = format!("{}/{}", local_parent.trim_end_matches('/'), root_name);
    // (remote_dir, local_dir) pairs still to mirror.
    let mut stack = vec![(remote_root.trim_end_matches('/').to_string(), root_local)];
    while let Some((rdir, ldir)) = stack.pop() {
        tokio::fs::create_dir_all(&ldir)
            .await
            .with_context(|| format!("create local dir {ldir}"))?;
        for entry in list_dir_impl(sftp, &rdir).await? {
            if cancel.load(Ordering::Relaxed) {
                return Err(anyhow!("folder transfer cancelled"));
            }
            if entry.is_dir {
                let child_local = format!("{}/{}", ldir, sanitize_filename(&entry.name));
                stack.push((entry.full_path, child_local));
            } else {
                let fname = sanitize_filename(&entry.name);
                let lpath = format!("{}/{}", ldir, fname);
                // Reuse the folder's single id so the transfer panel keeps ONE
                // aggregate row (updated per file) instead of one row per file
                // — and the row's cancel button actually reaches us (#folder-cancel).
                download_impl(
                    handle,
                    &entry.full_path,
                    &lpath,
                    &fname,
                    file_id,
                    events,
                    cancel,
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// Recursively remove a remote directory tree (#50 follow-up).
///
/// A plain `remove_dir` only deletes an *empty* directory, so deleting an
/// uploaded folder failed. We BFS to discover every sub-directory (deleting
/// files as we go), then rmdir them deepest-first.
pub(crate) async fn remove_dir_recursive(sftp: &SftpSession, root: &str) -> Result<()> {
    let mut all_dirs = vec![root.trim_end_matches('/').to_string()];
    let mut i = 0;
    while i < all_dirs.len() {
        let d = all_dirs[i].clone();
        i += 1;
        for entry in list_dir_impl(sftp, &d).await? {
            if entry.is_dir {
                all_dirs.push(entry.full_path);
            } else {
                sftp.remove_file(&entry.full_path)
                    .await
                    .map_err(|e| anyhow::anyhow!("remove file {}: {e}", entry.full_path))?;
            }
        }
    }
    // BFS discovered parents before children, so reversing gives deepest-first.
    for d in all_dirs.iter().rev() {
        sftp.remove_dir(d)
            .await
            .map_err(|e| anyhow::anyhow!("remove dir {d}: {e}"))?;
    }
    Ok(())
}

/// Recursively upload a local directory tree into `remote_parent` (#50).
///
/// Iterative work-stack: mirror each local dir to the remote (create_dir, whose
/// "already exists" error is ignored), then upload its files with the pipelined
/// path. Symlinks and other special files are skipped.
pub(crate) async fn upload_dir(
    handle: &client::Handle<SftpClientHandler>,
    sftp: &SftpSession,
    local_root: &Path,
    remote_parent: &str,
    events: &UnboundedSender<SessionEvent>,
    file_id: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    let root_name = local_file_name_utf8(local_root)?;
    let remote_root = format!("{}/{}", remote_parent.trim_end_matches('/'), root_name);
    let mut stack = vec![(local_root.to_path_buf(), remote_root)];
    while let Some((ldir, rdir)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("folder transfer cancelled"));
        }
        // Best-effort mkdir; an error usually just means the dir already exists.
        let _ = sftp.create_dir(&rdir).await;
        let mut rd = tokio::fs::read_dir(&ldir)
            .await
            .with_context(|| format!("read local dir {}", ldir.display()))?;
        while let Some(entry) = rd.next_entry().await.context("read dir entry")? {
            let lpath = entry.path();
            let name = local_file_name_utf8(&lpath)?;
            let rchild = format!("{}/{}", rdir, name);
            let ft = entry.file_type().await.context("file type")?;
            if ft.is_dir() {
                stack.push((lpath, rchild));
            } else if ft.is_file() {
                // Single aggregate row keyed by the folder's id (#folder-cancel).
                upload_pipelined(handle, &lpath, &rchild, &name, file_id, events, cancel).await?;
            }
        }
    }
    Ok(())
}
