use super::*;

impl ConfigStore {
    // ── Portable export / import (issue #46) ──────────────────────────────

    /// Encrypt a password with the portable export key → `"enc:exp:v1:<b64>"`.
    pub(crate) fn encrypt_export(plaintext: &str) -> Result<String> {
        Self::encrypt_with(&Self::EXPORT_KEY, Self::EXPORT_PREFIX, "export", plaintext)
    }

    /// Decrypt a value produced by [`Self::encrypt_export`]; `None` if it isn't one.
    pub(crate) fn decrypt_export(s: &str) -> Option<String> {
        Self::decrypt_with(&Self::EXPORT_KEY, Self::EXPORT_PREFIX, s)
    }

    /// Export all sessions to a portable JSON file. Passwords are re-encrypted
    /// with the built-in export key; everything else stays plaintext so the
    /// file is human-readable and editable. Returns the number of sessions.
    pub fn export_json(&self) -> Result<(String, usize)> {
        let mut out = ExportFile {
            rudder_export: 1,
            sessions: self.cache.sessions.clone(),
        };
        for s in &mut out.sessions {
            // `cache` holds plaintext passwords; obfuscate with the export key.
            if !s.password.is_empty() {
                let enc = Self::encrypt_export(s.password.as_str())?;
                s.password = Secret::new(enc);
            }
            if !s.private_key_inline.is_empty() {
                let enc = Self::encrypt_export(s.private_key_inline.as_str())?;
                s.private_key_inline = Secret::new(enc);
            }
            // `last_used` is machine-local noise — don't carry it across.
            s.last_used = None;
        }
        Ok((serde_json::to_string_pretty(&out)?, out.sessions.len()))
    }

    /// Export all sessions to a portable JSON file. Passwords are re-encrypted
    /// with the built-in export key; everything else stays plaintext so the
    /// file is human-readable and editable. Returns the number of sessions.
    pub fn export_to(&self, path: &Path) -> Result<usize> {
        let (raw, count) = self.export_json()?;
        // 导出文件里含（用内置导出密钥再加密的）口令，而且用户常把它写到 U 盘 / 共享盘：
        // 走**原子写 + 建文件时就是 0600**（`write_private` 内含 fsync，再 rename 发布），
        // 而不是裸 `fs::write` —— 中断会留下半截文件，权限则完全跟着 umask 走。
        Self::write_atomic(path, &raw)?;
        Ok(count)
    }

    /// Import sessions from a Rudder portable export or a FinalShell connection
    /// export. New sessions get fresh ids; duplicates (same host+user+port+kind)
    /// are skipped.
    /// Returns `(added, skipped)`. The store is saved if anything was added.
    pub fn import_json(&mut self, raw: &str) -> Result<(usize, usize)> {
        let (sessions, decrypt_rudder_secrets) = match serde_json::from_str::<ExportFile>(raw) {
            Ok(file) => (file.sessions, true),
            Err(rudder_error) => (
                crate::config::finalshell::parse_export(raw).with_context(|| {
                    format!(
                        "not a valid Rudder or FinalShell export file; Rudder parser: {rudder_error}"
                    )
                })?,
                false,
            ),
        };

        let mut added = 0usize;
        let mut skipped = 0usize;
        for mut s in sessions {
            // Recover the plaintext password (cache stores plaintext). Accept an
            // export blob, our local enc:v1 blob, or a legacy plaintext value.
            // FinalShell's parser has already decrypted its DES password, so avoid
            // interpreting a coincidental `enc:*` plaintext prefix as ours.
            if decrypt_rudder_secrets {
                if let Some(plain) = Self::decrypt_export(s.password.as_str()) {
                    s.password = Secret::new(plain);
                } else if let Some(plain) = Self::try_decrypt(&self.key, s.password.as_str()) {
                    s.password = Secret::new(plain);
                }
                if let Some(plain) = Self::decrypt_export(s.private_key_inline.as_str()) {
                    s.private_key_inline = Secret::new(plain);
                } else if let Some(plain) =
                    Self::try_decrypt(&self.key, s.private_key_inline.as_str())
                {
                    s.private_key_inline = Secret::new(plain);
                }
            }
            let dup = self.cache.sessions.iter().any(|x| {
                x.host == s.host && x.user == s.user && x.port == s.port && x.kind == s.kind
            });
            if dup {
                skipped += 1;
                continue;
            }
            s.id = Uuid::new_v4().to_string();
            self.upsert(s);
            added += 1;
        }
        if added > 0 {
            self.save()?;
        }
        Ok((added, skipped))
    }

    /// Import sessions from a Rudder or FinalShell JSON export file.
    pub fn import_from(&mut self, path: &Path) -> Result<(usize, usize)> {
        let raw = crate::files::read_string_capped(
            path,
            crate::files::MAX_IMPORT_BYTES,
            "session import",
        )?;
        self.import_json(&raw)
    }
}
