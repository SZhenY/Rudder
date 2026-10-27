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

pub(super) fn webdav_auth(
    req: reqwest::blocking::RequestBuilder,
    auth: Option<&str>,
) -> reqwest::blocking::RequestBuilder {
    match auth {
        Some(auth) => req.header("Authorization", auth),
        None => req,
    }
}

/// WebDAV 客户端。
///
/// 「信任自签名证书」走 `use_preconfigured_tls` + 我们自己的校验器，而不是 reqwest 的
/// `danger_accept_invalid_certs`：后者会丢掉**证书指纹固定**（配了指纹就只认那一张证书）。
pub(super) fn webdav_agent(accept_invalid_certs: bool) -> reqwest::blocking::Client {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20));
    if accept_invalid_certs {
        if let Some(cfg) = accept_any_tls_config() {
            builder = builder.use_preconfigured_tls(cfg);
        } else {
            builder = builder.danger_accept_invalid_certs(true);
        }
    }
    builder.build().expect("build WebDAV HTTP client")
}

/// 接受任意（或仅匹配指纹的）服务端证书的 rustls 配置。
fn accept_any_tls_config() -> Option<rustls::ClientConfig> {
    use std::sync::Arc;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Some(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .ok()?
            // `dangerous()` 必须在 WantsVerifier 阶段调用：自定义校验器取代根证书校验，
            // 因此这里不建 RootCertStore。
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(WebDavAcceptAnyCertVerifier::default()))
            .with_no_client_auth(),
    )
}

/// 非 2xx 的响应 → 带状态码与响应体片段的错误（400/405 有专门文案）。
pub(super) fn webdav_status_error(resp: reqwest::blocking::Response) -> anyhow::Error {
    let status = resp.status().as_u16();
    let url = resp.url().to_string();
    let body = resp.text().unwrap_or_default();
    webdav_status_message(status, &url, body.trim())
}

fn webdav_status_message(status: u16, url: &str, body: &str) -> anyhow::Error {
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
    anyhow::anyhow!("{url}: status code {status}{detail}")
}

/// 传输层错误（连接 / TLS / 超时）→ 人话。
pub(super) fn webdav_error(e: reqwest::Error) -> anyhow::Error {
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(src) = source {
        msg.push_str(" | ");
        msg.push_str(&src.to_string());
        source = src.source();
    }
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
    agent: &reqwest::blocking::Client,
    url: &str,
    auth: Option<&str>,
) -> Result<bool> {
    let method = reqwest::Method::from_bytes(b"PROPFIND").expect("PROPFIND is a valid method");
    let resp = webdav_auth(agent.request(method, url).header("Depth", "0"), auth)
        .send()
        .map_err(webdav_error)?;
    match resp.status().as_u16() {
        200..=299 => Ok(true),
        404 | 409 => Ok(false),
        401 | 403 | 405 => Err(webdav_dir_missing_or_no_create_error()),
        _ => Err(webdav_status_error(resp)),
    }
}

pub(super) fn webdav_create_dir(
    agent: &reqwest::blocking::Client,
    url: &str,
    auth: Option<&str>,
) -> Result<()> {
    let method = reqwest::Method::from_bytes(b"MKCOL").expect("MKCOL is a valid method");
    let resp = webdav_auth(agent.request(method, url), auth)
        .send()
        .map_err(webdav_error)?;
    match resp.status().as_u16() {
        200..=299 => Ok(()),
        405 => Ok(()),
        401 | 403 | 404 | 409 => Err(webdav_dir_missing_or_no_create_error()),
        _ => Err(webdav_status_error(resp)),
    }
}

pub(super) fn webdav_ensure_parent_dirs(
    agent: &reqwest::blocking::Client,
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
    let resp = webdav_auth(
        agent.put(&url).header("Content-Type", "application/json"),
        auth.as_deref(),
    )
    .body(json)
    .send()
    .map_err(webdav_error)?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(webdav_status_error(resp))
    }
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
    let resp = webdav_auth(agent.get(&url), auth.as_deref())
        .send()
        .map_err(webdav_error)?;
    if !resp.status().is_success() {
        return Err(webdav_status_error(resp));
    }
    resp.text().map_err(|e| anyhow::anyhow!("{e}"))
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
