//! Session-lifecycle callback wiring, extracted from `app.rs` (refactor plan
//! stage D step 2). Slint-thread-only: connects every `on_*` session event
//! (connect / disconnect / resize / rename / triggers …) to session state.

use std::cell::RefCell;
use std::rc::Rc;

use slint::{ComponentHandle, SharedString};

use crate::session::ConnectCtx;
use crate::ssh::{SessionEvent, test_session_auth};
use crate::ui::*;
use crate::app::pane_layout::refresh_panes;
use crate::app::port_forward::{blank_forward_draft, forward_drafts, forward_model, validated_port_forwards};
use crate::app::session_models::{builtin_local_sessions, jump_candidates, parse_batch_import, refresh_session_rows_in_place, session_from_draft, session_groups_model, sync_sessions_to_model, sync_sessions_to_model_with_filter};
use crate::app::auth_dialogs::{enqueue_cred_prompt, enqueue_hostkey_prompt, enqueue_mfa_prompt};
use crate::app::session_runtime::resolve_jump;
use crate::app::session_trigger::{blank_trigger_draft, trigger_drafts, trigger_model, validated_triggers};
use crate::app::tab_ops::{self, TabEnv};
use crate::app::{
    AppContext, compose_proxy, split_proxy, sync_sessions_for_window,
    tab_title_len,
};
use crate::config::{AuthMethod, Secret, Session, SessionKind, is_reserved_session_group};
use crate::i18n::t;

pub(super) struct SessionEditScope {
    pub(super) edit_forwards: Rc<RefCell<Vec<PortFwd>>>,
    pub(super) edit_triggers: Rc<RefCell<Vec<TriggerDraft>>>,
}

#[path = "session_callbacks_search.rs"] mod session_callbacks_search;
#[path = "session_callbacks_sessions.rs"] mod session_callbacks_sessions;
#[path = "session_callbacks_dialog.rs"] mod session_callbacks_dialog;
#[path = "session_callbacks_advanced.rs"] mod session_callbacks_advanced;
#[path = "session_callbacks_tabs.rs"] mod session_callbacks_tabs;

pub(crate) fn wire_session_callbacks(window: &AppWindow, ctx: &AppContext) {
    let scope = SessionEditScope {
        edit_forwards: Rc::new(RefCell::new(vec![blank_forward_draft()])),
        edit_triggers: Rc::new(RefCell::new(vec![blank_trigger_draft()])),
    };
    session_callbacks_search::wire_session_search(window, ctx, &scope);
    session_callbacks_sessions::wire_session_list(window, ctx, &scope);
    session_callbacks_dialog::wire_session_dialog(window, ctx, &scope);
    session_callbacks_advanced::wire_session_advanced(window, ctx, &scope);
    session_callbacks_tabs::wire_session_tabs(window, ctx);
}
/// 「移动到分组」的目标名：`default`（忽略大小写）= 未分组 → 空串；
/// 保留分组（`system` 等，属于内置本地会话）→ None，表示整体中止、不写入任何东西。
///
/// 先归一空白再用。以前判保留名用 `trim()`、写入却用原始串：`" system "` 会被拒（对），
/// 但 `" Prod "` 会带着空白存进配置；`" default "` 更隐蔽 —— 它既不等于 `default`、
/// 也不是保留名，于是变成一个名叫 `" default "` 的**新分组**，而不是"移出分组"。
/// 口径与 `add_group` / `rename_group`（都是先 `trim()` 再写）一致。
fn normalize_move_group(group: &str) -> Option<String> {
    let group = group.trim();
    if group.eq_ignore_ascii_case("default") {
        Some(String::new())
    } else if is_reserved_session_group(group) {
        None
    } else {
        Some(group.to_string())
    }
}

/// 连接状态弹窗里显示的连接标签（按传输类型区分）。
pub(super) fn conn_label(session: &Session) -> String {
    match &session.kind {
        SessionKind::Ssh => format!("{}@{}", session.user, session.host),
        SessionKind::Serial => format!("{} @{}", session.serial_port, session.baud_rate),
        SessionKind::Telnet => format!("telnet {}:{}", session.host, session.port),
        SessionKind::Local => format!("local {}", session.name),
    }
}

/// 只有 SSH 有 SFTP 旁路；串口 / Telnet / 本地会话都不该显示 SFTP 面板。
pub(super) fn session_has_sftp(kind: &SessionKind) -> bool {
    *kind == SessionKind::Ssh
}

/// 重命名提交时算标题：非空 → 用 trim 后的名字；空 → 回落到保存的会话名，
/// 再回落到内置本地会话名；都没有 → None（**不动**标题，而不是把它清空）。
fn resolve_tab_title(
    name: &str,
    saved: Option<String>,
    builtin: Option<String>,
) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        saved.or(builtin)
    } else {
        Some(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_of(kind: SessionKind) -> Session {
        let mut s = Session::new_empty();
        s.kind = kind;
        s.user = "root".into();
        s.host = "10.0.0.5".into();
        s.port = 22;
        s.serial_port = "COM3".into();
        s.baud_rate = 115200;
        s.name = "prod".into();
        s
    }

    /// `default` = 未分组（存空串，忽略大小写）；保留分组要被拒绝（#324 那类事故：
    /// 把服务器会话移进只属于内置本地会话的 `system` 组）。
    #[test]
    fn move_group_maps_default_and_rejects_reserved() {
        assert_eq!(normalize_move_group("default"), Some(String::new()));
        assert_eq!(normalize_move_group("Default"), Some(String::new()));
        assert_eq!(normalize_move_group("DEFAULT"), Some(String::new()));
        assert_eq!(normalize_move_group("system"), None, "保留分组");
        assert_eq!(normalize_move_group(" system "), None, "带空白也算保留分组");
        assert_eq!(normalize_move_group("Prod"), Some("Prod".to_string()));
        assert_eq!(
            normalize_move_group(" Prod "),
            Some("Prod".to_string()),
            "带空白的组名要归一后再写"
        );
        assert_eq!(
            normalize_move_group(" default "),
            Some(String::new()),
            "带空白的 default 同样是移出分组"
        );
    }

    /// 四种传输各一条标签 —— 标签错会让用户在侧边栏认错会话。
    #[test]
    fn conn_label_is_per_transport() {
        assert_eq!(conn_label(&session_of(SessionKind::Ssh)), "root@10.0.0.5");
        assert_eq!(conn_label(&session_of(SessionKind::Serial)), "COM3 @115200");
        assert_eq!(
            conn_label(&session_of(SessionKind::Telnet)),
            "telnet 10.0.0.5:22"
        );
        assert_eq!(conn_label(&session_of(SessionKind::Local)), "local prod");
    }

    /// 只有 SSH 有 SFTP 旁路（否则串口 / Telnet 会显示一个永远连不上的 SFTP 面板）。
    #[test]
    fn only_ssh_has_sftp() {
        assert!(session_has_sftp(&SessionKind::Ssh));
        assert!(!session_has_sftp(&SessionKind::Serial));
        assert!(!session_has_sftp(&SessionKind::Telnet));
        assert!(!session_has_sftp(&SessionKind::Local));
    }

    /// 空输入**回落**而不是清空：先保存的会话名、再内置本地会话名，都没有就什么都不做。
    #[test]
    fn tab_title_falls_back_then_gives_up() {
        assert_eq!(
            resolve_tab_title("  my-tab  ", None, None),
            Some("my-tab".to_string()),
            "非空 → 用 trim 后的名字"
        );
        assert_eq!(
            resolve_tab_title("", Some("prod".into()), Some("WSL".into())),
            Some("prod".to_string()),
            "优先回落保存的会话名"
        );
        assert_eq!(
            resolve_tab_title("   ", None, Some("WSL".into())),
            Some("WSL".to_string()),
            "再回落内置本地会话名"
        );
        assert_eq!(
            resolve_tab_title("", None, None),
            None,
            "都没有 → 不动标题（不能清空）"
        );
    }
}
