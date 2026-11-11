//! Top-level UI state machine.
//!
//! Responsibilities:
//!   * Load the config store and expose sessions to Slint.
//!   * Drive the 1-Hz system sampler.
//!   * Manage the tab list + per-tab `SessionHandle` map.
//!   * Route Slint callbacks to the right domain module.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
#[path = "impls/term_buffers.rs"]
mod term_buffers;
#[path = "impls/run.rs"] mod run;
#[path = "impls/render_fallback.rs"] mod render_fallback;
#[path = "impls/session_filters.rs"] mod session_filters;
#[path = "impls/proxy_edit.rs"] mod proxy_edit;
#[path = "impls/ui_helpers.rs"] mod ui_helpers;
#[path = "struct/activity.rs"] mod activity;
#[path = "struct/app_context.rs"] mod app_context;
#[path = "struct/secret_cred.rs"] mod secret_cred;
#[cfg(test)]
#[path = "impls/process_row_tests.rs"] mod process_row_tests;
#[cfg(test)]
#[path = "impls/port_forward_tests.rs"] mod port_forward_draft_tests;
#[cfg(test)]
#[path = "impls/history_view_tests.rs"] mod history_view_tests;
#[cfg(test)]
#[path = "impls/key_input_tests.rs"] mod key_tests;
#[cfg(test)]
#[path = "impls/log_highlight_tests.rs"] mod log_highlight_tests;
#[cfg(test)]
#[path = "impls/fonts_ui_tests.rs"] mod font_choice_tests;

pub(crate) use term_buffers::*;
pub(crate) use run::{run, run_renderer_probe};
pub(crate) use render_fallback::*;
pub(crate) use session_filters::*;
pub(crate) use proxy_edit::*;
pub(crate) use ui_helpers::*;
pub(crate) use activity::WinActivity;
pub(crate) use app_context::AppContext;
pub(crate) use secret_cred::SecretCred;


use anyhow::{Context, Result};
use i_slint_backend_winit::WinitWindowAccessor as _;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::runtime::Runtime;

use crate::config::{
    AuthMethod, ConfigStore, OutputHighlightRule, Secret, Session, SessionKind,
    is_reserved_session_group,
};
use crate::i18n::t;

// Dependency versions baked in by build.rs → $OUT_DIR/deps.rs.
include!(concat!(env!("OUT_DIR"), "/deps.rs"));
use crate::resource::system::{format_bytes_per_sec, format_mem};
use crate::resource::{LocalSnap, NetHist, TabStatus, TabStatuses};
use crate::resource::SystemSnapshot;
use crate::session::{ConnectCtx, PendingCred, PendingHostKey, PendingMfa};
use crate::sftp::{SftpHandles, SftpLastCwd, spawn_sftp};
use crate::ssh::{
    ProcInfo, SessionCommand, SessionEvent, SessionHandle, SystemDetails, format_mtime,
    format_size, spawn_session,
};
#[cfg(test)]
use crate::terminal::{
    encode_command_bar_input, encode_pasted_text, key_to_pty_bytes, paste_requires_large_review,
    should_drop_bare_ctrl_marker,
    CompiledOutputRule, HistSpan, build_row, highlight_plain_output, log_level_marker,
    normalize_pasted_newlines, process_bytes, text_cell_width,
};
use crate::terminal::{
    OutputHighlightPreset, RenderGates, TermBuffer, TermBufferHandle,
    TermBuffers, cell_prefix, compile_output_rules,
};
#[cfg(any(target_os = "windows", test))]
use crate::terminal::{CtrlKeySide, windows_process_ctrl_release};
use crate::ui::*;
use crate::webdav::WebDavAcceptAnyCertVerifier;
#[path = "impls/auth_dialogs.rs"]
pub(crate) mod auth_dialogs;
#[path = "impls/self_updater/mod.rs"]
pub(crate) mod self_updater;
#[path = "impls/port_forward.rs"]
mod port_forward;
#[path = "impls/quick_commands.rs"]
mod quick_commands;
#[path = "impls/resource_ui.rs"]
pub(crate) mod resource_ui;
#[path = "impls/session_event.rs"]
mod session_event;
#[path = "impls/session_models.rs"]
mod session_models;
#[path = "impls/session_runtime.rs"]
mod session_runtime;
#[path = "impls/session_trigger.rs"]
mod session_trigger;
#[path = "impls/sftp_callbacks.rs"]
mod sftp_callbacks;
#[path = "impls/sftp_ui.rs"]
mod sftp_ui;
#[path = "impls/sidebar.rs"]
mod sidebar;
#[path = "impls/tab_callbacks.rs"]
mod tab_callbacks;
#[path = "impls/tab_callbacks_navigation.rs"]
mod tab_callbacks_navigation;
pub(crate) use tab_callbacks_navigation::*;
#[path = "impls/tab_callbacks_lifecycle.rs"]
mod tab_callbacks_lifecycle;
pub(crate) use tab_callbacks_lifecycle::*;
#[path = "impls/tab_callbacks_panes.rs"]
mod tab_callbacks_panes;
pub(crate) use tab_callbacks_panes::*;
#[path = "impls/tab_ops.rs"]
mod tab_ops;
#[path = "impls/terminal_ui.rs"]
mod terminal_ui;
#[path = "impls/webdav.rs"]
mod webdav;
#[path = "impls/window.rs"]
mod window;
#[path = "impls/window_geometry.rs"]
mod window_geometry;
#[path = "impls/key_input.rs"]
mod key_input;
#[path = "impls/mcp_ui/mod.rs"]
pub(crate) mod mcp_ui;
#[path = "impls/session_callbacks.rs"]
mod session_callbacks;
#[path = "impls/pane_layout.rs"]
mod pane_layout;
#[path = "impls/fonts_ui.rs"]
mod fonts_ui;
#[path = "impls/updater.rs"]
mod updater;
#[path = "impls/update_net.rs"]
mod update_net;
#[path = "impls/sampler.rs"]
mod sampler;
#[path = "impls/mem.rs"]
mod mem;
#[path = "impls/settings/mod.rs"]
mod settings;
#[path = "impls/settings_ui.rs"]
mod settings_ui;
#[path = "impls/settings_window.rs"]
mod settings_window;
#[path = "impls/sub_windows.rs"]
mod sub_windows;
#[path = "impls/window_chrome.rs"]
mod window_chrome;
#[cfg(test)]
#[path = "impls/contract_sources.rs"]
mod contract_sources;
use window_chrome::wire_window_chrome;
use sampler::spawn_system_sampler;
use updater::wire_update_check;
pub(crate) use fonts_ui::{FontEntry, font_choices, resolve_ui_font_family, term_font_covers_cjk};
pub(crate) use pane_layout::{
    refresh_panes, save_layout, update_terminal_row,
};
pub(crate) use window_geometry::{
    center_window, handle_file_drop, handle_macos_terminal_wheel,
    place_main_window_before_show,
};

#[path = "impls/render_tickets.rs"]
mod render_tickets;
use self::auth_dialogs::*;
use self::quick_commands::*;
use self::resource_ui::*;
use self::session_event::*;
use self::session_models::*;
use self::sftp_callbacks::*;
use self::sftp_ui::*;
use self::sidebar::*;
use self::tab_callbacks::*;
use self::terminal_ui::*;
use self::window::*;




thread_local! {
    /// The config store, made reachable from the Slint-thread event handler so
    /// terminal-captured commands (#113) can be appended to history. Set once at
    /// startup; only touched on the Slint event-loop thread.
    static HISTORY_STORE: RefCell<Option<Rc<RefCell<ConfigStore>>>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// Host-key confirmation (#109-5)
// ---------------------------------------------------------------------------

thread_local! {
    /// Prompts awaiting a decision; the front one is shown. Lives on the Slint
    /// event-loop thread (all access is from there).
    static HOSTKEY_QUEUE: RefCell<VecDeque<PendingHostKey>> = const { RefCell::new(VecDeque::new()) };
    /// host:port → decision, remembered for this run so a duplicate prompt
    /// (second connection to the same host) is answered without a new dialog.
    static HOSTKEY_DECIDED: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
}

// ---------------------------------------------------------------------------
// Connect-time credential prompt (#110)
// ---------------------------------------------------------------------------

thread_local! {
    static CRED_QUEUE: RefCell<VecDeque<PendingCred>> = const { RefCell::new(VecDeque::new()) };
    /// session id → the accepted answer given this run, so a second connection
    /// for the same session is answered without re-prompting.
    ///
    /// 存的是 `SecretCred`（口令零化 + Debug 屏蔽）而不是裸 `(String, String, bool)`：
    /// 口令不该以明文 `String` 在进程内存里躺一整个运行期。只缓存**接受**的回答，
    /// 取消不进缓存（否则后续尝试会被静默判成取消）。
    static CRED_DECIDED: RefCell<HashMap<String, SecretCred>> = RefCell::new(HashMap::new());
}


// ---------------------------------------------------------------------------
// MFA / keyboard-interactive prompt (#86-MFA)
// ---------------------------------------------------------------------------

thread_local! {
    static MFA_QUEUE: RefCell<VecDeque<PendingMfa>> = const { RefCell::new(VecDeque::new()) };
}

// ---------------------------------------------------------------------------
// Split panes (v0.5)
// ---------------------------------------------------------------------------





#[path = "impls/version.rs"]
mod version;
pub(crate) use version::{is_test_build, parse_version, Version};

