use super::*;

pub(super) fn webdav_url(base: &str, remote_path: &str) -> Result<String> {
    let base = base.trim().trim_end_matches('/');
    if !base.starts_with("http://") && !base.starts_with("https://") {
        anyhow::bail!(
            "{}",
            t(
                "WebDAV 地址必须以 http:// 或 https:// 开头",
                "WebDAV URL must start with http:// or https://"
            )
        );
    }
    if base.starts_with("http://") && webdav_url_uses_port(base, 5006) {
        anyhow::bail!(
            "{}",
            t(
                "飞牛 WebDAV 的 5006 通常是 HTTPS 端口，请改用 https://...:5006；如果要用 HTTP，请改用 5005 端口",
                "FnOS WebDAV port 5006 is usually HTTPS; use https://...:5006, or use port 5005 for HTTP"
            )
        );
    }
    if base.starts_with("https://") && webdav_url_uses_port(base, 5005) {
        anyhow::bail!(
            "{}",
            t(
                "飞牛 WebDAV 的 5005 通常是 HTTP 端口，请改用 http://...:5005；如果要用 HTTPS，请改用 5006 端口",
                "FnOS WebDAV port 5005 is usually HTTP; use http://...:5005, or use port 5006 for HTTPS"
            )
        );
    }
    if base.ends_with(".json") {
        return Ok(base.to_string());
    }
    let remote = remote_path.trim().trim_start_matches('/');
    if (webdav_url_uses_port(base, 5005) || webdav_url_uses_port(base, 5006))
        && !webdav_url_has_path(base)
        && !remote.contains('/')
    {
        anyhow::bail!(
            "{}",
            t(
                "飞牛 WebDAV 需要写入某个共享目录，不能直接写到根路径；请把 WebDAV 地址改成 https://IP:5006/all/，或把远端文件改成 all/rudder-connections.json",
                "FnOS WebDAV needs a writable shared folder, not the server root; use https://IP:5006/all/ or set the remote file to all/rudder-connections.json"
            )
        );
    }
    if remote.is_empty() {
        anyhow::bail!("{}", t("远端文件不能为空", "remote file cannot be empty"));
    }
    Ok(format!("{base}/{remote}"))
}

pub(super) fn webdav_url_uses_port(base: &str, port: u16) -> bool {
    let Some(authority) = base.split("://").nth(1) else {
        return false;
    };
    let host_port = authority.split('/').next().unwrap_or(authority);
    host_port
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        == Some(port)
}

pub(super) fn webdav_url_has_path(base: &str) -> bool {
    let Some(authority) = base.split("://").nth(1) else {
        return false;
    };
    authority
        .split_once('/')
        .is_some_and(|(_, path)| !path.is_empty())
}

pub(super) fn webdav_auth_header(username: &str, password: &str) -> Option<String> {
    if username.is_empty() && password.is_empty() {
        return None;
    }
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    Some(format!(
        "Basic {}",
        STANDARD.encode(format!("{username}:{password}"))
    ))
}

pub(super) fn webdav_auth_req(mut req: ureq::Request, auth: Option<&str>) -> ureq::Request {
    if let Some(auth) = auth {
        req = req.set("Authorization", auth);
    }
    req
}

pub(super) fn webdav_agent(accept_invalid_certs: bool) -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(20));
    if accept_invalid_certs {
        let tls_config = ureq::rustls::ClientConfig::builder_with_provider(
            ureq::rustls::crypto::ring::default_provider().into(),
        )
        .with_protocol_versions(&[&ureq::rustls::version::TLS12, &ureq::rustls::version::TLS13])
        .expect("rustls ring provider supports TLS 1.2 and TLS 1.3")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(WebDavAcceptAnyCertVerifier::default()))
        .with_no_client_auth();
        builder = builder.tls_config(Arc::new(tls_config));
    }
    builder.build()
}

pub(super) fn webdav_error(e: ureq::Error) -> anyhow::Error {
    if let ureq::Error::Status(status, response) = e {
        let url = response.get_url().to_string();
        let body = response.into_string().unwrap_or_default();
        let body = body.trim();
        let detail = if body.is_empty() {
            String::new()
        } else {
            format!(": {}", body.chars().take(240).collect::<String>())
        };
        if status == 400 {
            return anyhow::anyhow!(
                "{}: {url}: status code 400{detail}",
                t(
                    "请求被 WebDAV 服务拒绝，请检查地址协议/端口是否匹配，以及远端文件所在目录是否已开启 WebDAV 协议访问",
                    "WebDAV rejected the request; check the URL scheme/port and whether the remote folder allows WebDAV access"
                )
            );
        }
        if status == 405 {
            return anyhow::anyhow!(
                "{}: {url}: status code 405{detail}",
                t(
                    "当前 WebDAV 路径不允许上传；飞牛请写入已开启协议访问的共享目录，例如 WebDAV 地址填 https://IP:5006/all/，或远端文件填 all/rudder-connections.json",
                    "The current WebDAV path does not allow upload; for FnOS, write into a shared folder such as https://IP:5006/all/ or set remote file to all/rudder-connections.json"
                )
            );
        }
        return anyhow::anyhow!("{url}: status code {status}{detail}");
    }
    let msg = e.to_string();
    if msg.contains("UnknownIssuer") || msg.contains("invalid peer certificate") {
        anyhow::anyhow!(
            "{} ({msg})",
            t(
                "HTTPS 证书不受信任；如果这是可信 NAS/局域网 WebDAV，请在设置里开启“信任自签名/内网证书”",
                "HTTPS certificate is not trusted; enable \"Trust self-signed / intranet certs\" for a trusted NAS/LAN WebDAV"
            )
        )
    } else {
        anyhow::anyhow!("{msg}")
    }
}

pub(super) fn webdav_parent_dirs(url: &str) -> Vec<String> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Vec::new();
    };
    let Some((authority, path)) = rest.split_once('/') else {
        return Vec::new();
    };
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.len() <= 1 {
        return Vec::new();
    }
    let mut dirs = Vec::with_capacity(parts.len() - 1);
    let mut current = format!("{scheme}://{authority}");
    for part in parts.iter().take(parts.len() - 1) {
        current.push('/');
        current.push_str(part);
        current.push('/');
        dirs.push(current.clone());
        current.pop();
    }
    dirs
}

pub(super) fn webdav_dir_missing_or_no_create_error() -> anyhow::Error {
    anyhow::anyhow!(
        "{}",
        t(
            "文件夹不存在也无权限创建",
            "folder does not exist and cannot be created"
        )
    )
}

pub(super) fn webdav_dir_exists(
    agent: &ureq::Agent,
    url: &str,
    auth: Option<&str>,
) -> Result<bool> {
    let req = webdav_auth_req(agent.request("PROPFIND", url).set("Depth", "0"), auth);
    match req.call() {
        Ok(_) => Ok(true),
        Err(ureq::Error::Status(status, _)) if status == 404 || status == 409 => Ok(false),
        Err(ureq::Error::Status(status, _)) if status == 401 || status == 403 || status == 405 => {
            Err(webdav_dir_missing_or_no_create_error())
        }
        Err(e) => Err(webdav_error(e)),
    }
}

pub(super) fn webdav_create_dir(agent: &ureq::Agent, url: &str, auth: Option<&str>) -> Result<()> {
    let req = webdav_auth_req(agent.request("MKCOL", url), auth);
    match req.call() {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(405, _)) => Ok(()),
        Err(ureq::Error::Status(status, _))
            if status == 401 || status == 403 || status == 404 || status == 409 =>
        {
            Err(webdav_dir_missing_or_no_create_error())
        }
        Err(e) => Err(webdav_error(e)),
    }
}

pub(super) fn webdav_ensure_parent_dirs(
    agent: &ureq::Agent,
    url: &str,
    auth: Option<&str>,
) -> Result<()> {
    for dir in webdav_parent_dirs(url) {
        if !webdav_dir_exists(agent, &dir, auth)? {
            webdav_create_dir(agent, &dir, auth)?;
        }
    }
    Ok(())
}

pub(super) fn webdav_put_json(
    base_url: &str,
    remote_path: &str,
    username: &str,
    password: &str,
    accept_invalid_certs: bool,
    json: String,
) -> Result<()> {
    let url = webdav_url(base_url, remote_path)?;
    let agent = webdav_agent(accept_invalid_certs);
    let auth = webdav_auth_header(username, password);
    webdav_ensure_parent_dirs(&agent, &url, auth.as_deref())?;
    let req = webdav_auth_req(
        agent.put(&url).set("Content-Type", "application/json"),
        auth.as_deref(),
    );
    req.send_string(&json).map(|_| ()).map_err(webdav_error)
}

pub(super) fn webdav_get_json(
    base_url: &str,
    remote_path: &str,
    username: &str,
    password: &str,
    accept_invalid_certs: bool,
) -> Result<String> {
    let url = webdav_url(base_url, remote_path)?;
    let agent = webdav_agent(accept_invalid_certs);
    let auth = webdav_auth_header(username, password);
    let req = webdav_auth_req(agent.get(&url), auth.as_deref());
    req.call()
        .map_err(webdav_error)?
        .into_string()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_joins_base_and_remote_with_a_single_slash() {
        assert_eq!(
            webdav_url("https://nas:5006/all/", "/rudder.json").unwrap(),
            "https://nas:5006/all/rudder.json",
            "尾斜杠与前导斜杠只留一个"
        );
        assert_eq!(
            webdav_url("  https://nas:5006/all  ", "rudder.json").unwrap(),
            "https://nas:5006/all/rudder.json",
            "两端空白应去掉"
        );
    }

    #[test]
    fn url_rejects_missing_scheme() {
        assert!(webdav_url("nas:5006/all", "x.json").is_err());
    }

    /// 飞牛的 5005/5006 是 HTTP/HTTPS 配对端口；写反了只会撞上难懂的握手错误，
    /// 所以在这里前置拦下并给出改法。
    #[test]
    fn url_rejects_swapped_fnos_ports() {
        assert!(webdav_url("http://nas:5006/all", "x.json").is_err(), "http + 5006");
        assert!(webdav_url("https://nas:5005/all", "x.json").is_err(), "https + 5005");
        assert!(webdav_url("http://nas:5005/all", "x.json").is_ok(), "配对正确要放行");
        assert!(webdav_url("https://nas:5006/all", "x.json").is_ok());
    }

    /// 地址本身就以 `.json` 结尾时**原样返回**（文件名由地址决定，不再拼 remote）。
    #[test]
    fn url_short_circuits_when_the_base_already_points_at_a_file() {
        assert_eq!(
            webdav_url("https://nas:5006/all/rudder.json", "").unwrap(),
            "https://nas:5006/all/rudder.json",
            "即使 remote 为空也直接返回"
        );
    }

    /// 飞牛不允许写到根：地址没带共享目录时，remote 必须自带一段路径。
    #[test]
    fn url_requires_a_shared_folder_on_fnos_ports() {
        assert!(webdav_url("https://nas:5006", "rudder.json").is_err(), "根路径 + 平铺文件名");
        assert!(webdav_url("https://nas:5006/", "rudder.json").is_err(), "根路径（带尾斜杠）");
        assert!(
            webdav_url("https://nas:5006", "all/rudder.json").is_ok(),
            "remote 自带目录则放行"
        );
    }

    #[test]
    fn url_rejects_empty_remote() {
        assert!(webdav_url("https://nas:5006/all", "").is_err());
        assert!(webdav_url("https://nas:5006/all", "   ").is_err());
    }

    #[test]
    fn uses_port_detects_an_explicit_numeric_port_only() {
        assert!(webdav_url_uses_port("https://nas:5006/all/", 5006));
        assert!(webdav_url_uses_port("https://nas:5006", 5006));
        assert!(!webdav_url_uses_port("https://nas:5006/all/", 5005), "端口不符");
        assert!(!webdav_url_uses_port("https://nas/all/", 5006), "没写端口");
        assert!(!webdav_url_uses_port("https://nas:abc/", 5006), "端口不是数字");
        assert!(!webdav_url_uses_port("nas:5006", 5006), "没有 scheme");
    }

    /// 带用户名密码的地址：只有**最后一段**才是端口，密码里的冒号不是分隔符。
    #[test]
    fn uses_port_survives_userinfo() {
        assert!(webdav_url_uses_port("https://u:p@nas:5006/all/", 5006));
        assert!(
            !webdav_url_uses_port("https://u:p@nas/all/", 5006),
            "`u:p@nas` 不能把 `p@nas` 当端口"
        );
    }

    #[test]
    fn has_path_requires_a_non_empty_path_segment() {
        assert!(!webdav_url_has_path("https://nas:5006"));
        assert!(!webdav_url_has_path("https://nas:5006/"), "空路径也算没有");
        assert!(webdav_url_has_path("https://nas:5006/all/"));
        assert!(!webdav_url_has_path("not a url"));
    }

    /// **匿名必须真的匿名**：两个字段都空时不能送 `Basic Og==`（空用户名 + 空密码），
    /// 否则部分 NAS 会直接判成认证失败而不是按匿名处理。
    #[test]
    fn auth_header_is_absent_when_both_fields_are_empty() {
        assert_eq!(webdav_auth_header("", ""), None);
        assert_eq!(
            webdav_auth_header("u", ""),
            Some("Basic dTo=".to_string()),
            "只有用户名"
        );
        assert_eq!(
            webdav_auth_header("", "p"),
            Some("Basic OnA=".to_string()),
            "只有密码"
        );
    }

    /// 返回的是**要逐级创建的父目录**（带尾斜杠），不含文件名本身。
    #[test]
    fn parent_dirs_lists_every_intermediate_directory() {
        assert_eq!(
            webdav_parent_dirs("https://nas:5006/all/sub/x.json"),
            vec!["https://nas:5006/all/", "https://nas:5006/all/sub/"]
        );
    }

    #[test]
    fn parent_dirs_is_empty_when_there_is_nothing_to_create() {
        assert!(webdav_parent_dirs("https://nas:5006/x.json").is_empty(), "顶层文件");
        assert!(webdav_parent_dirs("https://nas:5006/all/").is_empty(), "只有一层目录");
        assert!(webdav_parent_dirs("not a url").is_empty(), "没有 scheme");
        assert!(webdav_parent_dirs("https://nas").is_empty(), "没有路径段");
    }
}
