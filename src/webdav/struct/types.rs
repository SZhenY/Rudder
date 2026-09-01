#[derive(Debug, Default)]
pub(crate) struct WebDavAcceptAnyCertVerifier {
    /// Optional SHA-256 certificate fingerprint (lowercase hex). When set,
    /// only a server presenting a cert with this fingerprint passes — a much
    /// tighter fallback than "accept any" for the accept-invalid-certs path.
    pub(crate) pin: Option<String>,
}

/// Global pin set once at startup from the config store (mirrors the
/// OSC52_ENABLED pattern) so `webdav_agent` call sites need no signature churn.
pub(crate) static WEBDAV_CERT_PIN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

pub(crate) fn set_webdav_cert_pin(pin: String) {
    let _ = WEBDAV_CERT_PIN.set(if pin.trim().is_empty() {
        None
    } else {
        Some(pin.trim().to_lowercase())
    });
}

pub(crate) fn webdav_cert_pin() -> Option<&'static str> {
    WEBDAV_CERT_PIN.get_or_init(|| None).as_deref()
}
