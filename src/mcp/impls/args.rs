//! 工具参数的取值规则。
//!
//! 权限判定（`tools::required_perms`）与执行（`calls`）必须用同一套规则，否则会出现
//! "权限门放过了、执行时又报参数错"这类前后不一致。schema 里要求 `minLength: 1`
//! 的字段在这里按空串同样处理。

use serde_json::Value;

use super::types::McpError;

pub(super) fn str_arg(args: &Value, key: &str) -> Result<String, McpError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| McpError::Invalid(format!("missing string argument `{key}`")))
}

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

pub(super) fn int_arg(args: &Value, key: &str, default: u64, min: u64, max: u64) -> u64 {
    args.get(key)
        .and_then(Value::as_u64)
        .unwrap_or(default)
        .clamp(min, max)
}

pub(super) fn bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}
