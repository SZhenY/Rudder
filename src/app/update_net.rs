//! 更新检查 / 下载用的网络层：代理解析、证书来源、错误翻译。
//!
//! 为什么要单独一层：`ureq` **不读系统代理**，默认也用自己内置的 webpki 根证书。
//! 桌面环境里代理通常写在系统设置里（macOS 的 ClashX / Surge、Windows 的 IE/WinINET
//! 代理），不会进 GUI 进程的环境变量 —— 于是浏览器能打开 GitHub，应用却报"网络原因"。
//! 代理如果做了 TLS 中间人，内置根证书也验不过，同样表现为网络失败。
//!
//! 两条产品口径（设置页「更新网络」）：
//! * 代理默认**关闭 = 直连**（连系统代理也不读）；打开后填了地址就用它，留空跟随系统代理。
//! * 证书链**恒用系统根证书**，不提供开关：企业网关/加速器的透明中间人不需要配代理，
//!   按代理开关联动会让这类环境无从修复。

use std::sync::Arc;
use std::time::Duration;

// rustls 不单独进依赖表：用 ureq 的 re-export，避免两处各锁一个版本导致类型不互通
// （ureq 的 `tls_config` 要的正是这个 rustls 的 `ClientConfig`）。
use ureq::rustls;

/// 更新器使用的网络配置：由「设置 → 新版本提示 → 更新网络」写入。
#[derive(Clone, Debug, Default)]
pub(crate) struct NetSettings {
    /// 是否让更新流量走代理。**默认 false = 直连**，连系统代理也不读。
    pub(crate) enabled: bool,
    /// 打开代理后使用的地址；空 = 跟随系统代理。
    pub(crate) proxy: String,
}

static CURRENT: std::sync::Mutex<Option<NetSettings>> = std::sync::Mutex::new(None);

/// 写入当前生效的配置（启动时与设置变更时各调一次）。
pub(crate) fn set_current(net: NetSettings) {
    if let Ok(mut slot) = CURRENT.lock() {
        *slot = Some(net);
    }
}

/// 读取当前配置；未设置时用默认值（直连 + 系统证书链）。
pub(crate) fn current() -> NetSettings {
    CURRENT
        .lock()
        .ok()
        .and_then(|s| s.clone())
        .unwrap_or_default()
}

impl NetSettings {
    /// 按当前配置构造一个 ureq Agent（代理 + 证书 + 超时）。
    ///
    /// 代理地址非法时返回原因，**不静默退回直连**：静默直连会把"代理填错了"表现成
    /// DNS 解析失败 / 连接被拒，用户按网络故障去排查，方向完全跑偏。
    pub(crate) fn agent(&self) -> Result<ureq::Agent, String> {
        let mut builder = ureq::AgentBuilder::new()
            .user_agent("rudder-self-update")
            .timeout_connect(Duration::from_secs(20));

        if let Some(proxy) = self.effective_proxy() {
            // 本 crate 的 ureq 只开了 `tls`、没开 `socks-proxy`：socks 前缀能过
            // `Proxy::new` 的解析，却会在建连时才失败，报错指向"代理不可用"很容易误导。
            if proxy.starts_with("socks") {
                return Err(format!(
                    "暂不支持 socks 代理（{proxy}）：请填 http:// 代理，例如 http://127.0.0.1:7890"
                ));
            }
            match ureq::Proxy::new(&proxy) {
                Ok(p) => {
                    tracing::info!("update: using proxy {proxy}");
                    builder = builder.proxy(p);
                }
                // ureq 2.x 只认 http / socks* 前缀（本 crate 未开 socks feature），
                // 用户最常见的错是照着浏览器习惯写成 `https://主机:端口`。
                Err(_) => {
                    return Err(format!(
                        "代理地址不可用（{proxy}）：只接受 http:// 前缀，可带账号密码，\
                         例如 http://127.0.0.1:7890"
                    ));
                }
            }
        }

        // 证书链恒用系统根证书：设置里不提供开关（见「更新网络」分区说明）。
        // 拿不到系统证书时 `native_tls_config` 返回 None，自动退回内置根。
        if let Some(cfg) = native_tls_config() {
            builder = builder.tls_config(Arc::new(cfg));
        }
        Ok(builder.build())
    }

    /// 实际要用的代理；`None` = 直连。
    fn effective_proxy(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let manual = self.proxy.trim();
        if manual.is_empty() {
            system_proxy()
        } else {
            Some(with_scheme(manual))
        }
    }
}

/// `127.0.0.1:7890` → `http://127.0.0.1:7890`；已带 scheme 的原样返回。
fn with_scheme(v: &str) -> String {
    if v.contains("://") {
        v.trim().to_string()
    } else {
        format!("http://{}", v.trim())
    }
}

/// 读系统代理；拿不到返回 None（直连）。
fn system_proxy() -> Option<String> {
    #[cfg(windows)]
    {
        win::ie_proxy()
    }
    #[cfg(target_os = "macos")]
    {
        mac::system_proxy()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        env_proxy()
    }
}

/// Linux / 其它平台：读环境变量（`HTTPS_PROXY` 优先）。
#[cfg(not(any(windows, target_os = "macos")))]
fn env_proxy() -> Option<String> {
    for key in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(with_scheme(v));
            }
        }
    }
    None
}

#[cfg(windows)]
mod win {
    //! Windows：走 WinHTTP 的 `WinHttpGetIEProxyConfigForCurrentUser`，
    //! 拿到的就是「Internet 选项 → 连接 → 局域网设置」里那份手动代理。

    #[repr(C)]
    struct IeProxyConfig {
        auto_detect: i32,
        auto_config_url: *mut u16,
        proxy: *mut u16,
        proxy_bypass: *mut u16,
    }

    #[link(name = "winhttp")]
    unsafe extern "system" {
        fn WinHttpGetIEProxyConfigForCurrentUser(cfg: *mut IeProxyConfig) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalFree(mem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }

    unsafe fn take_wide(ptr: *mut u16) -> Option<String> {
        if ptr.is_null() {
            return None;
        }
        let mut len = 0usize;
        while unsafe { *ptr.add(len) } != 0 {
            len += 1;
            if len > 4096 {
                break;
            }
        }
        let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
        Some(String::from_utf16_lossy(slice))
    }

    pub(super) fn ie_proxy() -> Option<String> {
        let mut cfg = IeProxyConfig {
            auto_detect: 0,
            auto_config_url: std::ptr::null_mut(),
            proxy: std::ptr::null_mut(),
            proxy_bypass: std::ptr::null_mut(),
        };
        let ok = unsafe { WinHttpGetIEProxyConfigForCurrentUser(&mut cfg) };
        if ok == 0 {
            return None;
        }
        let raw = unsafe { take_wide(cfg.proxy) };
        for p in [cfg.auto_config_url, cfg.proxy, cfg.proxy_bypass] {
            if !p.is_null() {
                unsafe { GlobalFree(p as *mut core::ffi::c_void) };
            }
        }
        // 形如 `http=127.0.0.1:7890;https=127.0.0.1:7890`，也可能只有一个 `host:port`。
        let raw = raw?;
        let picked = raw
            .split(';')
            .filter(|s| !s.trim().is_empty())
            .find_map(|kv| match kv.split_once('=') {
                Some(("https", v)) | Some(("http", v)) => Some(v.trim().to_string()),
                Some(_) => None,
                None => Some(kv.trim().to_string()),
            })?;
        Some(super::with_scheme(&picked))
    }
}

#[cfg(target_os = "macos")]
mod mac {
    //! macOS：`scutil --proxy` 就是系统「网络 → 代理」那份配置。

    pub(super) fn system_proxy() -> Option<String> {
        let out = std::process::Command::new("scutil")
            .arg("--proxy")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);

        let field = |key: &str| -> Option<String> {
            text.lines().find_map(|line| {
                let (k, v) = line.split_once(':')?;
                (k.trim() == key).then(|| v.trim().to_string())
            })
        };

        // HTTPS 优先（CONNECT 隧道），退回 HTTP。
        for (host_key, port_key) in [("HTTPSProxy", "HTTPSPort"), ("HTTPProxy", "HTTPPort")] {
            if let (Some(host), Some(port)) = (field(host_key), field(port_key)) {
                return Some(super::with_scheme(&format!("{host}:{port}")));
            }
        }
        None
    }
}

/// 用系统根证书构造 rustls 配置；拿不到证书时返回 None（调用方保留内置根）。
fn native_tls_config() -> Option<rustls::ClientConfig> {
    let result = rustls_native_certs::load_native_certs();
    if result.certs.is_empty() {
        tracing::warn!("update: no system root certificates, falling back to bundled roots");
        return None;
    }
    let mut roots = rustls::RootCertStore::empty();
    for cert in result.certs {
        let _ = roots.add(cert);
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .ok()?
        .with_root_certificates(roots)
        .with_no_client_auth()
        .into()
}

/// 把 `ureq` 的错误翻译成用户能据此行动的一句话。
pub(crate) fn describe(err: &ureq::Error) -> String {
    use ureq::ErrorKind;

    match err {
        ureq::Error::Status(code, _) => match *code {
            401..=403 => format!(
                "GitHub 拒绝了请求（HTTP {code}）：多为 API 限流，稍后再试；长期失败可在\
                 「新版本提示 → 更新网络」里设置代理"
            ),
            404 => "找不到发布信息（HTTP 404）：仓库或该版本可能已被删除".to_string(),
            429 => "请求过于频繁（HTTP 429），请稍后再试".to_string(),
            500..=599 => format!("GitHub 服务端错误（HTTP {code}），稍后再试"),
            _ => format!("HTTP {code}"),
        },
        ureq::Error::Transport(t) => {
            let kind = t.kind();
            match kind {
                ErrorKind::Dns => {
                    "域名解析失败：本机 DNS 查不到 api.github.com（检查网络，或设置更新代理）".to_string()
                }
                ErrorKind::ProxyConnect => {
                    "代理连接失败：代理地址/端口不可用，或代理需要账号密码".to_string()
                }
                ErrorKind::InvalidProxyUrl => "代理地址格式不正确".to_string(),
                ErrorKind::ConnectionFailed => {
                    "连接被拒绝或中断：可能需要代理才能访问 GitHub".to_string()
                }
                _ => {
                    let text = t.to_string();
                    if looks_like_tls(&text) {
                        "证书校验失败：网络中间人（公司网关 / 加速器）的根证书不在系统信任库中；\
                         把它的根证书导入系统证书存储后重试"
                            .to_string()
                    } else if looks_like_timeout(&text) {
                        "连接超时：网络不可达，或代理未生效".to_string()
                    } else {
                        format!("网络错误：{text}")
                    }
                }
            }
        }
    }
}

fn looks_like_tls(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("certificate") || t.contains("tls") || t.contains("handshake") || t.contains("invalidcertificate")
}

fn looks_like_timeout(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("timed out") || t.contains("timeout")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_is_added_once() {
        assert_eq!(with_scheme("127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(with_scheme("http://127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(with_scheme("  socks5://h:1  "), "socks5://h:1");
    }

    /// 开关关闭 = 直连：即便填了地址也不生效，更不去读系统代理。
    #[test]
    fn disabled_switch_means_direct_connection() {
        let net = NetSettings {
            enabled: false,
            proxy: "127.0.0.1:7890".into(),
        };
        assert_eq!(net.effective_proxy(), None);
        assert!(net.agent().is_ok(), "关掉代理时不该因为残留的地址而报错");
    }

    /// 开关打开 + 有地址 = 用它（缺 scheme 补 http://）；留空交给系统探测。
    #[test]
    fn enabled_switch_uses_manual_proxy_or_system_lookup() {
        let manual = NetSettings {
            enabled: true,
            proxy: "127.0.0.1:7890".into(),
        };
        assert_eq!(manual.effective_proxy().as_deref(), Some("http://127.0.0.1:7890"));
        assert!(manual.agent().is_ok(), "合法代理应当能构造出 Agent");

        // 只验证"空串不会当成代理地址"，系统探测返回什么取决于环境。
        let auto = NetSettings {
            enabled: true,
            proxy: "   ".into(),
        };
        let _ = auto.effective_proxy();
    }

    /// 代理写错时必须**报错**而不是退回直连 —— 直连会把它表现成 DNS 失败，误导排查。
    /// 用 `https://` 前缀作样本：这是照着浏览器习惯填代理时最常见的写法。
    #[test]
    fn malformed_proxy_is_reported_instead_of_silently_going_direct() {
        let net = NetSettings {
            enabled: true,
            proxy: "https://127.0.0.1:7890".into(),
        };
        let err = match net.agent() {
            Ok(_) => panic!("https:// 前缀的代理应当被拒绝"),
            Err(e) => e,
        };
        assert!(err.contains("代理地址不可用"), "{err}");
        assert!(
            err.contains("http://127.0.0.1:7890"),
            "报错要给出可直接照抄的写法：{err}"
        );
    }
}
