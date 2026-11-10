use super::*;

impl ConfigStore {
    // ── Session groups / folders (#41) ────────────────────────────────────

    /// Explicit groups (empty folders included). "default" is implicit.
    pub fn groups(&self) -> &[String] {
        &self.cache.groups
    }

    pub fn collapsed_session_groups(&self) -> Option<&[String]> {
        self.cache.collapsed_session_groups.as_deref()
    }

    /// Remember a Quick Connect folder's open/closed state. On the first
    /// interaction, materialise the default-collapsed state for every existing
    /// folder so expanding one folder does not accidentally expand the rest.
    pub fn set_session_group_collapsed(&mut self, name: &str, collapsed: bool) {
        if self.cache.collapsed_session_groups.is_none() {
            let mut groups = vec!["system".to_string()];
            if self
                .cache
                .sessions
                .iter()
                .any(|session| session.group.is_empty())
            {
                groups.push("default".to_string());
            }
            groups.extend(self.cache.groups.iter().cloned());
            groups.extend(
                self.cache
                    .sessions
                    .iter()
                    .filter(|session| !session.group.is_empty())
                    .map(|session| session.group.clone()),
            );
            groups.sort();
            groups.dedup();
            self.cache.collapsed_session_groups = Some(groups);
        }

        let groups = self.cache.collapsed_session_groups.as_mut().unwrap();
        groups.retain(|group| group != name);
        if collapsed {
            groups.push(name.to_string());
            groups.sort();
            groups.dedup();
        }
    }

    /// Whether a user group already exists, including groups inferred from
    /// sessions that were created before explicit group records were added.
    pub fn session_group_exists(&self, name: &str) -> bool {
        let target = name.trim();
        if target.is_empty() {
            return false;
        }
        self.cache
            .groups
            .iter()
            .any(|group| group.trim().eq_ignore_ascii_case(target))
            || self.cache.sessions.iter().any(|session| {
                !session.group.trim().is_empty()
                    && session.group.trim().eq_ignore_ascii_case(target)
            })
    }

    /// Create an empty group. Ignores blank/reserved names and duplicates.
    pub fn add_group(&mut self, name: String) {
        let n = name.trim().to_string();
        if n.is_empty() || is_reserved_session_group(&n) || self.session_group_exists(&n) {
            return;
        }
        self.cache.groups.push(n.clone());
        if let Some(groups) = &mut self.cache.collapsed_session_groups {
            groups.push(n);
            groups.sort();
            groups.dedup();
        }
    }

    /// Delete a group. Any session still in it falls back to ungrouped — the UI
    /// only offers delete on empty groups, but we clear sessions defensively.
    pub fn remove_group(&mut self, name: &str) {
        if is_reserved_session_group(name.trim()) {
            return;
        }
        self.cache.groups.retain(|g| g != name);
        if let Some(groups) = &mut self.cache.collapsed_session_groups {
            groups.retain(|group| group != name);
        }
        for s in &mut self.cache.sessions {
            if s.group == name {
                s.group.clear();
            }
        }
    }

    /// Rename a group, moving its sessions along. No-op for reserved names.
    pub fn rename_group(&mut self, old: &str, new: String) {
        let n = new.trim().to_string();
        if n.is_empty()
            || is_reserved_session_group(old.trim())
            || is_reserved_session_group(&n)
            || n == old
            || (!n.eq_ignore_ascii_case(old) && self.session_group_exists(&n))
        {
            return;
        }
        for g in &mut self.cache.groups {
            if g == old {
                *g = n.clone();
            }
        }
        for s in &mut self.cache.sessions {
            if s.group == old {
                s.group = n.clone();
            }
        }
        if let Some(groups) = &mut self.cache.collapsed_session_groups {
            for group in groups.iter_mut() {
                if group == old {
                    *group = n.clone();
                }
            }
            groups.sort();
            groups.dedup();
        }
        self.cache.groups.sort();
        self.cache.groups.dedup();
    }

    /// 立即落盘（写两个文件）。
    ///
    /// 保持同步写不变 —— 47 处调用方的语义各不相同（导入/导出、迁移、退出），
    /// 改动它们的时序风险大于收益。高频路径（滑块、命令历史）请走
    /// `save_debounced` + `flush`。
    pub fn save(&self) -> Result<()> {
        // Build disk copies where every non-empty password is encrypted.
        let mut sessions_disk = SessionsFile::from_cache(&self.cache);
        for session in &mut sessions_disk.sessions {
            if !session.password.is_empty()
                && !session.password.as_str().starts_with(Self::ENC_PREFIX)
            {
                let enc = Self::encrypt(&self.key, session.password.as_str())?;
                session.password = Secret::new(enc);
            }
            if !session.private_key_inline.is_empty()
                && !session
                    .private_key_inline
                    .as_str()
                    .starts_with(Self::ENC_PREFIX)
            {
                let enc = Self::encrypt(&self.key, session.private_key_inline.as_str())?;
                session.private_key_inline = Secret::new(enc);
            }
            // 代理密码与登录密码同一套处理：这是明文网络凭据，不能裸存。
            if !session.proxy_pass.is_empty()
                && !session.proxy_pass.as_str().starts_with(Self::ENC_PREFIX)
            {
                let enc = Self::encrypt(&self.key, session.proxy_pass.as_str())?;
                session.proxy_pass = Secret::new(enc);
            }
            // Trigger responses are credentials too — the documented use case
            // is auto-answering a `Password:` prompt — so they get exactly the
            // same at-rest treatment as the session password. `Secret` has no
            // built-in encryption (it only zeroes on drop), so this has to be
            // done here explicitly.
            for trigger in &mut session.triggers {
                if !trigger.response.is_empty()
                    && !trigger.response.as_str().starts_with(Self::ENC_PREFIX)
                {
                    let enc = Self::encrypt(&self.key, trigger.response.as_str())?;
                    trigger.response = Secret::new(enc);
                }
            }
        }
        // 命令历史也用同一套处理：里面常有 `mysql -p…`、`curl -H 'Authorization: …'`
        // 这类自带口令的命令，此前是明文躺在 sessions.json 里。已带前缀的跳过（幂等）。
        for cmd in &mut sessions_disk.command_history {
            if !cmd.is_empty() && !cmd.starts_with(Self::ENC_PREFIX) {
                *cmd = Self::encrypt(&self.key, cmd)?;
            }
        }
        let mut settings_disk = SettingsFile::from_cache(&self.cache);
        if !settings_disk.sync.webdav_password.is_empty()
            && !settings_disk
                .sync
                .webdav_password
                .as_str()
                .starts_with(Self::ENC_PREFIX)
        {
            let enc = Self::encrypt(&self.key, settings_disk.sync.webdav_password.as_str())?;
            settings_disk.sync.webdav_password = Secret::new(enc);
        }
        // MCP 令牌与口令同一套加密；已带前缀的跳过（幂等）。
        if !settings_disk.mcp.mcp_token.is_empty()
            && !settings_disk
                .mcp
                .mcp_token
                .as_str()
                .starts_with(Self::ENC_PREFIX)
        {
            let enc = Self::encrypt(&self.key, settings_disk.mcp.mcp_token.as_str())?;
            settings_disk.mcp.mcp_token = Secret::new(enc);
        }
        let settings_raw = serde_json::to_string_pretty(&settings_disk)?;
        let sessions_raw = serde_json::to_string_pretty(&sessions_disk)?;
        // **先写 settings.json**。合并前的 sessions.json 里也带着一份设置，所以
        // 即便写 sessions.json 时崩溃，设置也已经安全落地；反过来（先写 sessions）
        // 则会在崩溃时丢掉全部设置 —— 那时 sessions.json 里的设置已被覆盖成新格式
        // （不再含设置），而 settings.json 还没写出来。
        Self::write_atomic(&self.settings_path, &settings_raw)?;
        Self::write_atomic(&self.path, &sessions_raw)?;
        self.sync_backup(&sessions_raw, &settings_raw);
        Ok(())
    }

    /// 立即写盘，失败**记日志**。
    ///
    /// 这是全项目统一的写盘失败策略（与设置面板的 `persist` 一致）：设置坏掉不值得
    /// panic，但也不能像以前那样用 `let _ = s.save();` **静默**吞掉 —— 那样用户会以为
    /// 改动保住了，重启之后才发现没存上，而且日志里什么线索都没有。
    pub fn save_logging(&mut self) {
        if let Err(e) = self.save() {
            tracing::warn!("failed to save {}: {e:#}", self.path.display());
        }
    }

    /// 退出路径的收尾写盘，失败**记日志** —— 这是最后一次落盘机会，
    /// 静默丢掉的话用户改的设置就白改了。
    pub fn flush_logging(&mut self) {
        if let Err(e) = self.flush() {
            tracing::warn!("failed to flush {} on exit: {e:#}", self.path.display());
        }
    }

    /// 标记"有改动待落盘"；真正的写入由 `flush` 在防抖窗口结束后执行。
    pub fn save_debounced(&mut self) {
        self.pending = true;
    }

    /// 落盘挂起的改动。没有挂起时是空操作 —— 退出路径可以无条件调用。
    pub fn flush(&mut self) -> Result<()> {
        if !self.pending {
            return Ok(());
        }
        self.pending = false;
        self.save()
    }

    /// 以 0600 **创建**文件并写入内容；`durable` 为真时再 `fsync`。
    ///
    /// 两个容易漏的点：
    ///
    /// * **权限窗口**：`fs::write` + 事后 `chmod 0600` 之间，文件是按 umask（通常 0644）
    ///   建的，这段时间**别的本地账户就能读**。`secret.key` 和 `sessions.json.tmp` 都不
    ///   该有这段窗口，所以用 `OpenOptions::mode(0o600)` 让权限在创建时就带上。
    ///   （Windows 的 `%APPDATA%` 默认 ACL 就是属主专属，那里的 `mode` 是空操作。）
    /// * **掉电**：写完不 `fsync` 直接 `rename`，目录项可能先落盘而数据没有 —— 崩溃后
    ///   会看到一个 0 字节的 `sessions.json`。主文件走 `durable = true`。
    pub(crate) fn write_private(path: &Path, bytes: &[u8], durable: bool) -> Result<()> {
        use std::io::Write as _;

        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        #[cfg(unix)]
        {
            // 旧版本留下的文件可能是 0644（那时是先建后 chmod）：顺手补一次。
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
        }
        file.write_all(bytes)
            .with_context(|| format!("failed to write {}", path.display()))?;
        if durable {
            file.sync_all()
                .with_context(|| format!("failed to sync {}", path.display()))?;
        }
        Ok(())
    }

    /// `rename` 之后同步父目录，让目录项本身也落地。
    ///
    /// 只用 unix；失败只记 debug 日志 —— 它坏的最坏结果只是"崩溃后回到旧文件"，
    /// 不是数据损坏，不值得把整次保存判成失败。
    pub(crate) fn sync_parent_dir(path: &Path) {
        #[cfg(unix)]
        if let Some(dir) = path.parent() {
            match fs::File::open(dir) {
                Ok(d) => {
                    if let Err(e) = d.sync_all() {
                        tracing::debug!("failed to sync {}: {e}", dir.display());
                    }
                }
                Err(e) => tracing::debug!("failed to open {} for sync: {e}", dir.display()),
            }
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    /// 写兄弟临时文件 → rename：廉价的原子发布。
    ///
    /// 权限在**建文件时**就带上 0600，主文件还带 `fsync`（见 `write_private`）：
    /// `sessions.json` 里有（加密的）凭据，既不欢迎本地其他账户读，也不能掉电变 0 字节。
    pub(crate) fn write_atomic(path: &Path, raw: &str) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        Self::write_private(&tmp, raw.as_bytes(), true)?;
        fs::rename(&tmp, path).with_context(|| format!("failed to finalise {}", path.display()))?;
        Self::sync_parent_dir(path);
        Ok(())
    }

    pub(crate) fn sync_backup(&self, sessions_raw: &str, settings_raw: &str) {
        let Some(backup_dir) = &self.backup_dir else {
            return;
        };
        if let Err(e) = fs::create_dir_all(backup_dir) {
            tracing::warn!(
                "failed to create user config backup dir {}: {e}",
                backup_dir.display()
            );
            return;
        }

        for (name, raw) in [(SESSIONS_FILE, sessions_raw), (SETTINGS_FILE, settings_raw)] {
            let dst = backup_dir.join(name);
            let tmp = dst.with_extension("json.tmp");
            // 备份是尽力而为：权限同样要在建文件时就 0600（里面是加密凭据），
            // 但不值得为它 fsync —— 主文件已经在 write_atomic 里同步过了。
            if let Err(e) = Self::write_private(&tmp, raw.as_bytes(), false) {
                tracing::warn!("failed to write {}: {e}", tmp.display());
                continue;
            }
            if let Err(e) = fs::rename(&tmp, &dst) {
                tracing::warn!("failed to finalise {}: {e}", dst.display());
            }
        }

        if let Some(config_dir) = self.path.parent() {
            for name in ["secret.key", "known_hosts"] {
                let src = config_dir.join(name);
                let dst = backup_dir.join(name);
                if src.exists() {
                    if let Err(e) = fs::copy(&src, &dst) {
                        tracing::warn!(
                            "failed to sync {} to user config backup {}: {e}",
                            src.display(),
                            dst.display()
                        );
                    }
                    #[cfg(unix)]
                    if name == "secret.key" {
                        set_private_mode(&dst);
                    }
                }
            }
        }
    }
}
