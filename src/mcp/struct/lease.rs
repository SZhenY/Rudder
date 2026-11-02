//! 客户端租约与「记住的授权」：谁连着、这次给了哪些权限。

use std::time::Duration;
use std::time::Instant;
use rmcp::RoleServer;
use rmcp::service::Peer;
use super::perms::Permissions;

/// 会话 id 的前 8 个字符。审计、日志与设置页用它标识一次连接 ——
/// 完整 id 是 32 位随机串，进日志与界面都太长。
pub(crate) fn session8(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

/// 在记忆授权里找这个客户端的记录。
///
/// 精确匹配优先；找不到时退回 `name version`（升级前的格式没有 `@ip` 后缀），
/// 让老信任还能用一轮 —— 否则升级后所有客户端都要重新授权。
pub(crate) fn find_grant<'a>(
    grants: &'a [crate::config::settings::McpClientGrant],
    client_id: &str,
) -> Option<&'a crate::config::settings::McpClientGrant> {
    if let Some(found) = grants.iter().find(|grant| grant.id == client_id) {
        return Some(found);
    }
    let legacy = client_id.split('@').next().unwrap_or(client_id);
    grants.iter().find(|grant| grant.id == legacy)
}

/// 已记住的授权能否**直接放行（不弹窗）**。
///
/// 三个条件里任一满足即可：用户勾了「永久信任」；或上次授权距现在不足
/// `window_secs`（一个租约空闲期，15 分钟）。权限集合为空时一律重新弹窗 ——
/// 那是"用户上次一位都没给"，不该被当成"已授权"。
pub(crate) fn grant_allows_silent(
    grant: &crate::config::settings::McpClientGrant,
    now_unix: u64,
    window_secs: u64,
) -> bool {
    // 「永久信任」先判：弹窗允许一位都不勾（那样只能调 get_server_info），
    // 这种客户端勾了永久信任后不该被反复打扰，否则设置页的开关等于摆设。
    if grant.trusted {
        return true;
    }
    if grant.permissions.is_empty() {
        return false;
    }
    grant.last_auth_unix > 0 && now_unix.saturating_sub(grant.last_auth_unix) < window_secs
}

/// 记忆授权里存的权限 key 列表 → 权限位。
///
/// 连接静默放行、弹窗预勾、AI 页摘要三处都要做同一次转换，集中在这里。
pub(crate) fn grant_perms(grant: &crate::config::settings::McpClientGrant) -> Permissions {
    let keys: Vec<&str> = grant.permissions.iter().map(String::as_str).collect();
    Permissions::from_keys(&keys)
}

/// 一个已接入客户端的可展示形态（AI 页的客户端卡）。
#[derive(Debug, Clone)]
pub(crate) struct McpSessionInfo {
    pub(crate) session_id: String,
    pub(crate) client: String,
    pub(crate) connected_wall: String,
    pub(crate) connected_secs: u64,
    pub(crate) granted: Permissions,
    /// 来源：`local` 或 IP；空串表示拿不到。用于在卡片上区分"同一台设备"与"不同设备"。
    pub(crate) peer_ip: String,
    /// 该客户端的记忆标识；`None` = 客户端没上报名字，无法记忆/信任。
    pub(crate) client_id: Option<String>,
    /// 「永久信任」当前状态（来自配置里的 grant），驱动卡片开关。
    pub(crate) trusted: bool,
}

/// 一个客户端会话。多个客户端各自一份，权限只活在这里。
#[derive(Debug, Clone)]
pub(crate) struct ClientLease {
    pub(crate) session_id: String,
    /// 连接来源 IP（initialize 那一刻记录）。用于区分"不同设备上的同名客户端"。
    pub(crate) peer_ip: Option<std::net::IpAddr>,
    pub(crate) last_seen: Instant,
    /// 建立时刻（单调，用于算"已连接多久"）。
    pub(crate) connected_at: Instant,
    /// 展示用的壁钟（`HH:MM:SS`）。
    pub(crate) connected_wall: String,
    /// 客户端自称的名字 / 版本，来自 `initialize` 的 `clientInfo`。
    pub(crate) client_name: String,
    pub(crate) client_version: String,
    /// 授权状态机。
    pub(crate) verdict: ConsentState,
    /// 本会话的授权位。
    pub(crate) granted: Permissions,
    /// 重连 / 授权变化时 +1，用来打断在途调用。
    pub(crate) epoch: u64,
    /// 用来在授权变化后发 `notifications/tools/list_changed`。
    pub(crate) peer: Option<Peer<RoleServer>>,
}

/// 会话授权状态。拒绝 / 超时会把租约整条移除，所以活着的只有"待确认"与"已授权"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsentState {
    Pending,
    Granted,
}

impl ClientLease {
    /// 空闲这么久就自动释放，避免客户端崩溃后端口永久占着。
    pub(crate) const IDLE: Duration = Duration::from_secs(15 * 60);

    /// 弹窗后等用户决定的时长。超时 = 未接入，租约会被移除。
    pub(crate) const CONSENT_TIMEOUT: Duration = Duration::from_secs(45);

    pub(crate) fn new(
        session_id: String,
        epoch: u64,
        peer_ip: Option<std::net::IpAddr>,
    ) -> Self {
        Self {
            session_id,
            peer_ip,
            last_seen: Instant::now(),
            connected_at: Instant::now(),
            connected_wall: chrono::Local::now().format("%H:%M:%S").to_string(),
            client_name: String::new(),
            client_version: String::new(),
            verdict: ConsentState::Pending,
            granted: Permissions::default(),
            epoch,
            peer: None,
        }
    }

    pub(crate) fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_seen) > Self::IDLE
    }

    /// `"Claude Desktop 0.7.2"`；未上报时给个占位。
    pub(crate) fn client_label(&self) -> String {
        let name = if self.client_name.is_empty() {
            "unknown client"
        } else {
            self.client_name.as_str()
        };
        if self.client_version.is_empty() {
            name.to_string()
        } else {
            format!("{name} {}", self.client_version)
        }
    }

    /// 来源的展示形式：本机回环统一显示 `local`，其余显示 IP；拿不到时为空串。
    pub(crate) fn peer_label(&self) -> String {
        match self.peer_ip {
            Some(ip) if ip.is_loopback() => "local".to_string(),
            Some(ip) => ip.to_string(),
            None => String::new(),
        }
    }

    /// 用于记忆授权的身份；未上报名字时返回 `None`（不持久化 unknown client）。
    ///
    /// 末尾附带来源（`@local` / `@192.168.1.5`）：同一台设备的多个连接共享一条记忆授权，
    /// 不同设备各自独立 —— 否则一个 token 多设备时会互相覆盖。
    pub(crate) fn client_id(&self) -> Option<String> {
        let name = self.client_name.trim();
        if name.is_empty() {
            return None;
        }
        let version = self.client_version.trim();
        let raw = if version.is_empty() {
            name.to_string()
        } else {
            format!("{name} {version}")
        };
        let cleaned: String = raw
            .chars()
            .filter(|ch| !ch.is_control())
            .take(128)
            .collect();
        if cleaned.is_empty() {
            return None;
        }
        Some(match self.peer_ip {
            // 回环统一归一化：127.0.0.1 与 ::1 是同一台本机。
            Some(ip) if ip.is_loopback() => format!("{cleaned}@local"),
            Some(ip) => format!("{cleaned}@{ip}"),
            None => cleaned,
        })
    }
}

#[cfg(test)]
mod grant_tests {
    use super::grant_allows_silent;
    use crate::config::settings::McpClientGrant;

    fn grant(trusted: bool, last_auth_unix: u64, perms: &[&str]) -> McpClientGrant {
        McpClientGrant {
            id: "Claude Desktop 0.7.2".to_string(),
            permissions: perms.iter().map(|key| (*key).to_string()).collect(),
            last_auth_unix,
            trusted,
        }
    }

    const WINDOW: u64 = 15 * 60;

    /// 永久信任：无论多久没来都直接放行。
    #[test]
    fn trusted_client_never_prompts() {
        assert!(grant_allows_silent(
            &grant(true, 0, &["read"]),
            1_000_000,
            WINDOW
        ));
    }

    /// 一个租约空闲期内来过的客户端不再被弹窗 —— 这正是"每次操作都弹"要解决的问题。
    #[test]
    fn recent_grant_within_the_window_is_silent() {
        let now = 1_000_000;
        assert!(grant_allows_silent(
            &grant(false, now - 60, &["read"]),
            now,
            WINDOW
        ));
        assert!(grant_allows_silent(
            &grant(false, now - WINDOW + 1, &["read"]),
            now,
            WINDOW
        ));
    }

    /// 超过一个租约空闲期就要重新授权。
    #[test]
    fn stale_grant_requires_a_new_prompt() {
        let now = 1_000_000;
        assert!(!grant_allows_silent(
            &grant(false, now - WINDOW, &["read"]),
            now,
            WINDOW
        ));
        assert!(!grant_allows_silent(
            &grant(false, now - 3600, &["read"]),
            now,
            WINDOW
        ));
    }

    /// 上次一位权限都没给：不算"已授权"，要重新问 —— 但**勾了永久信任的例外**，
    /// 否则设置页里那个开关对这类客户端等于摆设。
    #[test]
    fn empty_permissions_prompt_unless_trusted() {
        assert!(!grant_allows_silent(
            &grant(false, 1_000_000, &[]),
            1_000_000,
            WINDOW
        ));
        assert!(grant_allows_silent(
            &grant(true, 1_000_000, &[]),
            1_000_000,
            WINDOW
        ));
    }

    /// 从没授权过（时间戳 0）的老配置：必须走弹窗，不能被算进免弹窗口。
    #[test]
    fn never_authorised_grant_prompts() {
        assert!(!grant_allows_silent(
            &grant(false, 0, &["read"]),
            1_000_000,
            WINDOW
        ));
    }
}

#[cfg(test)]
mod client_identity_tests {
    use super::{ClientLease, find_grant};
    use crate::config::settings::McpClientGrant;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn lease(name: &str, version: &str, peer: Option<IpAddr>) -> ClientLease {
        let mut lease = ClientLease::new("session-1".to_string(), 1, peer);
        lease.client_name = name.to_string();
        lease.client_version = version.to_string();
        lease
    }

    /// 不同设备上的同名客户端必须得到不同身份 —— 这是"一个 token 多设备"能分开管的前提。
    #[test]
    fn same_name_on_different_devices_gets_distinct_ids() {
        let a = lease(
            "rudder",
            "1.0.0",
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5))),
        );
        let b = lease(
            "rudder",
            "1.0.0",
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 9))),
        );
        assert_eq!(a.client_id().as_deref(), Some("rudder 1.0.0@192.168.1.5"));
        assert_eq!(b.client_id().as_deref(), Some("rudder 1.0.0@192.168.1.9"));
        assert_ne!(a.client_id(), b.client_id());
    }

    /// 同一台设备上的两个连接共享身份（`@local`），这与"同一台机器本来就该共享信任"一致。
    #[test]
    fn same_device_shares_the_id() {
        let first = lease("rudder", "1.0.0", Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        let second = lease("rudder", "1.0.0", Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_eq!(first.client_id(), second.client_id());
    }

    /// 回环地址归一化：127.0.0.1 与 ::1 是同一台本机，不该被当成两台设备。
    #[test]
    fn loopback_is_normalised_to_local() {
        let v4 = lease("rudder", "1.0.0", Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        let v6 = lease("rudder", "1.0.0", Some(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert_eq!(v4.client_id().as_deref(), Some("rudder 1.0.0@local"));
        assert_eq!(v4.client_id(), v6.client_id());
    }

    /// 拿不到来源时退回旧格式（测试环境与老路径仍要能工作）。
    #[test]
    fn missing_peer_falls_back_to_legacy_id() {
        assert_eq!(
            lease("rudder", "1.0.0", None).client_id().as_deref(),
            Some("rudder 1.0.0")
        );
    }

    /// 没上报名字的客户端不记忆（返回 None）。
    #[test]
    fn unnamed_client_has_no_id() {
        assert_eq!(lease("", "1.0.0", None).client_id(), None);
    }

    /// 升级前的老记录没有 `@ip` 后缀：要能被回退命中，否则升级后全员重授权。
    #[test]
    fn legacy_grant_is_matched_before_the_suffix() {
        let legacy = McpClientGrant {
            id: "rudder 1.0.0".to_string(),
            permissions: vec!["read".to_string()],
            last_auth_unix: 1,
            trusted: false,
        };
        let fresh = McpClientGrant {
            id: "rudder 1.0.0@192.168.1.5".to_string(),
            permissions: vec!["read".to_string(), "exec".to_string()],
            last_auth_unix: 2,
            trusted: true,
        };
        let grants = vec![legacy.clone(), fresh.clone()];

        // 精确命中优先：新格式存在时不会被老记录抢走。
        let hit = find_grant(&grants, "rudder 1.0.0@192.168.1.5").expect("应命中新记录");
        assert_eq!(hit.id, fresh.id);
        assert!(hit.trusted);

        // 另一台设备只能命中老记录（过渡期行为）。
        let fallback = find_grant(&grants, "rudder 1.0.0@192.168.1.9").expect("应回退命中老记录");
        assert_eq!(fallback.id, legacy.id);
        assert!(!fallback.trusted);
    }
}
