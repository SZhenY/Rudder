use super::*;

// ---------------------------------------------------------------------------
// Expect/send login triggers (#212)
// ---------------------------------------------------------------------------

/// One configured rule plus its rolling match buffer. `active` starts true and
/// stays true only for repeating rules once the rule has fired.
pub(crate) struct RuntimeTrigger {
    pub(crate) rule: SessionTrigger,
    pub(crate) buffer: String,
    pub(crate) active: bool,
}

/// Matches configured `expect` strings against the terminal stream and yields
/// the responses to send back. Rules are evaluated independently so one rule's
/// match never consumes another rule's text.
pub(crate) struct TriggerEngine(Vec<RuntimeTrigger>);

impl TriggerEngine {
    pub(crate) fn new(rules: &[SessionTrigger]) -> Self {
        Self(
            rules
                .iter()
                .filter(|rule| !rule.expect.is_empty() && !rule.response.is_empty())
                .cloned()
                .map(|rule| RuntimeTrigger {
                    rule,
                    buffer: String::new(),
                    active: true,
                })
                .collect(),
        )
    }

    pub(crate) fn feed(&mut self, text: &str) -> Vec<(Secret, bool)> {
        let mut replies = Vec::new();
        for trigger in &mut self.0 {
            if !trigger.active {
                continue;
            }
            trigger.buffer.push_str(text);
            if let Some(end) = trigger
                .buffer
                .find(&trigger.rule.expect)
                .map(|start| start + trigger.rule.expect.len())
            {
                replies.push((trigger.rule.response.clone(), trigger.rule.append_enter));
                trigger.buffer.drain(..end);
                trigger.active = trigger.rule.repeat;
            }
            // Preserve enough trailing text for a match split across chunks.
            let keep = trigger.rule.expect.len().saturating_sub(1).max(256);
            if trigger.buffer.len() > keep {
                let split = trigger.buffer.len() - keep;
                let split = trigger.buffer.ceil_char_boundary(split);
                trigger.buffer.drain(..split);
            }
        }
        replies
    }
}

// ---------------------------------------------------------------------------
// SFTP-related shared types
// ---------------------------------------------------------------------------

/// Metadata for a single remote filesystem entry returned by SFTP listing.
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub name: String,
    pub full_path: String,
    pub is_dir: bool,
    /// Raw size in bytes (0 for directories or unknown).
    pub size: u64,
    /// Modification time as Unix timestamp (seconds, u32 = SFTP wire format).
    pub modified: u32,
    /// POSIX permission bits (the low 12, i.e. rwx + setuid/setgid/sticky).
    /// 0 when the server didn't report permissions. Used to prefill the chmod
    /// dialog (#84).
    pub mode: u32,
    /// **完整**的 POSIX mode，含文件类型位（`S_IFMT`，例如目录是 `0o040_000`）。
    ///
    /// 「类型 / 权限」两列读这个（上游 `ed7f231`）；chmod 对话框仍用 `mode`
    /// （只有低 12 位，见上）。服务器没报权限时是 0。
    pub mode_full: u32,
}

/// One node in the remote directory tree panel.
#[derive(Debug, Clone)]
pub struct RemoteTreeNode {
    pub path: String,
    pub name: String,
    pub depth: u32,
    pub expanded: bool,
    pub has_children: bool,
}

pub(crate) fn load_session_private_key(session: &Session, pass: &str) -> Result<PrivateKey> {
    let pass = if pass.is_empty() { None } else { Some(pass) };
    let inline = session.private_key_inline.as_str().trim();
    if !inline.is_empty() {
        if crate::ssh::ppk::is_ppk(inline.as_bytes()) {
            return crate::ssh::ppk::decode_ppk(inline.as_bytes(), pass.unwrap_or_default())
                .context("failed to parse pasted PuTTY private key");
        }
        return decode_secret_key(inline, pass).context("failed to parse pasted private key");
    }

    let raw = session.private_key_path.trim();
    if raw.is_empty() {
        return Err(anyhow!(t(
            "私钥路径或私钥内容为空",
            "private key path or private key content is empty"
        )));
    }

    let normalised = raw.replace('\\', "/");
    let key_path = normalised
        .strip_suffix(".pub")
        .map(str::to_string)
        .unwrap_or(normalised);
    if Path::new(&key_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ppk"))
    {
        let raw = std::fs::read(&key_path)
            .with_context(|| format!("failed to read PuTTY key {key_path}"))?;
        return crate::ssh::ppk::decode_ppk(&raw, pass.unwrap_or_default())
            .with_context(|| format!("failed to load PuTTY key {key_path}"));
    }
    load_secret_key(Path::new(&key_path), pass)
        .with_context(|| format!("failed to load key {key_path}"))
}

/// Format a byte count with the given 1024-based unit suffixes. The first
/// unit is rendered as an integer; ≥TB sizes (when more than 4 units exist)
/// get two decimals, everything else one.
pub(crate) fn format_bytes_units(bytes: u64, units: &[&str]) -> String {
    let mut value = bytes as f64;
    let mut idx = 0;
    while value >= 1024.0 && idx < units.len() - 1 {
        value /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{} {}", bytes, units[0])
    } else if units.len() > 4 && idx >= 4 {
        format!("{:.2} {}", value, units[idx])
    } else {
        format!("{:.1} {}", value, units[idx])
    }
}

/// Format a byte count as a human-readable string.
pub fn format_size(bytes: u64) -> String {
    format_bytes_units(bytes, &["B", "KB", "MB", "GB", "TB", "PB", "EB"])
}
