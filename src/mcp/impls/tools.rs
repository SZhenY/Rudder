//! 工具定义与权限映射。
//!
//! schema 写成 JSON 字面量（与上游 `src/mcp/impls/tools.rs` 同一写法），再由
//! `handler::list_tools` 反序列化成 `rmcp::model::Tool`。这样定义与协议类型解耦，
//! 加一个工具只需要动这里和 `calls`。

use serde_json::{Value, json};

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
                "openWorldHint": false
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
                "destructiveHint": false,
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
                "destructiveHint": false,
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
                "idempotentHint": true,
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
            "description": "Download one remote file to a path on this machine over SFTP. Both paths are exact file paths (not directories). Files above 64 MiB are refused — use the SFTP panel for those.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab_id": { "type": "string", "minLength": 1 },
                    "remote_path": { "type": "string", "minLength": 1 },
                    "local_path": { "type": "string", "minLength": 1, "description": "Destination path on the machine running Rudder." }
                },
                "required": ["tab_id", "remote_path", "local_path"],
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
        }
    ])
}

/// 工具名 → 需要的权限位。只读工具不登记（`None`）。
///
/// 写工具必须在这里登记，否则会绕过权限门；`handler::call_tool` 对未知工具名
/// 返回错误，双重兜底。
const PERMISSIONS: &[(&str, &str)] = &[
    ("open_session", "session_open_close"),
    ("close_session", "session_open_close"),
    ("send_input", "terminal_input"),
    ("run_command", "exec"),
    ("set_settings", "settings_write"),
    ("upsert_host", "host_write"),
    ("delete_host", "host_write"),
    ("sftp_list", "file_transfer"),
    ("sftp_download", "file_transfer"),
    ("sftp_upload", "file_transfer"),
];

pub(super) fn required_perm(name: &str) -> Option<&'static str> {
    PERMISSIONS
        .iter()
        .find(|(tool, _)| *tool == name)
        .map(|(_, permission)| *permission)
}
