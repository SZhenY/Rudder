use super::*;

impl ConfigStore {
    // ── MCP server（AI 页）────────────────────────────────────────────────
    pub fn mcp_enabled(&self) -> bool {
        self.cache.mcp.mcp_enabled
    }
    pub fn set_mcp_enabled(&mut self, v: bool) {
        self.cache.mcp.mcp_enabled = v;
    }
    /// 出厂端口之外的一律回退：0 会让系统随机分配，用户填了也找不回来。
    pub fn mcp_port(&self) -> u16 {
        let port = self.cache.mcp.mcp_port;
        if port < 1024 { DEFAULT_MCP_PORT } else { port }
    }
    pub fn set_mcp_port(&mut self, port: u16) {
        if port >= 1024 {
            self.cache.mcp.mcp_port = port;
        }
    }
    pub fn mcp_bind_loopback(&self) -> bool {
        self.cache.mcp.mcp_bind_loopback
    }
    pub fn mcp_lan_enabled(&self) -> bool {
        self.cache.mcp.mcp_lan_enabled
    }
    pub fn set_mcp_lan_enabled(&mut self, v: bool) {
        self.cache.mcp.mcp_lan_enabled = v;
    }
    pub fn mcp_lan_ifaces(&self) -> &[String] {
        &self.cache.mcp.mcp_lan_ifaces
    }
    /// 整表替换网口勾选（「还原本页默认」用）。
    pub fn set_mcp_lan_ifaces(&mut self, list: Vec<String>) {
        self.cache.mcp.mcp_lan_ifaces = list;
    }
    /// 开关某个网口：已勾选则去掉，否则追加。
    pub fn toggle_mcp_lan_iface(&mut self, name: &str) {
        if let Some(index) = self
            .cache
            .mcp
            .mcp_lan_ifaces
            .iter()
            .position(|iface| iface == name)
        {
            self.cache.mcp.mcp_lan_ifaces.remove(index);
        } else {
            self.cache.mcp.mcp_lan_ifaces.push(name.to_string());
        }
    }
    pub fn mcp_token(&self) -> &str {
        self.cache.mcp.mcp_token.as_str()
    }
    pub fn set_mcp_token(&mut self, token: String) {
        self.cache.mcp.mcp_token = Secret::new(token);
    }
    /// 记住的客户端授权（最近 32 个，新的在末尾）。
    pub fn mcp_client_grants(&self) -> &[McpClientGrant] {
        &self.cache.mcp.mcp_client_grants
    }

    /// 写入/更新一个客户端的记忆授权。空 id 或含控制字符的 id 不写。
    /// 列表上限 32，超出时丢最旧的（列表头部）。
    ///
    /// `last_auth_unix` 每次写入都刷新：它既是"多久没来过就要重新授权"的计时起点，
    /// 也是免弹窗窗口的滑动起点 —— 活跃客户端不会反复被打扰。
    pub fn upsert_mcp_client_grant(&mut self, id: &str, permissions: Vec<String>, trusted: bool) {
        let id = id.trim();
        if id.is_empty() || id.chars().any(char::is_control) {
            return;
        }
        let mut list = std::mem::take(&mut self.cache.mcp.mcp_client_grants);
        list.retain(|g| g.id != id);
        list.push(McpClientGrant {
            id: id.to_string(),
            permissions,
            last_auth_unix: unix_now(),
            trusted,
        });
        if list.len() > 32 {
            list.drain(..list.len() - 32);
        }
        self.cache.mcp.mcp_client_grants = list;
    }
    /// 忘掉这个客户端的记忆授权。用户点「拒绝接入」时调用 —— 拒绝之后它不该再靠
    /// 旧授权在 15 分钟窗口里静默重连。
    pub fn remove_mcp_client_grant(&mut self, id: &str) -> bool {
        let before = self.cache.mcp.mcp_client_grants.len();
        self.cache
            .mcp
            .mcp_client_grants
            .retain(|grant| grant.id != id);
        self.cache.mcp.mcp_client_grants.len() != before
    }
    /// 只改「永久信任」标记，不动权限与时间戳。设置页客户端卡片的开关走这里。
    pub fn set_mcp_client_trusted(&mut self, id: &str, trusted: bool) -> bool {
        let Some(grant) = self
            .cache
            .mcp
            .mcp_client_grants
            .iter_mut()
            .find(|grant| grant.id == id)
        else {
            return false;
        };
        grant.trusted = trusted;
        true
    }
    /// AI 执行命令走交互式 PTY（可见）还是一次性 exec 通道（默认，不可见）。
    pub fn mcp_pty_commands(&self) -> bool {
        self.cache.mcp.mcp_pty_commands
    }
    pub fn set_mcp_pty_commands(&mut self, v: bool) {
        self.cache.mcp.mcp_pty_commands = v;
    }
    pub fn mcp_audit_enabled(&self) -> bool {
        self.cache.mcp.mcp_audit_enabled
    }
    pub fn set_mcp_audit_enabled(&mut self, v: bool) {
        self.cache.mcp.mcp_audit_enabled = v;
    }
    /// 保留天数：0 = 永久；其余夹到 1..=365，避免一个 0 或 65535 造成意外。
    pub fn mcp_audit_retention_days(&self) -> u16 {
        match self.cache.mcp.mcp_audit_retention_days {
            0 => 0,
            days => days.clamp(1, 365),
        }
    }
    pub fn set_mcp_audit_retention_days(&mut self, days: u16) {
        self.cache.mcp.mcp_audit_retention_days = days;
    }

    pub fn wallpaper_overlay(&self) -> f32 {
        let a = self.cache.appearance.wallpaper_overlay;
        // Floor lowered 0.40 -> 0.30 so more see-through panels are reachable.
        if a <= 0.0 {
            DEFAULT_WALLPAPER_OVERLAY
        } else {
            a.clamp(0.30, 1.0)
        }
    }
    pub fn set_wallpaper_overlay(&mut self, v: f32) {
        self.cache.appearance.wallpaper_overlay = v.clamp(0.30, 1.0);
    }
    pub fn panel_font(&self) -> u32 {
        if self.cache.appearance.panel_font == 0 {
            100
        } else {
            self.cache.appearance.panel_font
        }
    }
    pub fn set_panel_font(&mut self, percent: u32) {
        self.cache.appearance.panel_font = percent.clamp(80, 160);
    }
    pub fn overview_panel_width(&self) -> f32 {
        let w = self.cache.layout.overview_panel_width;
        if w <= 0.0 { default_overview_width() } else { w }
    }
    pub fn set_overview_panel_width(&mut self, v: f32) {
        self.cache.layout.overview_panel_width = v;
    }
    pub fn overview_panel_height(&self) -> f32 {
        let h = self.cache.layout.overview_panel_height;
        if h <= 0.0 { default_overview_height() } else { h }
    }
    pub fn set_overview_panel_height(&mut self, v: f32) {
        self.cache.layout.overview_panel_height = v;
    }
    /// 曲线跨度只允许六个预设值；老配置 / 脏值回落到 1 分钟。
    pub fn chart_span_secs(&self) -> u32 {
        const ALLOWED: [u32; 6] = [15, 60, 300, 1800, 3600, 21600];
        let v = self.cache.appearance.chart_span_secs;
        if ALLOWED.contains(&v) { v } else { 60 }
    }
    pub fn set_chart_span_secs(&mut self, secs: u32) {
        const ALLOWED: [u32; 6] = [15, 60, 300, 1800, 3600, 21600];
        if ALLOWED.contains(&secs) {
            self.cache.appearance.chart_span_secs = secs;
        }
    }
    /// Last window size in logical px; `(0,0)` means unset (use the default).
    pub fn window_size(&self) -> (f32, f32) {
        (
            self.cache.layout.window_width,
            self.cache.layout.window_height,
        )
    }
    pub fn set_window_size(&mut self, w: f32, h: f32) {
        self.cache.layout.window_width = w;
        self.cache.layout.window_height = h;
    }

    /// Collapse the SFTP panel on startup (default false) (#78).
    pub fn collapse_overview_default(&self) -> bool {
        self.cache.layout.collapse_overview_default
    }

    pub fn set_collapse_overview_default(&mut self, v: bool) {
        self.cache.layout.collapse_overview_default = v;
    }

    /// Mirror SFTP uploads to other sessions while session-sync is on (default
    /// false). Only has effect when the session-sync toggle is on.
    pub fn sync_upload(&self) -> bool {
        self.cache.sync.sync_upload
    }

    pub fn set_sync_upload(&mut self, v: bool) {
        self.cache.sync.sync_upload = v;
    }

    pub fn webdav_enabled(&self) -> bool {
        self.cache.sync.webdav_enabled
    }

    pub fn webdav_url(&self) -> &str {
        &self.cache.sync.webdav_url
    }

    pub fn webdav_username(&self) -> &str {
        &self.cache.sync.webdav_username
    }

    pub fn webdav_password(&self) -> &str {
        self.cache.sync.webdav_password.as_str()
    }

    pub fn webdav_remote_path(&self) -> &str {
        if self.cache.sync.webdav_remote_path.trim().is_empty() {
            "rudder-connections.json"
        } else {
            &self.cache.sync.webdav_remote_path
        }
    }

    pub fn webdav_accept_invalid_certs(&self) -> bool {
        self.cache.sync.webdav_accept_invalid_certs
    }

    pub fn set_webdav_settings(
        &mut self,
        enabled: bool,
        url: String,
        username: String,
        password: String,
        remote_path: String,
        accept_invalid_certs: bool,
    ) {
        self.cache.sync.webdav_enabled = enabled;
        self.cache.sync.webdav_url = url.trim().trim_end_matches('/').to_string();
        self.cache.sync.webdav_username = username.trim().to_string();
        self.cache.sync.webdav_password = Secret::new(password);
        self.cache.sync.webdav_remote_path = if remote_path.trim().is_empty() {
            "rudder-connections.json".to_string()
        } else {
            remote_path.trim().trim_start_matches('/').to_string()
        };
        self.cache.sync.webdav_accept_invalid_certs = accept_invalid_certs;
    }

    /// Whether each download prompts for a save location (default false) (#87).
    pub fn download_always_ask(&self) -> bool {
        self.cache.transfer.download_always_ask
    }

    pub fn set_download_always_ask(&mut self, ask: bool) {
        self.cache.transfer.download_always_ask = ask;
    }

    /// Whether the quick-command bar under the terminal is hidden.
    pub fn cmd_bar_hidden(&self) -> bool {
        self.cache.layout.hide_cmd_bar
    }

    pub fn set_cmd_bar_hidden(&mut self, hidden: bool) {
        self.cache.layout.hide_cmd_bar = hidden;
    }

    /// Zen (focus) mode: sidebar and tab strip hidden.
    pub fn zen_mode(&self) -> bool {
        self.cache.layout.zen_mode
    }

    pub fn set_zen_mode(&mut self, enabled: bool) {
        self.cache.layout.zen_mode = enabled;
    }
}
