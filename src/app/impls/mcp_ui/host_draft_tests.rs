//! `build_host_draft` 的纯校验行为。抽成纯函数之前这些分支只能靠 MCP 调用
//! 触发，且每次都要过 UI 桥，回归测试无从下手。

use super::*;
use serde_json::json;

fn draft(args: Value) -> Result<HostDraft, McpError> {
    build_host_draft(None, &args)
}

fn invalid(args: Value, needle: &str) {
    match draft(args) {
        Err(McpError::Invalid(msg)) => {
            assert!(msg.contains(needle), "expected {needle:?}, got {msg:?}");
        }
        Err(other) => panic!("expected Invalid({needle:?}), got {other:?}"),
        Ok(_) => panic!("expected Invalid({needle:?}), got a valid draft"),
    }
}

#[test]
fn new_ssh_host_gets_default_port_and_generated_name() {
    let d = draft(json!({ "host": "example.com", "user": "alice" })).unwrap();
    assert!(d.created);
    assert_eq!(d.session.port, 22);
    assert_eq!(d.session.host, "example.com");
    assert!(d.session.name.contains("example.com"), "{}", d.session.name);
    assert_eq!(d.session.user, "alice");
}

#[test]
fn explicit_port_is_kept_for_existing_hosts() {
    let mut existing = Session::new_empty();
    existing.host = "old.example".to_string();
    existing.note = "keep me".to_string();
    existing.port = 2222;
    let d = build_host_draft(Some(existing), &json!({ "host": "new.example" })).unwrap();
    assert!(!d.created);
    assert_eq!(d.session.port, 2222);
    assert_eq!(d.session.note, "keep me");
    assert_eq!(d.session.host, "new.example");
}

#[test]
fn every_rejection_branch_names_its_field() {
    invalid(json!({ "host": "h", "port": 0 }), "`port`");
    invalid(json!({ "host": "h", "port": 70000 }), "`port`");
    invalid(json!({ "host": "h", "key_mode": "auto" }), "`key_mode`");
    invalid(
        json!({ "host": "h", "proxy": "ftp://proxy.local:21" }),
        "`proxy` scheme",
    );
    invalid(json!({ "host": "h", "encoding": "x".repeat(33) }), "`encoding`");
    invalid(json!({ "host": "h", "baud_rate": 100 }), "`baud_rate`");
    invalid(json!({ "host": "h", "data_bits": 5 }), "`data_bits`");
    invalid(json!({ "host": "h", "stop_bits": 3 }), "`stop_bits`");
    invalid(json!({ "host": "h", "parity": "mark" }), "`parity`");
    invalid(
        json!({ "host": "h", "flow_control": "dtr" }),
        "`flow_control`",
    );
    invalid(
        json!({ "host": "h", "forwards": [{ "kind": "bogus", "bind_port": 1 }] }),
        "forward `kind`",
    );
    invalid(
        json!({ "host": "h", "forwards": [{ "kind": "local", "bind_port": 0 }] }),
        "forward `bind_port`",
    );
    invalid(
        json!({ "host": "h", "forwards": [{ "kind": "local", "bind_port": 1, "host_port": 70000 }] }),
        "forward `host_port`",
    );
    invalid(json!({}), "`host` is required");
}

#[test]
fn serial_host_requires_a_device() {
    let mut existing = Session::new_empty();
    existing.kind = SessionKind::Serial;
    existing.host = "unused".to_string();
    assert!(matches!(
        build_host_draft(Some(existing), &json!({})),
        Err(McpError::Invalid(_))
    ));
}

#[test]
fn forwards_and_triggers_are_parsed_and_blank_expect_is_skipped() {
    let d = draft(json!({
        "host": "h",
        "forwards": [
            { "kind": "local", "bind_port": 8080, "host_port": 80, "host": "127.0.0.1" }
        ],
        "triggers": [
            { "expect": "", "response": "ignored" },
            { "expect": "Password:", "response": "p", "append_enter": false, "repeat": true }
        ]
    }))
    .unwrap();
    assert_eq!(d.session.forwards.len(), 1);
    assert_eq!(d.session.forwards[0].bind_port, 8080);
    assert_eq!(d.session.triggers.len(), 1);
    assert_eq!(d.session.triggers[0].expect, "Password:");
    assert!(!d.session.triggers[0].append_enter);
    assert!(d.session.triggers[0].repeat);
}

#[test]
fn empty_proxy_clears_all_three_proxy_fields() {
    let mut existing = Session::new_empty();
    existing.host = "h".to_string();
    existing.proxy = "http://proxy.local:8080".to_string();
    existing.proxy_user = "alice".to_string();
    let d = build_host_draft(Some(existing), &json!({ "proxy": "" })).unwrap();
    assert!(d.session.proxy.is_empty());
    assert!(d.session.proxy_user.is_empty());
}

#[test]
fn inline_key_clears_the_path() {
    let d = draft(json!({
        "host": "h",
        "private_key_path": "C:\\keys\\id_ed25519",
        "private_key_inline": "-----BEGIN KEY-----"
    }))
    .unwrap();
    assert!(d.session.private_key_path.is_empty());
}
