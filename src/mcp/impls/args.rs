//! 工具参数的取值规则。
//!
//! 权限判定（`tools::required_perms`）与执行（`calls`）必须用同一套规则，否则会出现
//! "权限门放过了、执行时又报参数错"这类前后不一致。schema 里要求 `minLength: 1`
//! 的字段在这里按空串同样处理。

use serde_json::Value;

use super::types::McpError;

/// 必填字符串：缺失、空串、类型不对都算参数错误。
pub(super) fn str_arg(args: &Value, key: &str) -> Result<String, McpError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| McpError::Invalid(format!("missing string argument `{key}`")))
}

/// 可选字符串：缺失或空串都当没给。
pub(super) fn opt_str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
}

/// 允许空串（`send_input` 只按回车时 `text` 就是空的）。
pub(super) fn raw_str_arg(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// 取整数并夹到 `[min, max]`；缺失或类型不对用 `default`。
pub(super) fn int_arg(args: &Value, key: &str, default: u64, min: u64, max: u64) -> u64 {
    args.get(key)
        .and_then(Value::as_u64)
        .unwrap_or(default)
        .clamp(min, max)
}

/// 取布尔；缺失或类型不对用 `default`。默认值必须由调用方显式给出 ——
/// 同一个字段在"没给"和"给了 false"之间不能有隐含区别。
pub(super) fn bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn required_string_rejects_missing_empty_and_wrong_type() {
        assert!(str_arg(&json!({}), "tab_id").is_err());
        assert!(str_arg(&json!({ "tab_id": "" }), "tab_id").is_err());
        assert!(str_arg(&json!({ "tab_id": 7 }), "tab_id").is_err());
        assert_eq!(str_arg(&json!({ "tab_id": "t1" }), "tab_id").unwrap(), "t1");
    }

    #[test]
    fn optional_string_treats_empty_as_absent() {
        assert_eq!(opt_str_arg(&json!({}), "pattern"), None);
        assert_eq!(opt_str_arg(&json!({ "pattern": "" }), "pattern"), None);
        assert_eq!(
            opt_str_arg(&json!({ "pattern": "ok" }), "pattern"),
            Some("ok".to_string())
        );
    }

    #[test]
    fn raw_string_keeps_empty() {
        assert_eq!(raw_str_arg(&json!({}), "text"), "");
        assert_eq!(raw_str_arg(&json!({ "text": "" }), "text"), "");
        assert_eq!(raw_str_arg(&json!({ "text": "a" }), "text"), "a");
    }

    #[test]
    fn integer_is_clamped_to_the_range() {
        assert_eq!(int_arg(&json!({}), "n", 30, 1, 100), 30);
        assert_eq!(int_arg(&json!({ "n": 0 }), "n", 30, 1, 100), 1);
        assert_eq!(int_arg(&json!({ "n": 999 }), "n", 30, 1, 100), 100);
        assert_eq!(int_arg(&json!({ "n": 42 }), "n", 30, 1, 100), 42);
    }

    #[test]
    fn boolean_uses_the_given_default() {
        assert!(!bool_arg(&json!({}), "refresh", false));
        assert!(bool_arg(&json!({}), "refresh", true));
        assert!(bool_arg(&json!({ "refresh": true }), "refresh", false));
        // 类型不对按没给处理，不悄悄当成 true。
        assert!(!bool_arg(&json!({ "refresh": "yes" }), "refresh", false));
    }
}
