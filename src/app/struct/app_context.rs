use super::*;

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
    /// Saved-session list backing the welcome page and the quick-connect search.
    pub(crate) sessions_model: Rc<VecModel<SessionInfo>>,
    /// Latest local-machine sample + its network history sparkline.
    /// Live "SFTP follows the terminal's cd" flag; the shell event pumps read it
    /// on every CwdChanged, so toggling it applies to open sessions too.
    pub(crate) sftp_follow_cd: Arc<std::sync::atomic::AtomicBool>,
}

impl AppContext {
    pub(crate) fn new(config: ConfigStore, window: AppWindow) -> anyhow::Result<Self> {
        // SSH / SFTP / MCP 都是 I/O 型任务，2 个 worker 足够；默认按核数起线程会
        // 多预留数份 2 MiB 栈。真遇到并发传输瓶颈再调大。
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .context("failed to start tokio runtime")?,
        );
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
        // welcome tab.
        let layout: Rc<RefCell<crate::layout::Layout>> = Rc::new(RefCell::new(
            crate::layout::Layout::new(vec!["welcome".into()], "welcome".into()),
        ));
        let content_size: Rc<std::cell::Cell<(f32, f32)>> =
            Rc::new(std::cell::Cell::new((1200.0, 800.0)));
        let panes_model: Rc<VecModel<PaneInfo>> = Rc::new(VecModel::default());
        window.set_panes(ModelRc::from(panes_model.clone()));
        let splitters_model: Rc<VecModel<SplitterInfo>> = Rc::new(VecModel::default());
        window.set_splitters(ModelRc::from(splitters_model.clone()));
        crate::app::pane_layout::refresh_panes(
            &window,
            &layout,
            content_size.get(),
            &tabs_model,
            &panes_model,
            &splitters_model,
        );

        // Saved-session list. Seeded here rather than later in `run()` so every
        // consumer of the context sees a populated model.
        let sessions_model: Rc<VecModel<SessionInfo>> = Rc::new(VecModel::default());
        window.set_sessions(ModelRc::from(sessions_model.clone()));
        sync_sessions_for_window(&window.as_weak(), &store.borrow(), &sessions_model);

        let sftp_follow_cd = Arc::new(std::sync::atomic::AtomicBool::new(
            store.borrow().sftp_follow_cd(),
        ));

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
            sessions_model,
            sftp_follow_cd,
        })
    }
}
