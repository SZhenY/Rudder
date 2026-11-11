use super::*;

pub(crate) const MAX_BUILTIN_EDITOR_BYTES: usize = 512 * 1024;
/// 内置编辑器的行数上限。从 20 000 收到 10 000（上游 96cb659）：我们的行号栏是**一整个
/// O(N) 字符串 + 一次全量排版**（`ui/file_editor.slint` 的 editor gutter），比上游的原生行号更怕
/// 大文件，先卡在更保守的线上。
pub(crate) const MAX_BUILTIN_EDITOR_LINES: usize = 10_000;
pub(crate) const MAX_BUILTIN_EDITOR_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EditorTextRejection {
    TooLarge,
    TooManyLines,
    LineTooLong,
    Binary,
    InvalidUtf8,
}

pub(crate) fn validate_editor_text(
    bytes: Vec<u8>,
) -> std::result::Result<String, EditorTextRejection> {
    if bytes.len() > MAX_BUILTIN_EDITOR_BYTES {
        return Err(EditorTextRejection::TooLarge);
    }
    if bytes.iter().any(|&byte| {
        (byte < 0x20 && byte != b'\t' && byte != b'\n' && byte != b'\r') || byte == 0x7f
    }) {
        return Err(EditorTextRejection::Binary);
    }

    let text = String::from_utf8(bytes).map_err(|_| EditorTextRejection::InvalidUtf8)?;
    let mut line_count = 1usize;
    let mut line_bytes = 0usize;
    for byte in text.bytes() {
        if byte == b'\n' {
            line_count += 1;
            line_bytes = 0;
            if line_count > MAX_BUILTIN_EDITOR_LINES {
                return Err(EditorTextRejection::TooManyLines);
            }
        } else {
            line_bytes += 1;
            if line_bytes > MAX_BUILTIN_EDITOR_LINE_BYTES {
                return Err(EditorTextRejection::LineTooLong);
            }
        }
    }
    Ok(text)
}

pub(crate) fn editor_rejection_message(rejection: EditorTextRejection) -> String {
    match rejection {
        EditorTextRejection::TooLarge => t(
            "文件过大,无法在内置编辑器中打开(上限 512 KB),请使用外部打开/编辑或下载",
            "Too large for the built-in editor (512 KB limit); open/edit externally or download it instead",
        ),
        EditorTextRejection::TooManyLines => t(
            "文件行数过多(上限 10,000 行),无法在内置编辑器中安全打开,请使用外部打开/编辑或下载",
            "Too many lines for the built-in editor (10,000 line limit); open/edit externally or download it instead",
        ),
        EditorTextRejection::LineTooLong => t(
            "文件包含过长的单行,无法在内置编辑器中安全打开,请使用外部打开/编辑或下载",
            "Contains a line too long for the built-in editor; open/edit externally or download it instead",
        ),
        EditorTextRejection::Binary => t(
            "包含控制字符(疑似二进制),无法以文本打开,请下载查看",
            "Contains control characters (likely binary); download it instead",
        ),
        EditorTextRejection::InvalidUtf8 => {
            t("非 UTF-8 文本,无法打开", "Not UTF-8 text; cannot open")
        }
    }
    .into()
}

/// Read a remote file as UTF-8 text for the built-in editor, rejecting content
/// that would make Slint eagerly lay out an unsafe amount of text (#70, #331).
/// Returns the text on success or a human-readable error message on failure.
///
/// **边读边判**（8 KB 一块）：控制字符、行数、单行长度一旦越界立刻返回 —— 以前是先把
/// 512 KB 整个读进内存、再回头判定，远端丢来一个大日志就白白读满并分配一次
/// （上游 96cb659 里可借鉴的那一半）。最终仍由 [`validate_editor_text`] 做权威校验，
/// 顺带完成 UTF-8 检查。
pub(crate) async fn read_text_guarded(
    sftp: &SftpSession,
    remote: &str,
) -> std::result::Result<String, String> {
    use tokio::io::AsyncReadExt;
    const CHUNK_BYTES: usize = 8 * 1024;

    let size = sftp
        .metadata(remote)
        .await
        .ok()
        .and_then(|m| m.size)
        .unwrap_or(0);
    if size > MAX_BUILTIN_EDITOR_BYTES as u64 {
        return Err(editor_rejection_message(EditorTextRejection::TooLarge));
    }
    let mut f = sftp
        .open(remote)
        .await
        .map_err(|e| format!("{}: {e}", t("打开失败", "Open failed")))?;
    // Metadata may be missing or stale. Stop one byte past the limit so an
    // untrusted remote file can never make this path allocate without bound.
    let mut bytes = Vec::with_capacity((size as usize).min(MAX_BUILTIN_EDITOR_BYTES));
    let mut buf = [0u8; CHUNK_BYTES];
    let mut line_count = 1usize;
    let mut line_bytes = 0usize;
    loop {
        let read = f
            .read(&mut buf)
            .await
            .map_err(|e| format!("{}: {e}", t("读取失败", "Read failed")))?;
        if read == 0 {
            break;
        }
        for &byte in &buf[..read] {
            if (byte < 0x20 && byte != b'\t' && byte != b'\n' && byte != b'\r') || byte == 0x7f {
                return Err(editor_rejection_message(EditorTextRejection::Binary));
            }
            if byte == b'\n' {
                line_count += 1;
                line_bytes = 0;
                if line_count > MAX_BUILTIN_EDITOR_LINES {
                    return Err(editor_rejection_message(EditorTextRejection::TooManyLines));
                }
            } else {
                line_bytes += 1;
                if line_bytes > MAX_BUILTIN_EDITOR_LINE_BYTES {
                    return Err(editor_rejection_message(EditorTextRejection::LineTooLong));
                }
            }
        }
        if bytes.len() + read > MAX_BUILTIN_EDITOR_BYTES {
            return Err(editor_rejection_message(EditorTextRejection::TooLarge));
        }
        bytes.extend_from_slice(&buf[..read]);
    }
    validate_editor_text(bytes).map_err(editor_rejection_message)
}

/// Overwrite a remote file with the given text (CREATE | WRITE | TRUNCATE).
pub(crate) async fn write_text_file(sftp: &SftpSession, remote: &str, content: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut f = sftp
        .create(remote)
        .await
        .with_context(|| format!("create remote {remote}"))?;
    f.write_all(content.as_bytes())
        .await
        .context("write remote file")?;
    f.flush().await.context("flush remote file")?;
    let _ = f.shutdown().await;
    Ok(())
}

/// File name component of a path.  Handles both remote (`/`) and local Windows
/// (`\`) separators, so uploading `C:\…\frp.tar.gz` yields `frp.tar.gz` rather
/// than the whole path (which previously became the remote file name).
/// MCP 变更之后刷新面板列表。
///
/// 左树不跟着同步（那要改 `tree_dirs` / `tree_expanded` 两张表），代价是展开的树节点
/// 可能短暂陈旧，用户下次 Refresh / 展开就会修正。
pub(crate) async fn refresh_panel_dir(
    sftp: &SftpSession,
    events: &tokio::sync::mpsc::UnboundedSender<SessionEvent>,
    dir: &str,
) {
    if let Ok(entries) = list_dir_impl(sftp, dir).await {
        let _ = events.send(SessionEvent::SftpEntries {
            path: dir.to_string(),
            entries,
        });
    }
}

/// File name component of a path.  Handles both remote (`/`) and local Windows
/// (`\`) separators, so uploading `C://…//frp.tar.gz` yields `frp.tar.gz` rather
/// than the whole path (which previously became the remote file name).
pub(crate) fn base_name(path: &str) -> String {
    let sep = |c: char| c == '/' || c == '\\';
    path.trim_end_matches(sep)
        .rsplit(sep)
        .next()
        .unwrap_or(path)
        .to_string()
}

pub(crate) fn local_file_name_utf8(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("local file name is not valid UTF-8: {}", path.display()))
}

/// Single-quote a string for safe interpolation into a remote `/bin/sh`
/// command. Remote names come from the *server's* listing and are therefore
/// untrusted — without quoting, a crafted name like `; rm -rf ~` would run.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run a one-shot command on the remote over its own exec channel and return
/// the exit status. Stdout/stderr are drained and discarded.
pub(crate) async fn exec_remote(
    handle: &client::Handle<SftpClientHandler>,
    cmd: &str,
) -> Result<u32> {
    let mut ch = handle
        .channel_open_session()
        .await
        .context("open exec channel")?;
    ch.exec(true, cmd.as_bytes())
        .await
        .context("exec remote command")?;
    let mut status = 0u32;
    while let Some(msg) = ch.wait().await {
        match msg {
            russh::ChannelMsg::ExitStatus { exit_status } => status = exit_status,
            russh::ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(status)
}
