use super::*;

const CHUNK: usize = 32 * 1024;
const MAX_INFLIGHT: usize = 32; // ~1 MB outstanding hides the RTT

fn chunk_len(total: u64, off: u64) -> usize {
    ((total - off) as usize).min(CHUNK)
}

fn should_keep_partial(written: u64) -> bool {
    written > 0
}

/// Sidecar marker written next to a partial download. Without it a same-named
/// local file cannot be proven to be the same remote object, and appending to
/// it would silently corrupt the result.
#[derive(serde::Serialize, serde::Deserialize)]
struct PartialMark {
    remote: String,
    size: u64,
    mtime: u32,
}

const PARTIAL_SUFFIX: &str = ".rudderpart";

fn partial_marker_path(local: &str) -> String {
    format!("{local}{PARTIAL_SUFFIX}")
}

/// Resume only when the marker proves the remote path, size and mtime all match
/// and the local file is shorter than the remote one.
fn resume_offset(
    local_len: Option<u64>,
    total: u64,
    mark: Option<&PartialMark>,
    remote: &str,
    mtime: u32,
) -> u64 {
    let Some(len) = local_len else { return 0 };
    let Some(mark) = mark else { return 0 };
    if total == 0 || len == 0 || len >= total {
        return 0;
    }
    if mark.remote != remote || mark.size != total || mark.mtime != mtime {
        return 0;
    }
    len
}

/// Download a remote file over a dedicated, *pipelined* raw SFTP channel.
///
/// The high-level reader issues one READ and waits for the reply before the
/// next, so throughput is capped by the round-trip time (slow on any latent
/// link). Here we keep many READ requests in flight at once, each tagged with
/// its absolute offset so out-of-order completion is fine — mirroring
/// `upload_pipelined`.
///
/// Returns `Ok(true)` when the whole file was written, or `Ok(false)` if the
/// transfer was cancelled. In both the cancel and error cases the partial
/// local file is removed so no half-downloaded junk is left behind.
pub(crate) async fn download_impl(
    handle: &client::Handle<SftpClientHandler>,
    remote: &str,
    local: &str,
    name: &str,
    id: &str,
    events: &UnboundedSender<SessionEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<bool> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    let channel = handle
        .channel_open_session()
        .await
        .context("open sftp download channel")?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("request sftp subsystem")?;
    let raw = Arc::new(RawSftpSession::new(channel.into_stream()));
    raw.init().await.context("sftp download handshake")?;

    let stat = raw.stat(remote).await.ok();
    let total = stat.as_ref().and_then(|a| a.attrs.size).unwrap_or(0);
    let mtime = stat.as_ref().and_then(|a| a.attrs.mtime).unwrap_or(0);
    let fhandle = raw
        .open(remote, OpenFlags::READ, FileAttributes::default())
        .await
        .with_context(|| format!("open remote {remote}"))?
        .handle;

    // Resume support (#resume-download): only a sidecar marker that matches the
    // remote path, size and mtime proves the local file is our own partial.
    // A same-named file from another source must be overwritten from byte 0.
    let mark_path = partial_marker_path(local);
    let mark = tokio::fs::read_to_string(&mark_path)
        .await
        .ok()
        .and_then(|s| serde_json::from_str::<PartialMark>(&s).ok());
    let local_len = tokio::fs::metadata(local)
        .await
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len());
    let continue_from = resume_offset(local_len, total, mark.as_ref(), remote, mtime);
    let mut local_file = if continue_from > 0 {
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(local)
            .await
            .with_context(|| format!("open local (resume) {local}"))?
    } else {
        tokio::fs::File::create(local)
            .await
            .with_context(|| format!("create local {local}"))?
    };

    // Record the marker before the first byte: a cancelled transfer must leave
    // behind enough identity for the next attempt to resume safely.
    if total > 0 {
        let body = serde_json::to_vec(&PartialMark {
            remote: remote.to_string(),
            size: total,
            mtime,
        })
        .unwrap_or_default();
        if let Err(e) = tokio::fs::write(&mark_path, body).await {
            // Losing the marker only costs resume-ability, not correctness.
            tracing::debug!("download: cannot write resume marker {mark_path}: {e}");
        }
    }

    emit_transfer(TransferEmit {
        events,
        id,
        name,
        is_upload: false,
        transferred: continue_from,
        total,
        state: 0,
        msg: if continue_from > 0 { "resuming" } else { "" },
    });

    let mut done: u64 = continue_from;
    let mut last = Instant::now();
    let mut err: Option<anyhow::Error> = None;
    let mut cancelled = false;

    if total > 0 {
        let mut next_off = continue_from;
        let mut inflight = FuturesUnordered::new();
        loop {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
            }
            // Top up the pipeline with fresh READ requests.
            while !cancelled && err.is_none() && next_off < total && inflight.len() < MAX_INFLIGHT {
                let off = next_off;
                let want = chunk_len(total, off);
                next_off += want as u64;
                let raw2 = raw.clone();
                let h = fhandle.clone();
                inflight.push(async move {
                    // Fill the whole chunk, coping with short reads.
                    let mut data = Vec::with_capacity(want);
                    let mut o = off;
                    let end = off + want as u64;
                    while o < end {
                        match raw2.read(h.clone(), o, (end - o) as u32).await {
                            Ok(d) => {
                                if d.data.is_empty() {
                                    break;
                                }
                                o += d.data.len() as u64;
                                data.extend_from_slice(&d.data);
                            }
                            Err(SftpError::Status(s)) if s.status_code == StatusCode::Eof => break,
                            Err(e) => return Err(anyhow!("read remote: {e}")),
                        }
                    }
                    Ok::<(u64, Vec<u8>), anyhow::Error>((off, data))
                });
            }
            if inflight.is_empty() {
                break;
            }
            match inflight.next().await {
                Some(Ok((off, data))) => {
                    if !data.is_empty() {
                        if let Err(e) = local_file.seek(std::io::SeekFrom::Start(off)).await {
                            err = Some(anyhow!("seek local: {e}"));
                        } else if let Err(e) = local_file.write_all(&data).await {
                            err = Some(anyhow!("write local: {e}"));
                        } else {
                            done += data.len() as u64;
                        }
                    }
                    if last.elapsed() >= Duration::from_millis(150) {
                        last = Instant::now();
                        emit_transfer(TransferEmit {
                            events,
                            id,
                            name,
                            is_upload: false,
                            transferred: done,
                            total,
                            state: 0,
                            msg: "",
                        });
                    }
                }
                Some(Err(e)) => err = Some(e),
                None => {}
            }
            if (cancelled || err.is_some()) && inflight.is_empty() {
                break;
            }
        }
    } else {
        // Unknown / zero size: serial drain until EOF (rare; keeps correctness).
        let mut off = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            match raw.read(fhandle.clone(), off, CHUNK as u32).await {
                Ok(d) => {
                    if d.data.is_empty() {
                        break;
                    }
                    local_file
                        .write_all(&d.data)
                        .await
                        .context("write local file")?;
                    off += d.data.len() as u64;
                    done += d.data.len() as u64;
                    if last.elapsed() >= Duration::from_millis(150) {
                        last = Instant::now();
                        emit_transfer(TransferEmit {
                            events,
                            id,
                            name,
                            is_upload: false,
                            transferred: done,
                            total: done,
                            state: 0,
                            msg: "",
                        });
                    }
                }
                Err(SftpError::Status(s)) if s.status_code == StatusCode::Eof => break,
                Err(e) => {
                    err = Some(anyhow!("read remote: {e}"));
                    break;
                }
            }
        }
    }

    let _ = raw.close(fhandle).await;

    // Partial files are deliberately KEPT on error/cancel when any byte was
    // written: the next download of the same remote resumes from the current
    // length (#resume-download). Only a zero-byte stub is cleaned up.
    // `done` already starts at `continue_from`: adding it again double-counted
    // the resumed prefix in the transfer row.
    let written = done;
    let partial_kept = should_keep_partial(written);

    if let Some(e) = err {
        drop(local_file);
        if !partial_kept {
            let _ = tokio::fs::remove_file(local).await;
            let _ = tokio::fs::remove_file(&mark_path).await;
        }
        return Err(e);
    }
    if cancelled {
        drop(local_file);
        if !partial_kept {
            let _ = tokio::fs::remove_file(local).await;
            let _ = tokio::fs::remove_file(&mark_path).await;
        }
        emit_transfer(TransferEmit {
            events,
            id,
            name,
            is_upload: false,
            transferred: done,
            total,
            state: 4,
            msg: if partial_kept {
                t("已取消（可续传）", "Cancelled (resume-able)")
            } else {
                t("已取消", "Cancelled")
            },
        });
        return Ok(false);
    }
    local_file.flush().await.context("flush local file")?;
    let _ = tokio::fs::remove_file(&mark_path).await;
    emit_transfer(TransferEmit {
        events,
        id,
        name,
        is_upload: false,
        transferred: done,
        total: total.max(done),
        state: 1,
        msg: "",
    });
    Ok(true)
}

#[cfg(test)]
#[path = "download_leaf_tests.rs"]
mod download_leaf_tests;
