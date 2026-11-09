use super::*;

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

    const CHUNK: usize = 32 * 1024;
    const MAX_INFLIGHT: usize = 32; // ~1 MB outstanding hides the RTT

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

    let total = raw
        .stat(remote)
        .await
        .ok()
        .and_then(|a| a.attrs.size)
        .unwrap_or(0);
    let fhandle = raw
        .open(remote, OpenFlags::READ, FileAttributes::default())
        .await
        .with_context(|| format!("open remote {remote}"))?
        .handle;

    // Resume support (#resume-download): the read pipeline is offset-based,
    // so when a partial local file already exists (smaller than the remote
    // size) we continue from its current length instead of re-downloading.
    // A full-size existing file is re-created as usual.
    let continue_from = match tokio::fs::metadata(local).await {
        Ok(m) if m.is_file() && total > 0 && m.len() < total => m.len(),
        _ => 0,
    };
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
                let want = ((total - off) as usize).min(CHUNK);
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
    let written = done + continue_from;
    let partial_kept = written > 0;

    if let Some(e) = err {
        drop(local_file);
        if !partial_kept {
            let _ = tokio::fs::remove_file(local).await;
        }
        return Err(e);
    }
    if cancelled {
        drop(local_file);
        if !partial_kept {
            let _ = tokio::fs::remove_file(local).await;
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
