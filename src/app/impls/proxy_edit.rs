pub(crate) fn split_proxy(url: &str) -> (String, String) {
    let s = url.trim();
    if s.is_empty() {
        return ("none".to_string(), String::new());
    }
    let lower = s.to_ascii_lowercase();
    for p in ["http://", "https://"] {
        if lower.starts_with(p) {
            return (
                "http".to_string(),
                s[p.len()..].trim_end_matches('/').to_string(),
            );
        }
    }
    for p in ["socks5h://", "socks5://", "socks://"] {
        if lower.starts_with(p) {
            return (
                "socks5".to_string(),
                s[p.len()..].trim_end_matches('/').to_string(),
            );
        }
    }
    ("socks5".to_string(), s.trim_end_matches('/').to_string())
}

/// 把对话框里的代理输入整理成配置的三段：`(url, user, password)`。
///
/// 地址框允许写 `user:pass@host:port`（老配置就是这样），但密码写在 URL 里
/// 就没法按密码字段加密落盘，所以保存时统一把它摘出来：
///
/// * `proxy_type == "none"` 或地址为空 → 三段全空，凭据一并清掉；
/// * 用户名以独立输入框为准，空则取 URL 里的 userinfo；
/// * 密码留空表示"保留已存的"：依次回退到 URL 里的密码、同一用户名下的旧密码。
pub(crate) fn compose_proxy(
    proxy_type: &str,
    hostport: &str,
    user: &str,
    password: &str,
    existing: Option<&crate::config::Session>,
) -> (String, String, crate::config::Secret) {
    let hostport = hostport.trim();
    if proxy_type == "none" || hostport.is_empty() {
        return (String::new(), String::new(), crate::config::Secret::default());
    }
    let (url_user, url_pass, hostport) = match hostport.rsplit_once('@') {
        Some((userinfo, rest)) => {
            let (u, p) = userinfo.split_once(':').unwrap_or((userinfo, ""));
            (u.to_string(), p.to_string(), rest)
        }
        None => (String::new(), String::new(), hostport),
    };
    let final_user = if user.trim().is_empty() {
        url_user
    } else {
        user.trim().to_string()
    };
    let saved = existing
        .filter(|session| session.proxy_user == final_user)
        .map(|session| session.proxy_pass.clone())
        .unwrap_or_default();
    let final_pass = if !password.is_empty() {
        crate::config::Secret::new(password.to_string())
    } else if !url_pass.is_empty() {
        crate::config::Secret::new(url_pass)
    } else {
        saved
    };
    (
        format!("{proxy_type}://{hostport}"),
        final_user,
        final_pass,
    )
}
