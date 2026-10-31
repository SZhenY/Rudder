//! 工具定义与权限映射。
//!
//! schema 写成 JSON 字面量（与上游 `src/mcp/impls/tools.rs` 同一写法），再由
//! `handler::list_tools` 反序列化成 `rmcp::model::Tool`。这样定义与协议类型解耦，
//! 加一个工具只需要动这里和 `calls`。

use serde_json::{Value, json};

use super::perms::{Permissions, is_high_risk_setting};
use crate::config::SessionKind;
use crate::resource::TabStatuses;

/// 全部工具定义。
pub(super) fn definitions() -> Value {
    json!([
        {
            "name": "get_server_info",
            "description": "Report this Rudder MCP server's version, listening endpoints and the tool permissions currently granted. Call it first to learn what the server allows.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "list_sessions",
            "description": "List saved SSH/serial/telnet hosts in this Rudder profile. Passwords, private keys and notes are never returned. Use the returned session_id with open_session or get_session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "group": { "type": "string", "description": "Optional exact group name filter." }
                },
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "get_session",
            "description": "Get non-secret metadata for one saved host.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "minLength": 1, "description": "Saved session id returned by list_sessions." }
                },
                "required": ["session_id"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "list_tabs",
            "description": "List the session tabs currently open in the Rudder window. tab_id identifies a running terminal; session_id identifies the saved host it came from.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "get_tab_status",
            "description": "Read remote host status for an open tab: CPU, memory, swap, network interfaces, filesystems, top processes and system overview. For local shell tabs it returns this machine's own figures.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": { "type": "string", "minLength": 1 },
                    "refresh": {
                        "type": "boolean",
                        "default": true,
                        "description": "Ask for a fresh sample when the last one is older than 5 seconds. The response reports stale=true if no fresh sample arrived in time."
                    }
                },
                "required": ["tab_id"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "read_terminal",
            "description": "Read a tab's terminal as plain text. `source=screen` returns the visible viewport; `source=scrollback` returns the most recent lines including history. output_seq can be passed to wait_for_output as since_seq.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": { "type": "string", "minLength": 1 },
                    "source": { "type": "string", "enum": ["screen", "scrollback"], "default": "screen" },
                    "max_rows": { "type": "integer", "minimum": 1, "maximum": 400, "default": 200 }
                },
                "required": ["tab_id"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "wait_for_output",
            "description": "Block until a tab produces new output. With `pattern` (a Rust regex) it returns as soon as the screen text matches; without it, it returns once output has been quiet for `quiet_ms` after at least one new byte. Returns the last 4 KiB of screen text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": { "type": "string", "minLength": 1 },
                    "pattern": { "type": "string", "description": "Rust regex matched against the terminal's plain text." },
                    "timeout_ms": { "type": "integer", "minimum": 100, "maximum": 120000, "default": 15000 },
                    "quiet_ms": { "type": "integer", "minimum": 0, "maximum": 5000, "default": 300 },
                    "since_seq": { "type": "integer", "minimum": 0, "description": "Only count output after this output_seq (from read_terminal). Defaults to now." },
                    "max_rows": { "type": "integer", "minimum": 1, "maximum": 400, "default": 400 }
                },
                "required": ["tab_id"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": false
            }
        },
        {
            "name": "open_session",
            "description": "Open a saved host in a new Rudder tab and start connecting. Returns the new tab_id; connection progress is visible in the app window. A host seen for the first time normally needs a human to confirm its key in the Rudder window — pass trust_unknown_host_key=true to accept and record it automatically for this connection. A CHANGED key is never auto-accepted: it still requires the user, because it may be a man-in-the-middle.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "minLength": 1, "description": "Saved session id from list_sessions, or a built-in `system:` id." },
                    "trust_unknown_host_key": {
                        "type": "boolean",
                        "default": false,
                        "description": "Only affects hosts with no saved key: record the key and connect without prompting. Ignored for changed keys, which always require the user."
                    }
                },
                "required": ["session_id"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "close_session",
            "description": "Close one open tab, tearing down its SSH/SFTP connection and buffers exactly like clicking the tab's close button.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": { "type": "string", "minLength": 1 }
                },
                "required": ["tab_id"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "send_input",
            "description": "Type text into a tab's terminal, as if the user typed it. The bytes go to the PTY, so the user sees them and can take over. Use read_terminal or wait_for_output to see the result.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": { "type": "string", "minLength": 1 },
                    "text": { "type": "string", "description": "Text to type. May be empty to send only Enter." },
                    "enter": { "type": "boolean", "default": true, "description": "Append a carriage return (press Enter) after the text." }
                },
                "required": ["tab_id"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "run_command",
            "description": "Run one command without touching the user's terminal. SSH sessions use a one-shot exec channel on the existing connection; local shell tabs run a child process. Returns exit code, stdout and stderr. Telnet and serial sessions have no exec channel.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": { "type": "string", "minLength": 1 },
                    "command": { "type": "string", "minLength": 1 },
                    "timeout_ms": { "type": "integer", "minimum": 1000, "maximum": 300000, "default": 30000 },
                    "max_output_bytes": { "type": "integer", "minimum": 1024, "maximum": 4194304, "default": 1048576 }
                },
                "required": ["tab_id", "command"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "get_settings",
            "description": "Read Rudder settings by key. With no keys it returns the whole whitelist. Secrets (passwords, tokens, endpoints) are never included.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "keys": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Exact setting keys such as terminal.font_size. Omit for all."
                    }
                },
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "set_settings",
            "description": "Change Rudder settings. Applied atomically: if any key is rejected, nothing is written. Each response lists applied keys and, for rejections, the reason (unknown_key, out_of_range, secret_not_exposed, restart_required_process, ...).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "values": {
                        "type": "object",
                        "description": "Map of setting key to new value, e.g. {\"terminal.font_size\": 15}."
                    }
                },
                "required": ["values"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "upsert_host",
            "description": "Create a saved host, or update one when session_id is given. Only the fields you pass are changed. Covers every option of the host dialog, including the advanced ones. Passwords, inline keys and trigger responses are stored encrypted and never returned.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "description": "Empty or omitted = create a new host." },
                    "name": { "type": "string" },
                    "host": { "type": "string", "description": "Required for ssh/telnet." },
                    "port": { "type": "integer", "minimum": 1, "maximum": 65535 },
                    "user": { "type": "string" },
                    "kind": { "type": "string", "enum": ["ssh", "serial", "telnet", "local"] },
                    "auth": { "type": "string", "enum": ["password", "key", "keyboard-interactive"] },
                    "password": { "type": "string" },
                    "key_mode": { "type": "string", "enum": ["path", "inline"], "description": "Which key source to use; clears the other one." },
                    "private_key_path": { "type": "string" },
                    "private_key_inline": { "type": "string", "description": "PEM/OpenSSH key text; implies key_mode=inline." },
                    "group": { "type": "string" },
                    "note": { "type": "string" },
                    "jump_session_id": { "type": "string", "description": "Saved host id to use as a jump/bastion host." },
                    "proxy": {
                        "type": "string",
                        "description": "Outbound proxy for this host: socks5://host:port or http://host:port. Credentials may be embedded as user:pass@; they are split out and the password is stored encrypted. Empty string = direct (or ALL_PROXY)."
                    },
                    "proxy_user": {
                        "type": "string",
                        "description": "Proxy username. Omit to keep the saved one; empty string clears it."
                    },
                    "proxy_password": {
                        "type": "string",
                        "description": "Proxy password, stored encrypted and never returned. Omit to keep the saved one; empty string clears it."
                    },
                    "encoding": { "type": "string", "description": "Terminal encoding, e.g. UTF-8 or GBK." },
                    "disable_shell_integration": { "type": "boolean" },
                    "serial_port": { "type": "string", "description": "Device name, e.g. COM3 or /dev/ttyUSB0. Required for kind=serial." },
                    "baud_rate": { "type": "integer", "minimum": 300, "maximum": 4000000 },
                    "data_bits": { "type": "integer", "enum": [7, 8] },
                    "stop_bits": { "type": "integer", "enum": [1, 2] },
                    "parity": { "type": "string", "enum": ["none", "odd", "even"] },
                    "flow_control": { "type": "string", "enum": ["none", "hardware", "software"] },
                    "forwards": {
                        "type": "array",
                        "description": "Replaces the host's tunnel list when given.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "enum": ["local", "remote", "dynamic"], "description": "local = -L, remote = -R, dynamic = -D." },
                                "name": { "type": "string" },
                                "bind_addr": { "type": "string" },
                                "bind_port": { "type": "integer", "minimum": 1, "maximum": 65535 },
                                "host": { "type": "string" },
                                "host_port": { "type": "integer", "minimum": 0, "maximum": 65535 }
                            },
                            "required": ["kind", "bind_port"],
                            "additionalProperties": false
                        }
                    },
                    "triggers": {
                        "type": "array",
                        "description": "Replaces the host's login triggers when given. Rules with an empty `expect` are skipped.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "expect": { "type": "string" },
                                "response": { "type": "string" },
                                "append_enter": { "type": "boolean", "default": true },
                                "repeat": { "type": "boolean", "default": false }
                            },
                            "required": ["expect"],
                            "additionalProperties": false
                        }
                    }
                },
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": false
            }
        },
        {
            "name": "delete_host",
            "description": "Delete a saved host. Open tabs pointing at it keep running unless close_open_tabs is true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string", "minLength": 1 },
                    "close_open_tabs": { "type": "boolean", "default": false }
                },
                "required": ["session_id"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": false
            }
        },
        {
            "name": "sftp_list",
            "description": "List a directory on the remote host through the tab's SFTP channel. Directories come first, then files, each sorted by name. Only SSH tabs have SFTP.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": { "type": "string", "minLength": 1 },
                    "path": { "type": "string", "default": "/" }
                },
                "required": ["tab_id"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        },
        {
            "name": "sftp_download",
            "description": "Download one remote file into Rudder's configured download folder over SFTP. Never overwrites: a colliding name becomes `report (1).pdf`. Files above 64 MiB are refused — use the SFTP panel for those.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": { "type": "string", "minLength": 1 },
                    "remote_path": { "type": "string", "minLength": 1 }
                },
                "required": ["tab_id", "remote_path"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": false,
                // 不覆盖已有文件，只写一个新名字。
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": true
            }
        },
        {
            "name": "sftp_upload",
            "description": "Upload one local file to an exact remote path over SFTP, overwriting it if present. Files above 64 MiB are refused — use the SFTP panel for those.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": { "type": "string", "minLength": 1 },
                    "local_path": { "type": "string", "minLength": 1, "description": "Source path on the machine running Rudder." },
                    "remote_path": { "type": "string", "minLength": 1 }
                },
                "required": ["tab_id", "local_path", "remote_path"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": true,
                "openWorldHint": true
            }
        },
        {
            "name": "run_action",
            "description": "Trigger a one-shot UI action: check for updates now, or push/pull the host list to the configured WebDAV. Returns as soon as the action starts; progress and the outcome show up in the status bar and the settings page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["check_update", "webdav_upload", "webdav_download"]
                    }
                },
                "required": ["action"],
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": true
            }
        }
    ])
}

/// 工具名不认识 / 参数不足以判定权限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PermLookup {
    UnknownTool,
    BadArgument(&'static str),
}

/// 全部 19 个工具名。`required_perms` 用它把"未登记"与"动态判定"分开：
/// 未登记就是 fail closed，绝不回落成 `read`。
fn known_tool(name: &str) -> bool {
    matches!(
        name,
        "get_server_info"
            | "list_sessions"
            | "get_session"
            | "list_tabs"
            | "get_tab_status"
            | "read_terminal"
            | "wait_for_output"
            | "get_settings"
            | "open_session"
            | "close_session"
            | "send_input"
            | "run_command"
            | "set_settings"
            | "upsert_host"
            | "delete_host"
            | "sftp_list"
            | "sftp_download"
            | "sftp_upload"
            | "run_action"
    )
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn bool_arg(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// `run_command` / `send_input` 的基位由标签页类型决定。
fn command_perm(kind: SessionKind) -> &'static str {
    if kind == SessionKind::Local {
        "local_exec"
    } else {
        "exec"
    }
}

fn tab_kind(statuses: &TabStatuses, args: &Value) -> Result<SessionKind, PermLookup> {
    let tab_id = str_arg(args, "tab_id").ok_or(PermLookup::BadArgument("missing tab_id"))?;
    let statuses = statuses
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    statuses
        .get(tab_id)
        .map(|status| status.kind)
        .ok_or(PermLookup::BadArgument("unknown tab_id"))
}

/// 本次调用需要的权限位。动态分支只看参数与标签页类型，不读全局配置。
pub(super) fn required_perms(
    statuses: &TabStatuses,
    name: &str,
    args: &Value,
) -> Result<Permissions, PermLookup> {
    if !known_tool(name) {
        return Err(PermLookup::UnknownTool);
    }
    let mut perms = Permissions::default();
    let mut add = |key: &str| {
        let _ = perms.set(key, true);
    };
    match name {
        "get_server_info" => {}
        "list_sessions"
        | "get_session"
        | "list_tabs"
        | "get_tab_status"
        | "read_terminal"
        | "wait_for_output"
        | "get_settings"
        | "sftp_list" => add("read"),
        "open_session" => {
            add("session_open_close");
            if bool_arg(args, "trust_unknown_host_key") {
                add("host_key_trust");
            }
        }
        "close_session" => add("session_open_close"),
        "send_input" => {
            add("terminal_input");
            // 本地标签页上打字等价于在本机执行命令。
            if tab_kind(statuses, args)? == SessionKind::Local {
                add("local_exec");
            }
        }
        "run_command" => add(command_perm(tab_kind(statuses, args)?)),
        "set_settings" => {
            add("settings_write");
            if args
                .get("values")
                .and_then(Value::as_object)
                .is_some_and(|values| values.keys().any(|key| is_high_risk_setting(key)))
            {
                add("settings_high");
            }
        }
        "upsert_host" => add("host_upsert"),
        "delete_host" => {
            add("host_delete");
            if bool_arg(args, "close_open_tabs") {
                add("session_open_close");
            }
        }
        "sftp_download" => add("file_download"),
        "sftp_upload" => add("file_upload"),
        "run_action" => match str_arg(args, "action") {
            Some("check_update") => add("settings_write"),
            // webdav 两个动作会先写回 WebDAV 配置，并移动整份主机库。
            Some("webdav_upload" | "webdav_download") => {
                add("settings_write");
                add("settings_high");
                add("host_upsert");
            }
            _ => add("settings_write"),
        },
        _ => return Err(PermLookup::UnknownTool),
    }
    Ok(perms)
}

/// `list_tools` 的过滤谓词。OR 语义：任一相关位开启就展示。
pub(super) fn listable(perms: &Permissions, name: &str) -> bool {
    match name {
        "get_server_info" => true,
        "list_sessions"
        | "get_session"
        | "list_tabs"
        | "get_tab_status"
        | "read_terminal"
        | "wait_for_output"
        | "get_settings"
        | "sftp_list" => perms.allows("read"),
        "open_session" | "close_session" => perms.allows("session_open_close"),
        "send_input" => perms.allows("terminal_input"),
        "run_command" => perms.allows("exec") || perms.allows("local_exec"),
        // `set_settings` / `run_action` 的基位始终是 `settings_write`；高风险键
        // 只是额外要求，单有 `settings_high` 不足以调用。
        "set_settings" => perms.allows("settings_write"),
        "upsert_host" => perms.allows("host_upsert"),
        "delete_host" => perms.allows("host_delete"),
        "sftp_download" => perms.allows("file_download"),
        "sftp_upload" => perms.allows("file_upload"),
        "run_action" => perms.allows("settings_write"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition_names() -> Vec<String> {
        definitions()
            .as_array()
            .expect("definitions 必须是数组")
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    #[test]
    fn every_definition_is_registered() {
        let names = definition_names();
        assert_eq!(names.len(), 19);
        for name in &names {
            assert!(known_tool(name), "{name} 没有登记权限");
        }
    }

    #[test]
    fn unknown_tool_fails_closed() {
        let statuses = std::sync::Arc::new(std::sync::Mutex::new(Default::default()));
        assert_eq!(
            required_perms(&statuses, "not_a_tool", &json!({})),
            Err(PermLookup::UnknownTool)
        );
    }

    #[test]
    fn dynamic_branches_add_the_expected_bits() {
        let statuses = std::sync::Arc::new(std::sync::Mutex::new(Default::default()));

        let open = required_perms(
            &statuses,
            "open_session",
            &json!({"session_id": "s", "trust_unknown_host_key": true}),
        )
        .unwrap();
        assert!(open.allows("session_open_close"));
        assert!(open.allows("host_key_trust"));

        let delete = required_perms(
            &statuses,
            "delete_host",
            &json!({"session_id": "s", "close_open_tabs": true}),
        )
        .unwrap();
        assert!(delete.allows("host_delete"));
        assert!(delete.allows("session_open_close"));

        let settings = required_perms(
            &statuses,
            "set_settings",
            &json!({"values": {"update.proxy": "http://x:1"}}),
        )
        .unwrap();
        assert!(settings.allows("settings_write"));
        assert!(settings.allows("settings_high"));

        let webdav = required_perms(
            &statuses,
            "run_action",
            &json!({"action": "webdav_upload"}),
        )
        .unwrap();
        assert!(webdav.allows("settings_write"));
        assert!(webdav.allows("settings_high"));
        assert!(webdav.allows("host_upsert"));

        assert_eq!(
            required_perms(&statuses, "run_command", &json!({"tab_id": "missing"})),
            Err(PermLookup::BadArgument("unknown tab_id"))
        );
    }

    #[test]
    fn listable_is_or_for_split_bits() {
        let mut perms = Permissions::default();
        assert!(listable(&perms, "get_server_info"));
        assert!(!listable(&perms, "run_command"));
        assert!(!listable(&perms, "sftp_download"));

        perms.set("local_exec", true);
        assert!(listable(&perms, "run_command"));
        assert!(!listable(&perms, "sftp_download"));

        perms.set("file_download", true);
        assert!(listable(&perms, "sftp_download"));
        assert!(!listable(&perms, "sftp_upload"));
    }

    #[test]
    fn command_perm_follows_tab_kind() {
        assert_eq!(command_perm(SessionKind::Ssh), "exec");
        assert_eq!(command_perm(SessionKind::Local), "local_exec");
    }
}
