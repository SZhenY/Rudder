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


/// Max bytes merged into one Output event before starting a fresh chunk (#209).
/// Keeps a single UI callback from spending hundreds of ms in vt100 ingest.
const OUTPUT_MERGE_BYTE_CAP: usize = 64 * 1024;

/// Output parsed between UI-flush checkpoints during sustained traffic.
const INGEST_FRAME_BUDGET: usize = 64 * 1024;


/// Do not deliberately pace a pump while a large unbounded-channel backlog is
/// already present. It catches up first, then paces the tail of the stream.
const PACED_LOCAL_BACKLOG_LIMIT: usize = 1024 * 1024;
const PACED_QUEUE_EVENT_LIMIT: usize = 256;

pub(crate) const INTERACTIVE_ECHO_WINDOW: std::time::Duration = std::time::Duration::from_millis(180);

pub(crate) fn term_buf(bufs: &TermBuffers, tab_id: &str) -> Option<TermBufferHandle> {
    bufs.lock().unwrap_or_else(|e| e.into_inner()).get(tab_id).cloned()
}

pub(crate) fn with_term_buf<R>(
    bufs: &TermBuffers,
    tab_id: &str,
    f: impl FnOnce(&mut TermBuffer) -> R,
) -> Option<R> {
    let h = term_buf(bufs, tab_id)?;
    let mut guard = h.lock().unwrap_or_else(|e| e.into_inner());
    Some(f(&mut guard))
}

fn ingest_terminal_output(bufs: &TermBuffers, tab_id: &str, chunk: &[u8]) -> Vec<u8> {
    if let Some(h) = term_buf(bufs, tab_id) {
        h.lock().unwrap_or_else(|e| e.into_inner()).ingest(chunk)
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
    format_size, spawn_session,
};
#[cfg(windows)]
use crate::terminal::c0_letter_key_down;
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
mod key_input;
mod session_callbacks;
mod pane_layout;
mod fonts_ui;
mod updater;
mod sampler;
mod window_chrome;
use window_chrome::wire_window_chrome;
use sampler::spawn_system_sampler;
use updater::wire_update_check;
pub(crate) use fonts_ui::{FontEntry, family_from_label, font_choices, resolve_ui_font_family, term_font_covers_cjk};
pub(crate) use pane_layout::{
    drag_target, refresh_panes, save_layout, update_terminal_row,
};
pub(crate) use window_geometry::{
    center_window, handle_file_drop, handle_macos_terminal_wheel,
    macos_terminal_wheel_can_target_terminal,
};

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
use self::webdav::*;
use self::window::*;

fn tab_title_len(title: &str) -> i32 {
    title
        .chars()
        .map(|ch| if ch.is_ascii() { 1usize } else { 2usize })
        .sum::<usize>()
        .min(i32::MAX as usize) as i32
}

pub(crate) fn should_block_close(exit_confirmed: bool, has_live_sessions: bool) -> bool {
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
/// App-wide shared state roots, created once at startup and threaded through
/// `run()` (and eventually the wire-* callbacks).  Centralising the roots here
/// gives them a home instead of ~40 scattered locals in `run()` (refactor
/// plan stage C, step 1: create + initialise in one constructor).
pub(crate) struct AppContext {
    pub(crate) runtime: Arc<Runtime>,
    pub(crate) store: Rc<RefCell<ConfigStore>>,
    /// Per-tab SSH handles (shell only; lives on Slint thread via Rc).
    pub(crate) handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    /// Per-tab SFTP handles — Arc<Mutex> so the event-pump OS thread and the
    /// Slint UI thread can both post SftpCommands.
    pub(crate) sftp_handles: SftpHandles,
    /// Per-tab cwd the SFTP panel last followed (see SftpLastCwd).
    pub(crate) sftp_last_cwd: SftpLastCwd,
    /// Per-tab vt100 parsers + history logs (Arc<Mutex> so they can be cloned
    /// into the thread that pumps session events into invoke_from_event_loop).
    pub(crate) bufs: TermBuffers,
    pub(crate) render_gates: RenderGates,
    /// Last-known terminal pixel dimensions (80×24 SSH minimum default),
    /// shared so on_connect_session can pass a sensible initial PTY size to
    /// spawn_session before the first resize callback fires.
    pub(crate) last_term_size: Arc<Mutex<(u32, u32)>>,
    /// Startup window-size tracking: the native window's preferred size is
    /// applied while it is created; those Resized events must not overwrite the
    /// persisted size before restoration (#278).
    pub(crate) window_size_tracking_ready: Rc<Cell<bool>>,
    pub(crate) pending_window_size_restore: Rc<Cell<Option<(f32, f32)>>>,
    /// Per-tab connection status + remote resources (#23).
    pub(crate) tab_statuses: TabStatuses,
    /// Display-name overrides for open session tabs (Rename): tab-id → override.
    pub(crate) tab_titles: Rc<RefCell<HashMap<String, String>>>,
    pub(crate) tabs_model: Rc<VecModel<TabInfo>>,
    pub(crate) terminals_model: Rc<VecModel<TerminalState>>,
    pub(crate) layout: Rc<RefCell<crate::layout::Layout>>,
    pub(crate) content_size: Rc<std::cell::Cell<(f32, f32)>>,
    pub(crate) panes_model: Rc<VecModel<PaneInfo>>,
    pub(crate) splitters_model: Rc<VecModel<SplitterInfo>>,
}

impl AppContext {
    pub(crate) fn new(config: ConfigStore, window: AppWindow) -> anyhow::Result<Self> {
        let runtime = Arc::new(Runtime::new().context("failed to start tokio runtime")?);
        let store = Rc::new(RefCell::new(config));
        // Reachable from the Slint-thread event handler for recording terminal
        // commands into history (#113).
        HISTORY_STORE.with(|s| *s.borrow_mut() = Some(store.clone()));

        let handles: Rc<RefCell<HashMap<String, SessionHandle>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let sftp_handles: SftpHandles = Arc::new(Mutex::new(HashMap::new()));
        let sftp_last_cwd: SftpLastCwd = Arc::new(Mutex::new(HashMap::new()));
        let bufs: TermBuffers = Arc::new(Mutex::new(HashMap::new()));
        let render_gates: RenderGates = Arc::new(Mutex::new(HashMap::new()));
        let last_term_size: Arc<Mutex<(u32, u32)>> = Arc::new(Mutex::new((80, 24)));
        let window_size_tracking_ready = Rc::new(Cell::new(false));
        let pending_window_size_restore = Rc::new(Cell::new(None::<(f32, f32)>));
        let tab_statuses: TabStatuses = Arc::new(Mutex::new(HashMap::new()));
        let tab_titles: Rc<RefCell<HashMap<String, String>>> = Rc::new(RefCell::new(HashMap::new()));

        // --- UI models (window is created before the context) -------------
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

        // Split-pane layout tree (v0.5). Starts as a single pane owning the
        // welcome tab; in welcome-as-sidebar mode the session list lives in a
        // left panel, so the layout starts empty.
        let welcome_sidebar = store.borrow().welcome_as_sidebar();
        let layout: Rc<RefCell<crate::layout::Layout>> = Rc::new(RefCell::new(if welcome_sidebar {
            crate::layout::Layout::new(Vec::new(), String::new())
        } else {
            crate::layout::Layout::new(vec!["welcome".into()], "welcome".into())
        }));
        let content_size: Rc<std::cell::Cell<(f32, f32)>> =
            Rc::new(std::cell::Cell::new((1200.0, 800.0)));
        let panes_model: Rc<VecModel<PaneInfo>> = Rc::new(VecModel::default());
        window.set_panes(ModelRc::from(panes_model.clone()));
        let splitters_model: Rc<VecModel<SplitterInfo>> = Rc::new(VecModel::default());
        window.set_splitters(ModelRc::from(splitters_model.clone()));
        crate::app::pane_layout::refresh_panes(
            &window,
            &layout.borrow(),
            content_size.get(),
            &tabs_model,
            &panes_model,
            &splitters_model,
        );

        Ok(Self {
            runtime,
            store,
            handles,
            sftp_handles,
            sftp_last_cwd,
            bufs,
            render_gates,
            last_term_size,
            window_size_tracking_ready,
            pending_window_size_restore,
            tab_statuses,
            tab_titles,
            tabs_model,
            terminals_model,
            layout,
            content_size,
            panes_model,
            splitters_model,
        })
    }
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
    // Stage C: the roots are built by AppContext::new and re-exported as
    // locals here so the rest of run() (and its closures) keep their names;
    // a later step can thread `ctx` directly.
    // The window must exist before the context so the UI models can be wired
    // to it inside AppContext::new.
    let window = AppWindow::new().context("failed to build Slint window")?;
    let ctx = AppContext::new(config, window.clone_strong())?;
    let runtime = ctx.runtime.clone();
    let store = ctx.store.clone();
    let handles = ctx.handles.clone();
    let sftp_handles = ctx.sftp_handles.clone();
    let sftp_last_cwd = ctx.sftp_last_cwd.clone();
    let bufs = ctx.bufs.clone();
    let render_gates = ctx.render_gates.clone();
    let last_term_size = ctx.last_term_size.clone();
    let window_size_tracking_ready = ctx.window_size_tracking_ready.clone();
    let pending_window_size_restore = ctx.pending_window_size_restore.clone();
    let tab_statuses = ctx.tab_statuses.clone();
    let tab_titles = ctx.tab_titles.clone();
    let tabs_model = ctx.tabs_model.clone();
    let terminals_model = ctx.terminals_model.clone();
    let layout = ctx.layout.clone();
    let content_size = ctx.content_size.clone();
    let panes_model = ctx.panes_model.clone();
    let splitters_model = ctx.splitters_model.clone();

    // --- Build window + models ------------------------------------------
    // Set the Wayland app_id / X11 WM_CLASS *before* the window is created so
    // the Linux desktop shell can match the running window to the installed
    // `rudder.desktop` entry and show our icon in the dock/taskbar.  (On
    // Windows the icon comes from the embedded .ico, so this is a no-op there.)
    let _ = slint::set_xdg_app_id("rudder");

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
    let fonts_dirs = crate::fonts::external_fonts_dirs();
    tracing::info!(
        "external fonts dirs: {}",
        fonts_dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")
    );
    let external_fonts = crate::fonts::load_external_fonts(&fonts_dirs);
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
        crate::webdav::set_webdav_cert_pin(s.webdav_cert_pin());
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
            for buffer in bufs.lock().unwrap_or_else(|e| e.into_inner()).values() {
                buffer.lock().unwrap_or_else(|e| e.into_inner()).json_format_output = enabled;
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
                .set_title(t("选择壁纸", "Choose wallpaper"))
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

    // The latest local sample + the local machine's network history (bottom
    // sparkline); the per-tab `tab_statuses` now lives in AppContext.
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
                    let states = statuses.lock().unwrap_or_else(|e| e.into_inner());
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
    session_callbacks::wire_session_callbacks(session_callbacks::SessionWireCtx {
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
            if let Some(st) = statuses.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&active) {
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
    key_input::wire_key_input(
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
                            _slint_window.dispatch_event(
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
                        _slint_window.with_winit_window(|window| window.set_ime_allowed(true));
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
                            _slint_window.with_winit_window(|window| window.set_ime_allowed(true));

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

    wire_window_chrome(&window, &handles, &store, &exit_confirmed);

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

/// Re-sync the session list honouring the current Quick Connect search box
/// contents (upstream 547b588). All mutation paths funnel through this so the
/// filtered view stays live.
pub(crate) fn sync_sessions_for_window(
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
pub(crate) fn convert_eol() -> bool {
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
    use crate::app::port_forward::{blank_forward_draft, validated_port_forwards};

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
    fn lists_and_filters_commands_newest_last() {
        let history = vec![
            "git status".to_string(),
            "cargo check".to_string(),
            "git log".to_string(),
        ];

        let all: Vec<String> = history_view_rows(&history, "")
            .into_iter()
            .map(Into::into)
            .collect();
        assert_eq!(all, ["git status", "cargo check", "git log"]);

        let filtered: Vec<String> = history_view_rows(&history, "GIT")
            .into_iter()
            .map(Into::into)
            .collect();
        assert_eq!(filtered, ["git status", "git log"]);
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
pub(crate) fn clipboard_set_text(text: String) {
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

pub(crate) fn split_proxy(url: &str) -> (String, String) {
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
    use crate::app::key_input::{parse_tunnel_forward, redact_key};

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
        assert!(!paste_requires_large_review(&["line"; 12].join("\r\n")));
        assert!(paste_requires_large_review(&["line"; 13].join("\r\n")));
    }

    #[test]
    fn confirmed_exit_never_reopens_close_prompt() {
        assert!(should_block_close(false, true));
        assert!(!should_block_close(false, false));
        assert!(!should_block_close(true, true));
        assert!(!should_block_close(true, false));
    }

    #[test]
    fn redact_key_hides_secrets_but_keeps_control_codes() {
        // Empty key.
        assert_eq!(redact_key(""), "(empty)");
        // Control codes are shown as U+XXXX, printable chars are redacted.
        assert_eq!(redact_key("\x03"), "U+0003");
        assert_eq!(redact_key("secret"), "<6 printable redacted>");
        // Mixed: only the control bytes leak their codepoints.
        let mixed = redact_key("a\x1b[b\x7f");
        assert!(mixed.contains("U+001B"));
        assert!(mixed.contains("U+007F"));
        assert!(mixed.contains("printable redacted"));
        // Multi-byte printable chars count individually.
        assert_eq!(redact_key("密码"), "<2 printable redacted>");
    }

    #[test]
    fn parse_tunnel_forward_validates_fields() {
        // Local forward: all fields parsed + trimmed.
        let f = parse_tunnel_forward(
            "local", "  web  ", " 127.0.0.1 ", "8080", "db.internal", "5432",
        )
        .unwrap();
        assert_eq!(f.kind, "local");
        assert_eq!(f.name, "web");
        assert_eq!(f.bind_addr, "127.0.0.1");
        assert_eq!(f.bind_port, 8080);
        assert_eq!(f.host, "db.internal");
        assert_eq!(f.host_port, 5432);

        // Dynamic: host port is forced to 0 (SOCKS).
        let d = parse_tunnel_forward(
            "dynamic", "socks", "0.0.0.0", "1080", "", "ignored",
        )
        .unwrap();
        assert_eq!(d.kind, "dynamic");
        assert_eq!(d.host_port, 0);

        // Unknown kind → None.
        assert!(parse_tunnel_forward("remote", "x", "h", "1", "h", "2").is_none());
        // Unparseable ports → None.
        assert!(parse_tunnel_forward("local", "x", "h", "abc", "h", "2").is_none());
        assert!(parse_tunnel_forward("local", "x", "h", "8080", "h", "x").is_none());
        // Port out of u16 range → None.
        assert!(parse_tunnel_forward("local", "x", "h", "65536", "h", "2").is_none());
    }

    #[test]
    fn tab_title_len_counts_wide_chars_as_two() {
        assert_eq!(tab_title_len(""), 0);
        assert_eq!(tab_title_len("abc"), 3);
        assert_eq!(tab_title_len("中文"), 4);
        assert_eq!(tab_title_len("a中文b"), 1 + 4 + 1);
    }

    #[test]
    fn is_special_partition_filters_pseudo_fs() {
        assert!(is_special_partition("/proc"));
        assert!(is_special_partition("/proc/cpuinfo"));
        assert!(is_special_partition("/sys"));
        assert!(is_special_partition("/dev/sda1"));
        assert!(is_special_partition("/run"));
        assert!(is_special_partition("/tmp"));
        assert!(is_special_partition("/snap/core20/1234"));
        assert!(is_special_partition("/var/snap/foo"));
        assert!(is_special_partition("/boot/efi"));
        // Real user mounts stay visible.
        assert!(!is_special_partition("/"));
        assert!(!is_special_partition("/home"));
        assert!(!is_special_partition("/mnt/data"));
        assert!(!is_special_partition("/Users/zheny"));
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


