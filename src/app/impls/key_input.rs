//! Keyboard / input callback wiring, extracted from `app.rs` (refactor plan
//! stage D). Everything here is Slint-thread-only: it receives key/tunnel/
//! paste events from the UI and turns them into PTY bytes or session actions.

use std::cell::RefCell;
use alacritty_terminal::grid::Dimensions;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::session::ConnectCtx;
use crate::ssh::SessionCommand;
use crate::terminal::{
    bare_ctrl_marker_workaround_enabled, clear_pending_paste, encode_command_bar_input,
    encode_pasted_text, is_back_tab, key_to_pty_bytes, paste_requires_large_review,
    scrollback_key_from_text, should_drop_bare_ctrl_marker, store_pending_paste,
    take_pending_paste, terminal_uses_bracketed_paste, PendingPaste, BACK_TAB_BYTES,
};
#[cfg(windows)]
use crate::terminal::c0_letter_key_down;
use crate::ui::{AppWindow, TermMatch, TermSpan};
use crate::app::quick_commands::{all_quick_group_names, quick_cmd_model, reorder_quick_command};
use crate::app::pane_layout::zoom_term_font;
use crate::app::session_runtime::start_session_in_tab;
use crate::app::{AppContext, INTERACTIVE_ECHO_WINDOW, clipboard_set_text, convert_eol, set_terminal_row, term_buf, with_term_buf};
use crate::app::terminal_ui::{apply_terminal_resize, compute_find_matches, history_model, history_preview_model, history_view_model, rebuild_tab_display, refresh_terminal_selection};
use super::render_tickets::request_tab_render_from_ui;

// 把隧道表单字段解析成 PortForward；kind 非 local/dynamic 或端口非法时返回 None。
pub(crate) fn parse_tunnel_forward(
    kind: &str,
    name: &str,
    bind: &str,
    bind_port: &str,
    host: &str,
    host_port: &str,
) -> Option<crate::config::PortForward> {
    if kind != "local" && kind != "dynamic" {
        return None;
    }
    let bind_port = bind_port.trim().parse::<u16>().ok()?;
    let host_port = if kind == "dynamic" {
        0
    } else {
        host_port.trim().parse::<u16>().ok()?
    };
    Some(crate::config::PortForward {
        kind: kind.to_string(),
        name: name.trim().to_string(),
        bind_addr: bind.trim().to_string(),
        bind_port,
        host: host.trim().to_string(),
        host_port,
    })
}

#[path = "key_input_commands.rs"] mod key_input_commands;
#[path = "key_input_history.rs"] mod key_input_history;
#[path = "key_input_quick.rs"] mod key_input_quick;
#[path = "key_input_keys.rs"] mod key_input_keys;
#[path = "key_input_view.rs"] mod key_input_view;
#[path = "key_input_paste.rs"] mod key_input_paste;
#[path = "key_input_select.rs"] mod key_input_select;

pub(crate) fn wire_key_input(window: &AppWindow, app: &AppContext) {
    key_input_commands::wire_key_commands(window, app);
    key_input_history::wire_key_history(window, app);
    key_input_quick::wire_key_quick(window, app);
    key_input_keys::wire_key_keys(window, app);
    key_input_view::wire_key_view(window, app);
    key_input_paste::wire_key_paste(window, app);
    key_input_select::wire_key_select(window, app);
}

/// Render a key string for diagnostic logs WITHOUT leaking its content (#15).
///
/// Any printable character could be a password character, so we never emit it.
/// Only C0/C1 control code points (Backspace, Esc, the IME-injected 0x10/0x15
/// markers, …) are revealed — those are exactly what the Shift/Backspace IME
/// diagnostics need and are never password material. Printable characters are
/// collapsed to a count, so the logs stay useful without exposing keystrokes.
pub(crate) fn redact_key(key: &str) -> String {
    if key.is_empty() {
        return "(empty)".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut printable = 0usize;
    for c in key.chars() {
        let cp = c as u32;
        if cp < 0x20 || (0x7f..=0x9f).contains(&cp) {
            parts.push(format!("U+{cp:04X}"));
        } else {
            printable += 1;
        }
    }
    if printable > 0 {
        parts.push(format!("<{printable} printable redacted>"));
    }
    parts.join(",")
}

