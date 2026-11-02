//! 审计日志的数据形态与参数脱敏。
//!
//! 写盘在 `impls/audit.rs`（按天切分 + 有界通道），这里只放"记什么"的规则：
//! 字段名黑名单、截断长度、`send_input` 只记长度。

use serde_json::{Map, Value};

use super::types::{ClientLease, CloseReason, ConsentState, session8};

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

/// 一条"会话结束"记录（AI 页「最近断开」用）。
///
/// 与 `AuditEntry` 分开：那个是工具调用，这个是会话生命周期；混在一起会把
/// "最近调用"列表冲掉。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionCloseEntry {
    /// `HH:MM:SS`（本地时间）。
    pub(crate) at: String,
    /// session id 前 8 位 —— 两个同名客户端靠它区分。
    pub(crate) session8: String,
    pub(crate) client: String,
    pub(crate) reason: CloseReason,
    /// 从接入到断开经过的秒数；很短说明"刚连上就没了"。
    pub(crate) alive_secs: u64,
    /// 断开时是否已经授权过（区分"被拒"与"接入后掉线"）。
    pub(crate) granted: bool,
    /// 汇总条目（服务停止 / 令牌轮换一次关掉多个会话）的数量；单条关闭为 `None`。
    /// 存数字而不是拼好的文案，是为了让界面按当前语言拼 —— 否则英文界面会显示中文。
    pub(crate) count: Option<usize>,
}

impl SessionCloseEntry {
    /// 一次关掉多个会话时的汇总条目（服务停止 / 令牌轮换）。
    pub(crate) fn from_summary(count: usize, reason: CloseReason) -> Self {
        Self {
            at: chrono::Local::now().format("%H:%M:%S").to_string(),
            session8: String::from("—"),
            client: String::new(),
            reason,
            alive_secs: 0,
            granted: true,
            count: Some(count),
        }
    }

    pub(crate) fn from_lease(lease: &ClientLease, reason: CloseReason, alive_secs: u64) -> Self {
        Self {
            at: chrono::Local::now().format("%H:%M:%S").to_string(),
            session8: session8(&lease.session_id),
            client: lease.client_label(),
            reason,
            alive_secs,
            granted: lease.verdict == ConsentState::Granted,
            count: None,
        }
    }
}

/// 只数字节、不留内容的 writer：给 `serde_json::to_writer` 算序列化长度用。
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 参数序列化后的字节数。走 `to_writer` 而不是 `to_vec`：原来为了一个审计字段
/// 要把整份参数（可能上兆的 `write_text`）再序列化一遍、留一份完整副本。
fn json_bytes(value: &Value) -> usize {
    let mut counter = ByteCounter(0);
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => counter.0,
        Err(_) => 0,
    }
}

/// 会话关闭事件的审计记录（只进文件，不进「最近调用」环）。
///
/// 两条路径共用：`audit_close`（单个租约移除）与批量停止（`Stopped` 汇总后逐条落盘）。
/// 汇总进内存环时只记一条，但文件里保留每条，事后排查需要完整清单。
pub(crate) fn session_close_record(
    lease: &ClientLease,
    reason: CloseReason,
    tool: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "ts": chrono::Local::now().to_rfc3339(),
        "event": "session_close",
        "reason": reason.as_str(),
        "session": session8(&lease.session_id),
        "client": lease.client_label(),
        "tool": tool.unwrap_or(""),
        "epoch": lease.epoch,
    })
}

/// 脱敏后的参数 + 原始字节数 + 是否发生截断。
pub(crate) fn redact_args(tool: &str, args: &Value) -> (Value, usize, bool) {
    let bytes = json_bytes(args);

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

    /// 两条关闭路径必须是同一个字段形状 —— 之前手写两份，批量停止那条漏了 `epoch`。
    #[test]
    fn session_close_record_has_one_shape() {
        let lease = ClientLease::new("abcdefgh12345".to_string(), 7, None);
        let with_tool = session_close_record(&lease, CloseReason::Delete, Some("run_command"));
        let batch = session_close_record(&lease, CloseReason::Stopped, None);
        for field in ["ts", "event", "reason", "session", "client", "tool", "epoch"] {
            assert!(with_tool.get(field).is_some(), "缺字段 {field}");
            assert!(batch.get(field).is_some(), "缺字段 {field}");
        }
        assert_eq!(with_tool["session"], "abcdefgh");
        assert_eq!(with_tool["tool"], "run_command");
        assert_eq!(batch["tool"], "");
        assert_eq!(batch["epoch"], 7);
    }

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
        let (out, _, _) = redact_args(
            "run_command",
            &json!({ "command": "line1\nline2\u{1b}[31m" }),
        );
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
        let (out, _, _) = redact_args(
            "send_input",
            &json!({ "tab_id": "t", "text": "hunter2", "enter": true }),
        );
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
