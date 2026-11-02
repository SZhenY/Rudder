//! 工具定义与权限约定。
//!
//! schema 放在同目录的 `tools.json`，用 `include_str!` 编进二进制，首次 `list_tools` 时解析一次。
//! 不在 Rust 里写 `json!` 字面量：26 个工具的嵌套远超默认 `recursion_limit`，宏展开要在
//! worker 线程的 2 MB 栈上重建整棵树（实测能把测试线程顶爆）。
//!
//! 工具名、调用所需权限、`list_tools` 的可见条件集中在 [`TOOLS`] 一张表里；
//! 表里的名字与 `tools.json` 的一致性由 `every_definition_is_registered` 测试保证。

use std::sync::OnceLock;

use rmcp::model::Tool;
use serde_json::Value;

use super::args::{bool_arg, opt_str_arg, str_arg};
use super::perms::{Permissions, is_high_risk_setting};
use crate::config::SessionKind;
use crate::resource::TabStatuses;

const TOOL_SCHEMA: &str = include_str!("tools.json");

/// 全部工具定义；解析只做一次，之后按引用返回。
pub(super) fn definitions() -> &'static [Tool] {
    static TOOLS: OnceLock<Vec<Tool>> = OnceLock::new();
    TOOLS.get_or_init(|| serde_json::from_str(TOOL_SCHEMA).expect("tools.json 必须是工具定义数组"))
}

/// 工具的权限约定。
struct ToolMeta {
    name: &'static str,
    /// 调用需要的基础位；空 = 不需要授权。
    ///
    /// `run_command` 的基位取决于标签页类型（`exec` / `local_exec`），表里留空，
    /// 由 [`required_perms`] 按 `tab_id` 判定。
    require: &'static [&'static str],
    /// `list_tools` 是否展示（OR 语义）；空 = 始终展示。
    show: &'static [&'static str],
}

const TOOLS: &[ToolMeta] = &[
    ToolMeta { name: "get_server_info", require: &[], show: &[] },
    ToolMeta { name: "list_sessions", require: &["read"], show: &["read"] },
    ToolMeta { name: "get_session", require: &["read"], show: &["read"] },
    ToolMeta { name: "list_tabs", require: &["read"], show: &["read"] },
    ToolMeta { name: "get_tab_status", require: &["read"], show: &["read"] },
    ToolMeta { name: "get_host_info", require: &["read"], show: &["read"] },
    ToolMeta { name: "read_terminal", require: &["read"], show: &["read"] },
    ToolMeta { name: "wait_for_output", require: &["read"], show: &["read"] },
    ToolMeta { name: "get_update_status", require: &["read"], show: &["read"] },
    ToolMeta { name: "get_settings", require: &["read"], show: &["read"] },
    ToolMeta { name: "sftp_list", require: &["read"], show: &["read"] },
    ToolMeta {
        name: "open_session",
        require: &["session_open_close"],
        show: &["session_open_close"],
    },
    ToolMeta {
        name: "close_session",
        require: &["session_open_close"],
        show: &["session_open_close"],
    },
    ToolMeta { name: "send_input", require: &["terminal_input"], show: &["terminal_input"] },
    ToolMeta { name: "run_command", require: &[], show: &["exec", "local_exec"] },
    ToolMeta { name: "kill_process", require: &["exec"], show: &["exec"] },
    ToolMeta { name: "set_settings", require: &["settings_write"], show: &["settings_write"] },
    ToolMeta { name: "upsert_host", require: &["host_upsert"], show: &["host_upsert"] },
    ToolMeta { name: "delete_host", require: &["host_delete"], show: &["host_delete"] },
    ToolMeta { name: "sftp_download", require: &["file_download"], show: &["file_download"] },
    ToolMeta { name: "sftp_upload", require: &["file_upload"], show: &["file_upload"] },
    ToolMeta { name: "sftp_mkdir", require: &["file_upload"], show: &["file_upload"] },
    ToolMeta { name: "sftp_remove", require: &["file_upload"], show: &["file_upload"] },
    ToolMeta { name: "sftp_rename", require: &["file_upload"], show: &["file_upload"] },
    ToolMeta { name: "sftp_chmod", require: &["file_upload"], show: &["file_upload"] },
    ToolMeta { name: "run_action", require: &["settings_write"], show: &["settings_write"] },
];

fn meta(name: &str) -> Option<&'static ToolMeta> {
    TOOLS.iter().find(|meta| meta.name == name)
}

/// 工具名不认识 / 参数不足以判定权限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PermLookup {
    UnknownTool,
    BadArgument(&'static str),
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
    // 空串等同于没给：schema 里 tab_id 是 minLength: 1。
    let tab_id = str_arg(args, "tab_id").map_err(|_| PermLookup::BadArgument("missing tab_id"))?;
    let statuses = statuses
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    statuses
        .get(&tab_id)
        .map(|status| status.kind)
        .ok_or(PermLookup::BadArgument("unknown tab_id"))
}

/// 本次调用需要的权限位。表里给出基础位，这里只补依赖参数与标签页类型的部分。
pub(super) fn required_perms(
    statuses: &TabStatuses,
    name: &str,
    args: &Value,
) -> Result<Permissions, PermLookup> {
    // 未登记就是 fail closed，绝不回落成 `read`。
    let meta = meta(name).ok_or(PermLookup::UnknownTool)?;
    let mut perms = Permissions::default();
    let mut add = |key: &str| {
        let _ = perms.set(key, true);
    };
    for bit in meta.require {
        add(bit);
    }
    match name {
        "open_session" => {
            if bool_arg(args, "trust_unknown_host_key", false) {
                add("host_key_trust");
            }
        }
        "send_input" => {
            // 本地标签页上打字等价于在本机执行命令。
            if tab_kind(statuses, args)? == SessionKind::Local {
                add("local_exec");
            }
        }
        "run_command" => add(command_perm(tab_kind(statuses, args)?)),
        "set_settings" => {
            let high_risk = args
                .get("values")
                .and_then(Value::as_object)
                .is_some_and(|values| values.keys().any(|key| is_high_risk_setting(key)));
            if high_risk {
                add("settings_high");
            }
        }
        "delete_host" => {
            if bool_arg(args, "close_open_tabs", false) {
                add("session_open_close");
            }
        }
        "run_action" => {
            // 两个 webdav 动作先写回 WebDAV 配置，再移动整份主机库。
            let action = opt_str_arg(args, "action");
            if matches!(action.as_deref(), Some("webdav_upload" | "webdav_download")) {
                add("settings_high");
                add("host_upsert");
            }
        }
        _ => {}
    }
    Ok(perms)
}

/// `list_tools` 的过滤谓词。OR 语义：任一相关位开启就展示。
pub(super) fn listable(perms: &Permissions, name: &str) -> bool {
    match meta(name) {
        Some(meta) if meta.show.is_empty() => true,
        Some(meta) => meta.show.iter().any(|bit| perms.allows(bit)),
        // 未登记的工具不展示，与 `required_perms` 的 fail closed 保持一致。
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition_names() -> Vec<String> {
        definitions()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    #[test]
    fn every_definition_is_registered() {
        let names = definition_names();
        assert_eq!(names.len(), TOOLS.len());
        for name in &names {
            assert!(meta(name).is_some(), "{name} 没有登记权限");
        }
    }

    #[test]
    fn every_registered_tool_has_a_definition() {
        for tool in TOOLS {
            assert!(
                definitions().iter().any(|def| def.name == tool.name),
                "{} 有权限约定但 tools.json 里没有定义",
                tool.name
            );
        }
    }

    #[test]
    fn unknown_tool_fails_closed() {
        let statuses = std::sync::Arc::new(std::sync::Mutex::new(Default::default()));
        assert_eq!(
            required_perms(&statuses, "not_a_tool", &serde_json::json!({})),
            Err(PermLookup::UnknownTool)
        );
        let perms = Permissions::default();
        assert!(!listable(&perms, "not_a_tool"));
    }

    #[test]
    fn dynamic_branches_add_the_expected_bits() {
        let statuses = std::sync::Arc::new(std::sync::Mutex::new(Default::default()));

        let open = required_perms(
            &statuses,
            "open_session",
            &serde_json::json!({"session_id": "s", "trust_unknown_host_key": true}),
        )
        .unwrap();
        assert!(open.allows("session_open_close"));
        assert!(open.allows("host_key_trust"));

        let delete = required_perms(
            &statuses,
            "delete_host",
            &serde_json::json!({"session_id": "s", "close_open_tabs": true}),
        )
        .unwrap();
        assert!(delete.allows("host_delete"));
        assert!(delete.allows("session_open_close"));

        let settings = required_perms(
            &statuses,
            "set_settings",
            &serde_json::json!({"values": {"update.proxy": "http://x:1"}}),
        )
        .unwrap();
        assert!(settings.allows("settings_write"));
        assert!(settings.allows("settings_high"));

        let webdav = required_perms(
            &statuses,
            "run_action",
            &serde_json::json!({"action": "webdav_upload"}),
        )
        .unwrap();
        assert!(webdav.allows("settings_write"));
        assert!(webdav.allows("settings_high"));
        assert!(webdav.allows("host_upsert"));

        assert_eq!(
            required_perms(&statuses, "run_command", &serde_json::json!({"tab_id": "missing"})),
            Err(PermLookup::BadArgument("unknown tab_id"))
        );
    }

    #[test]
    fn static_table_bits_match_call_and_list_policy() {
        let statuses = std::sync::Arc::new(std::sync::Mutex::new(Default::default()));

        // 纯读工具：表里的 require 与 show 是同一批位。
        let read = required_perms(&statuses, "list_sessions", &serde_json::json!({})).unwrap();
        assert!(read.allows("read"));

        // run_command 在表里没有静态位（取决于标签页），但展示条件覆盖两个基位。
        let exec_only = required_perms(&statuses, "kill_process", &serde_json::json!({})).unwrap();
        assert!(exec_only.allows("exec"));
        assert!(listable(&exec_only, "run_command"));
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
