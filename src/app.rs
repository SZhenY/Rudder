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

use alacritty_terminal::grid::Dimensions;

/// Max bytes merged into one Output event before starting a fresh chunk (#209).
/// Keeps a single UI callback from spending hundreds of ms in vt100 ingest.
const OUTPUT_MERGE_BYTE_CAP: usize = 64 * 1024;

/// Output parsed between UI-flush checkpoints during sustained traffic.
const INGEST_FRAME_BUDGET: usize = 64 * 1024;


/// Do not deliberately pace a pump while a large unbounded-channel backlog is
/// already present. It catches up first, then paces the tail of the stream.
const PACED_LOCAL_BACKLOG_LIMIT: usize = 1024 * 1024;
const PACED_QUEUE_EVENT_LIMIT: usize = 256;

const INTERACTIVE_ECHO_WINDOW: std::time::Duration = std::time::Duration::from_millis(180);

fn term_buf(bufs: &TermBuffers, tab_id: &str) -> Option<TermBufferHandle> {
    bufs.lock().unwrap().get(tab_id).cloned()
}

pub(crate) fn with_term_buf<R>(
    bufs: &TermBuffers,
    tab_id: &str,
    f: impl FnOnce(&mut TermBuffer) -> R,
) -> Option<R> {
    let h = term_buf(bufs, tab_id)?;
    let mut guard = h.lock().unwrap();
    Some(f(&mut guard))
}

fn ingest_terminal_output(bufs: &TermBuffers, tab_id: &str, chunk: &[u8]) -> Vec<u8> {
    if let Some(h) = term_buf(bufs, tab_id) {
        h.lock().unwrap().ingest(chunk)
    } else {
        Vec::new()
    }
}

fn record_ingested_chunk(chunk_len: usize, ingested_since_checkpoint: &mut usize) -> bool {
    debug_assert!(*ingested_since_checkpoint < INGEST_FRAME_BUDGET);
    if chunk_len == 0 {
        return false;
    }

    let remaining = INGEST_FRAME_BUDGET - *ingested_since_checkpoint;
    if chunk_len < remaining {
        *ingested_since_checkpoint += chunk_len;
        false
    } else {
        *ingested_since_checkpoint = (chunk_len - remaining) % INGEST_FRAME_BUDGET;
        true
    }
}

fn event_requires_immediate_ui(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::Connected
            | SessionEvent::Closed(_)
            | SessionEvent::HostKeyPrompt { .. }
            | SessionEvent::CredentialPrompt { .. }
            | SessionEvent::MfaPrompt { .. }
    )
}

#[cfg(test)]
mod ingest_frame_tests {
    use super::{INGEST_FRAME_BUDGET, event_requires_immediate_ui, record_ingested_chunk};
    use crate::ssh::SessionEvent;

    fn count_requests(chunk_lengths: &[usize]) -> (usize, usize) {
        let mut since_checkpoint = 0usize;
        let mut requests = 0usize;
        let mut dirty_since_request = false;
        for &chunk_len in chunk_lengths {
            dirty_since_request = true;
            if record_ingested_chunk(chunk_len, &mut since_checkpoint) {
                requests += 1;
                dirty_since_request = false;
            }
        }
        if dirty_since_request {
            requests += 1;
        }
        (requests, since_checkpoint)
    }

    #[test]
    fn exact_frame_budget_chunks_do_not_add_an_empty_tail_request() {
        let (requests, remainder) = count_requests(&[INGEST_FRAME_BUDGET, INGEST_FRAME_BUDGET]);
        assert_eq!(requests, 2);
        assert_eq!(remainder, 0);
    }

    #[test]
    fn a_partial_tail_gets_one_final_request() {
        let (requests, remainder) = count_requests(&[INGEST_FRAME_BUDGET, INGEST_FRAME_BUDGET, 1]);
        assert_eq!(requests, 3);
        assert_eq!(remainder, 1);
    }

    #[test]
    fn checkpoint_budget_carries_across_input_events() {
        let mut since_checkpoint = 0usize;
        assert!(!record_ingested_chunk(
            INGEST_FRAME_BUDGET - 1,
            &mut since_checkpoint
        ));
        assert!(record_ingested_chunk(1, &mut since_checkpoint));
        assert_eq!(since_checkpoint, 0);
    }

    #[test]
    fn an_oversized_output_event_stays_one_atomic_checkpoint() {
        let (requests, remainder) = count_requests(&[INGEST_FRAME_BUDGET * 2 + 1]);
        assert_eq!(requests, 1);
        assert_eq!(remainder, 1);
    }

    #[test]
    fn routine_shell_metadata_does_not_disable_tail_pacing() {
        assert!(!event_requires_immediate_ui(&SessionEvent::CommandRan(
            "tail -n 1000000 app.log".into()
        )));
        assert!(!event_requires_immediate_ui(&SessionEvent::CwdChanged(
            "/var/log".into()
        )));
        assert!(event_requires_immediate_ui(&SessionEvent::Connected));
        assert!(event_requires_immediate_ui(&SessionEvent::Closed(
            "connection lost".into()
        )));
    }
}

use anyhow::{Context, Result};
use i_slint_backend_winit::WinitWindowAccessor;
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
    format_size, spawn_session, test_session_auth,
};
#[cfg(windows)]
use crate::terminal::c0_letter_key_down;
#[cfg(test)]
use crate::terminal::{
    CompiledOutputRule, HistSpan, build_row, highlight_plain_output, log_level_marker,
    normalize_pasted_newlines, process_bytes, text_cell_width,
};
use crate::terminal::{
    CsiState, OutputHighlightPreset, RenderGates, TabRenderGate, TermBuffer, TermBufferHandle,
    TermBuffers, bare_ctrl_marker_workaround_enabled, cell_prefix, compile_output_rules,
    encode_command_bar_input, encode_pasted_text, key_to_pty_bytes, new_term,
    paste_requires_large_review, should_drop_bare_ctrl_marker, terminal_uses_bracketed_paste,
};
#[cfg(any(target_os = "windows", test))]
use crate::terminal::{CtrlKeySide, windows_process_ctrl_release};
use crate::ui::*;
use crate::webdav::WebDavAcceptAnyCertVerifier;
mod auth_dialogs;
mod port_forward;
mod quick_commands;
mod resource_ui;
mod session_event;
mod session_models;
mod session_runtime;
mod session_trigger;
mod sftp_callbacks;
mod sftp_ui;
mod sidebar;
mod tab_callbacks;
mod terminal_ui;
mod webdav;
mod window;
mod window_geometry;
mod pane_layout;
mod fonts_ui;
mod updater;
mod sampler;
use sampler::spawn_system_sampler;
use updater::wire_update_check;
pub(crate) use fonts_ui::{FontEntry, family_from_label, font_choices, resolve_ui_font_family, term_font_covers_cjk};
pub(crate) use pane_layout::{
    drag_target, refresh_panes, save_layout, update_terminal_row, zoom_term_font,
};
pub(crate) use window_geometry::{
    center_window, handle_file_drop, handle_macos_terminal_wheel,
    macos_terminal_wheel_can_target_terminal,
};

mod render_tickets;
use render_tickets::RENDER_MIN_INTERVAL;
use self::auth_dialogs::*;
use self::port_forward::*;
use self::quick_commands::*;
use self::resource_ui::*;
use self::session_event::*;
use self::session_models::*;
use self::session_trigger::*;
use self::session_runtime::*;
use self::sftp_callbacks::*;
use self::sftp_ui::*;
use self::sidebar::*;
use self::tab_callbacks::*;
use self::terminal_ui::*;
use self::webdav::*;
use self::window::*;

fn tab_title_len(title: &str) -> i32 {
    title
        .chars()
        .map(|ch| if ch.is_ascii() { 1usize } else { 2usize })
        .sum::<usize>()
        .min(i32::MAX as usize) as i32
}

fn should_block_close(exit_confirmed: bool, has_live_sessions: bool) -> bool {
    !exit_confirmed && has_live_sessions
}

/// Tab ids currently shown in a pane (`term.id == pane.active-id` in Slint).
pub(crate) fn visible_tab_ids(win: &AppWindow) -> HashSet<String> {
    use slint::Model as _;
    let mut out = HashSet::new();
    let panes = win.get_panes();
    if let Some(pm) = panes.as_any().downcast_ref::<VecModel<PaneInfo>>() {
        for i in 0..pm.row_count() {
            if let Some(pane) = pm.row_data(i) {
                out.insert(pane.active_id.to_string());
            }
        }
    }
    out
}

/// Number of samples kept for the sparkline.
const NET_HISTORY_LEN: usize = 60;



#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WinActivity {
    Active,     // focused & visible → full rate
    Background, // visible but unfocused → throttled
    Hidden,     // minimized / occluded → paused
}
pub fn run() -> Result<()> {
    // Load the renderer preference before creating any Slint window. Reuse the
    // same store for the rest of the app so startup does not read the config
    // twice merely to select a backend (#280).
    let config = ConfigStore::load().context("failed to load config")?;

    // Windows frameless-window attributes must be fixed before the first Slint
    // window is created; doing it afterwards leaves some Win10 machines with an
    // invisible frame that shifts mouse hit testing (#193).
    #[cfg(windows)]
    setup_windows_platform(config.renderer_mode());

    // Linux renderer selection from Settings (SLINT_BACKEND still wins).
    #[cfg(target_os = "linux")]
    setup_linux_platform(config.renderer_mode());

    // Immersive native title bar on macOS (must precede the first window).
    #[cfg(target_os = "macos")]
    setup_macos_platform(config.renderer_mode());

    // --- Runtime + store -------------------------------------------------
    let runtime = Arc::new(Runtime::new().context("failed to start tokio runtime")?);
    let store = Rc::new(RefCell::new(config));
    // Reachable from the Slint-thread event handler for recording terminal
    // commands into history (#113).
    HISTORY_STORE.with(|s| *s.borrow_mut() = Some(store.clone()));

    // Per-tab SSH handles (shell only; lives on Slint thread via Rc).
    let handles: Rc<RefCell<HashMap<String, SessionHandle>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Per-tab SFTP handles — Arc<Mutex> so the event-pump OS thread and the
    // Slint UI thread can both post SftpCommands.
    let sftp_handles: SftpHandles = Arc::new(Mutex::new(HashMap::new()));
    // Per-tab cwd the SFTP panel last followed (see SftpLastCwd).
    let sftp_last_cwd: SftpLastCwd = Arc::new(Mutex::new(HashMap::new()));

    // Per-tab vt100 parsers + history logs (Arc<Mutex> so they can be cloned
    // into the thread that pumps session events into invoke_from_event_loop).
    let bufs: TermBuffers = Arc::new(Mutex::new(HashMap::new()));
    let render_gates: RenderGates = Arc::new(Mutex::new(HashMap::new()));

    // Last-known terminal pixel dimensions, updated by every terminal-resize
    // callback.  Shared so on_connect_session can pass a sensible initial PTY
    // size to spawn_session before the first resize callback fires.
    // Default: 80 cols × 24 rows (SSH spec minimum).
    let last_term_size: Arc<Mutex<(u32, u32)>> = Arc::new(Mutex::new((80, 24)));

    // --- Build window + models ------------------------------------------
    // Set the Wayland app_id / X11 WM_CLASS *before* the window is created so
    // the Linux desktop shell can match the running window to the installed
    // `rudder.desktop` entry and show our icon in the dock/taskbar.  (On
    // Windows the icon comes from the embedded .ico, so this is a no-op there.)
    let _ = slint::set_xdg_app_id("rudder");
    let window = AppWindow::new().context("failed to build Slint window")?;
    // Slint applies preferred-width/height while the native window is being
    // created. Do not treat those startup Resized events as user adjustments;
    // otherwise they overwrite the persisted size before restoration (#278).
    let window_size_tracking_ready = Rc::new(Cell::new(false));
    let pending_window_size_restore = Rc::new(Cell::new(None::<(f32, f32)>));

    // Show the crate version (from Cargo.toml at compile time) in the sidebar,
    // so the footer never drifts out of sync with the actual build.
    window.set_app_version(env!("CARGO_PKG_VERSION").into());

    // Set the window icon from the PNG embedded in the binary so the dock
    // shows the correct icon even without a system-installed .desktop entry
    // (e.g. AppImage without AppImageLauncher, or plain binary in ~/bin).
    #[cfg(target_os = "linux")]
    set_window_icon(&window);

    // The window defaults to frameless + custom title bar (#119). macOS keeps
    // its native decorations, so turn the custom bar off there.
    #[cfg(target_os = "macos")]
    window.set_custom_titlebar(false);

    // --- Detachable process monitor window (#23) -----------------------------
    // The process table is its own top-level OS window so it can be dragged
    // outside the main window (or onto a second monitor). Both windows render
    // the *same* VecModel, so the table stays live wherever it's parked; closing
    // it just hides it, so reopening is instant.
    let proc_rows_model: Rc<VecModel<ProcRow>> = Rc::new(VecModel::default());
    window.set_proc_list(ModelRc::from(proc_rows_model.clone()));
    let sys_metrics_model: Rc<VecModel<SysMetricRow>> = Rc::new(VecModel::default());
    let sys_net_rows_model: Rc<VecModel<SysNetRow>> = Rc::new(VecModel::default());
    let sys_disks_model: Rc<VecModel<DiskInfo>> = Rc::new(VecModel::default());
    let sys_overview_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_cpu_info_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_gpu_info_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_cpu_usage_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_memory_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_swap_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_network_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    let sys_filesystem_model: Rc<VecModel<SysInfoRow>> = Rc::new(VecModel::default());
    window.set_sys_metrics(ModelRc::from(sys_metrics_model.clone()));
    window.set_sys_net_rows(ModelRc::from(sys_net_rows_model.clone()));
    window.set_sys_disks(ModelRc::from(sys_disks_model.clone()));
    window.set_sys_overview_rows(ModelRc::from(sys_overview_model.clone()));
    window.set_sys_cpu_info_rows(ModelRc::from(sys_cpu_info_model.clone()));
    window.set_sys_gpu_info_rows(ModelRc::from(sys_gpu_info_model.clone()));
    window.set_sys_cpu_usage_rows(ModelRc::from(sys_cpu_usage_model.clone()));
    window.set_sys_memory_rows(ModelRc::from(sys_memory_model.clone()));
    window.set_sys_swap_rows(ModelRc::from(sys_swap_model.clone()));
    window.set_sys_network_rows(ModelRc::from(sys_network_model.clone()));
    window.set_sys_filesystem_rows(ModelRc::from(sys_filesystem_model.clone()));
    let proc_win = ProcWindow::new().context("failed to build process window")?;
    proc_win.set_custom_titlebar(cfg!(not(target_os = "macos")));
    proc_win.set_proc_list(ModelRc::from(proc_rows_model.clone()));
    let sys_win = SystemInfoWindow::new().context("failed to build system info window")?;
    sys_win.set_custom_titlebar(cfg!(not(target_os = "macos")));
    sys_win.set_metrics(ModelRc::from(sys_metrics_model.clone()));
    sys_win.set_nets(ModelRc::from(sys_net_rows_model.clone()));
    sys_win.set_disks(ModelRc::from(sys_disks_model.clone()));
    sys_win.set_overview_rows(ModelRc::from(sys_overview_model.clone()));
    sys_win.set_cpu_info_rows(ModelRc::from(sys_cpu_info_model.clone()));
    sys_win.set_gpu_info_rows(ModelRc::from(sys_gpu_info_model.clone()));
    sys_win.set_cpu_usage_rows(ModelRc::from(sys_cpu_usage_model.clone()));
    sys_win.set_memory_rows(ModelRc::from(sys_memory_model.clone()));
    sys_win.set_swap_rows(ModelRc::from(sys_swap_model.clone()));
    sys_win.set_network_rows(ModelRc::from(sys_network_model.clone()));
    sys_win.set_filesystem_rows(ModelRc::from(sys_filesystem_model.clone()));
    {
        // ✕ hides the window (data keeps flowing into the shared model).
        let weak = proc_win.as_weak();
        proc_win.on_request_close(move || {
            if let Some(w) = weak.upgrade() {
                let _ = w.hide();
            }
        });
    }
    {
        proc_win.on_copy_pid(move |pid: SharedString| {
            let text = pid.to_string();
            std::thread::spawn(move || clipboard_set_text(text));
        });
    }
    {
        // Frameless titlebar drag, via winit on the process window's own handle.
        let weak = proc_win.as_weak();
        proc_win.on_win_drag(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_window();
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        // Bottom-right resize grip.
        use i_slint_backend_winit::winit::window::ResizeDirection;
        let weak = proc_win.as_weak();
        proc_win.on_win_resize_se(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_resize_window(ResizeDirection::SouthEast);
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        // The sidebar "Processes" button shows / focuses the window.
        let win_weak = window.as_weak();
        let proc_weak = proc_win.as_weak();
        window.on_open_processes(move || {
            let (Some(main), Some(pw)) = (win_weak.upgrade(), proc_weak.upgrade()) else {
                return;
            };
            pw.set_host(main.get_connection_state());
            sync_proc_theme(&main, &pw);
            let _ = pw.show();
            place_process_window(&main, &pw);
            pw.window().with_winit_window(|ww| ww.focus_window());
        });
    }
    {
        let weak = sys_win.as_weak();
        sys_win.on_request_close(move || {
            if let Some(w) = weak.upgrade() {
                let _ = w.hide();
            }
        });
    }
    {
        let weak = sys_win.as_weak();
        sys_win.on_win_drag(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_window();
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        use i_slint_backend_winit::winit::window::ResizeDirection;
        let weak = sys_win.as_weak();
        sys_win.on_win_resize_se(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_resize_window(ResizeDirection::SouthEast);
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        let win_weak = window.as_weak();
        let sys_weak = sys_win.as_weak();
        window.on_open_system_info(move || {
            let (Some(main), Some(sw)) = (win_weak.upgrade(), sys_weak.upgrade()) else {
                return;
            };
            // Detailed system information is remote-only. Keep this guard even
            // though the sidebar hides/disables its affordance when unavailable.
            if !main.get_system_info_available() {
                return;
            }
            sw.set_host(main.get_conn_host());
            sw.set_connection_state(main.get_connection_state());
            sw.set_resource_title(main.get_resource_title());
            sync_system_info_theme(&main, &sw);
            let _ = sw.show();
            place_system_info_window(&main, &sw);
            sw.window().with_winit_window(|ww| ww.focus_window());
        });
    }

    // Apply the saved UI language.  The Rust-side flag drives `i18n::t(...)`;
    // `apply_to_slint` selects the bundled `.po` for the static `@tr(...)` text
    // (must run after the first component exists, which it now does).
    crate::i18n::set_language(store.borrow().language());
    crate::i18n::apply_to_slint();
    window.set_lang_en(crate::i18n::is_en());

    // Apply the saved (or system-detected) theme.
    // "dark" / "light" → use that directly; "system" or unset → ask the OS;
    // OS unknown → fall back to dark.
    {
        let is_dark = theme_pref_is_dark(&store.borrow());
        window.set_dark_mode(is_dark);
    }
    // On macOS, app shortcuts use Cmd (⌘) so physical Ctrl stays free for the
    // shell (#158); on Windows/Linux they stay Ctrl-based.
    window.set_is_mac(cfg!(target_os = "macos"));
    window.set_is_windows(cfg!(windows));

    // Apply the saved terminal font (Interface settings). An empty family keeps
    // the built-in default; the size always applies (defaults to 13).
    {
        let s = store.borrow();
        let fam = s.font_family().to_string();
        // Does the active terminal font cover CJK? Terminal spans then keep
        // it for Chinese text (italic/thin variants apply) instead of falling
        // back to the UI sans font (#54). Family-name tag probe: CN/SC/TC/
        // JP/KR/CJK/Han. An empty family means the embedded JetBrains Mono
        // default, which has no CJK glyphs → Chinese falls back to the UI
        // font (external CJK fonts like Maple Mono CN self-identify via "CN").
        let cjk = term_font_covers_cjk(if fam.is_empty() {
            "JetBrains Mono"
        } else {
            &fam
        });
        if !fam.is_empty() {
            window.set_term_font_family(fam.into());
        }
        window.set_term_font_cjk(cjk);
        window.set_term_font_size(s.font_size() as f32);
        window.set_term_font_bold(s.terminal_bold());
        window.set_scrollback_lines(s.scrollback_lines().to_string().into());
        window.set_term_cursor_style(s.terminal_cursor_style().into());
        if let Some(color) = parse_hex_color(s.terminal_cursor_color()) {
            window.set_term_cursor_color_hex(s.terminal_cursor_color().into());
            window.set_term_cursor_color(color);
        }
        window.set_output_highlight_enabled(s.output_highlight_enabled());
        window.set_output_highlight_preset(s.output_highlight_preset().into());
        window.set_output_highlight_rules(output_highlight_rule_model(&s));
        window.set_json_format_output(s.json_format_output());
        window.set_ui_scale(s.ui_scale() as f32 / 100.0); // global UI zoom (#100)
        window.set_panel_font(s.panel_font() as f32 / 100.0); // settings-panel font scale
        window.set_renderer_mode(s.renderer_mode().into());
    }

    // Apply the saved immersive wallpaper (overrides dark/light when set; a
    // missing custom file falls back to the plain theme).
    {
        let id = store.borrow().wallpaper().to_string();
        // Restoring a saved wallpaper must not override the user's persisted
        // light/dark preference. Built-in wallpapers only suggest their paired
        // theme when the user actively selects them (#theme-persistence).
        apply_wallpaper(&window, &store.borrow(), &bufs, &id, false);
    }
    // Editable inputs (e.g. the SFTP path bar) need a CJK-capable font: the
    // embedded mono font has no Chinese glyphs and native TextInput doesn't
    // glyph-fallback like Text does, so typed Chinese would render as tofu (#54).
    //
    // We must NOT hard-code one system font name: on macOS 26 (Tahoe) fontdb
    // failed to register "PingFang SC", so the UI default font resolved to nothing
    // and *all* text vanished (#129) — icons survived only because they use an
    // embedded font. Instead probe what fontdb actually loaded and pick the first
    // resolvable CJK family, falling back to the embedded "Meatshell Mono" so the
    // window is never fully blank even when the system font DB is unreadable.
    window.set_ui_font_family(resolve_ui_font_family());
    // Runtime font loading: fonts dropped into the fonts dir (Windows:
    // <exe_dir>/config/fonts; macOS/Linux: per-user config dir) are registered
    // with Slint's shared collection and become selectable below — large CJK
    // families like Maple Mono no longer need to be embedded at build time.
    let external_fonts = crate::fonts::load_external_fonts(&crate::fonts::external_fonts_dir());
    // Populate the Interface font picker: embedded first, external next,
    // system monospace families last, each labelled with its source.
    let (font_labels, font_entries) = font_choices(&external_fonts, true);
    window.set_term_fonts(ModelRc::from(Rc::new(VecModel::from(font_labels))));
    // Restore the saved family: find its index in the picker list (fall back
    // to the first selectable family when it isn't listed).
    let saved_family = store.borrow().font_family().to_string();
    let font_index = font_entries
        .iter()
        .position(|e| matches!(e, FontEntry::Family(f) if *f == saved_family))
        .or_else(|| {
            font_entries
                .iter()
                .position(|e| matches!(e, FontEntry::Family(_)))
        })
        .unwrap_or(0);
    window.set_term_font_index(font_index as i32);

    // UI font picker: embedded + external + system (proportional fonts allowed).
    let (ui_labels, ui_entries) = font_choices(&external_fonts, false);
    window.set_ui_fonts(ModelRc::from(Rc::new(VecModel::from(ui_labels))));
    // Restore saved UI font index (empty string = auto-detect).
    let ui_saved = store.borrow().ui_font_family().to_string();
    let ui_index = if ui_saved.is_empty() {
        0 // "auto" position — first header
    } else {
        ui_entries
            .iter()
            .position(|e| matches!(e, FontEntry::Family(f) if *f == ui_saved))
            .unwrap_or(0)
    };
    window.set_ui_font_index(ui_index as i32);

    // Command bar (#55): seed quick commands + history from the config. Groups
    // start collapsed by default (#55).
    window.set_quick_commands(quick_cmd_model(
        &store.borrow(),
        &all_quick_group_names(&store.borrow()),
    ));
    window.set_command_history(history_model(&store.borrow()));
    window.set_history_view(history_view_model(&store.borrow(), "")); // #101

    // Interface setting: SFTP follows the terminal's cd. The shell event pumps
    // read this AtomicBool on every CwdChanged, so toggling applies live to
    // already-open sessions too.
    let sftp_follow_cd = Arc::new(std::sync::atomic::AtomicBool::new(
        store.borrow().sftp_follow_cd(),
    ));
    window.set_sftp_follow_cd(store.borrow().sftp_follow_cd());
    {
        let store = store.clone();
        let flag = sftp_follow_cd.clone();
        window.on_set_sftp_follow_cd(move |follow| {
            flag.store(follow, std::sync::atomic::Ordering::Relaxed);
            let mut s = store.borrow_mut();
            s.set_sftp_follow_cd(follow);
            let _ = s.save();
        });
    }

    // Interface setting: always ask where to save on download (#87). Read live
    // by the download handler from the window property, so just set + persist.
    window.set_download_always_ask(store.borrow().download_always_ask());
    {
        let store = store.clone();
        window.on_set_download_always_ask(move |ask| {
            let mut s = store.borrow_mut();
            s.set_download_always_ask(ask);
            let _ = s.save();
        });
    }

    // Toolbar toggle: hide/show the quick-command bar (persisted globally).
    window.set_cmd_bar_hidden(store.borrow().cmd_bar_hidden());
    {
        let store = store.clone();
        window.on_set_cmd_bar_hidden(move |hidden| {
            let mut s = store.borrow_mut();
            s.set_cmd_bar_hidden(hidden);
            if let Err(error) = s.save() {
                tracing::warn!("failed to save config: {error:#}");
            }
        });
    }

    // Zen (focus) mode: sidebar + tab strip hidden, persisted across launches.
    window.set_zen_mode(store.borrow().zen_mode());
    {
        let store = store.clone();
        window.on_set_zen_mode(move |enabled| {
            let mut s = store.borrow_mut();
            s.set_zen_mode(enabled);
            if let Err(error) = s.save() {
                tracing::warn!("failed to save config: {error:#}");
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_toggle_zen_key(move || {
            if let Some(w) = weak.upgrade() {
                let next = !w.get_zen_mode();
                let mut s = store.borrow_mut();
                s.set_zen_mode(next);
                if let Err(error) = s.save() {
                    tracing::warn!("failed to save config: {error:#}");
                }
                w.set_zen_mode(next);
            }
        });
    }

    // Terminal: EOL conversion + OSC 52 clipboard.  Read-once seed + persist.
    {
        let s = store.borrow();
        window.set_convert_eol(s.convert_eol());
        window.set_osc52_clipboard(s.osc52_clipboard());
        crate::terminal::vt_adapter::OSC52_ENABLED
            .store(s.osc52_clipboard(), std::sync::atomic::Ordering::Relaxed);
    }
    {
        let s = store.borrow();
        window.set_hide_special_partitions(s.hide_special_partitions());
        window.set_mount_filter(s.mount_filter().into());
    }

    // Interface setting: collapse the sidebars by default (#78). Seed the
    // checkboxes, apply the collapsed state once at startup, and persist toggles.
    {
        let s = store.borrow();
        let collapse_sidebar = s.collapse_sidebar_default();
        let collapse_sftp = s.collapse_sftp_default();
        let sidebar_dock = s.sidebar_dock();
        let welcome_as_sidebar = s.welcome_as_sidebar();
        let quick_commands_as_sidebar = s.quick_commands_as_sidebar();
        let quick_panel_open = quick_commands_as_sidebar && s.quick_panel_open();
        let quick_panel_collapsed = s.quick_panel_collapsed();
        let quick_panel_dock = s.quick_panel_dock();
        let welcome_sidebar_dock = s.welcome_sidebar_dock();
        let mut sidebar_collapsed = s.sidebar_collapsed().unwrap_or(collapse_sidebar);
        let mut welcome_collapsed = s.welcome_collapsed().unwrap_or(false);
        if welcome_as_sidebar
            && sidebar_dock == welcome_sidebar_dock
            && !sidebar_collapsed
            && !welcome_collapsed
        {
            sidebar_collapsed = true;
        }
        if quick_panel_open && !quick_panel_collapsed {
            if sidebar_dock == quick_panel_dock {
                sidebar_collapsed = true;
            }
            if welcome_as_sidebar && welcome_sidebar_dock == quick_panel_dock {
                welcome_collapsed = true;
            }
        }
        window.set_collapse_sidebar_default(collapse_sidebar);
        window.set_collapse_sftp_default(collapse_sftp);
        // Restore the persisted panel docking layout (#dock).
        window.set_sidebar_width(s.sidebar_width());
        window.set_sidebar_height(s.sidebar_height());
        window.set_sidebar_dock(sidebar_dock.into());
        window.set_sftp_panel_width(s.sftp_panel_width());
        window.set_sftp_panel_height(s.sftp_panel_height());
        window.set_sftp_dock(s.sftp_dock().into());
        window.set_quick_commands_as_sidebar(quick_commands_as_sidebar);
        window.set_quick_panel_open(quick_panel_open);
        window.set_quick_panel_collapsed(quick_panel_collapsed);
        window.set_quick_panel_width(s.quick_panel_width());
        window.set_quick_panel_height(s.quick_panel_height());
        window.set_quick_panel_dock(quick_panel_dock.into());
        window.set_welcome_as_sidebar(welcome_as_sidebar);
        window.set_welcome_sidebar_width(s.welcome_sidebar_width());
        window.set_welcome_sidebar_dock(welcome_sidebar_dock.into());
        window.set_welcome_collapsed(welcome_collapsed);
        window.set_sidebar_collapsed(sidebar_collapsed);
        window.set_wallpaper_overlay(s.wallpaper_overlay());
        window.set_update_check_enabled(s.update_check_enabled()); // #184
        if collapse_sftp {
            window.set_sftp_collapsed(true);
            window.set_sftp_saved_height(s.sftp_panel_height());
        }
        // Capture the user's preferred size. The first native Resized event
        // drives restoration below; this is deterministic and avoids guessing
        // how long Slint/window-manager initialization takes (#278).
        let (ww, wh) = s.window_size();
        let preferred = (ww > 0.0 && wh > 0.0).then_some((ww, wh));
        pending_window_size_restore.set(preferred);
    }
    {
        let store = store.clone();
        window.on_set_collapse_sidebar_default(move |v| {
            let mut s = store.borrow_mut();
            s.set_collapse_sidebar_default(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_quick_commands_as_sidebar(move |v| {
            let mut s = store.borrow_mut();
            s.set_quick_commands_as_sidebar(v);
            let _ = s.save();
        });
    }
    {
        // Toggle the startup new-version check (#184). Takes effect next launch
        // for the check itself; the banner just won't appear once it's off.
        let store = store.clone();
        window.on_set_update_check_enabled(move |v| {
            let mut s = store.borrow_mut();
            s.set_update_check_enabled(v);
            let _ = s.save();
        });
    }
    {
        // Renderer selection is consumed before the first native window exists,
        // so persist it now and apply it on the next launch (#280).
        let store = store.clone();
        window.on_set_renderer_mode(move |mode: SharedString| {
            let mut s = store.borrow_mut();
            s.set_renderer_mode(mode.to_string());
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_persist_sidebar_width(move |w| {
            let mut s = store.borrow_mut();
            s.set_sidebar_width(w);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        let handles = handles.clone();
        window.on_set_sidebar_collapsed(move |v| {
            let mut s = store.borrow_mut();
            s.set_sidebar_collapsed(v);
            let _ = s.save();
            // Pause resource monitoring for every live session while the
            // sidebar is hidden; resume when it comes back (upstream b17da25).
            for handle in handles.borrow().values() {
                handle.set_resource_monitoring(!v);
            }
        });
    }
    {
        let store = store.clone();
        window.on_persist_welcome_sidebar_width(move |w| {
            let mut s = store.borrow_mut();
            s.set_welcome_sidebar_width(w);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_persist_welcome_sidebar_dock(move |dock| {
            let mut s = store.borrow_mut();
            s.set_welcome_sidebar_dock(dock.to_string());
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_welcome_collapsed(move |v| {
            let mut s = store.borrow_mut();
            s.set_welcome_collapsed(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_persist_wallpaper_overlay(move |v| {
            let mut s = store.borrow_mut();
            s.set_wallpaper_overlay(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_collapse_sftp_default(move |v| {
            let mut s = store.borrow_mut();
            s.set_collapse_sftp_default(v);
            let _ = s.save();
        });
    }

    // Session-sync upload setting (#sync). Persisted; only has effect while the
    // session-sync toggle is on. Read live from the window in the upload handler.
    window.set_sync_upload_enabled(store.borrow().sync_upload());
    {
        let store = store.clone();
        window.on_set_sync_upload_enabled(move |v| {
            let mut s = store.borrow_mut();
            s.set_sync_upload(v);
            let _ = s.save();
        });
    }

    // WebDAV config sync (#185): manual upload/download of the portable session
    // export JSON. It is intentionally not automatic on startup.
    {
        let s = store.borrow();
        window.set_webdav_enabled(s.webdav_enabled());
        window.set_webdav_url(s.webdav_url().into());
        window.set_webdav_username(s.webdav_username().into());
        window.set_webdav_password(s.webdav_password().into());
        window.set_webdav_remote_path(s.webdav_remote_path().into());
        window.set_webdav_accept_invalid_certs(s.webdav_accept_invalid_certs());
        window.set_webdav_status(String::new().into());
    }
    {
        let store = store.clone();
        window.on_save_webdav_settings(
            move |enabled: bool,
                  url: SharedString,
                  username: SharedString,
                  password: SharedString,
                  remote_path: SharedString,
                  accept_invalid_certs: bool| {
                let mut s = store.borrow_mut();
                s.set_webdav_settings(
                    enabled,
                    url.to_string(),
                    username.to_string(),
                    password.to_string(),
                    remote_path.to_string(),
                    accept_invalid_certs,
                );
                let _ = s.save();
            },
        );
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_webdav_upload(move || {
            let Some(w) = weak.upgrade() else { return };
            let enabled = w.get_webdav_enabled();
            let url = w.get_webdav_url().to_string();
            let username = w.get_webdav_username().to_string();
            let password = w.get_webdav_password().to_string();
            let remote_path = w.get_webdav_remote_path().to_string();
            let accept_invalid_certs = w.get_webdav_accept_invalid_certs();
            {
                let mut s = store.borrow_mut();
                s.set_webdav_settings(
                    enabled,
                    url.clone(),
                    username.clone(),
                    password.clone(),
                    remote_path.clone(),
                    accept_invalid_certs,
                );
                let _ = s.save();
            }
            if !enabled {
                w.set_webdav_status(t("请先启用 WebDAV 同步", "enable WebDAV sync first").into());
                return;
            }
            let res = store.borrow().export_json().and_then(|(json, count)| {
                webdav_put_json(
                    &url,
                    &remote_path,
                    &username,
                    &password,
                    accept_invalid_certs,
                    json,
                )
                .map(|_| count)
            });
            let msg = match res {
                Ok(n) => format!("{} {}", t("已上传连接", "uploaded connections"), n),
                Err(e) => format!("{}: {}", t("上传失败", "upload failed"), e),
            };
            w.set_webdav_status(msg.into());
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_cursor_color(move |value: SharedString| {
            let Some(color) = parse_hex_color(value.as_str()) else {
                return false;
            };
            {
                let mut s = store.borrow_mut();
                if !s.set_terminal_cursor_color(value.as_str()) {
                    return false;
                }
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_cursor_color(color);
            }
            true
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_add_output_highlight_rule(
            move |pattern: SharedString,
                  is_regex,
                  case_sensitive,
                  whole_line,
                  color: SharedString| {
                let pattern = pattern.trim().to_string();
                let validation = validate_output_highlight_rule(&pattern, is_regex, case_sensitive);
                let Some(w) = weak.upgrade() else {
                    return false;
                };
                if let Err(message) = validation {
                    w.set_output_highlight_rule_status(message.into());
                    return false;
                }
                if store.borrow().output_highlight_rules().len() >= 128 {
                    w.set_output_highlight_rule_status(
                        t("自定义规则最多 128 条", "Custom rules are limited to 128").into(),
                    );
                    return false;
                }
                {
                    let mut s = store.borrow_mut();
                    s.add_output_highlight_rule(OutputHighlightRule {
                        pattern,
                        regex: is_regex,
                        case_sensitive,
                        whole_line,
                        color: color.to_string(),
                        enabled: true,
                    });
                    let _ = s.save();
                    w.set_output_highlight_rules(output_highlight_rule_model(&s));
                    apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
                }
                w.set_output_highlight_rule_status("".into());
                true
            },
        );
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_remove_output_highlight_rule(move |index| {
            let Some(w) = weak.upgrade() else { return };
            let mut s = store.borrow_mut();
            s.remove_output_highlight_rule(index.max(0) as usize);
            let _ = s.save();
            w.set_output_highlight_rules(output_highlight_rule_model(&s));
            apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
            w.set_output_highlight_rule_status("".into());
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_output_highlight_rule_enabled(move |index, enabled| {
            let Some(w) = weak.upgrade() else { return };
            let mut s = store.borrow_mut();
            s.set_output_highlight_rule_enabled(index.max(0) as usize, enabled);
            let _ = s.save();
            w.set_output_highlight_rules(output_highlight_rule_model(&s));
            apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
        });
    }
    // Interface settings: apply + persist the terminal font family / size.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font(move |label: SharedString| {
            // The picker labels entries with their source; store only the
            // bare family name so the config stays portable. Group headers
            // (▍…) are not selectable — ignore them.
            let Some(family) = family_from_label(&label) else {
                return;
            };
            {
                let mut s = store.borrow_mut();
                s.set_font_family(family.to_string());
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_font_family(family.into());
                w.set_term_font_cjk(term_font_covers_cjk(family));
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_ui_font(move |label: SharedString| {
            let Some(family) = family_from_label(&label) else {
                return;
            };
            {
                let mut s = store.borrow_mut();
                s.set_ui_font_family(family.to_string());
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_ui_font_family(family.into());
            }
        });
    }
    // Output highlighting: persist the switch/preset and immediately rebuild
    // every open terminal, including scrollback captured before the change.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_output_highlight(move |enabled, preset: SharedString| {
            let preset = preset.to_string();
            {
                let mut s = store.borrow_mut();
                s.set_output_highlight_enabled(enabled);
                s.set_output_highlight_preset(preset.clone());
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                apply_output_highlight(&w, &bufs, enabled, &preset);
            }
        });
    }
    {
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_json_format_output(move |enabled| {
            {
                let mut s = store.borrow_mut();
                s.set_json_format_output(enabled);
                let _ = s.save();
            }
            // Flip live buffers so the change applies without reconnecting.
            for buffer in bufs.lock().unwrap().values() {
                buffer.lock().unwrap().json_format_output = enabled;
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font_size(move |size: i32| {
            {
                let mut s = store.borrow_mut();
                s.set_font_size(size as u32);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_font_size(size as f32);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font_bold(move |bold: bool| {
            {
                let mut s = store.borrow_mut();
                s.set_terminal_bold(bold);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_font_bold(bold);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_scrollback_lines(move |lines: slint::SharedString| -> bool {
            // Validate: 100..=1_000_000. Malformed input is rejected (UI shows
            // the invalid state) and nothing is persisted.
            let digits: String = lines.chars().filter(|c| c.is_ascii_digit()).collect();
            match digits.parse::<usize>() {
                Ok(n) if (100..=1_000_000).contains(&n) => {
                    let mut s = store.borrow_mut();
                    s.set_scrollback_lines(n);
                    let _ = s.save();
                    // Write the canonical value back to the UI so the settings
                    // panel (conditionally rendered) shows the new value when
                    // reopened — without this it reverts to the stale one.
                    if let Some(w) = weak.upgrade() {
                        w.set_scrollback_lines(digits.into());
                    }
                    true
                }
                _ => false,
            }
        });
    }
    {
        let store = store.clone();
        window.on_set_convert_eol(move |v: bool| {
            let mut s = store.borrow_mut();
            s.set_convert_eol(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_osc52_clipboard(move |v: bool| {
            let mut s = store.borrow_mut();
            s.set_osc52_clipboard(v);
            let _ = s.save();
            crate::terminal::vt_adapter::OSC52_ENABLED
                .store(v, std::sync::atomic::Ordering::Relaxed);
        });
    }
    {
        let store = store.clone();
        window.on_set_hide_special_partitions(move |v: bool| {
            let mut s = store.borrow_mut();
            s.set_hide_special_partitions(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_mount_filter(move |v: slint::SharedString| {
            let mut s = store.borrow_mut();
            s.set_mount_filter(v.to_string());
            let _ = s.save();
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_cursor_style(move |style: SharedString| {
            let normalized = {
                let mut s = store.borrow_mut();
                s.set_terminal_cursor_style(style.to_string());
                let normalized = s.terminal_cursor_style().to_string();
                let _ = s.save();
                normalized
            };
            if let Some(w) = weak.upgrade() {
                w.set_term_cursor_style(normalized.into());
            }
        });
    }
    // Global UI scale (#100): persist the percent and apply it live.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_ui_scale(move |percent: i32| {
            let clamped = (percent.max(0) as u32).clamp(80, 200);
            {
                let mut s = store.borrow_mut();
                s.set_ui_scale(clamped);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_ui_scale(clamped as f32 / 100.0);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_panel_font(move |percent: i32| {
            let clamped = (percent.max(0) as u32).clamp(80, 160);
            {
                let mut s = store.borrow_mut();
                s.set_panel_font(clamped);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_panel_font(clamped as f32 / 100.0);
            }
        });
    }

    // Wallpaper: pick a built-in / none, or open the file dialog for a custom one.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_wp = bufs.clone();
        let proc_weak = proc_win.as_weak();
        window.on_set_wallpaper(move |id: SharedString| {
            let id = id.to_string();
            let mut selected_builtin_theme = None;
            if let Some(w) = weak.upgrade() {
                apply_wallpaper(&w, &store.borrow(), &bufs_wp, &id, true);
                if crate::wallpaper::is_builtin(&id) {
                    selected_builtin_theme = Some(w.get_dark_mode());
                }
                // Keep an already-open process window in sync with the change.
                if let Some(p) = proc_weak.upgrade() {
                    sync_proc_theme(&w, &p);
                }
            }
            let mut s = store.borrow_mut();
            s.set_wallpaper(id);
            // Choosing a built-in wallpaper applies its recommended palette once;
            // persist that result so it too survives the next launch. A later
            // manual theme toggle will overwrite this preference as expected.
            if let Some(dark) = selected_builtin_theme {
                s.set_theme_pref(if dark { "dark" } else { "light" }.to_string());
            }
            let _ = s.save();
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_wp = bufs.clone();
        let proc_weak = proc_win.as_weak();
        window.on_pick_wallpaper_file(move || {
            let picked = rfd::FileDialog::new()
                .set_title("选择壁纸 / Choose wallpaper")
                .add_filter("Images", &["png", "jpg", "jpeg", "webp", "bmp"])
                .pick_file();
            if let Some(path) = picked {
                let id = path.to_string_lossy().to_string();
                if let Some(w) = weak.upgrade() {
                    apply_wallpaper(&w, &store.borrow(), &bufs_wp, &id, false);
                    if let Some(p) = proc_weak.upgrade() {
                        sync_proc_theme(&w, &p);
                    }
                }
                let mut s = store.borrow_mut();
                s.set_wallpaper(id);
                let _ = s.save();
            }
        });
    }

    let sessions_model: Rc<VecModel<SessionInfo>> = Rc::new(VecModel::default());
    window.set_sessions(ModelRc::from(sessions_model.clone()));
    let init_weak = window.as_weak();
    sync_sessions_for_window(&init_weak, &store.borrow(), &sessions_model);
    window.set_wsl_profiles(wsl_profile_model(&store.borrow()));
    {
        let weak = window.as_weak();
        window.on_pick_wsl_directory(move || {
            if let Some(folder) = rfd::FileDialog::new().pick_folder()
                && let Some(w) = weak.upgrade()
            {
                w.set_wsl_new_directory(folder.to_string_lossy().to_string().into());
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_add_wsl_profile(move |name, distribution, directory| {
            let mut s = store.borrow_mut();
            s.add_wsl_profile(
                name.to_string(),
                distribution.to_string(),
                directory.to_string(),
            );
            let _ = s.save();
            if let Some(w) = weak.upgrade() {
                w.set_wsl_profiles(wsl_profile_model(&s));
                sync_sessions_to_model(&s, &sessions_model);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_remove_wsl_profile(move |id| {
            let mut s = store.borrow_mut();
            s.remove_wsl_profile(id.as_str());
            let _ = s.save();
            if let Some(w) = weak.upgrade() {
                w.set_wsl_profiles(wsl_profile_model(&s));
                sync_sessions_to_model(&s, &sessions_model);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_webdav_download(move || {
            let Some(w) = weak.upgrade() else { return };
            let enabled = w.get_webdav_enabled();
            let url = w.get_webdav_url().to_string();
            let username = w.get_webdav_username().to_string();
            let password = w.get_webdav_password().to_string();
            let remote_path = w.get_webdav_remote_path().to_string();
            let accept_invalid_certs = w.get_webdav_accept_invalid_certs();
            {
                let mut s = store.borrow_mut();
                s.set_webdav_settings(
                    enabled,
                    url.clone(),
                    username.clone(),
                    password.clone(),
                    remote_path.clone(),
                    accept_invalid_certs,
                );
                let _ = s.save();
            }
            if !enabled {
                w.set_webdav_status(t("请先启用 WebDAV 同步", "enable WebDAV sync first").into());
                return;
            }
            let res = webdav_get_json(
                &url,
                &remote_path,
                &username,
                &password,
                accept_invalid_certs,
            )
            .and_then(|json| store.borrow_mut().import_json(&json));
            let msg = match res {
                Ok((added, skipped)) => {
                    sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
                    format!(
                        "{} {}, {} {}",
                        t("已导入", "imported"),
                        added,
                        t("跳过", "skipped"),
                        skipped
                    )
                }
                Err(e) => format!("{}: {}", t("下载失败", "download failed"), e),
            };
            w.set_webdav_status(msg.into());
        });
    }

    let tabs_model: Rc<VecModel<TabInfo>> = Rc::new(VecModel::default());
    tabs_model.push(TabInfo {
        id: "welcome".into(),
        title_len: tab_title_len(t("新标签页", "New tab")),
        title: t("新标签页", "New tab").into(),
        kind: "welcome".into(),
        connected: false,
    });
    window.set_tabs(ModelRc::from(tabs_model.clone()));
    window.set_active_tab_id("welcome".into());

    let terminals_model: Rc<VecModel<TerminalState>> = Rc::new(VecModel::default());
    window.set_terminals(ModelRc::from(terminals_model.clone()));

    // Split-pane layout tree (v0.5). Starts as a single pane owning the welcome
    // tab; tab opens/closes/moves mutate it and re-flatten into the `panes`
    // model. `content_size` is the pane-area px size reported from Slint.
    // In welcome-as-sidebar mode the session list lives in a left panel, so the
    // layout starts empty (no "welcome" tab); otherwise it owns the welcome tab.
    let welcome_sidebar = store.borrow().welcome_as_sidebar();
    let layout: Rc<RefCell<crate::layout::Layout>> = Rc::new(RefCell::new(if welcome_sidebar {
        crate::layout::Layout::new(Vec::new(), String::new())
    } else {
        crate::layout::Layout::new(vec!["welcome".into()], "welcome".into())
    }));
    let content_size: Rc<std::cell::Cell<(f32, f32)>> =
        Rc::new(std::cell::Cell::new((1200.0, 800.0)));
    // Persistent pane / splitter models. refresh_panes updates these IN PLACE so
    // the rendered `for pane` / `for sp` elements are reused (terminals survive,
    // and the splitter keeps its pointer-grab during a drag).
    let panes_model: Rc<VecModel<PaneInfo>> = Rc::new(VecModel::default());
    window.set_panes(ModelRc::from(panes_model.clone()));
    let splitters_model: Rc<VecModel<SplitterInfo>> = Rc::new(VecModel::default());
    window.set_splitters(ModelRc::from(splitters_model.clone()));
    refresh_panes(
        &window,
        &layout.borrow(),
        content_size.get(),
        &tabs_model,
        &panes_model,
        &splitters_model,
    );
    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_content_resized(move |w: f32, h: f32| {
            content_size.set((w, h));
            if let Some(win) = weak.upgrade() {
                refresh_panes(
                    &win,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }
        });
    }
    // Toggle welcome-as-sidebar at runtime: persist, then move the welcome tab in
    // or out of the split-tree (sidebar mode = no welcome tab) and re-flatten.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_set_welcome_as_sidebar(move |v| {
            // Persist first: saving config never touches the Slint tree, and
            // doing it synchronously means an immediate window close cannot
            // lose the preference. Only the property/layout transition has to
            // wait — it is two-way-bound through InterfacePanel and changing it
            // destroys/recreates the Welcome subtree that owns the Switch, so
            // defer the *entire* transition until this callback has returned;
            // deferring only refresh_panes still destroys the component tree
            // recursively on Windows (#323).
            {
                let mut s = store.borrow_mut();
                s.set_welcome_as_sidebar(v);
                if let Err(error) = s.save() {
                    tracing::warn!("failed to save config: {error:#}");
                }
            }
            let weak = weak.clone();
            let layout = layout.clone();
            let content_size = content_size.clone();
            let tabs_model = tabs_model.clone();
            let panes_model = panes_model.clone();
            let splitters_model = splitters_model.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                if let Some(w) = weak.upgrade() {
                    w.set_welcome_as_sidebar(v);
                    {
                        let mut lay = layout.borrow_mut();
                        if v {
                            lay.remove_tab("welcome");
                        } else if lay.leaf_of_tab("welcome").is_none() {
                            lay.add_tab("welcome".into());
                        }
                    }
                    refresh_panes(
                        &w,
                        &layout.borrow(),
                        content_size.get(),
                        &tabs_model,
                        &panes_model,
                        &splitters_model,
                    );
                }
            });
        });
    }
    // Per-session SFTP state: collapse + sizes live in each tab's TerminalState so
    // split panes / other tabs each keep their own (resizing/collapsing one no
    // longer bleeds onto the rest) (#v0.5).
    {
        let terminals_model = terminals_model.clone();
        window.on_set_pane_sftp_collapsed(move |tab_id: SharedString, v: bool| {
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_collapsed = v);
        });
    }
    {
        let terminals_model = terminals_model.clone();
        let weak = window.as_weak();
        window.on_set_pane_sftp_height(move |tab_id: SharedString, v: f32| {
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_panel_height = v);
            // Mirror to the global default so it persists (saved on close) and
            // seeds new sessions; other open tabs use their own field, unaffected.
            if let Some(w) = weak.upgrade() {
                w.set_sftp_panel_height(v);
            }
        });
    }
    {
        let terminals_model = terminals_model.clone();
        let weak = window.as_weak();
        window.on_set_pane_sftp_width(move |tab_id: SharedString, v: f32| {
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_panel_width = v);
            if let Some(w) = weak.upgrade() {
                w.set_sftp_panel_width(v);
            }
        });
    }
    {
        let terminals_model = terminals_model.clone();
        window.on_set_pane_sftp_saved_height(move |tab_id: SharedString, v: f32| {
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_saved_height = v);
        });
    }

    // Per-tab connection status + remote resources, the latest local sample,
    // and the local machine's network history (bottom sparkline).
    let tab_statuses: TabStatuses = Arc::new(Mutex::new(HashMap::new()));
    let local_snap: LocalSnap = Arc::new(Mutex::new(SystemSnapshot::default()));
    let local_net_hist: NetHist = Arc::new(Mutex::new(vec![0.0; NET_HISTORY_LEN]));

    {
        let proc_weak = proc_win.as_weak();
        let handles = handles.clone();
        let statuses = tab_statuses.clone();
        let runtime = runtime.clone();
        proc_win.on_terminate_process(
            move |tab_id: SharedString, pid: SharedString, password: SharedString| {
                let tab_id = tab_id.to_string();
                let Ok(pid) = pid.parse::<u32>() else {
                    set_process_action_error(&proc_weak, t("无效的 PID", "Invalid PID"));
                    return;
                };

                // Re-check the source tab, PID, and owner against the latest sample;
                // the main window may have switched tabs since the menu was opened.
                let ownership = {
                    let states = statuses.lock().unwrap();
                    states.get(&tab_id).map_or_else(
                        || Err(t("当前会话不可用", "The current session is unavailable")),
                        |status| {
                            status
                                .procs
                                .iter()
                                .find(|p| p.pid == pid)
                                .map(|process| process_needs_root(&status.user, &process.user))
                                .ok_or_else(|| t("进程已退出", "The process has already exited"))
                        },
                    )
                };
                let needs_root = match ownership {
                    Ok(value) => value,
                    Err(message) => {
                        set_process_action_error(&proc_weak, message);
                        return;
                    }
                };
                if needs_root && password.is_empty() {
                    set_process_action_error(
                        &proc_weak,
                        t(
                            "请输入管理员（sudo）密码",
                            "Enter the administrator (sudo) password",
                        ),
                    );
                    return;
                }

                let root_password =
                    needs_root.then(|| crate::config::Secret::new(password.to_string()));
                let response = handles
                    .borrow()
                    .get(&tab_id)
                    .map(|handle| handle.kill_process(pid, root_password));
                let Some(response) = response else {
                    set_process_action_error(
                        &proc_weak,
                        t("SSH 会话不可用", "The SSH session is unavailable"),
                    );
                    return;
                };

                let done_weak = proc_weak.clone();
                runtime.spawn(async move {
                    let result = response
                        .await
                        .unwrap_or_else(|_| crate::ssh::ProcessKillResult {
                            success: false,
                            message: t("SSH 会话已关闭", "The SSH session has closed").to_string(),
                        });
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(pw) = done_weak.upgrade() {
                            pw.set_action_busy(false);
                            pw.set_action_error(!result.success);
                            pw.set_action_status(result.message.into());
                        }
                    });
                });
            },
        );
    }

    // --- Wire callbacks --------------------------------------------------
    // Display-name overrides for open session tabs (Rename in the tab context
    // menu): tab-id → override. Consulted when a tab's title is (re)built;
    // removed when the tab closes.
    let tab_titles: Rc<RefCell<HashMap<String, String>>> = Rc::new(RefCell::new(HashMap::new()));
    wire_session_callbacks(SessionWireCtx {
        window: &window,
        store: store.clone(),
        sessions_model: sessions_model.clone(),
        tabs_model: tabs_model.clone(),
        terminals_model: terminals_model.clone(),
        layout: layout.clone(),
        content_size: content_size.clone(),
        panes_model: panes_model.clone(),
        splitters_model: splitters_model.clone(),
        handles: handles.clone(),
        bufs: bufs.clone(),
        render_gates: render_gates.clone(),
        runtime: runtime.clone(),
        last_term_size: last_term_size.clone(),
        sftp_handles: sftp_handles.clone(),
        sftp_last_cwd: sftp_last_cwd.clone(),
        tab_statuses: tab_statuses.clone(),
        local_snap: local_snap.clone(),
        local_net_hist: local_net_hist.clone(),
        sftp_follow_cd: sftp_follow_cd.clone(),
        tab_titles: tab_titles.clone(),
    });

    // Recompute the sidebar whenever the active tab changes (fired from Slint's
    // `changed active-tab-id`).
    {
        let weak = window.as_weak();
        let statuses = tab_statuses.clone();
        let local = local_snap.clone();
        let net = local_net_hist.clone();
        window.on_refresh_sidebar(move || {
            if let Some(w) = weak.upgrade() {
                refresh_sidebar(&w, &statuses, &local, &net);
            }
        });
    }

    // Switch UI language at runtime.  Static `@tr(...)` text updates live via
    // select_bundled_translation; we additionally refresh the Rust-driven
    // dynamic strings (sidebar status + the welcome tab title).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let tabs_model = tabs_model.clone();
        window.on_set_language(move |code| {
            crate::i18n::set_language(code.as_ref());
            {
                let mut s = store.borrow_mut();
                s.set_language(crate::i18n::current_code().to_string());
                let _ = s.save();
            }
            // Re-translate the welcome tab's dynamic title.
            for i in 0..tabs_model.row_count() {
                if let Some(mut row) = tabs_model.row_data(i)
                    && row.id.as_str() == "welcome"
                {
                    row.title_len = tab_title_len(t("新标签页", "New tab"));
                    row.title = t("新标签页", "New tab").into();
                    tabs_model.set_row_data(i, row);
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_lang_en(crate::i18n::is_en());
                w.invoke_refresh_sidebar();
            }
        });
    }

    // Theme toggle: flip dark ↔ light, persist the preference, and re-render
    // every open terminal with the new ANSI palette so historical output is
    // also recoloured (not just new output).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_theme = bufs.clone();
        let proc_weak = proc_win.as_weak();
        window.on_toggle_theme(move || {
            let Some(w) = weak.upgrade() else { return };
            let next_dark = !w.get_dark_mode();
            // Flip theme + every terminal buffer + re-render (shared with wallpaper).
            apply_dark_mode(&w, &bufs_theme, next_dark);
            // Mirror the flip onto the detached process window (its Theme global
            // is a separate instance) so an open process window follows.
            if let Some(p) = proc_weak.upgrade() {
                sync_proc_theme(&w, &p);
            }
            let pref = if next_dark { "dark" } else { "light" };
            let mut s = store.borrow_mut();
            s.set_theme_pref(pref.to_string());
            let _ = s.save();
        });
    }

    // Host-key confirmation dialog (#109-5): the user trusts or rejects the
    // presented server key; the decision fans back out to the blocked SSH/SFTP
    // handler(s) and the next queued prompt (if any) is shown.
    {
        let weak = window.as_weak();
        window.on_hostkey_accept(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_hostkey(&w, true);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_hostkey_reject(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_hostkey(&w, false);
            }
        });
    }

    // Connect-time credential prompt (#110): the user supplies the missing
    // username/password (or cancels); the answer unblocks the SSH/SFTP auth.
    {
        let weak = window.as_weak();
        window.on_cred_accept(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_cred(&w, true);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_cred_reject(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_cred(&w, false);
            }
        });
    }

    // MFA / keyboard-interactive prompt (#86-MFA): the user enters the
    // verification code (or cancels); the answer unblocks the SSH/SFTP auth.
    {
        let weak = window.as_weak();
        window.on_mfa_submit(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_mfa(&w, true);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_mfa_cancel(move || {
            if let Some(w) = weak.upgrade() {
                resolve_front_mfa(&w, false);
            }
        });
    }

    // NIC selector: remember the user's choice for the active tab and refresh.
    {
        let weak = window.as_weak();
        let statuses = tab_statuses.clone();
        let local = local_snap.clone();
        let net = local_net_hist.clone();
        window.on_select_net_iface(move |iface: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let active = w.get_active_tab_id().to_string();
            if let Some(st) = statuses.lock().unwrap().get_mut(&active) {
                st.selected_iface = iface.to_string();
                st.net_hist = vec![0.0; NET_HISTORY_LEN]; // reset graph for new NIC
            }
            refresh_sidebar(&w, &statuses, &local, &net);
        });
    }

    // Settings: preset download directory (load + pick + open).
    // Default to the user's Downloads folder so files land somewhere sensible
    // without a prompt; only fall back to "ask every time" if we can't locate it
    // (#85). Persist it on first run so the setting reflects the real path.
    if store.borrow().download_dir().is_empty()
        && let Some(dl) = directories::UserDirs::new()
            .and_then(|u| u.download_dir().map(|p| p.to_string_lossy().to_string()))
    {
        let mut s = store.borrow_mut();
        s.set_download_dir(dl);
        let _ = s.save();
    }
    window.set_download_dir(store.borrow().download_dir().to_string().into());
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_pick_download_dir(move || {
            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                let dir = folder.to_string_lossy().to_string();
                {
                    let mut s = store.borrow_mut();
                    s.set_download_dir(dir.clone());
                    let _ = s.save();
                }
                if let Some(w) = weak.upgrade() {
                    w.set_download_dir(dir.into());
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_open_download_dir(move || {
            let Some(w) = weak.upgrade() else { return };
            let dir = w.get_download_dir().to_string();
            if dir.is_empty() {
                return;
            }
            #[cfg(windows)]
            {
                let _ = std::process::Command::new("explorer").arg(&dir).spawn();
            }
            #[cfg(not(windows))]
            {
                let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
            }
        });
    }

    // --- In-app update check (#48) -----------------------------------------
    wire_update_check(&window, &store, &sftp_handles);

    wire_tab_callbacks(TabWireCtx {
        window: &window,
        tabs_model: tabs_model.clone(),
        terminals_model: terminals_model.clone(),
        layout: layout.clone(),
        content_size: content_size.clone(),
        panes_model: panes_model.clone(),
        splitters_model: splitters_model.clone(),
        handles: handles.clone(),
        bufs: bufs.clone(),
        render_gates: render_gates.clone(),
        sftp_handles: sftp_handles.clone(),
        sftp_last_cwd: sftp_last_cwd.clone(),
        tab_titles: tab_titles.clone(),
    });
    wire_sftp_callbacks(&window, sftp_handles.clone(), sftp_last_cwd.clone());
    wire_key_input(
        &window,
        handles.clone(),
        bufs.clone(),
        last_term_size.clone(),
        store.clone(),
        ConnectCtx {
            weak: window.as_weak(),
            runtime: runtime.clone(),
            handles: handles.clone(),
            sftp_handles: sftp_handles.clone(),
            sftp_last_cwd: sftp_last_cwd.clone(),
            bufs: bufs.clone(),
            render_gates: render_gates.clone(),
            tab_statuses: tab_statuses.clone(),
            local_snap: local_snap.clone(),
            local_net_hist: local_net_hist.clone(),
            last_term_size: last_term_size.clone(),
            sftp_follow_cd: sftp_follow_cd.clone(),
            store: store.clone(),
        },
    );

    // --- Window activity, for idle-CPU throttling (#127) ----------------
    // Idle terminals shouldn't burn CPU: pause the sampler when the window is
    // minimized / occluded, throttle it when it's merely unfocused, and stop the
    // cursor blink whenever the window isn't focused (mirrors what Tabby / Windows
    // Terminal do). The winit event handler below updates this; the blink reads
    // Theme.window-focused.
    let activity = Rc::new(std::cell::Cell::new(WinActivity::Active));
    // Once the user confirms shutdown, every subsequent native/custom close
    // request must pass through without reopening the modal. Windows Installer
    // and Restart Manager may issue more than one close request while replacing
    // the executable (#267).
    let exit_confirmed = Rc::new(Cell::new(false));

    spawn_system_sampler(&window, &tab_statuses, &local_snap, &local_net_hist, &activity);

    // OS file drag-and-drop → upload to the active session's SFTP directory,
    // but only when the file is dropped over the file-list area.
    {
        use i_slint_backend_winit::EventResult;
        use i_slint_backend_winit::winit::event::{MouseScrollDelta, WindowEvent as WEvent};
        let weak = window.as_weak();
        let sh = sftp_handles.clone();
        let wheel_bufs = bufs.clone();
        let close_handles = handles.clone();
        let ev_store = store.clone();
        let ev_activity = activity.clone();
        let ev_exit_confirmed = exit_confirmed.clone();
        let ev_window_size_tracking_ready = window_size_tracking_ready.clone();
        let ev_pending_window_size_restore = pending_window_size_restore.clone();
        let mut last_cursor_logical: Option<(f32, f32)> = None;
        let mut macos_wheel_accum = 0.0_f32;
        // Track the inputs that make up WinActivity; recompute on each change.
        let mut focused = true;
        let mut minimized = false;
        let mut occluded = false;
        // Apply the Win11 rounded-corner hint once, on the first event (the HWND
        // reliably exists by then, unlike a pre-run timer) (#166).
        let mut chrome_done = false;
        window
            .window()
            .on_winit_window_event(move |_slint_window, event| {
                if !chrome_done {
                    chrome_done = true;
                    if let Some(win) = weak.upgrade() {
                        apply_window_chrome(win.window());
                    }
                }
                // Recompute window activity, push it to the shared cell, and update
                // Theme.window-focused (gates the cursor blink) (#127).
                let apply_activity = |focused: bool, minimized: bool, occluded: bool| {
                    let act = if minimized || occluded {
                        WinActivity::Hidden
                    } else if focused {
                        WinActivity::Active
                    } else {
                        WinActivity::Background
                    };
                    let prev = ev_activity.get();
                    ev_activity.set(act);
                    if let Some(win) = weak.upgrade() {
                        win.set_window_focused(act == WinActivity::Active);
                        if prev == WinActivity::Hidden && act != WinActivity::Hidden {
                            win.set_terminal_restore_cover(true);
                            let weak2 = weak.clone();
                            slint::Timer::single_shot(
                                std::time::Duration::from_millis(120),
                                move || {
                                    if let Some(w) = weak2.upgrade() {
                                        w.set_terminal_restore_cover(false);
                                    }
                                },
                            );
                        }
                    }
                };
                match event {
                    #[cfg(target_os = "windows")]
                    WEvent::KeyboardInput { event, .. } => {
                        // Microsoft IME can relabel a Ctrl key-up as Process while
                        // retaining the physical Ctrl scan code. Slint drops Process,
                        // so deliver the missing modifier release directly.
                        if let Some(side) = windows_process_ctrl_release(
                            event.state,
                            &event.logical_key,
                            &event.physical_key,
                        ) {
                            let key = match side {
                                CtrlKeySide::Left => slint::platform::Key::Control,
                                CtrlKeySide::Right => slint::platform::Key::ControlR,
                            };
                            slint_window.dispatch_event(
                                slint::platform::WindowEvent::KeyReleased { text: key.into() },
                            );
                            tracing::debug!(
                                "restored Windows IME Process-key Ctrl release side={side:?}"
                            );
                            return EventResult::PreventDefault;
                        }
                    }
                    #[cfg(target_os = "windows")]
                    WEvent::Ime(i_slint_backend_winit::winit::event::Ime::Disabled) => {
                        // Windows emits Ime::Disabled when a composition ends, including
                        // while switching between Chinese and English input methods. The
                        // Slint winit backend intentionally ignores this notification, so
                        // after several switches the native input context can remain
                        // detached and every TextInput appears to stop accepting keys
                        // (#236). Re-associate the window with its current default IME;
                        // the focused Slint TextInput keeps owning text input as before.
                        slint_window.with_winit_window(|window| window.set_ime_allowed(true));
                    }
                    WEvent::DroppedFile(path) => {
                        if let Some(win) = weak.upgrade() {
                            handle_file_drop(&win, &sh, path.clone());
                        }
                    }
                    WEvent::CursorMoved { position, .. } => {
                        if let Some(win) = weak.upgrade() {
                            let scale = win.window().scale_factor().max(0.01) as f64;
                            let p = position.to_logical::<f64>(scale);
                            last_cursor_logical = Some((p.x as f32, p.y as f32));
                        }
                    }
                    WEvent::MouseWheel { delta, .. } if cfg!(target_os = "macos") => {
                        let Some((x, y)) = last_cursor_logical else {
                            return EventResult::Propagate;
                        };
                        let Some(win) = weak.upgrade() else {
                            return EventResult::Propagate;
                        };
                        if !macos_terminal_wheel_can_target_terminal(win.get_interface_open()) {
                            // Do not carry a partially accumulated settings gesture
                            // into the terminal after the modal closes.
                            macos_wheel_accum = 0.0;
                            return EventResult::Propagate;
                        }
                        let wheel_lines = match delta {
                            MouseScrollDelta::LineDelta(_, dy) => dy * 3.0,
                            MouseScrollDelta::PixelDelta(p) => {
                                let scale = win.window().scale_factor().max(0.01) as f64;
                                let p = p.to_logical::<f64>(scale);
                                p.y as f32 / 18.0
                            }
                        };
                        if wheel_lines.abs() < f32::EPSILON {
                            return EventResult::Propagate;
                        }
                        macos_wheel_accum += wheel_lines;
                        let whole = macos_wheel_accum.trunc() as i32;
                        if whole == 0 {
                            return EventResult::Propagate;
                        }
                        macos_wheel_accum -= whole as f32;
                        if handle_macos_terminal_wheel(&win, &wheel_bufs, x, y, whole) {
                            return EventResult::PreventDefault;
                        }
                    }
                    WEvent::Focused(f) => {
                        focused = *f;
                        apply_activity(focused, minimized, occluded);
                        if *f {
                            #[cfg(target_os = "windows")]
                            slint_window.with_winit_window(|window| window.set_ime_allowed(true));

                            // Some window managers deliver the first Resized event
                            // before the native window belongs to a monitor. Focus
                            // is a reliable second opportunity to seed restoration;
                            // request_inner_size will produce the Resized event that
                            // verifies the native window actually reached the target.
                            if !ev_window_size_tracking_ready.get()
                                && let Some(win) = weak.upgrade()
                            {
                                if is_wayland_window(win.window()) {
                                    ev_pending_window_size_restore.set(None);
                                    ev_window_size_tracking_ready.set(true);
                                    tracing::info!(
                                        "[WINDOW_SIZE] skipped persisted-size restore on Wayland"
                                    );
                                } else if let Some(preferred) = ev_pending_window_size_restore.get()
                                    && let Some(target) =
                                        clamp_window_size_to_monitor(win.window(), Some(preferred))
                                {
                                    tracing::info!(
                                        "[WINDOW_SIZE] focus retry saved={:.0}x{:.0} \
                                             target={:.0}x{:.0}",
                                        preferred.0,
                                        preferred.1,
                                        target.0,
                                        target.1,
                                    );
                                }
                            }
                            refresh_revealed_main_window(weak.clone());
                        }
                    }
                    WEvent::Occluded(o) => {
                        occluded = *o;
                        apply_activity(focused, minimized, occluded);
                        if !*o {
                            refresh_revealed_main_window(weak.clone());
                        }
                    }
                    WEvent::ScaleFactorChanged { .. } => {
                        // Moving a maximized frameless window between mixed-DPI
                        // monitors can leave Win11 reporting "maximized" while the
                        // native rectangle/render surface still has the old size.
                        refresh_revealed_main_window(weak.clone());
                    }
                    WEvent::Resized(size) => {
                        // A 0-sized resize is how Windows reports a minimize; track it
                        // so we pause the sampler while minimized (#127).
                        minimized = size.width == 0 || size.height == 0;
                        apply_activity(focused, minimized, occluded);
                        // Keep the maximize/restore icon (and resize-edge gating) in
                        // sync when the OS changes the window state (#119).
                        if let Some(win) = weak.upgrade() {
                            let maxed = win
                                .window()
                                .with_winit_window(|ww| ww.is_maximized())
                                .unwrap_or(false);
                            win.set_window_maximized(maxed);
                            if !ev_window_size_tracking_ready.get()
                                && is_wayland_window(win.window())
                            {
                                // The configure size in this event is authoritative
                                // on Wayland. Accept and persist that actual size;
                                // never chase the advisory saved size (#286).
                                ev_pending_window_size_restore.set(None);
                                ev_window_size_tracking_ready.set(true);
                                tracing::info!(
                                    "[WINDOW_SIZE] accepted compositor size {}x{} on Wayland",
                                    size.width,
                                    size.height
                                );
                            }
                            if !ev_window_size_tracking_ready.get() {
                                if let Some(preferred) = ev_pending_window_size_restore.get() {
                                    let scale = win.window().scale_factor().max(0.01);
                                    let actual =
                                        (size.width as f32 / scale, size.height as f32 / scale);
                                    if let Some(target) =
                                        clamp_window_size_to_monitor(win.window(), Some(preferred))
                                    {
                                        tracing::info!(
                                            "[WINDOW_SIZE] restore requested saved={:.0}x{:.0} \
                                         target={:.0}x{:.0} actual={:.0}x{:.0} scale={:.2}",
                                            preferred.0,
                                            preferred.1,
                                            target.0,
                                            target.1,
                                            actual.0,
                                            actual.1,
                                            scale,
                                        );
                                        if (actual.0 - target.0).abs() <= 2.0
                                            && (actual.1 - target.1).abs() <= 2.0
                                        {
                                            ev_pending_window_size_restore.set(None);
                                            ev_window_size_tracking_ready.set(true);
                                            tracing::info!(
                                                "[WINDOW_SIZE] restore settled at {:.0}x{:.0}",
                                                actual.0,
                                                actual.1
                                            );
                                        }
                                    } else {
                                        tracing::warn!(
                                            "[WINDOW_SIZE] restore deferred: no monitor available \
                                         saved={:.0}x{:.0}",
                                            preferred.0,
                                            preferred.1,
                                        );
                                    }
                                } else {
                                    // First run: accept the initialized size as the
                                    // baseline, but do not persist this startup event.
                                    ev_window_size_tracking_ready.set(true);
                                }
                                return EventResult::Propagate;
                            }
                            // Record the last user-adjusted windowed size while the
                            // resize event still carries authoritative native
                            // geometry. Persisting only during CloseRequested can
                            // observe an installer/minimize transition instead
                            // (#278). Keep writes in memory here; save_layout flushes
                            // the config on exit.
                            if ev_window_size_tracking_ready.get() && !maxed && !minimized {
                                let scale = win.window().scale_factor().max(0.01);
                                let width = size.width as f32 / scale;
                                let height = size.height as f32 / scale;
                                if width > 200.0 && height > 200.0 {
                                    ev_store.borrow_mut().set_window_size(width, height);
                                    tracing::debug!(
                                        "[WINDOW_SIZE] recorded user size {:.0}x{:.0}",
                                        width,
                                        height
                                    );
                                }
                            }
                        }
                    }
                    WEvent::CloseRequested => {
                        // Confirm before closing if there are open session tabs (#88),
                        // so a stray double-click on the title-bar icon / X / Alt+F4
                        // doesn't silently drop live sessions. Installer/Restart
                        // Manager may send repeated requests, so never intercept
                        // again after the user has confirmed shutdown (#267).
                        if should_block_close(
                            ev_exit_confirmed.get(),
                            !close_handles.borrow().is_empty(),
                        ) {
                            if let Some(win) = weak.upgrade() {
                                win.set_confirm_close_open(true);
                            }
                            return EventResult::PreventDefault;
                        }
                        ev_exit_confirmed.set(true);
                        // No sessions → the window is about to close; persist layout.
                        if let Some(win) = weak.upgrade() {
                            save_layout(&win, &ev_store);
                        }
                    }
                    _ => {}
                }
                EventResult::Propagate
            });
    }
    // Confirm-close dialog "Close" → actually quit the event loop (#88).
    {
        let weak = window.as_weak();
        let proc_weak = proc_win.as_weak();
        let sys_weak = sys_win.as_weak();
        let cc_store = store.clone();
        let close_handles = handles.clone();
        let close_sftp_handles = sftp_handles.clone();
        let close_exit_confirmed = exit_confirmed.clone();
        window.on_confirm_close_yes(move || {
            // Guard against a double click and against another close request
            // arriving from Windows Installer while shutdown is in progress.
            if close_exit_confirmed.replace(true) {
                return;
            }
            if let Some(w) = weak.upgrade() {
                w.set_confirm_close_open(false);
                save_layout(&w, &cc_store);
                let _ = w.hide();
            }
            if let Some(w) = proc_weak.upgrade() {
                let _ = w.hide();
            }
            if let Some(w) = sys_weak.upgrade() {
                let _ = w.hide();
            }
            // Ask every worker to stop before the runtime/event loop is torn
            // down. Clearing the maps also makes any repeated close request see
            // no live sessions and pass through immediately.
            {
                let mut sessions = close_handles.borrow_mut();
                for handle in sessions.values() {
                    handle.close();
                }
                sessions.clear();
            }
            if let Ok(mut sftp) = close_sftp_handles.lock() {
                for handle in sftp.values() {
                    handle.close();
                }
                sftp.clear();
            }
            let _ = slint::quit_event_loop();
        });
    }

    // --- Custom title-bar window controls (#119) --------------------------
    {
        let weak = window.as_weak();
        window.on_win_minimize(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| ww.set_minimized(true));
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_win_maximize_toggle(move || {
            if let Some(w) = weak.upgrade() {
                let now = w.window().with_winit_window(|ww| {
                    let m = !ww.is_maximized();
                    ww.set_maximized(m);
                    m
                });
                if let Some(m) = now {
                    w.set_window_maximized(m);
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        let close_handles = handles.clone();
        let wc_store = store.clone();
        let wc_exit_confirmed = exit_confirmed.clone();
        window.on_win_close(move || {
            if let Some(w) = weak.upgrade() {
                // Mirror the native-X behaviour: confirm if sessions are open.
                if !should_block_close(wc_exit_confirmed.get(), !close_handles.borrow().is_empty())
                {
                    wc_exit_confirmed.set(true);
                    save_layout(&w, &wc_store);
                    let _ = slint::quit_event_loop();
                } else {
                    w.set_confirm_close_open(true);
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_win_drag(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_window();
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        use i_slint_backend_winit::winit::window::ResizeDirection;
        let weak = window.as_weak();
        window.on_win_resize(move |dir: i32| {
            if let Some(w) = weak.upgrade() {
                let d = match dir {
                    0 => ResizeDirection::North,
                    1 => ResizeDirection::South,
                    2 => ResizeDirection::East,
                    3 => ResizeDirection::West,
                    4 => ResizeDirection::NorthEast,
                    5 => ResizeDirection::NorthWest,
                    6 => ResizeDirection::SouthEast,
                    _ => ResizeDirection::SouthWest,
                };
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_resize_window(d);
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }

    // Center the window on the primary monitor once it's shown (size is only
    // known after the first frame, so defer via a single-shot timer).
    {
        let weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(30), move || {
            if let Some(w) = weak.upgrade() {
                center_window(&w);
            }
        });
    }

    window.run().context("event loop exited with error")?;
    Ok(())
}


// ---------------------------------------------------------------------------
// Session callbacks (welcome page + dialog)
// ---------------------------------------------------------------------------

/// Bundled handles/state threaded into [`wire_tab_callbacks`]
/// (clippy::too_many_arguments).
pub struct TabWireCtx<'a> {
    window: &'a AppWindow,
    tabs_model: Rc<VecModel<TabInfo>>,
    terminals_model: Rc<VecModel<TerminalState>>,
    layout: Rc<RefCell<crate::layout::Layout>>,
    content_size: Rc<std::cell::Cell<(f32, f32)>>,
    panes_model: Rc<VecModel<PaneInfo>>,
    splitters_model: Rc<VecModel<SplitterInfo>>,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    bufs: TermBuffers,
    render_gates: RenderGates,
    sftp_handles: SftpHandles,
    sftp_last_cwd: SftpLastCwd,
    tab_titles: Rc<RefCell<HashMap<String, String>>>,
}

/// Bundled handles/state threaded into [`wire_session_callbacks`]
/// (clippy::too_many_arguments).
struct SessionWireCtx<'a> {
    window: &'a AppWindow,
    store: Rc<RefCell<ConfigStore>>,
    sessions_model: Rc<VecModel<SessionInfo>>,
    tabs_model: Rc<VecModel<TabInfo>>,
    terminals_model: Rc<VecModel<TerminalState>>,
    layout: Rc<RefCell<crate::layout::Layout>>,
    content_size: Rc<std::cell::Cell<(f32, f32)>>,
    panes_model: Rc<VecModel<PaneInfo>>,
    splitters_model: Rc<VecModel<SplitterInfo>>,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    bufs: TermBuffers,
    render_gates: RenderGates,
    runtime: Arc<Runtime>,
    last_term_size: Arc<Mutex<(u32, u32)>>,
    sftp_handles: SftpHandles,
    sftp_last_cwd: SftpLastCwd,
    tab_statuses: TabStatuses,
    local_snap: LocalSnap,
    local_net_hist: NetHist,
    sftp_follow_cd: Arc<std::sync::atomic::AtomicBool>,
    tab_titles: Rc<RefCell<HashMap<String, String>>>,
}

/// Re-sync the session list honouring the current Quick Connect search box
/// contents (upstream 547b588). All mutation paths funnel through this so the
/// filtered view stays live.
fn sync_sessions_for_window(
    window: &slint::Weak<AppWindow>,
    store: &ConfigStore,
    model: &VecModel<SessionInfo>,
) {
    let query = window
        .upgrade()
        .map(|window| window.get_host_search_query().to_string())
        .unwrap_or_default();
    sync_sessions_to_model_with_filter(store, model, &query);
}

/// Build the effective session represented by the dialog. When editing, blank
/// secret fields retain their saved values because real passwords and pasted
/// private keys are deliberately never echoed back into the UI (#10, #276).
fn wire_session_callbacks(ctx: SessionWireCtx) {
    let SessionWireCtx {
        window,
        store,
        sessions_model,
        tabs_model,
        terminals_model,
        layout,
        content_size,
        panes_model,
        splitters_model,
        handles,
        bufs,
        render_gates,
        runtime,
        last_term_size,
        sftp_handles,
        sftp_last_cwd,
        tab_statuses,
        local_snap,
        local_net_hist,
        sftp_follow_cd,
        tab_titles,
    } = ctx;
    // on_connect_session moves panes_model/splitters_model into its closure;
    // the rename handler below needs its own handle, so clone up front.
    let panes_model_rename = panes_model.clone();
    let splitters_model_rename = splitters_model.clone();
    // Working set of port forwards (#56) for the session being created/edited.
    // The forward add/delete callbacks mutate it; saving reads it into
    // Session.forwards; opening the dialog (new/edit) resets it.
    let edit_forwards: Rc<RefCell<Vec<PortFwd>>> =
        Rc::new(RefCell::new(vec![blank_forward_draft()]));

    // Working set of expect/send login triggers (#212), same lifecycle as the
    // forward drafts above. Responses stay blank when editing a saved rule.
    let edit_triggers: Rc<RefCell<Vec<TriggerDraft>>> =
        Rc::new(RefCell::new(vec![blank_trigger_draft()]));

    // Rebuild the session list as the user edits the Quick Connect search.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_host_search_changed(move |query| {
            if let Some(window) = weak.upgrade() {
                let query = if query.trim().is_empty() {
                    SharedString::new()
                } else {
                    query
                };
                window.set_host_search_query(query.clone());
                sync_sessions_to_model_with_filter(
                    &store.borrow(),
                    &sessions_model,
                    query.as_str(),
                );
            }
        });
    }

    // New session -> open dialog with blank draft.
    let weak = window.as_weak();
    let ef_new = edit_forwards.clone();
    let et_new = edit_triggers.clone();
    let store_ng = store.clone();
    window.on_new_session_clicked(move || {
        if let Some(w) = weak.upgrade() {
            *ef_new.borrow_mut() = vec![blank_forward_draft()];
            *et_new.borrow_mut() = vec![blank_trigger_draft()];
            w.set_session_groups(session_groups_model(&store_ng.borrow()));
            w.set_dialog_forwards(forward_model(&ef_new.borrow()));
            w.set_dialog_triggers(trigger_model(&et_new.borrow()));
            let empty = Session::new_empty();
            let (jump_labels, jump_ids, jump_idx) =
                jump_candidates(&store_ng.borrow(), &empty.id, "");
            w.set_jump_choices(jump_labels);
            w.set_jump_ids(jump_ids);
            w.set_dialog_jump_index(jump_idx);
            w.set_dialog_id(empty.id.into());
            w.set_dialog_name("".into());
            w.set_dialog_host("".into());
            w.set_dialog_port("22".into());
            // No default username (#110): leaving it blank makes the connect-time
            // prompt ask for it, Xshell-style.
            w.set_dialog_user("".into());
            w.set_dialog_auth("password".into());
            w.set_dialog_password("".into());
            w.set_dialog_key_path("".into());
            w.set_dialog_key_inline("".into());
            w.set_dialog_key_inline_mode(false);
            w.set_dialog_test_status("".into());
            w.set_dialog_proxy_type("none".into());
            w.set_dialog_proxy_hostport("".into());
            w.set_dialog_group("".into());
            w.set_dialog_kind("ssh".into());
            w.set_dialog_serial_port("".into());
            w.set_dialog_baud("115200".into());
            w.set_dialog_data_bits("8".into());
            w.set_dialog_stop_bits("1".into());
            w.set_dialog_parity("none".into());
            w.set_dialog_flow("none".into());
            w.set_dialog_encoding("UTF-8".into());
            w.set_dialog_disable_shell_integration(false);
            w.set_dialog_note("".into());
            w.set_dialog_editing(false);
            w.set_dialog_open(true);
        }
    });

    // Import hosts from ~/.ssh/config -> add them as sessions (skipping dups).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_import_ssh_config(move || {
            let hosts = crate::ssh::ssh_config::parse_default();
            let mut added = 0usize;
            if hosts.is_empty() {
                if let Some(w) = weak.upgrade() {
                    w.set_ssh_import_hint(
                        t("未找到 ~/.ssh/config", "no ~/.ssh/config found").into(),
                    );
                }
                return;
            }
            {
                let mut s = store.borrow_mut();
                for h in hosts {
                    // Skip if a session already has this alias, or the same
                    // host + user pair.
                    let dup = s
                        .sessions()
                        .iter()
                        .any(|x| x.name == h.alias || (x.host == h.hostname && x.user == h.user));
                    if dup {
                        continue;
                    }
                    let auth = if h.identity_file.is_empty() {
                        AuthMethod::Password
                    } else {
                        AuthMethod::Key
                    };
                    s.upsert(Session {
                        name: h.alias,
                        host: h.hostname,
                        port: h.port,
                        user: if h.user.is_empty() {
                            "root".into()
                        } else {
                            h.user
                        },
                        auth,
                        private_key_path: h.identity_file,
                        ..Session::new_empty()
                    });
                    added += 1;
                }
                if added > 0 {
                    let _ = s.save();
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let hint = if added > 0 {
                    format!("{} {}", t("已导入", "imported"), added)
                } else {
                    t("没有新主机可导入", "no new hosts to import").to_string()
                };
                w.set_ssh_import_hint(hint.into());
            }
        });
    }

    // Export all sessions to a portable JSON file (issue #46). Passwords are
    // obfuscated with the built-in export key; host/user/port stay plaintext.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_export_sessions(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_file_name("rudder-connections.json")
                .add_filter("JSON", &["json"])
                .save_file()
            {
                let res = store.borrow().export_to(&path);
                if let Some(w) = weak.upgrade() {
                    let hint = match res {
                        Ok(n) => format!("{} {}", t("已导出连接", "exported"), n),
                        Err(e) => format!("{}: {}", t("导出失败", "export failed"), e),
                    };
                    w.set_ssh_import_hint(hint.into());
                }
            }
        });
    }

    // Batch-import connections from pasted text (#150). One per line:
    // `host|port|user|password|name` (trailing fields optional).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_batch_import_confirm(move |text: SharedString| {
            let parsed = parse_batch_import(text.as_str());
            let total = parsed.len();
            let mut added = 0usize;
            {
                let mut s = store.borrow_mut();
                for sess in parsed {
                    // Skip a host/user/port we already have.
                    let dup = s
                        .sessions()
                        .iter()
                        .any(|x| x.host == sess.host && x.user == sess.user && x.port == sess.port);
                    if dup {
                        continue;
                    }
                    s.upsert(sess);
                    added += 1;
                }
                if added > 0 {
                    let _ = s.save();
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let hint = if total == 0 {
                    t("没有可导入的连接", "nothing to import").to_string()
                } else if added > 0 {
                    format!("{} {}/{}", t("已导入", "imported"), added, total)
                } else {
                    t("没有新连接可导入(已存在)", "no new connections (all exist)").to_string()
                };
                w.set_ssh_import_hint(hint.into());
            }
        });
    }

    // Import sessions from a portable JSON file (issue #46).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_import_sessions(move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
            {
                let res = store.borrow_mut().import_from(&path);
                if let Some(w) = weak.upgrade() {
                    let hint = match res {
                        Ok((added, skipped)) => {
                            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
                            format!(
                                "{} {} / {} {}",
                                t("已导入", "imported"),
                                added,
                                t("跳过重复", "skipped"),
                                skipped
                            )
                        }
                        Err(e) => format!("{}: {}", t("导入失败", "import failed"), e),
                    };
                    w.set_ssh_import_hint(hint.into());
                }
            }
        });
    }

    // Edit -> open dialog prefilled.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let ef_edit = edit_forwards.clone();
        let et_edit = edit_triggers.clone();
        window.on_edit_session(move |id: SharedString| {
            let id = id.to_string();
            let store = store.borrow();
            let Some(session) = store.get(&id) else {
                return;
            };
            *ef_edit.borrow_mut() = forward_drafts(&session.forwards);
            if ef_edit.borrow().is_empty() {
                ef_edit.borrow_mut().push(blank_forward_draft());
            }
            // Saved responses are never echoed into the dialog (#10); the
            // validator keeps them when the response box stays blank.
            *et_edit.borrow_mut() = trigger_drafts(&session.triggers);
            if et_edit.borrow().is_empty() {
                et_edit.borrow_mut().push(blank_trigger_draft());
            }
            if let Some(w) = weak.upgrade() {
                w.set_session_groups(session_groups_model(&store));
                w.set_dialog_forwards(forward_model(&ef_edit.borrow()));
                w.set_dialog_triggers(trigger_model(&et_edit.borrow()));
                w.set_dialog_id(session.id.clone().into());
                w.set_dialog_name(session.name.clone().into());
                w.set_dialog_host(session.host.clone().into());
                w.set_dialog_port(session.port.to_string().into());
                w.set_dialog_user(session.user.clone().into());
                w.set_dialog_auth(session.auth.as_str().into());
                // Never echo the stored password back into the UI (issue #10) —
                // leave it blank; a blank field on save keeps the existing one.
                w.set_dialog_password("".into());
                w.set_dialog_key_path(session.private_key_path.clone().into());
                w.set_dialog_key_inline("".into());
                w.set_dialog_key_inline_mode(!session.private_key_inline.is_empty());
                w.set_dialog_test_status("".into());
                let (proxy_type, proxy_hostport) = split_proxy(&session.proxy);
                w.set_dialog_proxy_type(proxy_type.into());
                w.set_dialog_proxy_hostport(proxy_hostport.into());
                let (jump_labels, jump_ids, jump_idx) =
                    jump_candidates(&store, &session.id, &session.jump_session_id);
                w.set_jump_choices(jump_labels);
                w.set_jump_ids(jump_ids);
                w.set_dialog_jump_index(jump_idx);
                w.set_dialog_group(session.group.clone().into());
                w.set_dialog_kind(session.kind.as_str().into());
                w.set_dialog_serial_port(session.serial_port.clone().into());
                w.set_dialog_baud(session.baud_rate.to_string().into());
                w.set_dialog_data_bits(session.data_bits.to_string().into());
                w.set_dialog_stop_bits(session.stop_bits.to_string().into());
                w.set_dialog_parity(session.parity.clone().into());
                w.set_dialog_flow(session.flow_control.clone().into());
                w.set_dialog_encoding(session.encoding.clone().into());
                w.set_dialog_disable_shell_integration(session.disable_shell_integration);
                w.set_dialog_note(session.note.clone().into());
                w.set_dialog_editing(true);
                w.set_dialog_open(true);
            }
        });
    }

    // Remove session.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_remove_session(move |id: SharedString| {
            {
                let mut s = store.borrow_mut();
                s.remove(id.as_ref());
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                // Touch a property so the list re-renders reliably.
                let _ = w.get_sessions();
            }
        });
    }

    // Duplicate a session: clone it with a fresh id and a " (copy)" name (#41).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_duplicate_session(move |id: SharedString| {
            {
                let mut s = store.borrow_mut();
                if let Some(orig) = s.get(id.as_ref()).cloned() {
                    let mut copy = orig;
                    copy.id = uuid::Uuid::new_v4().to_string();
                    copy.name = format!("{} (copy)", copy.name);
                    copy.last_used = None;
                    s.upsert(copy);
                    if let Err(err) = s.save() {
                        tracing::warn!("failed to save config: {err:#}");
                    }
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Move a session to another group (#41).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_move_session(move |id: SharedString, group: SharedString| {
            {
                let mut s = store.borrow_mut();
                if let Some(orig) = s.get(id.as_ref()).cloned() {
                    let mut moved = orig;
                    // "default" is the display label for ungrouped → store empty.
                    moved.group = if group.as_str().eq_ignore_ascii_case("default") {
                        String::new()
                    } else if is_reserved_session_group(group.as_str().trim()) {
                        // `system` belongs exclusively to built-in local shells.
                        return;
                    } else {
                        group.to_string()
                    };
                    s.upsert(moved);
                    if let Err(err) = s.save() {
                        tracing::warn!("failed to save config: {err:#}");
                    }
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Collapse / expand a group in the welcome list (#41). Toggling flips the
    // `collapsed` flag on every row of that group in place — no full re-sync —
    // so the open/closed state stays put until the list is actually rebuilt.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_toggle_group(move |group: SharedString| {
            // While a search filter is active the list shows only matches with
            // groups force-expanded; toggling would corrupt that view (#264).
            if weak
                .upgrade()
                .map(|window| !window.get_host_search_query().trim().is_empty())
                .unwrap_or(false)
            {
                return;
            }
            use slint::Model as _;
            let target = group.to_string();
            let n = sessions_model.row_count();
            // New state = the opposite of the group's first row.
            let mut new_state = false;
            for i in 0..n {
                if let Some(row) = sessions_model.row_data(i)
                    && row.group.as_str() == target
                {
                    new_state = !row.collapsed;
                    break;
                }
            }
            for i in 0..n {
                if let Some(mut row) = sessions_model.row_data(i)
                    && row.group.as_str() == target
                {
                    row.collapsed = new_state;
                    sessions_model.set_row_data(i, row);
                }
            }
            {
                let mut store = store.borrow_mut();
                store.set_session_group_collapsed(&target, new_state);
                if let Err(err) = store.save() {
                    tracing::warn!("failed to save Quick Connect folder state: {err:#}");
                }
            }
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Drag-to-reorder a host card among its same-group siblings. The stored
    // Vec order is the display order (no alphabetical sort), so a swap plus
    // re-sync is all it takes. Reordering while the list is filtered would map
    // visible hops onto the wrong stored neighbours, so bail out then — the
    // Slint side already disables the gesture while searching.
    //
    // Per-hop updates mutate the model IN PLACE (set_row_data): a full set_vec
    // rebuild would recreate the rows and drop the dragging row's pointer grab,
    // ending the drag after one hop. Saving + broadcasting are deferred to
    // reorder-session-end (pointer release).
    let sessions_dirty = Rc::new(std::cell::Cell::new(false));
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let sessions_dirty = sessions_dirty.clone();
        window.on_reorder_session(move |id: SharedString, dir: i32| {
            if weak
                .upgrade()
                .map(|window| !window.get_host_search_query().trim().is_empty())
                .unwrap_or(false)
            {
                return;
            }
            let moved = {
                let mut s = store.borrow_mut();
                s.reorder_session(id.as_str(), dir as isize)
            };
            if moved {
                sessions_dirty.set(true);
                let query = weak
                    .upgrade()
                    .map(|w| w.get_host_search_query().to_string())
                    .unwrap_or_default();
                refresh_session_rows_in_place(&store.borrow(), &sessions_model, &query);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let sessions_dirty = sessions_dirty.clone();
        window.on_reorder_session_end(move || {
            if !sessions_dirty.replace(false) {
                return;
            }
            if let Err(err) = store.borrow_mut().save() {
                tracing::warn!("failed to save config: {err:#}");
            }
            sync_sessions_to_model(&store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Group create / rename (#41).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_submit_group(move |orig: SharedString, name: SharedString| {
            let trimmed = name.trim();
            let error = {
                let s = store.borrow();
                if trimmed.is_empty() {
                    Some(t("请输入分组名称", "Enter a group name"))
                } else if is_reserved_session_group(trimmed) {
                    Some(t("该名称为系统保留分组", "This group name is reserved"))
                } else if (orig.is_empty() || !trimmed.eq_ignore_ascii_case(orig.as_str()))
                    && s.session_group_exists(trimmed)
                {
                    Some(t("分组已存在", "Group already exists"))
                } else {
                    None
                }
            };
            if let Some(message) = error {
                return SharedString::from(message);
            }
            {
                let mut s = store.borrow_mut();
                if orig.is_empty() {
                    s.add_group(trimmed.to_string());
                } else {
                    s.rename_group(orig.as_str(), trimmed.to_string());
                }
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
            SharedString::new()
        });
    }
    // Group delete (#41) — UI only offers this on empty groups.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_delete_group(move |name: SharedString| {
            {
                let mut s = store.borrow_mut();
                s.remove_group(name.as_ref());
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let _ = w.get_sessions();
            }
        });
    }

    // Dialog submit -> persist + (optionally) connect.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let edit_forwards = edit_forwards.clone();
        let edit_triggers = edit_triggers.clone();
        window.on_session_dialog_submit(move |draft: SessionDraft| {
            let id = draft.id.to_string();
            let forwards = match validated_port_forwards(&edit_forwards.borrow()) {
                Ok(forwards) => forwards,
                Err(message) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                    return;
                }
            };
            // Triggers keep their saved response when the box is left blank,
            // so the previously stored secrets must be passed along (#212).
            let saved_responses: Vec<Secret> = store
                .borrow()
                .get(&id)
                .map(|s| s.triggers.iter().map(|t| t.response.clone()).collect())
                .unwrap_or_default();
            let triggers = match validated_triggers(&edit_triggers.borrow(), &saved_responses) {
                Ok(triggers) => triggers,
                Err(message) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                    return;
                }
            };
            // The edit dialog never echoes the real password (issue #10): a blank
            // field while editing means "keep the existing password" rather than
            // "clear it".  Only overwrite when the user actually typed something.
            let password = if draft.password.is_empty() {
                store
                    .borrow()
                    .get(&id)
                    .map(|s| s.password.clone())
                    .unwrap_or_default()
            } else {
                Secret::new(draft.password.to_string())
            };
            let private_key_inline = if draft.private_key_inline_mode {
                if draft.private_key_inline.is_empty() {
                    store
                        .borrow()
                        .get(&id)
                        .map(|s| s.private_key_inline.clone())
                        .unwrap_or_default()
                } else {
                    Secret::new(draft.private_key_inline.to_string())
                }
            } else {
                Secret::default()
            };
            let private_key_path = if draft.private_key_inline_mode {
                String::new()
            } else {
                draft.private_key_path.to_string().replace('\\', "/")
            };
            let kind = crate::config::SessionKind::from_str(draft.kind.as_ref());
            // Auto-name: serial → port label; otherwise user@host, or just the
            // host when no username was given (#110).
            let auto_name = match kind {
                crate::config::SessionKind::Serial => {
                    format!("{} @{}", draft.serial_port, draft.baud_rate)
                }
                _ if draft.user.trim().is_empty() => draft.host.to_string(),
                _ => format!("{}@{}", draft.user, draft.host),
            };
            // Telnet defaults to port 23, SSH to 22; serial ignores port.
            let default_port = if kind == crate::config::SessionKind::Telnet {
                23
            } else {
                22
            };
            let new_session = Session {
                id,
                name: if draft.name.is_empty() {
                    auto_name
                } else {
                    draft.name.to_string()
                },
                host: draft.host.to_string(),
                port: if draft.port <= 0 {
                    default_port
                } else {
                    draft.port as u16
                },
                user: draft.user.to_string(),
                auth: AuthMethod::from_str(draft.auth.as_ref()),
                password,
                // Store the key path with forward slashes uniformly.
                private_key_path,
                private_key_inline,
                proxy: draft.proxy.to_string(),
                last_used: None,
                group: draft.group.to_string(),
                kind,
                local_distribution: String::new(),
                local_working_dir: String::new(),
                serial_port: draft.serial_port.to_string(),
                baud_rate: if draft.baud_rate <= 0 {
                    115_200
                } else {
                    draft.baud_rate as u32
                },
                data_bits: draft.data_bits as u8,
                stop_bits: draft.stop_bits as u8,
                parity: draft.parity.to_string(),
                flow_control: draft.flow_control.to_string(),
                encoding: draft.encoding.to_string(),
                forwards,
                triggers,
                disable_shell_integration: draft.disable_shell_integration,
                note: draft.note.to_string(),
                jump_session_id: draft.jump_session_id.to_string(),
            };
            {
                let mut s = store.borrow_mut();
                s.upsert(new_session);
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                w.set_dialog_open(false);
            }
        });
    }

    // Test connection from the session dialog. SSH tests use the same handshake,
    // host-key verification, proxy/jump routing, and authentication as a real
    // terminal connection (#276). Telnet and serial retain reachability tests.
    {
        let weak = window.as_weak();
        let runtime = runtime.clone();
        let store = store.clone();
        let edit_forwards = edit_forwards.clone();
        window.on_session_dialog_test(move |draft: SessionDraft| {
            let kind = draft.kind.to_string();
            if kind == "serial" {
                let port_name = draft.serial_port.to_string();
                let baud = if draft.baud_rate <= 0 {
                    115_200
                } else {
                    draft.baud_rate as u32
                };
                let weak_done = weak.clone();
                runtime.spawn(async move {
                    let message = match tokio::task::spawn_blocking(move || {
                        serialport::new(&port_name, baud)
                            .timeout(std::time::Duration::from_millis(800))
                            .open()
                    })
                    .await
                    {
                        Ok(Ok(_)) => t("连接正常", "Connection OK").to_string(),
                        Ok(Err(e)) => format!("{}: {e}", t("连接失败", "Connection failed")),
                        Err(e) => format!("{}: {e}", t("连接失败", "Connection failed")),
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_done.upgrade() {
                            w.set_dialog_test_status(message.into());
                        }
                    });
                });
                return;
            }

            let existing = store.borrow().get(draft.id.as_str()).cloned();
            let forwards = match validated_port_forwards(&edit_forwards.borrow()) {
                Ok(forwards) => forwards,
                Err(message) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                    return;
                }
            };
            // Triggers are irrelevant to an authentication probe (and must not
            // block it with a validation error), so start from an empty set.
            let session = session_from_draft(&draft, existing.as_ref(), forwards, Vec::new());
            let weak_done = weak.clone();

            if kind == "ssh" {
                let jump = resolve_jump(&store, &session);
                let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
                runtime.spawn(async move {
                    let mut test = Box::pin(test_session_auth(session, jump, events_tx));
                    let result = loop {
                        tokio::select! {
                            result = &mut test => break result,
                            event = events_rx.recv() => {
                                let Some(event) = event else { continue };
                                if matches!(
                                    event,
                                    SessionEvent::HostKeyPrompt { .. }
                                        | SessionEvent::CredentialPrompt { .. }
                                        | SessionEvent::MfaPrompt { .. }
                                ) {
                                    let weak_prompt = weak_done.clone();
                                    let _ = slint::invoke_from_event_loop(move || {
                                        let Some(w) = weak_prompt.upgrade() else { return };
                                        match event {
                                            SessionEvent::HostKeyPrompt {
                                                host,
                                                port,
                                                key_type,
                                                fingerprint,
                                                changed,
                                                responder,
                                            } => enqueue_hostkey_prompt(
                                                &w,
                                                host,
                                                port,
                                                key_type,
                                                fingerprint,
                                                changed,
                                                responder,
                                            ),
                                            SessionEvent::CredentialPrompt {
                                                session_id,
                                                host,
                                                user,
                                                need_user,
                                                need_password,
                                                responder,
                                            } => enqueue_cred_prompt(
                                                &w,
                                                session_id,
                                                host,
                                                user,
                                                need_user,
                                                need_password,
                                                responder,
                                            ),
                                            SessionEvent::MfaPrompt {
                                                session_id,
                                                host,
                                                prompt,
                                                echo,
                                                responder,
                                            } => enqueue_mfa_prompt(
                                                &w,
                                                session_id,
                                                host,
                                                prompt,
                                                echo,
                                                responder,
                                            ),
                                            _ => {}
                                        }
                                    });
                                }
                            }
                        }
                    };
                    let message = match result {
                        Ok(()) => t("连接正常", "Connection OK").to_string(),
                        Err(e) => format!("{}: {e:#}", t("连接失败", "Connection failed")),
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_done.upgrade() {
                            w.set_dialog_test_status(message.into());
                        }
                    });
                });
                return;
            }

            let host = session.host;
            let port = session.port;
            runtime.spawn(async move {
                let target = format!("{host}:{port}");
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    tokio::net::TcpStream::connect((host.as_str(), port)),
                )
                .await;
                let message = match result {
                    Ok(Ok(_)) => t("连接正常", "Connection OK").to_string(),
                    Ok(Err(e)) => format!("{}: {e}", t("连接失败", "Connection failed")),
                    Err(_) => format!("{}: {target}", t("连接超时", "Connection timed out")),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_done.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                });
            });
        });
    }

    // Cancel dialog.
    {
        let weak = window.as_weak();
        window.on_session_dialog_cancel(move || {
            if let Some(w) = weak.upgrade() {
                w.set_dialog_open(false);
            }
        });
    }

    // Private-key file picker: pick the private key and store its path with
    // forward-slash separators (uniform across Windows/Linux; russh accepts them).
    {
        let weak = window.as_weak();
        window.on_session_dialog_pick_key(move || {
            let mut dialog = rfd::FileDialog::new()
                .set_title(t("选择私钥文件", "Choose private key file"));
            // OpenSSH's standard macOS key names (id_ed25519, id_rsa, …) have
            // no extension. A native macOS extension filter makes those files
            // visible but disabled, so leave the picker unfiltered there (#325).
            // Other platforms retain the narrower existing filter.
            #[cfg(not(target_os = "macos"))]
            {
                dialog = dialog.add_filter(
                    t("SSH 私钥", "SSH private keys"),
                    &["ppk", "pem", "key"],
                );
            }
            // Start in ~/.ssh if it exists.
            if let Some(home) = directories::UserDirs::new().map(|u| u.home_dir().join(".ssh"))
                && home.is_dir()
            {
                dialog = dialog.set_directory(home);
            }
            if let Some(file) = dialog.pick_file() {
                let path = file.to_string_lossy().replace('\\', "/");
                if let Some(w) = weak.upgrade() {
                    w.set_dialog_key_path(path.into());
                }
            }
        });
    }

    // Add another editable port-forward row (#56, #277).
    {
        let weak = window.as_weak();
        let ef = edit_forwards.clone();
        window.on_add_forward(move || {
            ef.borrow_mut().push(blank_forward_draft());
            if let Some(w) = weak.upgrade() {
                w.set_dialog_forwards(forward_model(&ef.borrow()));
            }
        });
    }
    // Keep each editable row in the Rust-side working set. Saving validates and
    // converts all non-empty rows together, so no separate "added" state exists.
    {
        let ef = edit_forwards.clone();
        window.on_update_forward(move |index: i32, forward: PortFwd| {
            let i = index as usize;
            let mut forwards = ef.borrow_mut();
            if i < forwards.len() {
                forwards[i] = forward;
            }
        });
    }
    // Delete a port forward by index (#56).
    {
        let weak = window.as_weak();
        let ef = edit_forwards.clone();
        window.on_delete_forward(move |index: i32| {
            let i = index as usize;
            {
                let mut v = ef.borrow_mut();
                if i < v.len() {
                    v.remove(i);
                }
                if v.is_empty() {
                    v.push(blank_forward_draft());
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_dialog_forwards(forward_model(&ef.borrow()));
            }
        });
    }

    // Add another expect/send trigger row (#212).
    {
        let weak = window.as_weak();
        let et = edit_triggers.clone();
        window.on_add_trigger(move || {
            et.borrow_mut().push(blank_trigger_draft());
            if let Some(w) = weak.upgrade() {
                w.set_dialog_triggers(trigger_model(&et.borrow()));
            }
        });
    }
    // Keep each row in the Rust-side working set, like the forward drafts.
    {
        let et = edit_triggers.clone();
        window.on_update_trigger(move |index: i32, trigger: TriggerDraft| {
            let i = index as usize;
            let mut triggers = et.borrow_mut();
            if i < triggers.len() {
                triggers[i] = trigger;
            }
        });
    }
    // Delete a trigger by index (#212). Always keep one blank row so the
    // dialog never collapses to an empty list.
    {
        let weak = window.as_weak();
        let et = edit_triggers.clone();
        window.on_delete_trigger(move |index: i32| {
            let i = index as usize;
            {
                let mut v = et.borrow_mut();
                if i < v.len() {
                    v.remove(i);
                }
                if v.is_empty() {
                    v.push(blank_trigger_draft());
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_dialog_triggers(trigger_model(&et.borrow()));
            }
        });
    }

    // Connect session -> open a new terminal tab.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let tabs_model = tabs_model.clone();
        let terminals_model = terminals_model.clone();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let handles = handles.clone();
        let bufs = bufs.clone();
        let render_gates = render_gates.clone();
        let runtime = runtime.clone();
        let last_term_size = last_term_size.clone();
        let sftp_handles = sftp_handles.clone();
        let sftp_last_cwd = sftp_last_cwd.clone();
        let tab_statuses = tab_statuses.clone();
        let local_snap = local_snap.clone();
        let local_net_hist = local_net_hist.clone();
        let sftp_follow_cd = sftp_follow_cd.clone();
        window.on_connect_session(move |id: SharedString| {
            let id = id.to_string();
            let session = if id.starts_with("system:") {
                match builtin_local_sessions(store.borrow().wsl_profiles())
                    .into_iter()
                    .find(|s| s.id == id)
                {
                    Some(s) => s,
                    None => return,
                }
            } else {
                match store.borrow().get(&id).cloned() {
                    Some(s) => s,
                    None => return,
                }
            };
            let tab_id = format!("term-{}", uuid::Uuid::new_v4());
            let tab_title = session.name.clone();

            // Connection label shown in the sidebar / status line, per transport.
            let conn_label = match session.kind {
                SessionKind::Ssh => format!("{}@{}", session.user, session.host),
                SessionKind::Serial => {
                    format!("{} @{}", session.serial_port, session.baud_rate)
                }
                SessionKind::Telnet => format!("telnet {}:{}", session.host, session.port),
                SessionKind::Local => format!("local {}", session.name),
            };
            // Serial / Telnet have no SFTP side-channel.
            let has_sftp = session.kind == SessionKind::Ssh;

            // Seed the per-tab status so the sidebar shows "连接中 host" the
            // moment this tab becomes active (the `changed active-tab-id`
            // handler fires refresh-sidebar right after set_active_tab_id below).
            tab_statuses.lock().unwrap().insert(
                tab_id.clone(),
                TabStatus {
                    host: conn_label.clone(),
                    user: session.user.clone(),
                    session_id: id.clone(),
                    state: 0,
                    is_local: session.kind == SessionKind::Local,
                    ..Default::default()
                },
            );

            // Register tab + terminal state (SFTP fields start empty/loading).
            tabs_model.push(TabInfo {
                id: tab_id.clone().into(),
                title_len: tab_title_len(&tab_title),
                title: tab_title.into(),
                kind: "terminal".into(),
                connected: false,
            });
            // Each session keeps its own SFTP collapse state + sizes, seeded from
            // the global defaults (the "collapse SFTP by default" pref and the
            // persisted panel sizes) so they no longer bleed across panes (#v0.5).
            let (sftp_collapsed_default, sftp_h_default, sftp_w_default) = weak
                .upgrade()
                .map(|w| {
                    (
                        w.get_collapse_sftp_default(),
                        w.get_sftp_panel_height(),
                        w.get_sftp_panel_width(),
                    )
                })
                .unwrap_or((false, 220.0, 380.0));
            terminals_model.push(TerminalState {
                id: tab_id.clone().into(),
                status: t("连接中...", "Connecting...").into(),
                spans: ModelRc::from(std::rc::Rc::new(VecModel::<TermSpan>::default())),
                cursor_row: 0,
                cursor_col: 0,
                rows_used: 0,
                scroll_max: 0,
                scroll_offset: 0,
                is_alt_screen: false,
                find_matches: ModelRc::from(std::rc::Rc::new(VecModel::<TermMatch>::default())),
                selection: ModelRc::from(std::rc::Rc::new(VecModel::<TermMatch>::default())),
                font_size: 0.0,
                sftp_path: "/".into(),
                sftp_entries: ModelRc::from(std::rc::Rc::new(VecModel::<SftpEntry>::default())),
                sftp_status: if has_sftp {
                    t("SFTP 连接中...", "SFTP connecting...").into()
                } else {
                    t(
                        "此会话类型不支持 SFTP",
                        "SFTP not available for this session",
                    )
                    .into()
                },
                sftp_loading: has_sftp,
                sftp_tree_nodes: ModelRc::from(std::rc::Rc::new(
                    VecModel::<SftpTreeNode>::default(),
                )),
                sftp_selected_count: 0,
                sftp_sort_key: "".into(),
                sftp_sort_dir: 0,
                sftp_available: has_sftp,
                tunnels: ModelRc::from(std::rc::Rc::new(VecModel::<TunnelInfo>::default())),
                sftp_collapsed: !has_sftp || sftp_collapsed_default,
                sftp_panel_height: sftp_h_default,
                sftp_panel_width: sftp_w_default,
                sftp_saved_height: sftp_h_default,
            });
            // Create the alacritty-backed terminal for this tab (default
            // 24×80; resized on the first terminal-resize callback). The
            // scrollback depth comes from the settings value
            // (scrollback_lines, clamped to 100..=1_000_000 in config.rs).
            let is_dark_now = weak.upgrade().map(|w| w.get_dark_mode()).unwrap_or(true);
            let (output_highlight, custom_highlight_rules) = {
                let settings = store.borrow();
                (
                    OutputHighlightPreset::from_settings(
                        settings.output_highlight_enabled(),
                        settings.output_highlight_preset(),
                    ),
                    compile_output_rules(settings.output_highlight_rules()),
                )
            };
            let (t24, p24) = new_term(24, 80, store.borrow().scrollback_lines());
            bufs.lock().unwrap().insert(
                tab_id.clone(),
                Arc::new(Mutex::new(TermBuffer {
                    term: t24,
                    processor: p24,
                    find_query: String::new(),
                    is_dark: is_dark_now,
                    output_highlight,
                    custom_highlight_rules,
                    prev: Vec::new(),
                    view_offset: 0,
                    displayed_text: Vec::new(),
                    csi_state: CsiState::Normal,
                    csi_pending: Vec::new(),
                    raw: std::collections::VecDeque::new(),
                    rendered: Vec::new(),
                    scroll_cache: HashMap::new(),
                    render_gen: 0,
                    overline_active: false,
                    overline_start: None,
                    overline_ranges: Vec::new(),
                    sgr_buf: Vec::new(),
                    interactive_echo_until: std::time::Instant::now(),
                    json_format_output: store.borrow().json_format_output(),
                })),
            );
            render_gates.lock().unwrap().insert(
                tab_id.clone(),
                Arc::new(TabRenderGate::new(RENDER_MIN_INTERVAL)),
            );
            // No followed-cwd yet: the first OSC 7 always triggers a follow.
            sftp_last_cwd.lock().unwrap().remove(&tab_id);
            // Add the new tab to the focused pane and re-flatten (this also sets
            // active-tab-id to the new tab via refresh_panes).
            layout.borrow_mut().add_tab(tab_id.clone());
            if let Some(w) = weak.upgrade() {
                refresh_panes(
                    &w,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }

            // Spawn the shell (+ SFTP) workers and their event-pump threads.
            // Shared with in-place reconnect (#79) via start_session_in_tab.
            let ctx = ConnectCtx {
                weak: weak.clone(),
                runtime: runtime.clone(),
                handles: handles.clone(),
                sftp_handles: sftp_handles.clone(),
                sftp_last_cwd: sftp_last_cwd.clone(),
                bufs: bufs.clone(),
                render_gates: render_gates.clone(),
                tab_statuses: tab_statuses.clone(),
                local_snap: local_snap.clone(),
                local_net_hist: local_net_hist.clone(),
                last_term_size: last_term_size.clone(),
                sftp_follow_cd: sftp_follow_cd.clone(),
                store: store.clone(),
            };
            start_session_in_tab(&tab_id, session, &ctx);
        });
    }

    // Duplicate a tab's connection (#v0.5): open a fresh tab to the same saved
    // session, landing in the same pane as the source tab.
    {
        let weak = window.as_weak();
        let tab_statuses = tab_statuses.clone();
        let layout = layout.clone();
        window.on_tab_duplicate(move |tab_id: SharedString| {
            let tab_id = tab_id.to_string();
            let session_id = tab_statuses
                .lock()
                .unwrap()
                .get(&tab_id)
                .map(|s| s.session_id.clone())
                .unwrap_or_default();
            if session_id.is_empty() {
                return;
            }
            // Land the new tab in the same pane as the source. Read the pane id
            // into a local first so the immutable borrow is dropped before the
            // borrow_mut (else RefCell panics on the overlapping borrow).
            let pane = layout.borrow().leaf_of_tab(&tab_id);
            if let Some(pane) = pane {
                layout.borrow_mut().focused = pane;
            }
            if let Some(w) = weak.upgrade() {
                w.invoke_connect_session(session_id.into());
            }
        });
    }

    // Rename session (tab context menu): open the dialog pre-filled with the
    // tab's current title.
    {
        use slint::Model as _;
        let weak = window.as_weak();
        let tabs_model = tabs_model.clone();
        window.on_tab_rename_request(move |tab_id: SharedString| {
            let tab_id = tab_id.to_string();
            if tab_id.is_empty() || tab_id == "welcome" {
                return;
            }
            let title = (0..tabs_model.row_count())
                .find_map(|i| {
                    let row = tabs_model.row_data(i)?;
                    (row.id.as_str() == tab_id).then(|| row.title.to_string())
                })
                .unwrap_or_default();
            if let Some(w) = weak.upgrade() {
                w.set_tab_rename_id(tab_id.into());
                w.set_tab_rename_value(title.into());
                w.set_tab_rename_open(true);
            }
        });
    }

    // Apply the new display name. An empty name clears the override and
    // restores the saved session's name. Display-only: the config is untouched.
    {
        use slint::Model as _;
        let weak = window.as_weak();
        let tabs_model = tabs_model.clone();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let panes_model = panes_model_rename.clone();
        let splitters_model = splitters_model_rename.clone();
        let tab_statuses = tab_statuses.clone();
        let tab_titles = tab_titles.clone();
        let store = store.clone();
        window.on_rename_tab(move |tab_id: SharedString, name: SharedString| {
            if let Some(w) = weak.upgrade() {
                w.set_tab_rename_open(false);
            }
            let tab_id = tab_id.to_string();
            let name = name.trim().to_string();
            let title = if name.is_empty() {
                tab_titles.borrow_mut().remove(&tab_id);
                let session_id = tab_statuses
                    .lock()
                    .unwrap()
                    .get(&tab_id)
                    .map(|s| s.session_id.clone())
                    .unwrap_or_default();
                // Two separate borrows: the builtin fallback must not run while
                // the store lookup's RefCell borrow is still alive.
                let saved = store.borrow().get(&session_id).map(|s| s.name.clone());
                saved.or_else(|| {
                    builtin_local_sessions(store.borrow().wsl_profiles())
                        .into_iter()
                        .find(|s| s.id == session_id)
                        .map(|s| s.name)
                })
            } else {
                tab_titles.borrow_mut().insert(tab_id.clone(), name.clone());
                Some(name)
            };
            let Some(title) = title else {
                return;
            };
            for i in 0..tabs_model.row_count() {
                if let Some(mut row) = tabs_model.row_data(i)
                    && row.id.as_str() == tab_id
                {
                    row.title_len = tab_title_len(&title);
                    row.title = title.clone().into();
                    tabs_model.set_row_data(i, row);
                    break;
                }
            }
            // Panes mirror the tab model, so re-derive them to refresh the
            // tab strip in every pane that shows this session.
            if let Some(w) = weak.upgrade() {
                refresh_panes(
                    &w,
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }
        });
    }
}

/// Read the current mount-filter string from the store.  Empty = show all.
fn mount_filter() -> String {
    HISTORY_STORE.with(|s| {
        s.borrow()
            .as_ref()
            .map(|st| st.borrow().mount_filter().to_owned())
            .unwrap_or_default()
    })
}

/// Read the hide-special-partitions flag from the store.
fn hide_special_partitions() -> bool {
    HISTORY_STORE.with(|s| {
        s.borrow()
            .as_ref()
            .map(|st| st.borrow().hide_special_partitions())
            .unwrap_or(true)
    })
}

/// Read the convert-eol flag from the store.
fn convert_eol() -> bool {
    HISTORY_STORE.with(|s| {
        s.borrow()
            .as_ref()
            .map(|st| st.borrow().convert_eol())
            .unwrap_or(false)
    })
}

/// Known pseudo-filesystems and tiny special partitions to suppress in the
/// resource panel when hide-special-partitions is on.
fn is_special_partition(mount: &str) -> bool {
    mount == "/proc"
        || mount == "/sys"
        || mount == "/dev"
        || mount == "/run"
        || mount.starts_with("/proc/")
        || mount.starts_with("/sys/")
        || mount.starts_with("/dev/")
        || mount.starts_with("/run/")
        || mount.starts_with("/boot/efi")
        || mount == "/tmp"
        || mount.starts_with("/snap/")
        || mount.starts_with("/var/snap/")
}

#[cfg(test)]
mod process_row_tests {
    use super::*;

    #[test]
    fn marks_owner_and_preserves_source_tab() {
        let input = vec![
            ProcInfo {
                pid: 10,
                user: "alice".into(),
                cpu: 1.0,
                mem: 2.0,
                command: "own".into(),
            },
            ProcInfo {
                pid: 11,
                user: "root".into(),
                cpu: 3.0,
                mem: 4.0,
                command: "other".into(),
            },
        ];
        let rows = proc_rows(&input, "alice", "term-a");
        assert!(rows[0].own_process);
        assert!(!rows[1].own_process);
        assert!(rows.iter().all(|row| row.tab_id.as_str() == "term-a"));
    }

    #[test]
    fn privilege_rules_match_effective_login_user() {
        assert!(!process_needs_root("alice", "alice"));
        assert!(process_needs_root("alice", "root"));
        assert!(process_needs_root("alice", "bob"));
        assert!(!process_needs_root("root", "root"));
        assert!(!process_needs_root("root", "alice"));
    }
}


#[cfg(test)]
mod port_forward_draft_tests {
    use super::{blank_forward_draft, validated_port_forwards};

    #[test]
    fn blank_rows_are_ignored_when_saving() {
        assert!(
            validated_port_forwards(&[blank_forward_draft()])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn filled_rows_are_saved_without_an_add_step() {
        let mut local = blank_forward_draft();
        local.bind_port = "8080".into();
        local.host = "service.internal".into();
        local.host_port = "80".into();

        let mut dynamic = blank_forward_draft();
        dynamic.kind = "dynamic".into();
        dynamic.bind_port = "1080".into();

        let forwards = validated_port_forwards(&[local, dynamic]).unwrap();
        assert_eq!(forwards.len(), 2);
        assert_eq!(forwards[0].bind_port, 8080);
        assert_eq!(forwards[0].host, "service.internal");
        assert_eq!(forwards[1].kind, "dynamic");
        assert_eq!(forwards[1].host_port, 0);
    }

    #[test]
    fn partially_filled_rows_block_saving() {
        let mut draft = blank_forward_draft();
        draft.bind_port = "8080".into();
        assert!(validated_port_forwards(&[draft]).is_err());
    }
}

#[cfg(test)]
mod history_view_tests {
    use super::history_view_rows;

    #[test]
    fn lists_and_filters_commands_newest_first() {
        let history = vec![
            "git status".to_string(),
            "cargo check".to_string(),
            "git log".to_string(),
        ];

        let all: Vec<String> = history_view_rows(&history, "")
            .into_iter()
            .map(Into::into)
            .collect();
        assert_eq!(all, ["git log", "cargo check", "git status"]);

        let filtered: Vec<String> = history_view_rows(&history, "GIT")
            .into_iter()
            .map(Into::into)
            .collect();
        assert_eq!(filtered, ["git log", "git status"]);
    }
}

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
    /// session id → the answer given this run (`None` = cancelled), so a second
    /// connection for the same session is answered without re-prompting.
    static CRED_DECIDED: RefCell<HashMap<String, Option<crate::ssh::CredentialReply>>> =
        RefCell::new(HashMap::new());
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





fn wire_key_input(
    window: &AppWindow,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    bufs: TermBuffers,
    last_term_size: Arc<Mutex<(u32, u32)>>,
    store: Rc<RefCell<ConfigStore>>,
    ctx: ConnectCtx,
) {
    // Runtime SSH tunnel panel (#206). These tunnels live only for the active
    // connection; saved session configuration remains unchanged.
    {
        let handles_rc = handles.clone();
        window.on_tunnel_add(
            move |tab_id: SharedString,
                  name: SharedString,
                  kind: SharedString,
                  bind: SharedString,
                  bind_port: SharedString,
                  host: SharedString,
                  host_port: SharedString| {
                let kind = kind.to_string();
                if kind != "local" && kind != "dynamic" {
                    return;
                }
                let Ok(bind_port) = bind_port.trim().parse::<u16>() else {
                    return;
                };
                let host_port = if kind == "dynamic" {
                    0
                } else {
                    match host_port.trim().parse::<u16>() {
                        Ok(p) => p,
                        Err(_) => return,
                    }
                };
                let forward = crate::config::PortForward {
                    kind,
                    name: name.trim().to_string(),
                    bind_addr: bind.trim().to_string(),
                    bind_port,
                    host: host.trim().to_string(),
                    host_port,
                };
                if let Some(handle) = handles_rc.borrow().get(tab_id.as_str()) {
                    handle.add_tunnel(format!("runtime-{}", uuid::Uuid::new_v4()), forward);
                }
            },
        );
    }
    {
        let handles_rc = handles.clone();
        window.on_tunnel_stop(move |tab_id: SharedString, tunnel_id: SharedString| {
            if let Some(handle) = handles_rc.borrow().get(tab_id.as_str()) {
                handle.stop_tunnel(tunnel_id.to_string());
            }
        });
    }

    // --- Command bar (#55): run command + quick-command management ---------
    {
        let handles_rc = handles.clone();
        let store_rc = store.clone();
        let weak = window.as_weak();
        window.on_run_command(
            move |tab_id: SharedString, cmd: SharedString, to_all: bool| {
                let Some((line, bytes)) = encode_command_bar_input(&cmd) else {
                    return;
                };
                {
                    let h = handles_rc.borrow();
                    if to_all {
                        for handle in h.values() {
                            handle.send_raw(bytes.clone());
                        }
                    } else if let Some(handle) = h.get(tab_id.as_str()) {
                        handle.send_raw(bytes);
                    }
                }
                {
                    let mut s = store_rc.borrow_mut();
                    s.push_command_history(line);
                    let _ = s.save();
                }
                if let Some(w) = weak.upgrade() {
                    w.set_command_history(history_model(&store_rc.borrow()));
                }
            },
        );
    }
    // Copy a history command to the clipboard (#96).
    {
        window.on_copy_text(move |text: SharedString| {
            let t = text.to_string();
            std::thread::spawn(move || clipboard_set_text(t));
        });
    }
    // Delete a history entry (#96). The command-history model remains in
    // storage order, so this legacy row index still maps straight through.
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        window.on_delete_history(move |i: i32| {
            {
                let mut s = store_rc.borrow_mut();
                let idx = i as usize;
                if idx < s.command_history().len() {
                    s.remove_command_history(idx);
                    let _ = s.save();
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_command_history(history_model(&store_rc.borrow()));
            }
        });
    }
    // History search (#101): filter the dropdown by a case-insensitive substring.
    // The current query is shared so a delete from a filtered view re-filters.
    let hist_query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let hist_query = hist_query.clone();
        window.on_search_history(move |query: SharedString| {
            *hist_query.borrow_mut() = query.to_string();
            if let Some(w) = weak.upgrade() {
                w.set_history_view(history_view_model(&store_rc.borrow(), &query));
            }
        });
    }
    // Delete a history entry by its command text (#101) — index-free so it works
    // from the filtered dropdown view.
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let hist_query = hist_query.clone();
        window.on_delete_history_cmd(move |cmd: SharedString| {
            {
                let mut s = store_rc.borrow_mut();
                if let Some(idx) = s.command_history().iter().position(|c| c == cmd.as_str()) {
                    s.remove_command_history(idx);
                    let _ = s.save();
                }
            }
            if let Some(w) = weak.upgrade() {
                let s = store_rc.borrow();
                w.set_command_history(history_model(&s));
                w.set_history_view(history_view_model(&s, &hist_query.borrow()));
            }
        });
    }
    // Runtime-only collapse state for quick-command groups (#55) — like the
    // welcome session groups, this is not persisted across restarts. Starts with
    // every group collapsed (default-collapsed view).
    let collapsed_quick_groups: Rc<RefCell<std::collections::HashSet<String>>> =
        Rc::new(RefCell::new(all_quick_group_names(&store.borrow())));
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_add_quick_command(
            move |name: SharedString,
                  command: SharedString,
                  group: SharedString,
                  send_enter: bool| {
                let name = name.trim().to_string();
                let command = command.to_string();
                let group = group.trim().to_string();
                if name.is_empty() || command.trim().is_empty() {
                    return;
                }
                {
                    let mut s = store_rc.borrow_mut();
                    let mut v = s.quick_commands().to_vec();
                    v.push(crate::config::QuickCommand {
                        name,
                        command,
                        group,
                        send_enter,
                    });
                    s.set_quick_commands(v);
                    let _ = s.save();
                }
                if let Some(w) = weak.upgrade() {
                    w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
                }
            },
        );
    }
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_delete_quick_command(move |index: i32| {
            {
                let mut s = store_rc.borrow_mut();
                let mut v = s.quick_commands().to_vec();
                let i = index as usize;
                if i < v.len() {
                    v.remove(i);
                }
                s.set_quick_commands(v);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
            }
        });
    }
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_toggle_quick_group(move |group: SharedString| {
            let g = group.to_string();
            {
                let mut set = collapsed.borrow_mut();
                if !set.remove(&g) {
                    set.insert(g);
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
            }
        });
    }
    // Edit (#55): load the entry into the manage form in edit mode.
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        window.on_edit_quick_command(move |index: i32| {
            let i = index as usize;
            let cmd = store_rc.borrow().quick_commands().get(i).cloned();
            if let (Some(c), Some(w)) = (cmd, weak.upgrade()) {
                w.set_qcm_name(c.name.into());
                w.set_qcm_command(c.command.into());
                w.set_qcm_group(c.group.into());
                w.set_qcm_send_enter(c.send_enter);
                w.set_qcm_edit_index(index);
                w.set_quick_cmd_manage_open(true);
            }
        });
    }
    // Save an edited entry (#55).
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_save_quick_command(
            move |index: i32,
                  name: SharedString,
                  command: SharedString,
                  group: SharedString,
                  send_enter: bool| {
                let name = name.trim().to_string();
                let command = command.to_string();
                let group = group.trim().to_string();
                if name.is_empty() || command.trim().is_empty() {
                    return;
                }
                {
                    let mut s = store_rc.borrow_mut();
                    s.update_quick_command(
                        index as usize,
                        crate::config::QuickCommand {
                            name,
                            command,
                            group,
                            send_enter,
                        },
                    );
                    let _ = s.save();
                }
                if let Some(w) = weak.upgrade() {
                    w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
                }
            },
        );
    }
    // Duplicate (#55): clone the entry as a starting point.
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_duplicate_quick_command(move |index: i32| {
            {
                let mut s = store_rc.borrow_mut();
                let mut v = s.quick_commands().to_vec();
                if let Some(c) = v.get(index as usize).cloned() {
                    let dup = crate::config::QuickCommand {
                        name: format!("{} (copy)", c.name),
                        command: c.command,
                        group: c.group,
                        send_enter: c.send_enter,
                    };
                    v.insert(index as usize + 1, dup);
                    s.set_quick_commands(v);
                    let _ = s.save();
                }
            }
            if let Some(w) = weak.upgrade() {
                w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
            }
        });
    }
    // Move to a group (#55): "default" maps to the empty (ungrouped) group.
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_move_quick_command(move |index: i32, group: SharedString| {
            let target = group.to_string();
            let target = if target == "default" {
                String::new()
            } else {
                target
            };
            {
                let mut s = store_rc.borrow_mut();
                let mut v = s.quick_commands().to_vec();
                if let Some(c) = v.get_mut(index as usize) {
                    c.group = target;
                }
                s.set_quick_commands(v);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
            }
        });
    }
    // Reorder inside the current group (#310). The stored Vec remains the
    // source of truth; the grouped display model preserves this relative order.
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_reorder_quick_command(move |index: i32, move_up: bool| {
            let changed = {
                let mut s = store_rc.borrow_mut();
                let mut commands = s.quick_commands().to_vec();
                let changed = reorder_quick_command(&mut commands, index as usize, move_up);
                if changed {
                    s.set_quick_commands(commands);
                    let _ = s.save();
                }
                changed
            };
            if changed
                && let Some(w) = weak.upgrade()
            {
                w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
            }
        });
    }
    // Quick-group create / rename (#55).
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_submit_quick_group(move |orig: SharedString, name: SharedString| {
            {
                let mut s = store_rc.borrow_mut();
                if orig.is_empty() {
                    s.add_quick_group(name.to_string());
                } else {
                    s.rename_quick_group(orig.as_ref(), name.to_string());
                }
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
            }
        });
    }
    // Quick-group delete (#55) — UI only offers this on empty groups.
    {
        let store_rc = store.clone();
        let weak = window.as_weak();
        let collapsed = collapsed_quick_groups.clone();
        window.on_delete_quick_group(move |name: SharedString| {
            {
                let mut s = store_rc.borrow_mut();
                s.remove_quick_group(name.as_ref());
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_quick_commands(quick_cmd_model(&store_rc.borrow(), &collapsed.borrow()));
            }
        });
    }

    // Session sync / broadcast input: when on, a keystroke in any terminal is
    // mirrored to every online session (Xshell-style; #78 pt.4). Read on the hot
    // keystroke path, so use an AtomicBool rather than a window-property lookup.
    let sync_input = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let flag = sync_input.clone();
        window.on_set_sync_input(move |on| {
            flag.store(on, std::sync::atomic::Ordering::Relaxed);
        });
    }

    // Forward each keystroke as raw bytes to the SSH PTY. The server's bash /
    // readline handles echo, history (↑↓), Tab completion, Ctrl+C, etc.
    let store_clear = store.clone(); // also used by 清空缓存 handler below
    {
        let handles = handles.clone();
        let bufs = bufs.clone();
        let sync_input = sync_input.clone();
        // Shared timestamp: the last time the Shift key alone was pressed
        // (key="", shift=true).  Used by the time-based Backspace filter below.
        let last_shift_time: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));
        window.on_send_key(move |tab_id: SharedString, key: SharedString, ctrl: bool, alt: bool, shift: bool| {
            // ── Enter on a disconnected tab → reconnect in place (#79) ──────
            // FinalShell-style: the tab shows "连接已断开,按 Enter 重新连接";
            // pressing Enter re-spawns the shell + SFTP workers in the SAME tab
            // with a fresh screen instead of forcing the user to open a new one.
            if key.as_str() == "\n" && !ctrl && !alt {
                let dead_session = {
                    let statuses = ctx.tab_statuses.lock().unwrap();
                    statuses
                        .get(tab_id.as_str())
                        .filter(|st| st.state == 2)
                        .map(|st| st.session_id.clone())
                };
                if let Some(session_id) = dead_session {
                    let Some(session) = store.borrow().get(&session_id).cloned() else {
                        return;
                    };
                    // Close (not just remove) the dead shell/SFTP handles for
                    // this tab. SessionHandle has no Drop impl, so removing it
                    // alone leaves the SSH/PTY worker threads running until the
                    // process exits — every reconnect would leak one. close()
                    // only posts a command, so it is safe on the UI thread.
                    let dead_shell = ctx.handles.borrow_mut().remove(tab_id.as_str());
                    if let Some(h) = dead_shell {
                        h.close();
                    }
                    let dead_sftp =
                        ctx.sftp_handles.lock().unwrap().remove(tab_id.as_str());
                    if let Some(h) = dead_sftp {
                        h.close();
                    }
                    // Fresh screen: new parser, cleared history/selection.
                    {
                        if let Some(h) = term_buf(&ctx.bufs, tab_id.as_str()) {
                            let mut b = h.lock().unwrap();
                            let (rows, cols) = crate::terminal::term_size(&b.term);
                            (b.term, b.processor) = crate::terminal::new_term(rows, cols, ctx.store.borrow().scrollback_lines());
                            b.rendered.clear();
                            b.prev.clear();
                            b.displayed_text.clear();
                            b.view_offset = 0;
                            b.term.selection = None;
                            b.raw.clear();
                        }
                    }
                    if let Some(st) =
                        ctx.tab_statuses.lock().unwrap().get_mut(tab_id.as_str())
                    {
                        st.state = 0;
                    }
                    // Fresh session: the first OSC 7 after reconnect follows.
                    ctx.sftp_last_cwd.lock().unwrap().remove(tab_id.as_str());
                    if let Some(w) = ctx.weak.upgrade() {
                        set_terminal_row(&w, tab_id.as_str(), |t| {
                            t.status =
                                crate::i18n::t("重连中...", "Reconnecting...").into();
                        });
                    }
                    start_session_in_tab(tab_id.as_str(), session, &ctx);
                    return;
                }
            }
            // ── Font zoom (Ctrl+=/-/0, ⌘ on macOS; Ctrl+Shift = whole window) ──
            // Handled here before anything reaches the PTY. Ctrl(+Shift)+0
            // resets to the size chosen in Settings.
            if ctrl && !alt {
                let direction = match key.as_str() {
                    "=" | "+" => Some(1),
                    "-" | "_" => Some(-1),
                    "0" | ")" => Some(0),
                    _ => None,
                };
                if let Some(direction) = direction {
                    if let Some(w) = ctx.weak.upgrade() {
                        zoom_term_font(&w, tab_id.as_str(), direction, shift, &ctx.store);
                    }
                    return;
                }
            }
            // Zen (focus) mode toggle: Ctrl+Alt+Z (⌘⌥Z on macOS). Consumed
            // here so it works whether the terminal or the sidebar holds focus.
            if ctrl && alt && matches!(key.as_str(), "z" | "Z" | "\u{001a}") {
                if let Some(w) = ctx.weak.upgrade() {
                    w.set_zen_mode(!w.get_zen_mode());
                }
                return;
            }
            // Check whether the remote PTY switched to application cursor mode
            // (DECCKM, set by nano/vim via \x1b[?1h). In that mode the terminal
            // must send \x1bOA/B/C/D instead of \x1b[A/B/C/D.
            let app_cursor = if let Some(h) = term_buf(&bufs, tab_id.as_str()) {
                let mut b = h.lock().unwrap();
                // Typing snaps the view back to the live bottom so the
                // user always sees what they're entering.
                b.view_offset = 0;
                crate::terminal::app_cursor(&b.term)
            } else {
                false
            };
            // Never log the raw key string — it can be a password character
            // (#15). redact_key keeps control codes but masks printable text.
            tracing::debug!(
                "send_key tab={} key={} ctrl={} alt={} shift={} app_cursor={}",
                tab_id, redact_key(key.as_str()), ctrl, alt, shift, app_cursor
            );

            // ── Shift / Backspace 诊断日志 (info 级, 无需 RUST_LOG=debug) ─────
            // 每个 Shift 相关事件都打印 key 的 Unicode 码位，方便对比
            // 左Shift / 右Shift 是否产生不同的 key 字符串。
            if shift || key.as_str() == "\u{0008}" {
                // INFO level (no RUST_LOG needed) — must not leak the key text.
                // redact_key reveals only control code points (the IME markers
                // this diagnostic cares about), masking any printable char that
                // could be part of a Shift-typed password symbol (#15).
                let codepoints = redact_key(key.as_str());
                let elapsed_ms = last_shift_time
                    .lock()
                    .unwrap()
                    .map(|t| format!("{}ms ago", t.elapsed().as_millis()))
                    .unwrap_or_else(|| "never".to_string());
                tracing::info!(
                    "[KEY_DIAG] key={} shift={} ctrl={} alt={} | last_shift={}",
                    codepoints, shift, ctrl, alt, elapsed_ms
                );
            }

            // ── Track lone-Shift presses for the time-based Backspace filter ──
            // Slint sends key="" (empty string) when a bare modifier key (Shift,
            // Ctrl, Alt) is pressed.  We record the timestamp whenever Shift
            // alone fires so the filter below can catch IME-injected Backspace
            // events even if they arrive with shift=false.
            if key.as_str().is_empty() && shift && !ctrl && !alt {
                *last_shift_time.lock().unwrap() = Some(std::time::Instant::now());
                tracing::info!("[KEY_DIAG] lone-Shift recorded → timestamp saved");
            }

            // ── 拦截百度拼音注入的 Shift 标记字符（核心修复）────────────────────
            // 诊断日志证实，百度拼音通过 WH_KEYBOARD_LL 钩子，在 Shift 键按下时
            // 向消息队列注入一个 C0 控制字符，而非空字符串：
            //
            //   左 Shift → U+0015 (Ctrl+U / NAK), shift=true, ctrl=false
            //   右 Shift → U+0010 (Ctrl+P / DLE), shift=true, ctrl=false
            //              紧接着注入: U+0008 (Backspace), shift=false
            //
            // 这些字符绝对不应送入 PTY：
            //   0x15 (Ctrl+U) 在 bash/vim 中会清空当前输入行 → "左Shift替换字符"
            //   0x10 (Ctrl+P) 在 vim 中翻历史/触发补全     → "右Shift乱跳"
            //   0x08 (Backspace) 紧随其后                   → "右Shift删除字符"
            //
            // 合法独立 C0 键（Backspace=0x08, Tab=0x09, LF=0x0A, CR=0x0D,
            // ESC=0x1B）不受此过滤影响，由下方代码单独处理。
            //
            // 检测到 IME Shift 标记后，记录时间戳，让 Layer 2 在 1500ms 内
            // 拦截随后可能到来的 Backspace（右Shift场景，日志显示间隔约 914ms）。
            if !ctrl && !alt
                && let Some(c) = key.as_str().chars().next() {
                    let cp = c as u32;
                    let is_standalone = matches!(cp, 0x08 | 0x09 | 0x0A | 0x0D | 0x1B);
                    if key.as_str().chars().count() == 1
                        && (0x01..=0x1f).contains(&cp)
                        && !is_standalone
                    {
                        *last_shift_time.lock().unwrap() = Some(std::time::Instant::now());
                        tracing::info!(
                            "[KEY_DIAG] DROPPED IME C0 marker U+{:04X} (shift={}) → timestamp saved",
                            cp, shift
                        );
                        return;
                    }
                }

            // ── Windows: filter synthetic Ctrl+char injections ──────────────
            // Some keyboards / IME drivers (e.g. Aula F99 + Baidu Pinyin)
            // inject a synthetic WM_CHAR 0x11 (Ctrl+Q) when Left Ctrl is
            // briefly tapped, WITHOUT sending a WM_KEYDOWN VK_Q beforehand.
            //
            // FinalShell avoids this because it builds Ctrl+letter from
            // WM_KEYDOWN (virtual-key codes).  Slint uses WM_CHAR, so it
            // sees the injected byte and forwards it straight to us.
            //
            // Fix: for C0 control chars (Ctrl+A…Ctrl+Z, i.e. 0x01–0x1A),
            // use GetKeyState — which returns the key state *as of the last
            // processed message*, not the live hardware state — to verify
            // the corresponding letter VK was actually queued as a keydown
            // before this WM_CHAR arrived.  If Q was never keyed down,
            // GetKeyState(VK_Q) = 0 → the event is synthetic → drop it.
            #[cfg(windows)]
            if ctrl
                && let Some(ch) = key.as_str().chars().next() {
                    let cp = ch as u32;
                    // Always let Enter / Tab pass through regardless of Ctrl
                    // state.  These C0 codes (0x09 Tab, 0x0a LF, 0x0d CR) are
                    // "double-duty" keys: pressing Enter while Ctrl is still
                    // physically held (e.g. just after Ctrl+O in nano) generates
                    // Ctrl+M (0x0d) with ctrl=true — but GetKeyState(VK_M) is 0
                    // because the user never pressed M.  Without this exemption
                    // the filter would silently drop the Enter, making it
                    // impossible to confirm nano's "File Name to Write:" prompt.
                    let always_pass = matches!(cp, 0x09 | 0x0a | 0x0d);
                    if !always_pass
                        && key.as_str().chars().count() == 1
                        && (0x01..=0x1a).contains(&cp)
                        && !c0_letter_key_down(cp)
                    {
                        tracing::debug!(
                            "send_key: dropped synthetic Ctrl+{} \
                             (VK_{:02X} not down per GetKeyState)",
                            (0x40u8 + cp as u8) as char,
                            cp + 0x40
                        );
                        return;
                    }
                }

            // ── Filter synthetic Backspace injected by Chinese IME ────────────
            // Baidu Pinyin (and similar Chinese IMEs) hooks the keyboard at the
            // driver level via WH_KEYBOARD_LL, below Win32's ImmDisableIME.
            // When the user presses Shift to switch from Chinese to English mode
            // while a pinyin syllable is in-flight, the IME:
            //   1. Cancels the composition (discards the syllable).
            //   2. Posts WM_KEYDOWN VK_BACK + WM_CHAR 0x08 to erase whatever
            //      character it had already forwarded to the app.
            //
            // Two-layer defence:
            //
            //   Layer 1 – shift=true guard.
            //     The synthetic Backspace arrives during Shift keydown, so
            //     GetKeyState(VK_SHIFT) is still "down" → Slint reports shift=true.
            //     Drop any Backspace (0x08) arriving while Shift is flagged.
            //
            //   Layer 2 – time-based guard.
            //     Baidu Pinyin posts WM_CHAR 0x08 asynchronously, so by the time
            //     the message is dequeued Shift may already read as "up"
            //     → shift=false defeats Layer 1.
            //     Mitigation: we recorded the timestamp when the Shift key alone
            //     was pressed (key="", shift=true) a few lines above. Drop a
            //     Backspace arriving within the guarded interval unless a real
            //     intervening key has already cleared the marker.
            // Any real intervening key proves a previous Shift/IME marker is no
            // longer paired with this Backspace. Without clearing it, the broad
            // safety window drops legitimate Vim insert-mode Backspace (#319).
            if key.as_str() != "\u{0008}" && !key.as_str().is_empty() {
                *last_shift_time.lock().unwrap() = None;
            }

            if key.as_str() == "\u{0008}" && !ctrl && !alt {
                // Layer 1
                if shift {
                    tracing::info!("[KEY_DIAG] Backspace DROPPED by layer-1 (shift=true)");
                    return;
                }
                // Layer 2 — 时间窗口 1500ms
                // 日志显示百度拼音注入 U+0010(右Shift标记) 到 Backspace 之间
                // 间隔约 914ms，因此窗口设为 1500ms 以覆盖该场景。
                let (shift_just_pressed, elapsed_ms) = {
                    let guard = last_shift_time.lock().unwrap();
                    match *guard {
                        Some(t) => {
                            let ms = t.elapsed().as_millis();
                            (ms < 1500, ms)
                        }
                        None => (false, 0),
                    }
                };
                if shift_just_pressed {
                    tracing::info!(
                        "[KEY_DIAG] Backspace DROPPED by layer-2 ({}ms after IME Shift marker)",
                        elapsed_ms
                    );
                    return;
                }
                // Layer 3
                // Do not consult the live VK_BACK state here. Under UI/SSH
                // backlog the key-up can be processed before this callback, so
                // that test drops a genuine queued Backspace (#319).
                tracing::info!("[KEY_DIAG] Backspace PASSED all filters → sent to PTY");
            }

            if should_drop_bare_ctrl_marker(
                key.as_str(),
                ctrl,
                bare_ctrl_marker_workaround_enabled(),
            ) {
                tracing::debug!(
                    "send_key: dropped Slint bare Ctrl modifier marker {}",
                    redact_key(key.as_str())
                );
                return;
            }

            let bytes = key_to_pty_bytes(key.as_str(), ctrl, alt, app_cursor);
            // Log only the length — never the keystroke bytes, which can be
            // password characters (#15).
            tracing::debug!(
                "send_key len={} handle_exists={}",
                bytes.len(),
                handles.borrow().contains_key(tab_id.as_str()),
            );
            if !bytes.is_empty() {
                let h = handles.borrow();
                if sync_input.load(std::sync::atomic::Ordering::Relaxed) {
                    // Broadcast the same bytes to every online session (#78 pt.4).
                    for (target_id, handle) in h.iter() {
                        if let Some(buffer) = term_buf(&bufs, target_id) {
                            buffer.lock().unwrap().interactive_echo_until =
                                std::time::Instant::now() + INTERACTIVE_ECHO_WINDOW;
                        }
                        handle.send_raw(bytes.clone());
                    }
                } else if let Some(handle) = h.get(tab_id.as_str()) {
                    if let Some(buffer) = term_buf(&bufs, tab_id.as_str()) {
                        buffer.lock().unwrap().interactive_echo_until =
                            std::time::Instant::now() + INTERACTIVE_ECHO_WINDOW;
                    }
                    handle.send_raw(bytes);
                }
            }
        });
    }

    // Propagate PTY resize to the SSH worker and vt100 parser. Pixel
    // dimensions come from Slint; we approximate col/row counts using
    // Consolas 13px metrics.
    //
    // terminal_view.slint now passes the FocusScope height (not the full
    // TerminalView height), so the SFTP panel is already excluded.
    // Layout breakdown for the FocusScope:
    //   16 px  – bottom strip (TouchArea for focus-regain)
    //    8 px  – y-offset of the output Text element inside the Flickable
    // = 24 px  total vertical chrome within FocusScope
    //
    // Consolas 13 px renders at ≈ 8 px wide × 16 px tall per cell.
    {
        let handles = handles.clone();
        let bufs_resize = bufs.clone(); // keep bufs alive for the copy handler below
        let weak_resize = window.as_weak();
        // The Slint side now measures the real Consolas cell size (via a hidden
        // probe Text) and passes whole column/row counts directly, so there is
        // no pixel→cell guesswork here.  This keeps full-screen programs like
        // nano from over-counting rows and clipping their bottom shortcut bar.
        // Debounce PTY resizes (#163): a layout reflow (a tab becoming visible,
        // the SFTP panel docking, a window drag) can momentarily report a
        // near-zero width, which collapses term-cols to its 10-col floor.
        // Applying that to the remote PTY immediately resizes the server to 10
        // columns and reflows vt100 — garbling running output (e.g. a `git clone`
        // progress meter wraps at 10 chars). Coalesce rapid changes and apply
        // only the size that's still set after a short quiet period, so a
        // transient bad value never reaches the server.
        let pending_size: Rc<RefCell<HashMap<String, (u32, u32)>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let resize_debounce = Rc::new(slint::Timer::default());
        window.on_terminal_resize(move |tab_id: SharedString, cols_f: f32, rows_f: f32| {
            // A hidden terminal (inactive tab, or a split sibling not currently
            // shown) reports 0 width/height. Ignore those: flooring 0 to the 10-col
            // minimum and applying it would shrink that tab's PTY *and* poison
            // `last_term_size`, so the next connection (e.g. "Duplicate connection")
            // would start at 10 cols and wrap its first output to ~10 chars (#v0.5).
            // Only genuine, visible sizes drive a resize.
            if cols_f < 1.0 || rows_f < 1.0 {
                return;
            }
            let cols = (cols_f as u32).max(10);
            let rows = (rows_f as u32).max(5);
            pending_size
                .borrow_mut()
                .insert(tab_id.to_string(), (cols, rows));
            let pending = pending_size.clone();
            let handles = handles.clone();
            let bufs = bufs_resize.clone();
            let last = last_term_size.clone();
            let weak = weak_resize.clone();
            // (Re)arm the single-shot timer; rapid changes keep resetting it so
            // only the final, settled size is applied.
            resize_debounce.start(
                slint::TimerMode::SingleShot,
                std::time::Duration::from_millis(150),
                move || {
                    let settled: Vec<(String, (u32, u32))> = pending.borrow_mut().drain().collect();
                    for (tab, (cols, rows)) in settled {
                        tracing::debug!("terminal_resize tab={} cols={} rows={}", tab, cols, rows);
                        apply_terminal_resize(&handles, &bufs, &last, &tab, cols, rows);
                        // Re-render so the reflowed (or resized) grid shows at once
                        // instead of waiting for the next remote output (#169).
                        if let Some(win) = weak.upgrade() {
                            rebuild_tab_display(&win, &bufs, &tab);
                        }
                    }
                },
            );
        });
    }

    // Ctrl+Shift+C: copy current terminal screen to clipboard.
    {
        let bufs = bufs.clone();
        window.on_copy_terminal_text(move |tab_id: SharedString| {
            let text = term_buf(&bufs, tab_id.as_str())
                .map(|h| {
                    let buf = h.lock().unwrap();
                    // Copy the drag-selection when there is one, else the
                    // whole displayed screen.
                    let sel = buf.term.selection_to_string().unwrap_or_default();
                    if sel.is_empty() {
                        buf.displayed_text.join("\n")
                    } else {
                        sel
                    }
                })
                .unwrap_or_default();
            // Run the clipboard write on a dedicated OS thread.  arboard's
            // Windows backend opens the clipboard and pumps Win32 messages;
            // doing that on the Slint/winit event-loop thread re-enters the
            // message loop and dead-locks the whole UI.
            std::thread::spawn(move || clipboard_set_text(text));
        });
    }

    // Middle-click / Ctrl+Shift+V: paste clipboard text into PTY.
    {
        let handles = handles.clone();
        let bufs = bufs.clone();
        let weak = window.as_weak();
        window.on_paste_from_clipboard(move |tab_id: SharedString| {
            // Clone the (Send) command sender for this tab so the clipboard read
            // can run off the UI thread.  Reading arboard on the event-loop
            // thread is what froze the app on middle-click / paste — see the
            // copy handler above for the deadlock explanation.
            let sender = handles
                .borrow()
                .get(tab_id.as_str())
                .map(|h| h.commands.clone());
            let Some(sender) = sender else { return };
            let bracketed = terminal_uses_bracketed_paste(&bufs, tab_id.as_str());
            let weak = weak.clone();
            let tab_id = tab_id.to_string();
            std::thread::spawn(move || {
                match arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
                    Ok(text) => {
                        if text.contains(['\r', '\n']) {
                            let large = paste_requires_large_review(&text);
                            let preview = text.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_paste_confirm_tab(tab_id.into());
                                    w.set_paste_confirm_text(text.into());
                                    w.set_paste_confirm_preview(preview.into());
                                    w.set_paste_confirm_large(large);
                                    w.set_paste_confirm_open(true);
                                }
                            });
                        } else {
                            let bytes = encode_pasted_text(&text, bracketed, convert_eol());
                            let _ = sender.send(SessionCommand::RawInput(bytes));
                        }
                    }
                    Err(e) => tracing::warn!("paste_from_clipboard: clipboard error: {}", e),
                }
            });
        });
    }

    // Accept a previously reviewed multi-line paste (#262).
    {
        let handles_paste = handles.clone();
        let bufs_paste = bufs.clone();
        let weak = window.as_weak();
        window.on_paste_confirmed(move |tab_id: SharedString| {
            let Some(sender) = handles_paste
                .borrow()
                .get(tab_id.as_str())
                .map(|h| h.commands.clone())
            else {
                return;
            };
            let Some(w) = weak.upgrade() else { return };
            let text = w.get_paste_confirm_text().to_string();
            let bracketed = terminal_uses_bracketed_paste(&bufs_paste, tab_id.as_str());
            let _ = sender.send(SessionCommand::RawInput(encode_pasted_text(
                &text,
                bracketed,
                convert_eol(),
            )));
            w.set_paste_confirm_open(false);
        });
    }

    window.on_paste_confirm_cancelled(|| {});

    // Context menu → 清空缓存: reset the local vt100 buffer (drops scrollback),
    // wipe the displayed screen, then nudge the remote to redraw a fresh prompt.
    {
        let bufs_clear = bufs.clone();
        let handles_clear = handles.clone();
        let weak = window.as_weak();
        window.on_clear_terminal(move |tab_id: SharedString| {
            let tid = tab_id.to_string();
            if let Some(h) = term_buf(&bufs_clear, &tid) {
                let mut buf = h.lock().unwrap();
                let (rows, cols) = crate::terminal::term_size(&buf.term);
                (buf.term, buf.processor) =
                    crate::terminal::new_term(rows, cols, store_clear.borrow().scrollback_lines());
                buf.rendered.clear();
                buf.find_query.clear();
                buf.prev = Vec::new();
                buf.view_offset = 0;
                buf.term.selection = None;
                buf.displayed_text = Vec::new();
                buf.raw.clear();
            }
            if let Some(win) = weak.upgrade() {
                set_terminal_row(&win, &tid, |row| {
                    row.spans = ModelRc::from(Rc::new(VecModel::<TermSpan>::default()));
                    row.find_matches = ModelRc::from(Rc::new(VecModel::<TermMatch>::default()));
                    row.selection = ModelRc::from(Rc::new(VecModel::<TermMatch>::default()));
                    row.cursor_row = 0;
                    row.cursor_col = 0;
                    row.rows_used = 0;
                    row.scroll_max = 0;
                    row.scroll_offset = 0;
                });
            }
            if let Some(h) = handles_clear.borrow().get(&tid) {
                h.send_raw(vec![0x0c]); // Ctrl+L → shell clears + redraws prompt
            }
        });
    }

    // Context menu → 查找: store the query and recompute highlight rectangles.
    {
        let bufs_find = bufs.clone();
        let weak = window.as_weak();
        window.on_find_query_changed(move |tab_id: SharedString, query: SharedString| {
            let tid = tab_id.to_string();
            let q = query.to_string();
            let (matches, jumped) = with_term_buf(&bufs_find, &tid, |buf| {
                buf.find_query = q.clone();
                let mut matches = compute_find_matches(&buf.displayed_text, &q);
                let jumped = matches.is_empty() && buf.scroll_to_first_find_match(&q);
                if jumped {
                    buf.render();
                    matches = compute_find_matches(&buf.displayed_text, &q);
                }
                (matches, jumped)
            })
            .unwrap_or_default();
            if let Some(win) = weak.upgrade() {
                if jumped {
                    rebuild_tab_display(&win, &bufs_find, &tid);
                    return;
                }
                let model = ModelRc::from(Rc::new(VecModel::from(matches)));
                set_terminal_row(&win, &tid, |row| {
                    row.find_matches = model.clone();
                });
            }
        });
    }

    // Mouse-wheel → scroll the scrollback history.
    {
        let bufs_scroll = bufs.clone();
        let weak = window.as_weak();
        window.on_terminal_scroll(move |tab_id: SharedString, delta: i32| {
            let tid = tab_id.to_string();
            with_term_buf(&bufs_scroll, &tid, |buf| {
                // Scroll within our own session scrollback (history lines above
                // the live screen).  Offset 0 = live bottom.
                let max_off = buf
                    .term
                    .total_lines()
                    .saturating_sub(buf.term.screen_lines()) as i64;
                let cur = buf.view_offset as i64;
                buf.view_offset = (cur + delta as i64).clamp(0, max_off) as usize;
            });
            if let Some(win) = weak.upgrade() {
                rebuild_tab_display(&win, &bufs_scroll, &tid);
            }
        });
    }

    // Wheel inside an alt-screen program (tmux / less / vim): forward it to the PTY
    // so the program scrolls, instead of doing nothing (#170 — FinalShell /
    // MobaXterm behave this way). If the app is tracking the mouse (e.g. tmux with
    // `mouse on`), send a real wheel mouse-event in the encoding it asked for;
    // otherwise fall back to arrow keys (xterm "alternate scroll"), which scrolls
    // less / man / vim.
    {
        let bufs_wheel = bufs.clone();
        let handles_wheel = handles.clone();
        window.on_terminal_wheel(move |tab_id: SharedString, dir: i32, col: i32, row: i32| {
            let tid = tab_id.to_string();
            let bytes = term_buf(&bufs_wheel, &tid).map(|h| {
                let buf = h.lock().unwrap();
                if crate::terminal::mouse_report(&buf.term) != crate::terminal::MouseReport::None {
                    // 1-based cell under the cursor, clamped to the screen.
                    let (rows, cols) = crate::terminal::term_size(&buf.term);
                    let c = (col.clamp(0, cols.saturating_sub(1) as i32) as u16) + 1;
                    let r = (row.clamp(0, rows.saturating_sub(1) as i32) as u16) + 1;
                    let btn: u16 = if dir > 0 { 64 } else { 65 }; // wheel up / down
                    if crate::terminal::mouse_report(&buf.term) == crate::terminal::MouseReport::Sgr
                    {
                        format!("\x1b[<{btn};{c};{r}M").into_bytes()
                    } else {
                        // Legacy X10 encoding: ESC [ M  Cb Cx Cy  (each value + 32).
                        let cb = (btn + 32) as u8;
                        let cx = (c.min(223) + 32) as u8;
                        let cy = (r.min(223) + 32) as u8;
                        vec![0x1b, b'[', b'M', cb, cx, cy]
                    }
                } else {
                    // alternate-scroll: 3 arrow presses per notch, app-cursor aware.
                    let one: &[u8] = if dir > 0 {
                        if crate::terminal::app_cursor(&buf.term) {
                            b"\x1bOA"
                        } else {
                            b"\x1b[A"
                        }
                    } else if crate::terminal::app_cursor(&buf.term) {
                        b"\x1bOB"
                    } else {
                        b"\x1b[B"
                    };
                    one.repeat(3)
                }
            });
            if let (Some(bytes), Some(h)) = (bytes, handles_wheel.borrow().get(&tid)) {
                h.send_raw(bytes);
            }
        });
    }

    // Scrollbar drag → jump to an absolute scrollback offset (#103).
    {
        let bufs_scroll = bufs.clone();
        let weak = window.as_weak();
        window.on_terminal_scroll_to(move |tab_id: SharedString, offset: i32| {
            let tid = tab_id.to_string();
            with_term_buf(&bufs_scroll, &tid, |buf| {
                let max_off = buf
                    .term
                    .total_lines()
                    .saturating_sub(buf.term.screen_lines()) as i64;
                buf.view_offset = (offset as i64).clamp(0, max_off) as usize;
            });
            if let Some(win) = weak.upgrade() {
                rebuild_tab_display(&win, &bufs_scroll, &tid);
            }
        });
    }

    // Drag-selection lifecycle — alacritty native Selection (grid-coordinate,
    // survives view_offset changes so the anchor doesn't drift on scroll).
    {
        let bufs_sel = bufs.clone();
        let weak = window.as_weak();
        window.on_term_select_start(
            move |tab_id, row: i32, col: i32, left_half: bool, _ctrl: bool, _shift: bool| {
                let tid = tab_id.to_string();
                with_term_buf(&bufs_sel, &tid, |buf| {
                    let (rows, cols) = crate::terminal::term_size(&buf.term);
                    let r = row.clamp(0, rows.saturating_sub(1) as i32);
                    let c = col.clamp(0, cols.saturating_sub(1) as i32);
                    let pt = alacritty_terminal::index::Point {
                        line: alacritty_terminal::index::Line(r - buf.view_offset as i32),
                        column: alacritty_terminal::index::Column(c.max(0) as usize),
                    };
                    // The anchor side is derived from where the pointer landed inside
                    // the cell: on the left half the cell itself is selected, on the
                    // right half the selection starts at the next cell.  A fixed
                    // `Side::Right` previously skipped the first character of a
                    // selection (the very first cell of a row could never be selected).
                    let side = if left_half {
                        alacritty_terminal::index::Side::Left
                    } else {
                        alacritty_terminal::index::Side::Right
                    };
                    buf.term.selection = Some(alacritty_terminal::selection::Selection::new(
                        alacritty_terminal::selection::SelectionType::Simple,
                        pt,
                        side,
                    ));
                });
                if let Some(win) = weak.upgrade() {
                    refresh_terminal_selection(&win, &bufs_sel, &tid);
                }
            },
        );
    }
    {
        let bufs_sel = bufs.clone();
        let weak = window.as_weak();
        window.on_term_select_update(move |tab_id, row: i32, col: i32, left_half: bool| {
            let tid = tab_id.to_string();
            with_term_buf(&bufs_sel, &tid, |buf| {
                let (rows, cols) = crate::terminal::term_size(&buf.term);
                let r = row.clamp(0, rows.saturating_sub(1) as i32);
                let c = col.clamp(0, cols.saturating_sub(1) as i32);
                if let Some(ref mut sel) = buf.term.selection {
                    let pt = alacritty_terminal::index::Point {
                        line: alacritty_terminal::index::Line(r - buf.view_offset as i32),
                        column: alacritty_terminal::index::Column(c.max(0) as usize),
                    };
                    let side = if left_half {
                        alacritty_terminal::index::Side::Left
                    } else {
                        alacritty_terminal::index::Side::Right
                    };
                    sel.update(pt, side);
                }
            });
            if let Some(win) = weak.upgrade() {
                refresh_terminal_selection(&win, &bufs_sel, &tid);
            }
        });
    }
    {
        let bufs_sel = bufs.clone();
        let weak = window.as_weak();
        window.on_term_select_end(move |tab_id| {
            let tid = tab_id.to_string();
            let text = with_term_buf(&bufs_sel, &tid, |buf| {
                let text = buf.term.selection_to_string().unwrap_or_default();
                if text.is_empty() {
                    buf.term.selection = None;
                    None
                } else {
                    Some(text)
                }
            })
            .flatten();
            if let Some(t) = text
                && !t.is_empty()
            {
                std::thread::spawn(move || clipboard_set_text(t));
            }
            if let Some(win) = weak.upgrade() {
                refresh_terminal_selection(&win, &bufs_sel, &tid);
            }
        });
    }
    {
        let bufs_sel = bufs.clone();
        let weak = window.as_weak();
        window.on_term_select_autoscroll(move |tab_id, dir: i32| {
            let tid = tab_id.to_string();
            let Some(h) = term_buf(&bufs_sel, &tid) else {
                return;
            };
            {
                let mut buf = h.lock().unwrap();
                if crate::terminal::is_alt(&buf.term) || buf.term.selection.is_none() {
                    return;
                }
                let rows = crate::terminal::term_size(&buf.term).0;
                let max_off = buf
                    .term
                    .total_lines()
                    .saturating_sub(buf.term.screen_lines());
                let step = 2usize;
                let edge_line = if dir < 0 {
                    let new_off = (buf.view_offset + step).min(max_off);
                    if new_off == buf.view_offset {
                        return;
                    }
                    buf.view_offset = new_off;
                    alacritty_terminal::index::Line(0 - buf.view_offset as i32)
                } else if dir > 0 {
                    let new_off = buf.view_offset.saturating_sub(step);
                    if new_off == buf.view_offset {
                        return;
                    }
                    buf.view_offset = new_off;
                    alacritty_terminal::index::Line(rows as i32 - 1 - buf.view_offset as i32)
                } else {
                    return;
                };
                if let Some(ref mut sel) = buf.term.selection {
                    let pt = alacritty_terminal::index::Point {
                        line: edge_line,
                        column: alacritty_terminal::index::Column(0),
                    };
                    sel.update(pt, alacritty_terminal::index::Side::Right);
                }
            }
            if let Some(win) = weak.upgrade() {
                refresh_terminal_selection(&win, &bufs_sel, &tid);
            }
        });
    }
}

/// Mutate the `TerminalState` whose id matches `tab_id` in the live model.
/// Must run on the Slint event loop thread.
pub(crate) fn set_terminal_row(win: &AppWindow, tab_id: &str, mutator: impl Fn(&mut TerminalState)) {
    let terminals = win.get_terminals();
    let Some(model) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
        return;
    };
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i)
            && row.id.as_str() == tab_id
        {
            mutator(&mut row);
            model.set_row_data(i, row);
            break;
        }
    }
}

/// Convert a Slint `KeyEvent.text` + modifier flags into the byte sequence
/// that the remote PTY expects.
///
/// Slint uses Unicode Private Use Area (`\u{F700}`…) for special keys.
/// Regular printable characters and C0 control characters are passed as-is.
///
/// Render a key string for diagnostic logs WITHOUT leaking its content (#15).
///
/// Any printable character could be a password character, so we never emit it.
/// Only C0/C1 control code points (Backspace, Esc, the IME-injected 0x10/0x15
/// markers, …) are revealed — those are exactly what the Shift/Backspace IME
/// diagnostics need and are never password material. Printable characters are
/// collapsed to a count, so the logs stay useful without exposing keystrokes.
fn redact_key(key: &str) -> String {
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

/// `app_cursor` mirrors the remote terminal's DECCKM mode (`\x1b[?1h/l`):
/// when true the four arrow keys must use SS3 sequences (`\x1bOA`…) instead
/// of the default CSI sequences (`\x1b[A`…).  Full-screen apps like nano and
/// vim set this mode on startup.
/// Build the editor's line-number gutter text: "1\n2\n…\nN", one number per line
/// of `content`, matching its (newline-separated) line count (#81).
fn line_numbers_for(content: &str) -> String {
    use std::fmt::Write;
    let lines = content.split('\n').count().max(1);
    let mut s = String::with_capacity(lines * 4);
    for i in 1..=lines {
        if i > 1 {
            s.push('\n');
        }
        let _ = write!(s, "{i}");
    }
    s
}

/// Write `text` to the system clipboard. Call from a dedicated thread, never the
/// UI thread (arboard pumps the Win32 message loop / blocks).
///
/// On Linux the clipboard selection only persists while the owning client stays
/// alive, so we use arboard's `set().wait()`, which blocks this thread until
/// another app takes ownership — otherwise the copied text vanishes the moment
/// the `Clipboard` handle is dropped. Combined with the `wayland-data-control`
/// feature this is also what makes copy work on Wayland sessions (issue #47).
fn clipboard_set_text(text: String) {
    #[cfg(target_os = "linux")]
    let result = {
        use arboard::SetExtLinux as _;
        arboard::Clipboard::new().and_then(|mut cb| cb.set().wait().text(text))
    };
    #[cfg(not(target_os = "linux"))]
    let result = arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text));
    if let Err(e) = result {
        tracing::warn!("clipboard set_text error: {}", e);
    }
}






/// Split a stored proxy URL into `(type, host:port)` for the session dialog.
///
/// `""` → `("none", "")`. Recognises `socks5`/`socks5h`/`socks` and
/// `http`/`https` scheme prefixes. A value without a (recognised) scheme is
/// treated as SOCKS5, matching proxy.rs's parse default, so older configs that
/// stored a bare `host:port` keep working.
/// Parse a "vX.Y.Z" / "X.Y.Z" tag into a comparable tuple, or None if it isn't
/// a three-part numeric version. A pre-release suffix on the patch (e.g.
/// "3-rc1") is tolerated by taking its leading digits (#48).
pub(crate) fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor, patch))
}

fn split_proxy(url: &str) -> (String, String) {
    let s = url.trim();
    if s.is_empty() {
        return ("none".to_string(), String::new());
    }
    let lower = s.to_ascii_lowercase();
    for p in ["http://", "https://"] {
        if lower.starts_with(p) {
            return (
                "http".to_string(),
                s[p.len()..].trim_end_matches('/').to_string(),
            );
        }
    }
    for p in ["socks5h://", "socks5://", "socks://"] {
        if lower.starts_with(p) {
            return (
                "socks5".to_string(),
                s[p.len()..].trim_end_matches('/').to_string(),
            );
        }
    }
    ("socks5".to_string(), s.trim_end_matches('/').to_string())
}

fn parent_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
        None => "/".to_string(),
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn windows_process_key_ctrl_release_keeps_physical_side() {
        use i_slint_backend_winit::winit::event::ElementState;
        use i_slint_backend_winit::winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

        let process = Key::Named(NamedKey::Process);
        assert_eq!(
            windows_process_ctrl_release(
                ElementState::Released,
                &process,
                &PhysicalKey::Code(KeyCode::ControlLeft),
            ),
            Some(CtrlKeySide::Left)
        );
        assert_eq!(
            windows_process_ctrl_release(
                ElementState::Released,
                &process,
                &PhysicalKey::Code(KeyCode::ControlRight),
            ),
            Some(CtrlKeySide::Right)
        );
    }

    #[test]
    fn windows_process_key_recovery_ignores_other_key_events() {
        use i_slint_backend_winit::winit::event::ElementState;
        use i_slint_backend_winit::winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

        let process = Key::Named(NamedKey::Process);
        let left_ctrl = PhysicalKey::Code(KeyCode::ControlLeft);
        assert_eq!(
            windows_process_ctrl_release(ElementState::Pressed, &process, &left_ctrl),
            None
        );
        assert_eq!(
            windows_process_ctrl_release(
                ElementState::Released,
                &Key::Named(NamedKey::Control),
                &left_ctrl,
            ),
            None
        );
        assert_eq!(
            windows_process_ctrl_release(
                ElementState::Released,
                &process,
                &PhysicalKey::Code(KeyCode::KeyC),
            ),
            None
        );
    }

    #[test]
    fn bare_alt_is_not_forwarded() {
        // Slint sends Alt-alone as key=0x12 with alt=true. It must produce no
        // bytes — otherwise it becomes ESC+0x12 and clears the input (issue #43).
        assert_eq!(
            key_to_pty_bytes("\u{0012}", false, true, false),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn bare_modifier_codes_are_dropped() {
        // Shift..MetaR (0x10..=0x18) pressed alone (ctrl=false) → nothing sent.
        for cp in 0x10u32..=0x18 {
            let s = char::from_u32(cp).unwrap().to_string();
            assert_eq!(
                key_to_pty_bytes(&s, false, false, false),
                Vec::<u8>::new(),
                "code point {:#04x} should be dropped",
                cp
            );
        }
    }

    #[test]
    fn home_and_end_follow_application_cursor_mode() {
        // Normal cursor mode: CSI sequences.
        assert_eq!(key_to_pty_bytes("\u{F729}", false, false, false), b"\x1b[H");
        assert_eq!(key_to_pty_bytes("\u{F72B}", false, false, false), b"\x1b[F");
        // Application cursor mode (DECCKM): SS3 sequences, required by
        // oh-my-zsh / ZLE for beginning-/end-of-line movement.
        assert_eq!(key_to_pty_bytes("\u{F729}", false, false, true), b"\x1bOH");
        assert_eq!(key_to_pty_bytes("\u{F72B}", false, false, true), b"\x1bOF");
    }

    #[test]
    fn ctrl_letter_c0_still_passes() {
        // A real Ctrl+R encoded as the C0 byte 0x12 with ctrl=true must still be
        // forwarded; the #274/#312 fix filters only bare Ctrl/CtrlR markers.
        assert_eq!(key_to_pty_bytes("\u{0012}", true, false, false), vec![0x12]);
        // Ctrl+X as C0 0x18.
        assert_eq!(key_to_pty_bytes("\u{0018}", true, false, false), vec![0x18]);
    }

    #[test]
    fn platform_bare_ctrl_markers_do_not_reach_nano() {
        // Slint on Debian and macOS emits these before the actual Ctrl+letter event.
        assert!(should_drop_bare_ctrl_marker("\u{0011}", true, true));
        assert!(should_drop_bare_ctrl_marker("\u{0016}", true, true));
        // Other platforms retain their existing direct-C0 behaviour.
        assert!(!should_drop_bare_ctrl_marker("\u{0011}", true, false));
        assert!(!should_drop_bare_ctrl_marker("x", true, true));
        // Genuine Ctrl+Q/Ctrl+V chords still encode from the final letter
        // event (#369).
        assert_eq!(key_to_pty_bytes("q", true, false, false), vec![0x11]);
        assert_eq!(key_to_pty_bytes("v", true, false, false), vec![0x16]);
        // The following Ctrl+X must still become CAN (0x18), which nano uses
        // for Exit.
        assert_eq!(key_to_pty_bytes("x", true, false, false), vec![0x18]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_ime_bare_ctrl_backspace_marker_is_filtered() {
        // #348: macOS IMEs may report bare Control as U+0008; it must not be
        // sent as Backspace. A genuine Ctrl+H still arrives through the final
        // printable letter and is encoded at the PTY boundary.
        assert!(should_drop_bare_ctrl_marker("\u{0008}", true, true));
        assert!(!should_drop_bare_ctrl_marker("\u{0008}", true, false));
        assert!(!should_drop_bare_ctrl_marker("\u{0008}", false, true));
        assert_eq!(key_to_pty_bytes("h", true, false, false), vec![0x08]);
    }

    #[test]
    fn alt_letter_still_sends_esc_prefix() {
        // Alt+a (a real Meta combo) must still send ESC + 'a'.
        assert_eq!(key_to_pty_bytes("a", false, true, false), vec![0x1b, b'a']);
    }

    #[test]
    fn split_proxy_recognises_schemes() {
        assert_eq!(split_proxy(""), ("none".into(), "".into()));
        assert_eq!(
            split_proxy("http://10.0.0.1:1022"),
            ("http".into(), "10.0.0.1:1022".into())
        );
        assert_eq!(
            split_proxy("socks5://127.0.0.1:1080"),
            ("socks5".into(), "127.0.0.1:1080".into())
        );
        // user:pass survive in the host:port part.
        assert_eq!(
            split_proxy("http://u:p@host:8080"),
            ("http".into(), "u:p@host:8080".into())
        );
        // bare host:port (legacy) → treated as socks5.
        assert_eq!(
            split_proxy("127.0.0.1:1080"),
            ("socks5".into(), "127.0.0.1:1080".into())
        );
    }

    #[test]
    fn paste_normalizes_newlines_to_cr() {
        // CRLF (Windows clipboard) and LF both collapse to a single CR so a
        // backslash-continued multi-line command pastes intact.
        assert_eq!(
            normalize_pasted_newlines("sudo apt install \\\r\n  docker-ce", false),
            "sudo apt install \\\r  docker-ce"
        );
        assert_eq!(normalize_pasted_newlines("a\nb\nc", false), "a\rb\rc");
        // A lone CR is left as-is; no doubling.
        assert_eq!(normalize_pasted_newlines("a\rb", false), "a\rb");
        // No newlines → unchanged.
        assert_eq!(normalize_pasted_newlines("echo hi", false), "echo hi");
    }

    #[test]
    fn command_bar_preserves_multiline_heredoc() {
        let command = "cat <<'EOF'\nHEREDOC-1\n中文-HEREDOC-2\nEOF\n";
        let (history, bytes) = encode_command_bar_input(command).unwrap();
        assert_eq!(history, command.trim_end());
        assert_eq!(bytes, command.as_bytes());
        assert!(!history.lines().any(|line| line.starts_with(' ')));
    }

    #[test]
    fn paste_uses_remote_bracketed_paste_mode() {
        assert_eq!(
            encode_pasted_text("first\r\n  second", true, false),
            b"\x1b[200~first\r  second\x1b[201~"
        );
        assert_eq!(
            encode_pasted_text("safe\x1b[201~\x03text", true, false),
            b"\x1b[200~safe[201~text\x1b[201~"
        );
        assert_eq!(
            encode_pasted_text("first\r\nsecond", false, false),
            b"first\rsecond"
        );
    }

    #[test]
    fn long_pastes_switch_to_large_review() {
        assert!(!paste_requires_large_review("short prompt\nsecond line"));
        assert!(!paste_requires_large_review(&"a".repeat(600)));
        assert!(paste_requires_large_review(&"a".repeat(601)));
        assert!(!paste_requires_large_review(&vec!["line"; 12].join("\r\n")));
        assert!(paste_requires_large_review(&vec!["line"; 13].join("\r\n")));
    }

    #[test]
    fn confirmed_exit_never_reopens_close_prompt() {
        assert!(should_block_close(false, true));
        assert!(!should_block_close(false, false));
        assert!(!should_block_close(true, true));
        assert!(!should_block_close(true, false));
    }
}

#[cfg(test)]
mod log_highlight_tests {
    use super::*;
    use crate::terminal::TermColor;

    fn plain_run(text: &str, col: i32) -> HistSpan {
        HistSpan {
            text: text.to_string(),
            fg: TermColor::Default,
            bg: TermColor::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: Default::default(),
            hidden: false,
            strike: false,
            overline: false,
            inverse: false,
            col,
            cells: text.chars().count() as i32,
        }
    }

    fn custom_rule(
        pattern: &str,
        regex: bool,
        case_sensitive: bool,
        whole_line: bool,
        color: &str,
    ) -> CompiledOutputRule {
        compile_output_rules(&[OutputHighlightRule {
            pattern: pattern.to_string(),
            regex,
            case_sensitive,
            whole_line,
            color: color.to_string(),
            enabled: true,
        }])
        .pop()
        .expect("test rule should compile")
    }

    #[test]
    fn highlights_uppercase_level_and_preserves_columns() {
        let runs = highlight_plain_output(
            vec![plain_run("2026-07-14T10:20:30Z ERROR request failed", 0)],
            OutputHighlightPreset::Log,
            &[],
        );
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[1].text, "ERROR");
        assert_eq!(runs[1].col, 21);
        assert_eq!(runs[1].cells, 5);
        assert!(runs[1].bold);
        assert!(matches!(runs[1].fg, TermColor::Idx(9)));
        assert_eq!(runs[2].col, 26);
    }

    #[test]
    fn highlights_structured_lowercase_level_only() {
        let json = r#"{"level":"warn","message":"disk nearly full"}"#;
        let runs =
            highlight_plain_output(vec![plain_run(json, 4)], OutputHighlightPreset::Log, &[]);
        let level = runs
            .iter()
            .find(|run| run.text == "warn")
            .expect("structured level should be highlighted");
        assert!(matches!(level.fg, TermColor::Idx(11)));

        assert!(log_level_marker("an error occurred", 96).is_none());
        assert!(log_level_marker("ERROR_CODE=5", 96).is_none());
    }

    #[test]
    fn preserves_existing_ansi_styles() {
        let mut coloured = plain_run("ERROR", 0);
        coloured.fg = TermColor::Idx(2);
        let runs = highlight_plain_output(vec![coloured], OutputHighlightPreset::Log, &[]);
        assert_eq!(runs.len(), 1);
        assert!(matches!(runs[0].fg, TermColor::Idx(2)));
        assert!(!runs[0].bold);
    }

    #[test]
    fn alternate_screen_does_not_add_log_colours() {
        let (mut term, mut processor) = crate::terminal::new_term(3, 30, 0);
        process_bytes(&mut processor, &mut term, b"\x1b[?1049hERROR");
        assert!(crate::terminal::is_alt(&term));
        let (_plain, runs, _wrapped) = build_row(&term, 0, 30, &[]);
        let level = runs
            .iter()
            .find(|run| run.text.contains("ERROR"))
            .expect("alternate-screen text should still render");
        assert!(matches!(level.fg, TermColor::Default));
        assert!(!level.bold);
    }

    #[test]
    fn off_preset_leaves_plain_levels_untouched() {
        let runs = highlight_plain_output(
            vec![plain_run("ERROR request failed", 0)],
            OutputHighlightPreset::Off,
            &[],
        );
        assert_eq!(runs.len(), 1);
        assert!(matches!(runs[0].fg, TermColor::Default));
        assert!(!runs[0].bold);
    }

    #[test]
    fn devops_preset_adds_deployment_and_structured_states() {
        let success = highlight_plain_output(
            vec![plain_run("deploy SUCCESS", 0)],
            OutputHighlightPreset::DevOps,
            &[],
        );
        let token = success
            .iter()
            .find(|run| run.text == "SUCCESS")
            .expect("DevOps success should be highlighted");
        assert!(matches!(token.fg, TermColor::Idx(10)));

        let json = highlight_plain_output(
            vec![plain_run(r#"{"status":"failed"}"#, 0)],
            OutputHighlightPreset::DevOps,
            &[],
        );
        let token = json
            .iter()
            .find(|run| run.text == "failed")
            .expect("structured DevOps state should be highlighted");
        assert!(matches!(token.fg, TermColor::Idx(9)));

        let conservative = highlight_plain_output(
            vec![plain_run("deploy SUCCESS", 0)],
            OutputHighlightPreset::Log,
            &[],
        );
        assert_eq!(conservative.len(), 1);
    }

    #[test]
    fn custom_literal_is_case_insensitive_and_overrides_builtin_colour() {
        let rule = custom_rule("error", false, false, false, "green");
        let runs = highlight_plain_output(
            vec![plain_run("ERROR then error", 0)],
            OutputHighlightPreset::Log,
            &[rule],
        );
        let hits: Vec<_> = runs
            .iter()
            .filter(|run| matches!(run.fg, TermColor::Idx(10)))
            .collect();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].text, "ERROR");
        assert_eq!(hits[1].text, "error");
        assert!(!runs.iter().any(|run| matches!(run.fg, TermColor::Idx(9))));
    }

    #[test]
    fn custom_regex_can_highlight_whole_line_without_overwriting_ansi() {
        let rule = custom_rule(r"timeout|denied", true, false, true, "magenta");
        let mut ansi = plain_run(" ANSI", 18);
        ansi.fg = TermColor::Idx(2);
        let runs = highlight_plain_output(
            vec![plain_run("request timeout   ", 0), ansi],
            OutputHighlightPreset::Log,
            &[rule],
        );
        assert!(matches!(runs[0].fg, TermColor::Idx(13)));
        assert!(runs[0].bold);
        assert!(matches!(runs[1].fg, TermColor::Idx(2)));
    }

    #[test]
    fn custom_unicode_match_preserves_terminal_grid_columns() {
        let rule = custom_rule("错误", false, true, false, "red");
        let text = "前缀错误 done";
        let mut run = plain_run(text, 0);
        run.cells = text_cell_width(text);
        let runs = highlight_plain_output(vec![run], OutputHighlightPreset::Log, &[rule]);
        let hit = runs
            .iter()
            .find(|run| run.text == "错误")
            .expect("CJK keyword should be highlighted");
        assert_eq!(hit.col, 4);
        assert_eq!(hit.cells, 4);
    }

    #[test]
    fn invalid_regex_is_rejected_before_persistence() {
        assert!(validate_output_highlight_rule("([", true, false).is_err());
        assert!(validate_output_highlight_rule("literal", false, false).is_ok());
    }

    #[test]
    fn builtin_preset_highlights_numbers_urls_ips_uuids_and_keywords() {
        // Bare number → left unstyled (no catch-all number rule, no grey)
        let num = highlight_plain_output(
            vec![plain_run("processed 42 records", 0)],
            OutputHighlightPreset::Builtin,
            &[],
        );
        assert!(
            num.iter()
                .any(|r| r.text.contains("42") && matches!(r.fg, TermColor::Default)),
            "bare number should stay default-coloured"
        );

        // URL → blue (12), including the port and path
        let url = highlight_plain_output(
            vec![plain_run("GET http://api.dev:8080/v1", 0)],
            OutputHighlightPreset::Builtin,
            &[],
        );
        assert!(
            url.iter()
                .any(|r| r.text.contains("http") && matches!(r.fg, TermColor::Idx(12)))
        );

        // Bare domain without scheme → blue (12)
        let domain = highlight_plain_output(
            vec![plain_run("ping www.baidu.com", 0)],
            OutputHighlightPreset::Builtin,
            &[],
        );
        assert!(
            domain
                .iter()
                .any(|r| r.text == "www.baidu.com" && matches!(r.fg, TermColor::Idx(12))),
            "bare domain www.baidu.com should be styled blue"
        );

        // A single-dot filename must NOT be mistaken for a domain.
        let fname = highlight_plain_output(
            vec![plain_run("-rw-r--r-- 1 root root 8388608 querylog.json", 0)],
            OutputHighlightPreset::Builtin,
            &[],
        );
        assert!(
            !fname
                .iter()
                .any(|r| r.text.contains("querylog.json") && matches!(r.fg, TermColor::Idx(12))),
            "single-dot filename should not be styled as a domain"
        );

        // IPv4 → blue (12)
        let ip = highlight_plain_output(
            vec![plain_run("client 192.168.1.100", 0)],
            OutputHighlightPreset::Builtin,
            &[],
        );
        assert!(
            ip.iter()
                .any(|r| r.text == "192.168.1.100" && matches!(r.fg, TermColor::Idx(12)))
        );

        // IPv6 with `::` compression → one blue span (not split, no grey hex)
        let ipv6 = highlight_plain_output(
            vec![plain_run("addr fe80::f0da:145:b458:4e3e", 0)],
            OutputHighlightPreset::Builtin,
            &[],
        );
        assert!(ipv6.iter().any(
            |r| r.text == "fe80::f0da:145:b458:4e3e" && matches!(r.fg, TermColor::Idx(12))
        ),
            "IPv6 with :: must be one blue span");

        // UUID → magenta (13)
        let uuid = highlight_plain_output(
            vec![plain_run("id=550e8400-e29b-41d4-a716-446655440000", 0)],
            OutputHighlightPreset::Builtin,
            &[],
        );
        assert!(
            uuid.iter()
                .any(|r| r.text.contains("550e8400") && matches!(r.fg, TermColor::Idx(13)))
        );

        // Severity keyword → red (9), case-insensitive
        let kw = highlight_plain_output(
            vec![plain_run("error failed", 0)],
            OutputHighlightPreset::Builtin,
            &[],
        );
        assert!(
            kw.iter()
                .any(|r| r.text == "error" && matches!(r.fg, TermColor::Idx(9)))
        );
    }

    #[test]
    fn builtin_preset_highlights_dates_and_times_without_grey_fragments() {
        // `date` output: month-day, time and year are yellow; no grey digits.
        let date = highlight_plain_output(
            vec![plain_run("Sat Aug 15 10:44:22 AM CST 2026", 0)],
            OutputHighlightPreset::Builtin,
            &[],
        );
        assert!(
            date.iter()
                .any(|r| r.text.contains("Aug 15") && matches!(r.fg, TermColor::Idx(11))),
            "month-day should be yellow"
        );
        assert!(
            date.iter()
                .any(|r| r.text.contains("10:44:22") && matches!(r.fg, TermColor::Idx(11))),
            "clock time should be yellow"
        );
        assert!(
            date.iter()
                .any(|r| r.text == "2026" && matches!(r.fg, TermColor::Idx(11))),
            "year should be yellow"
        );
        assert!(
            date.iter().all(|r| !matches!(r.fg, TermColor::Idx(8))),
            "no grey fragment should appear in date output"
        );
    }

    #[test]
    fn builtin_preset_handles_sized_numbers_root_mount_and_prompt_path() {
        // 1.9G must be highlighted as a whole, not just the leading `1`
        // (regression: `\b\d+...\b` backtracks at `9G` because both are word
        // chars, leaving `.9` unstyled).
        // Sized number must be styled as one green span (e.g. `391M`), not a
        // bare `391` followed by an unstyled `M`.
        let sized = highlight_plain_output(
            vec![plain_run("tmpfs  391M  40M  351M  11% /run", 0)],
            OutputHighlightPreset::Builtin,
            &[],
        );
        assert!(
            sized
                .iter()
                .any(|r| r.text == "391M" && matches!(r.fg, TermColor::Idx(10))),
            "sized number 391M should be one green span"
        );
        assert!(
            sized.iter().all(|r| r.text != "391"),
            "391 must not be split off from its M unit"
        );
        // The full `df`-style line: the bare root `/` mount point is styled.
        let df = highlight_plain_output(
            vec![plain_run("/dev/mmcblk0p2  6.7G  3.2G  3.4G  48% /", 0)],
            OutputHighlightPreset::Builtin,
            &[],
        );
        assert!(
            df.iter()
                .any(|r| r.text == "6.7G" && matches!(r.fg, TermColor::Idx(10)))
        );
        assert!(
            df.iter()
                .any(|r| r.text.ends_with('/') && matches!(r.fg, TermColor::Idx(14))),
            "bare root mount point `/` should be styled as a path"
        );

        // Prompt path after a colon: `ne@fnnas:/vol1/1000/...` — the path
        // following the `:` must be styled magenta, distinct from mount points.
        let prompt = highlight_plain_output(
            vec![plain_run("ne@fnnas:/vol1/1000/Adguardhome/work/data$", 0)],
            OutputHighlightPreset::Builtin,
            &[],
        );
        assert!(
            prompt
                .iter()
                .any(|r| r.text.contains("/vol1/1000") && matches!(r.fg, TermColor::Idx(13))),
            "prompt path after colon should be styled magenta"
        );
    }
}

#[cfg(test)]
mod font_choice_tests {
    use super::{FontEntry, family_from_label, font_choices};

    #[test]
    fn label_strips_indent_and_rejects_headers() {
        // Family rows carry a two-space indent under their group header.
        assert_eq!(
            family_from_label("  JetBrains Mono"),
            Some("JetBrains Mono")
        );
        assert_eq!(
            family_from_label("  Maple Mono Normal NL NF CN"),
            Some("Maple Mono Normal NL NF CN")
        );
        assert_eq!(family_from_label("  Consolas"), Some("Consolas"));
        // Group headers are not selectable.
        assert_eq!(family_from_label("▍内嵌字体"), None);
        assert_eq!(family_from_label("▍系统字体"), None);
        // Unknown/unlabelled values pass through unchanged (backward compat).
        assert_eq!(family_from_label("JetBrains Mono"), Some("JetBrains Mono"));
    }

    #[test]
    fn choices_list_built_embedded_external_system() {
        let external = vec!["外部字体 A".to_string(), "外部字体 B".to_string()];
        let (labels, entries) = font_choices(&external, true);
        // Grouped layout: header, then indented families under it.
        assert!(matches!(&entries[0], FontEntry::Header("内嵌字体")));
        assert!(labels[0].as_str().starts_with("▍"));
        assert!(matches!(&entries[1], FontEntry::Family(f) if f == "JetBrains Mono"));
        assert!(matches!(&entries[2], FontEntry::Family(f) if f == "Meatshell Mono"));
        assert!(matches!(&entries[3], FontEntry::Header("外置字体")));
        assert!(matches!(&entries[4], FontEntry::Family(f) if f == "外部字体 A"));
        // The last header is the system group.
        let last_header = entries
            .iter()
            .rev()
            .find_map(|e| match e {
                FontEntry::Header(h) => Some(*h),
                _ => None,
            })
            .expect("system header must exist");
        assert_eq!(last_header, "系统字体");
        // Every family row strips back to its bare family name.
        for (label, entry) in labels.iter().zip(entries.iter()) {
            if let FontEntry::Family(f) = entry {
                assert_eq!(family_from_label(label), Some(f.as_str()));
            } else {
                assert!(
                    family_from_label(label).is_none(),
                    "headers are not selectable"
                );
            }
        }
    }
}


