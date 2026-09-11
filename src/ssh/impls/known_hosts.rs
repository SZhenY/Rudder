//! Host-key verification store (#109-5 / #105).
//!
//! Replaces the old "accept any server key" behaviour with a TOFU-style
//! known_hosts file plus a first-connect confirmation dialog:
//!   • unknown host  → prompt the user with the key fingerprint; on accept the
//!                     key is remembered here.
//!   • known + match  → connect silently.
//!   • known + differ → flagged as *changed* (possible MITM); the user must
//!                     re-confirm before the new key replaces the stored one.
//!
//! The file lives next to `sessions.json` (one entry per line):
//!     `host:port ssh-ed25519 AAAA...`
//! i.e. the `host:port` id followed by the key in its OpenSSH one-line form.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ssh_key::{HashAlg, PublicKey};

/// Result of checking a server key against the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// No entry for this host:port — first time we've seen it.
    Unknown,
    /// Stored key matches the presented one — trusted.
    Match,
    /// A key is stored for this host:port but it differs (possible MITM).
    Changed,
}

/// `host:port` lookup key.
fn id(host: &str, port: u16) -> String {
    format!("{host}:{port}")
}

/// Path to the known_hosts file (alongside sessions.json, in the portable-first
/// data dir — #141).
fn path() -> Option<PathBuf> {
    Some(crate::config::data_dir().join("known_hosts"))
}

/// The presented key in its canonical OpenSSH one-line form (`type base64`,
/// no comment), used for exact comparison and for storage.
fn openssh_line(key: &PublicKey) -> String {
    // `to_openssh` only fails on an unsupported/!encodable key, which russh
    // would not have negotiated; fall back to the SHA256 fingerprint so a
    // freak case still stores *something* stable rather than panicking.
    key.to_openssh().unwrap_or_else(|_| fingerprint(key))
}

/// Human-readable SHA256 fingerprint (`SHA256:base64…`) shown in the dialog.
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// Parse a known_hosts file into `(id, openssh_key)` entries. A missing or
/// unreadable file yields no entries. Malformed / comment (`#`) lines are
/// skipped.
///
/// Takes the path explicitly so the parse / verify / rewrite logic can be
/// exercised against a temp file: `path()` resolves the real data directory,
/// which a unit test cannot redirect.
fn load_from(p: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(p) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (id, key) = line.split_once(char::is_whitespace)?;
            Some((id.to_string(), key.trim().to_string()))
        })
        .collect()
}

fn load() -> Vec<(String, String)> {
    path().map(|p| load_from(&p)).unwrap_or_default()
}

/// Check a presented key against already-parsed entries.
fn verify_against(
    entries: &[(String, String)],
    host: &str,
    port: u16,
    key: &PublicKey,
) -> HostKeyStatus {
    let want = openssh_line(key);
    let id = id(host, port);
    let mut seen_host = false;
    for (entry_id, entry_key) in entries {
        if *entry_id != id {
            continue;
        }
        seen_host = true;
        if *entry_key == want {
            return HostKeyStatus::Match;
        }
    }
    if seen_host {
        HostKeyStatus::Changed
    } else {
        HostKeyStatus::Unknown
    }
}

/// Check a presented server key against the store.
pub fn verify(host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
    verify_against(&load(), host, port, key)
}

/// Remember (or replace) the key for `host:port` in the file at `p`. Rewrites
/// the file with any stale entry for the same id removed, then appends the new
/// one. Every other entry is preserved verbatim.
fn remember_at(p: &Path, host: &str, port: u16, key: &PublicKey) -> Result<()> {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).context("create config dir")?;
    }
    let id = id(host, port);
    let line = openssh_line(key);
    let mut out = String::new();
    for (entry_id, entry_key) in load_from(p) {
        if entry_id == id {
            continue; // drop the old key for this host:port
        }
        out.push_str(&entry_id);
        out.push(' ');
        out.push_str(&entry_key);
        out.push('\n');
    }
    out.push_str(&id);
    out.push(' ');
    out.push_str(&line);
    out.push('\n');
    // Atomic write: temp file + rename (mirrors ConfigStore::save). A plain
    // overwrite could corrupt the file mid-crash and lose every known key;
    // 0600 keeps host-key fingerprints owner-only (#atomic-known-hosts).
    let tmp = p.with_extension("known_hosts.tmp");
    std::fs::write(&tmp, out).with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, p).with_context(|| format!("finalise {}", p.display()))?;
    Ok(())
}

/// Remember (or replace) the key for `host:port` in the config directory.
pub fn remember(host: &str, port: u16, key: &PublicKey) -> Result<()> {
    let p = path().context("could not determine config directory")?;
    remember_at(&p, host, port, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic ed25519 public key. `Ed25519PublicKey` is a tuple struct
    /// with a public field, so no key generation is needed.
    fn key(seed: u8) -> PublicKey {
        use ssh_key::public::Ed25519PublicKey;
        PublicKey::from(Ed25519PublicKey([seed; 32]))
    }

    fn temp_file() -> PathBuf {
        std::env::temp_dir().join(format!("ms-kh-{}.txt", uuid::Uuid::new_v4()))
    }

    #[test]
    fn verify_reports_unknown_match_and_changed() {
        let p = temp_file();
        let (a, b) = (key(1), key(2));

        // No file at all → the host has never been seen.
        assert_eq!(
            verify_against(&load_from(&p), "h", 22, &a),
            HostKeyStatus::Unknown
        );

        remember_at(&p, "h", 22, &a).unwrap();
        let entries = load_from(&p);
        assert_eq!(verify_against(&entries, "h", 22, &a), HostKeyStatus::Match);
        // Same host, different key → the MITM signal.
        assert_eq!(
            verify_against(&entries, "h", 22, &b),
            HostKeyStatus::Changed
        );
        // A different port is a different host id.
        assert_eq!(
            verify_against(&entries, "h", 2222, &a),
            HostKeyStatus::Unknown
        );

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn remember_replaces_only_the_target_host() {
        let p = temp_file();
        let other = key(9);
        remember_at(&p, "keep.example", 22, &other).unwrap();
        remember_at(&p, "rotate.example", 22, &key(1)).unwrap();
        // The host is reinstalled and presents a new key.
        remember_at(&p, "rotate.example", 22, &key(2)).unwrap();

        let entries = load_from(&p);
        assert_eq!(
            entries.len(),
            2,
            "rotation must replace, not append: {entries:?}"
        );
        assert_eq!(
            verify_against(&entries, "keep.example", 22, &other),
            HostKeyStatus::Match,
            "an unrelated host must survive a rotation"
        );
        assert_eq!(
            verify_against(&entries, "rotate.example", 22, &key(2)),
            HostKeyStatus::Match
        );
        assert_eq!(
            verify_against(&entries, "rotate.example", 22, &key(1)),
            HostKeyStatus::Changed
        );

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn malformed_and_comment_lines_are_skipped() {
        let p = temp_file();
        std::fs::write(
            &p,
            "# a comment\n\n   \nno-separator-on-this-line\nhost.example:22 ssh-ed25519 AAAA\n",
        )
        .unwrap();

        let entries = load_from(&p);
        assert_eq!(entries.len(), 1, "got {entries:?}");
        assert_eq!(entries[0].0, "host.example:22");
        assert_eq!(entries[0].1, "ssh-ed25519 AAAA");

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn stored_key_uses_the_canonical_openssh_form() {
        let p = temp_file();
        let k = key(7);
        remember_at(&p, "h", 22, &k).unwrap();

        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text, format!("h:22 {}\n", k.to_openssh().unwrap()));
        assert!(text.contains("ssh-ed25519"));

        let _ = std::fs::remove_file(&p);
    }

    #[cfg(unix)]
    #[test]
    fn known_hosts_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let p = temp_file();
        remember_at(&p, "h", 22, &key(3)).unwrap();

        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "known_hosts must not be world-readable");

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_successful_rewrite_leaves_no_temp_file_behind() {
        let p = temp_file();
        remember_at(&p, "h", 22, &key(4)).unwrap();
        assert!(
            !p.with_extension("known_hosts.tmp").exists(),
            "the temp file must be renamed away, not left in the config dir"
        );

        let _ = std::fs::remove_file(&p);
    }
}
