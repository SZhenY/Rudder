//! 审计日志的数据形态与参数脱敏。
//!
//! 写盘在 `impls/audit.rs`（按天切分 + 有界通道），这里只放"记什么"的规则：
//! 字段名黑名单、截断长度、`send_input` 只记长度。

use serde_json::{Map, Value};

/// 单条字符串的截断上限（按 char 计）。
const MAX_TEXT_CHARS: usize = 200;
/// 单条记录里 `args` 的总量上限，超过就整体降级为摘要。
const MAX_ARGS_BYTES: usize = 4096;
/// 递归深度与数组长度上限。
const MAX_DEPTH: usize = 6;
const MAX_ARRAY: usize = 32;

/// 字段名包含这些片段的，值一律替换成 `***`（小写后匹配）。
const SECRET_KEYS: &[&str] = &[
    "password",
    "passphrase",
    "token",
    "secret",
    "private_key",
    "proxy_pass",
    "authorization",
    "cookie",
    // 登录触发器里的 `response` 与口令同级：两者都能打开会话。
    "response",
];

/// AI 页"最近调用"用的最小集，不含参数。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AuditEntry {
    pub(crate) at: String,
    pub(crate) tool: String,
    pub(crate) ok: bool,
    pub(crate) code: String,
    pub(crate) ms: u64,
    pub(crate) client: String,
}

/// 脱敏后的参数 + 原始字节数 + 是否发生截断。
pub(crate) fn redact_args(tool: &str, args: &Value) -> (Value, usize, bool) {
    let bytes = serde_json::to_vec(args).map(|raw| raw.len()).unwrap_or(0);

    // `send_input` 可能就是在敲密码：只留长度，正文一个字都不记。
    if tool == "send_input" {
        let text = args.get("text").and_then(Value::as_str).unwrap_or("");
        let mut object = Map::new();
        for key in ["tab_id", "enter"] {
            if let Some(value) = args.get(key) {
                object.insert(key.to_string(), scrub(value, 1));
            }
        }
        object.insert(
            "text".to_string(),
            serde_json::json!({ "chars": text.chars().count(), "bytes": text.len() }),
        );
        return (Value::Object(object), bytes, false);
    }

    if bytes > MAX_ARGS_BYTES {
        let keys: Vec<&str> = args
            .as_object()
            .map(|object| object.keys().map(String::as_str).collect())
            .unwrap_or_default();
        return (
            serde_json::json!({ "_redacted": true, "bytes": bytes, "keys": keys }),
            bytes,
            true,
        );
    }

    let mut truncated = false;
    let scrubbed = scrub_with(args, 0, &mut truncated);
    (scrubbed, bytes, truncated)
}

fn scrub(value: &Value, depth: usize) -> Value {
    let mut ignored = false;
    scrub_with(value, depth, &mut ignored)
}

fn scrub_with(value: &Value, depth: usize, truncated: &mut bool) -> Value {
    if depth > MAX_DEPTH {
        *truncated = true;
        return Value::String("…".to_string());
    }
    match value {
        Value::Object(object) => {
            let mut out = Map::with_capacity(object.len());
            for (key, inner) in object {
                if is_secret_key(key) {
                    let len = inner.as_str().map(str::len).unwrap_or(0);
                    out.insert(key.clone(), Value::String("***".to_string()));
                    out.insert(format!("{key}_len"), Value::from(len));
                    continue;
                }
                out.insert(key.clone(), scrub_with(inner, depth + 1, truncated));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            let mut out: Vec<Value> = Vec::with_capacity(items.len().min(MAX_ARRAY));
            for item in items.iter().take(MAX_ARRAY) {
                out.push(scrub_with(item, depth + 1, truncated));
            }
            if items.len() > MAX_ARRAY {
                *truncated = true;
                out.push(Value::String(format!("…{} more", items.len() - MAX_ARRAY)));
            }
            Value::Array(out)
        }
        Value::String(text) => Value::String(truncate(text, truncated)),
        other => other.clone(),
    }
}

/// 剔除控制字符（防 ndjson 行注入与终端转义），再按 char 截断。
fn truncate(text: &str, truncated: &mut bool) -> String {
    let cleaned: String = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    if cleaned.chars().count() <= MAX_TEXT_CHARS {
        return cleaned;
    }
    *truncated = true;
    let mut out: String = cleaned.chars().take(MAX_TEXT_CHARS).collect();
    out.push('…');
    out
}

fn is_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SECRET_KEYS.iter().any(|needle| lower.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secrets_are_masked_by_field_name() {
        let (out, _, _) = redact_args(
            "upsert_host",
            &json!({ "name": "h", "password": "hunter2", "proxy_password": "p" }),
        );
        assert_eq!(out["password"], "***");
        assert_eq!(out["proxy_password"], "***");
        assert_eq!(out["password_len"], 7);
        assert_eq!(out["name"], "h");
    }

    #[test]
    fn newlines_and_controls_are_stripped() {
        let (out, _, _) = redact_args("run_command", &json!({ "command": "line1\nline2\u{1b}[31m" }));
        let text = out["command"].as_str().unwrap();
        assert!(!text.contains('\n'));
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn long_text_is_truncated_with_ellipsis() {
        let long = "a".repeat(MAX_TEXT_CHARS + 50);
        let (out, _, truncated) = redact_args("run_command", &json!({ "command": long }));
        assert!(truncated);
        let text = out["command"].as_str().unwrap();
        assert_eq!(text.chars().count(), MAX_TEXT_CHARS + 1);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn send_input_never_records_the_text() {
        let (out, _, _) = redact_args("send_input", &json!({ "tab_id": "t", "text": "hunter2", "enter": true }));
        assert!(out.get("text").is_some());
        assert_eq!(out["text"]["chars"], 7);
        assert!(!serde_json::to_string(&out).unwrap().contains("hunter2"));
    }

    #[test]
    fn huge_args_degrade_to_a_summary() {
        let big = "x".repeat(MAX_ARGS_BYTES);
        let (out, bytes, truncated) = redact_args("run_command", &json!({ "command": big }));
        assert!(truncated);
        assert_eq!(out["_redacted"], true);
        assert!(bytes > MAX_ARGS_BYTES);
    }
}
