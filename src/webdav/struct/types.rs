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

/// Normalise a user-supplied fingerprint: trim, **strip the separators users
/// habitually type** (`AB:CD:EF…` / `ab cd ef…` / `ab-cd-ef…`), fold to lowercase
/// (the verifier compares against lowercase hex without separators), and treat an
/// empty string as "no pin".
///
/// ⚠ 不剥分隔符的话，最常见的 `AB:CD:EF…` 写法**永远匹配失败** —— 校验端
/// （`certificate_verifier.rs`）拼出来的是连续小写 hex。
fn normalize_pin(pin: &str) -> Option<String> {
    let cleaned: String = pin
        .chars()
        .filter(|c| !matches!(c, ':' | ' ' | '-' | '.'))
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_lowercase())
    }
}

pub(crate) fn set_webdav_cert_pin(pin: String) {
    let _ = WEBDAV_CERT_PIN.set(normalize_pin(&pin));
}

pub(crate) fn webdav_cert_pin() -> Option<&'static str> {
    // `get()` rather than `get_or_init(|| None)`: with the latter, any read
    // that happened before `set_webdav_cert_pin` ran at startup would freeze
    // the cell to `None` permanently, silently disabling pinning for the rest
    // of the process. `get()` simply reports "not set yet" instead.
    WEBDAV_CERT_PIN.get().and_then(|v| v.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_normalisation_strips_separators_and_lowercases() {
        // 校验端拼的是连续小写 hex，所以这三种常见写法归一后必须一致。
        assert_eq!(normalize_pin("  AB:CD:EF  ").as_deref(), Some("abcdef"));
        assert_eq!(normalize_pin("ab cd ef").as_deref(), Some("abcdef"));
        assert_eq!(normalize_pin("ab-cd.ef").as_deref(), Some("abcdef"));
        assert_eq!(normalize_pin(""), None);
        assert_eq!(normalize_pin("   "), None);
        // 只有分隔符 → 视为"没填"，而不是一个永远匹配不上的 pin。
        assert_eq!(normalize_pin(" : - . "), None);
    }
}
