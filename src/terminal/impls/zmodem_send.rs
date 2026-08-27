//! ZMODEM sender used when the remote shell runs `rz`.

use super::*;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const SEND_BLOCK_SIZE: usize = 16 * 1024;

/// Send one or more local files to a remote `rz`. `first` contains the ZRINIT
/// frame that triggered upload detection. The returned bytes belong to the
/// shell after the ZMODEM close handshake and must be rendered by the caller.
pub(crate) async fn send(
    channel: &mut Channel<Msg>,
    first: &[u8],
    files: &[PathBuf],
    events: &UnboundedSender<SessionEvent>,
) -> Result<Vec<u8>> {
    tracing::debug!(
        "zmodem: send start, first[{}]={:02x?}, files={}",
        first.len(),
        &first[..first.len().min(80)],
        files.len()
    );

    let mut io = Rx::new(channel, first);
    let receiver = wait_for_header(&mut io, &[ZRINIT]).await.with_context(|| {
        t(
            "等待远端 rz 的初始 ZRINIT 握手",
            "waiting for the initial ZRINIT handshake from remote rz",
        )
    })?;
    let crc32 = receiver.1[3] & CANFC32 != 0;
    let total_bytes = files
        .iter()
        .filter_map(|path| path.metadata().ok())
        .map(|metadata| metadata.len())
        .sum();
    let mut bytes_left = total_bytes;

    for (index, path) in files.iter().enumerate() {
        let size = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("read metadata for {}", path.display()))?
            .len();
        if size > u32::MAX as u64 {
            bail!(
                "{}: {}",
                t(
                    "ZMODEM 不支持上传超过 4 GiB 的单个文件",
                    "ZMODEM cannot upload an individual file larger than 4 GiB"
                ),
                path.display()
            );
        }
        let files_left = files.len().saturating_sub(index);
        send_file(&mut io, path, size, files_left, bytes_left, crc32, events).await?;
        bytes_left = bytes_left.saturating_sub(size);
    }

    io.send_hex(ZFIN, [0; 4]).await?;
    let _ = wait_for_header(&mut io, &[ZFIN]).await.with_context(|| {
        t(
            "等待远端 rz 的 ZFIN 关闭握手",
            "waiting for the ZFIN close handshake from remote rz",
        )
    })?;
    io.ch
        .data(&b"OO"[..])
        .await
        .context("zmodem send close marker")?;

    // `byte()` pulls a whole SSH chunk; putting the first prompt byte back
    // preserves that complete chunk for the normal terminal output path.
    if io.buf.is_empty() {
        if let Ok(Ok(first_prompt_byte)) =
            tokio::time::timeout(Duration::from_millis(800), io.byte()).await
        {
            io.buf.push_front(first_prompt_byte);
        }
    }

    let _ = events.send(SessionEvent::Output(format!(
        "\r\n[meatshell] {} {}\r\n",
        files.len(),
        t("个文件已通过 rz 上传", "file(s) uploaded via rz")
    )));
    Ok(io.buf.drain(..).collect())
}

async fn send_file(
    io: &mut Rx<'_>,
    path: &Path,
    size: u64,
    files_left: usize,
    bytes_left: u64,
    crc32: bool,
    events: &UnboundedSender<SessionEvent>,
) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("upload");
    let id = format!("zmodem-{}", uuid::Uuid::new_v4());
    emit(events, &id, name, (true, 0, size, 0, ""));

    let result = send_file_inner(
        io, path, name, size, files_left, bytes_left, crc32, events, &id,
    )
    .await;
    match &result {
        Ok(()) => emit(events, &id, name, (true, size, size, 1, "")),
        Err(error) => emit(events, &id, name, (true, 0, size, 2, &error.to_string())),
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn send_file_inner(
    io: &mut Rx<'_>,
    path: &Path,
    name: &str,
    size: u64,
    files_left: usize,
    bytes_left: u64,
    crc32: bool,
    events: &UnboundedSender<SessionEvent>,
    id: &str,
) -> Result<()> {
    let metadata = tokio::fs::metadata(path).await?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let info = file_info(name, size, modified, files_left, bytes_left);

    // A receiver may repeat ZRINIT while the file picker is open. Resend ZFILE
    // until the current receiver answers with the requested starting offset.
    io.send_bin(ZFILE, [0; 4], crc32).await?;
    io.send_subpacket(info.as_bytes(), ZCRCW, crc32).await?;
    let mut requested = loop {
        let (frame, data) = io.read_header().await.with_context(|| {
            t(
                "已发送文件信息，等待远端 rz 返回 ZRPOS",
                "sent file metadata; waiting for ZRPOS from remote rz",
            )
        })?;
        tracing::debug!("zmodem upload rx header type={frame} data={data:02x?}");
        match frame {
            ZRPOS => break u32::from_le_bytes(data) as u64,
            ZSKIP => return Ok(()),
            ZCAN | ZABORT => bail!("{}", t("传输被远端取消", "transfer aborted by receiver")),
            ZRINIT => {
                io.send_bin(ZFILE, [0; 4], crc32).await?;
                io.send_subpacket(info.as_bytes(), ZCRCW, crc32).await?;
            }
            _ => {}
        }
    };

    loop {
        if requested > size {
            bail!("receiver requested invalid offset {requested} for {size}-byte file");
        }
        let mut file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("open {}", path.display()))?;
        file.seek(std::io::SeekFrom::Start(requested)).await?;
        io.send_bin(ZDATA, (requested as u32).to_le_bytes(), crc32)
            .await?;

        let mut position = requested;
        let mut buffer = vec![0u8; SEND_BLOCK_SIZE];
        if position == size {
            io.send_subpacket(&[], ZCRCE, crc32).await?;
        } else {
            while position < size {
                let read = file.read(&mut buffer).await.context("read upload file")?;
                if read == 0 {
                    bail!("upload file ended at {position} of {size} bytes");
                }
                position += read as u64;
                let terminator = if position == size { ZCRCE } else { ZCRCG };
                io.send_subpacket(&buffer[..read], terminator, crc32)
                    .await?;
                emit(events, id, name, (true, position, size, 0, ""));
            }
        }
        io.send_bin(ZEOF, (position as u32).to_le_bytes(), crc32)
            .await?;

        loop {
            let (frame, data) = io.read_header().await.with_context(|| {
                t(
                    "已发送文件数据和 ZEOF，等待远端 rz 确认",
                    "sent file data and ZEOF; waiting for remote rz confirmation",
                )
            })?;
            tracing::debug!("zmodem upload rx header type={frame} data={data:02x?}");
            match frame {
                ZRINIT => return Ok(()),
                ZRPOS => {
                    requested = u32::from_le_bytes(data) as u64;
                    break;
                }
                ZSKIP => return Ok(()),
                ZCAN | ZABORT => {
                    bail!("{}", t("传输被远端取消", "transfer aborted by receiver"))
                }
                _ => {}
            }
        }
    }
}

async fn wait_for_header(io: &mut Rx<'_>, expected: &[u8]) -> Result<(u8, [u8; 4])> {
    loop {
        let header = io.read_header().await?;
        tracing::debug!(
            "zmodem upload rx header type={} data={:02x?}",
            header.0,
            header.1
        );
        if expected.contains(&header.0) {
            return Ok(header);
        }
        if matches!(header.0, ZCAN | ZABORT) {
            bail!("{}", t("传输被远端取消", "transfer aborted by receiver"));
        }
    }
}

fn file_info(name: &str, size: u64, modified: u64, files_left: usize, bytes_left: u64) -> String {
    format!("{name}\0{size} {modified:o} 0 0 {files_left} {bytes_left}\0")
}

impl Rx<'_> {
    /// Send a binary CRC-16/CRC-32 header using the receiver's advertised mode.
    async fn send_bin(&mut self, ftype: u8, data: [u8; 4], crc32: bool) -> Result<()> {
        let payload = [ftype, data[0], data[1], data[2], data[3]];
        let mut out = vec![ZPAD, ZDLE, if crc32 { ZBIN32 } else { ZBIN }];
        append_escaped(&mut out, &payload);
        if crc32 {
            append_escaped(&mut out, &crc32_of(&payload).to_le_bytes());
        } else {
            append_escaped(&mut out, &crc16(&payload).to_be_bytes());
        }
        tracing::debug!("zmodem tx binary type={ftype} len={}", out.len());
        self.ch
            .data(&out[..])
            .await
            .context("zmodem send binary header")?;
        Ok(())
    }

    /// Send one escaped data subpacket and its CRC.
    async fn send_subpacket(&mut self, data: &[u8], end: u8, crc32: bool) -> Result<()> {
        let mut out = Vec::with_capacity(data.len() + data.len() / 16 + 8);
        append_escaped(&mut out, data);
        out.extend_from_slice(&[ZDLE, end]);
        let mut crc_input = Vec::with_capacity(data.len() + 1);
        crc_input.extend_from_slice(data);
        crc_input.push(end);
        if crc32 {
            append_escaped(&mut out, &crc32_of(&crc_input).to_le_bytes());
        } else {
            append_escaped(&mut out, &crc16(&crc_input).to_be_bytes());
        }
        self.ch
            .data(&out[..])
            .await
            .context("zmodem send data subpacket")?;
        Ok(())
    }
}

fn append_escaped(out: &mut Vec<u8>, bytes: &[u8]) {
    for &byte in bytes {
        match byte {
            0x7f => out.extend_from_slice(&[ZDLE, ZRUB0]),
            0xff => out.extend_from_slice(&[ZDLE, ZRUB1]),
            // Escaping every C0 control byte avoids XON/XOFF or the PTY line
            // discipline consuming file contents in transit.
            0x00..=0x1f | 0x90 | 0x91 | 0x93 => out.extend_from_slice(&[ZDLE, byte ^ 0x40]),
            _ => out.push(byte),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_uses_zmodem_fields_and_octal_mtime() {
        assert_eq!(
            file_info("report.txt", 12, 8, 2, 34),
            concat!("report.txt\0", "12 10 0 0 2 34\0")
        );
    }

    #[test]
    fn control_bytes_are_zdle_escaped() {
        let mut encoded = Vec::new();
        append_escaped(&mut encoded, &[0x00, 0x11, ZDLE, 0x7f, 0xff, b'A']);
        assert_eq!(
            encoded,
            [ZDLE, 0x40, ZDLE, 0x51, ZDLE, 0x58, ZDLE, ZRUB0, ZDLE, ZRUB1, b'A']
        );
    }
}
