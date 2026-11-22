use super::*;


/// 渲染探测进程的入口（`--probe-renderer=<mode>`）。见 `window::run_renderer_probe`。
pub fn run_renderer_probe(mode: &str) -> Result<()> {
    window::run_renderer_probe(mode)
}

/// 曲线跨度下拉的显示文案：中文用自然单位，英文用紧凑缩写。
fn span_labels() -> slint::ModelRc<slint::SharedString> {
    let zh = !crate::i18n::is_en();
    let labels: Vec<slint::SharedString> = if zh {
        vec!["15 秒", "1 分钟", "5 分钟", "30 分钟", "1 小时", "6 小时"]
    } else {
        vec!["15s", "1m", "5m", "30m", "1h", "6h"]
    }
    .into_iter()
    .map(slint::SharedString::from)
    .collect();
    slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(labels)))
}

pub fn run() -> Result<()> {
    // 启动阶段日志只走 stderr：文件层是 warn，release 又没有控制台。
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        pid = std::process::id(),
        "startup: run() entered"
    );
    // 自更新留下的旧 exe（Windows 上运行中删不掉，只能改名）在这里清掉。
    self_updater::cleanup_leftover_binaries();

    // Load the renderer preference before creating any Slint window. Reuse the
    // same store for the rest of the app so startup does not read the config
    // twice merely to select a backend (#280).
    let config = ConfigStore::load().context("failed to load config")?;

    // 更新器网络配置（代理开关 / 代理地址）先落一份全局快照：检查更新在后台线程里跑，
    // 拿 store 不方便，直接读这里。证书链恒用系统根证书，不需要配置。
    update_net::set_current(update_net::NetSettings {
        enabled: config.update_proxy_enabled(),
        proxy: config.update_proxy().to_string(),
    });

    // 内存回收策略是启动期快照：开关重启生效，运行期不再读配置。
    super::mem::set_aggressive(config.aggressive_memory_reclaim());
    tracing::info!(
        aggressive = config.aggressive_memory_reclaim(),
        "startup: memory reclaim policy"
    );

    // 三平台的"自动"：探测一次（子进程真渲染一帧，约一秒），把结论**固化进配置** ——
    // 有 GPU 就存 GPU 档（Windows / Linux 是 `wgpu`，macOS 是 `femtovg-wgpu`），没有就存
    // `software`；之后每次启动直接读配置，不再探测。想重新探测（换机器 / 装了显卡驱动）
    // 就在设置里再选一次"自动"。
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    let config = window::resolve_auto_renderer_mode(config);

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
    tracing::info!(renderer = config.renderer_mode(), "startup: renderer resolved");
    // The window must exist before the context so the UI models can be wired
    // to it inside AppContext::new.
    let window = AppWindow::new().context("failed to build Slint window")?;
    tracing::info!("startup: main window created");
    // 子窗口关闭后要把键盘焦点交还主窗口。
    sub_windows::watch_main_window(window.as_weak());
    // Wrapped in `Rc` so the same context can be handed to the `wire_*`
    // functions instead of each one re-declaring its own field list.
    let ctx = Rc::new(AppContext::new(config, window.clone_strong())?);
    // Only the roots still referenced by `run()` itself are re-exported as
    // locals; after stage A4 the settings seeding and every `wire_*` function
    // take `&ctx` directly.
    let store = ctx.store.clone();
    let handles = ctx.handles.clone();
    let sftp_handles = ctx.sftp_handles.clone();
    let bufs = ctx.bufs.clone();
    let window_size_tracking_ready = ctx.window_size_tracking_ready.clone();
    let pending_window_size_restore = ctx.pending_window_size_restore.clone();
    let tab_statuses = ctx.tab_statuses.clone();
    let tabs_model = ctx.tabs_model.clone();

    // 建窗（`window.run()` 内）之前先把配置里的尺寸与位置定下来：Slint 会把它们写进 winit
    // 的 WindowAttributes，窗口第一次出现就是最终几何，不再有"先大后小 + 位移"的跳变。
    let pre_placed = {
        let s = store.borrow();
        let (w, h) = s.window_size();
        drop(s);
        (w > 0.0 && h > 0.0).then(|| place_main_window_before_show(&window, (w, h)))
    };

    // --- Build window + models ------------------------------------------
    // Set the Wayland app_id / X11 WM_CLASS *before* the window is created so
    // the Linux desktop shell can match the running window to the installed
    // `rudder.desktop` entry and show our icon in the dock/taskbar.  (On
    // Windows the icon comes from the embedded .ico, so this is a no-op there.)
    let _ = slint::set_xdg_app_id("rudder");

    // Show the crate version (from Cargo.toml at compile time) in the About dialog.
    window.set_app_version(env!("CARGO_PKG_VERSION").into());

    // Set the window icon from the PNG embedded in the binary so the dock
    // shows the correct icon even without a system-installed .desktop entry
    // (e.g. AppImage without AppImageLauncher, or plain binary in ~/bin).
    #[cfg(target_os = "linux")]
    set_window_icon(&window);

    // 窗口默认无边框 + 自绘标题栏（#119）；macOS 保留原生装饰与红绿灯，
    // 顶部留白条由 `titlebar-inset` 让出，双击仍走 `win-zoom-toggle` 兜底。
    #[cfg(target_os = "macos")]
    window.set_custom_titlebar(false);

    // --- Detachable process monitor window (#23) -----------------------------
    // The process table is its own top-level OS window so it can be dragged
    // outside the main window (or onto a second monitor). Both windows render
    // the *same* VecModel, so the table stays live wherever it's parked; closing
    // it just hides it, so reopening is instant.
    let proc_rows_model: Rc<VecModel<ProcRow>> = Rc::new(VecModel::default());
    window.set_proc_list(ModelRc::from(proc_rows_model.clone()));
    // 进程窗也按需创建：行模型留在主窗口，组件树与第二套 wgpu 设备首次点击才建。
    {
        let win_weak = window.as_weak();
        let rows = proc_rows_model.clone();
        let proc_ctx = ctx.clone();
        window.on_open_processes(move || {
            if let Some(main) = win_weak.upgrade() {
                sub_windows::show_process(&main, &rows, &proc_ctx);
            }
        });
    }

    // ── 设置窗口（独立 root 窗口，原生标题栏）───────────────────────────────
    //
    // 首次打开才创建：设置窗含 TerminalPage 整棵树，启动期不需要。关闭时销毁，
    // 下次打开重建；`wire_settings_window` 在工厂里接一次回调。
    {
        let main = window.as_weak();
        window.on_toggle_interface_settings(move |open: bool| {
            let Some(m) = main.upgrade() else { return };
            if open {
                match sub_windows::ensure_settings(&m) {
                    Ok(sw) => {
                        settings_window::open_settings_window(&m, &sw);
                        m.set_interface_open(true);
                    }
                    Err(err) => {
                        m.set_interface_open(false);
                        // 复用连接状态弹窗做一次性提示；真实的连接提示正在显示时不要抢占。
                        if !m.get_conn_popup_open() {
                            m.set_conn_popup_host(
                                crate::i18n::t("无法打开“设置”窗口", "Could not open Settings")
                                    .into(),
                            );
                            m.set_conn_popup_detail(err.into());
                            m.set_conn_popup_phase(3);
                            m.set_conn_popup_open(true);
                        }
                    }
                }
            } else {
                sub_windows::release_settings();
                m.set_interface_open(false);
            }
        });
    }

    settings_ui::seed_settings(&window, &ctx);
    window.set_span_labels(span_labels());


    // --- Wire callbacks --------------------------------------------------
    session_callbacks::wire_session_callbacks(&window, &ctx);

    // Docker 页：进入页面或点刷新时，用活动会话（或最近的存活会话）跑一次 `docker ps`。
    {
        let weak = window.as_weak();
        let docker_handles = handles.clone();
        let docker_runtime = ctx.runtime.clone();
        let docker_tabs = tabs_model.clone();
        let docker_statuses = tab_statuses.clone();
        let docker_store = store.clone();
        window.on_refresh_docker(move || {
            if let Some(w) = weak.upgrade() {
                super::docker::refresh(
                    &w,
                    &docker_runtime,
                    &docker_handles,
                    &docker_tabs,
                    &docker_statuses,
                    &docker_store.borrow(),
                );
            }
        });

        let now_weak = window.as_weak();
        let now_runtime = ctx.runtime.clone();
        let now_handles = handles.clone();
        let now_tabs = tabs_model.clone();
        let now_statuses = tab_statuses.clone();
        let now_store = store.clone();
        window.on_refresh_docker_now(move || {
            if let Some(w) = now_weak.upgrade() {
                super::docker::refresh_forced(
                    &w,
                    &now_runtime,
                    &now_handles,
                    &now_tabs,
                    &now_statuses,
                    &now_store.borrow(),
                );
            }
        });

        let act_weak = window.as_weak();
        let act_runtime = ctx.runtime.clone();
        let act_handles = handles.clone();
        window.on_docker_action(move |id, kind| {
            if let Some(w) = act_weak.upgrade() {
                let key = w.get_docker_active_host().to_string();
                super::docker::act(&w, &act_runtime, &act_handles, &key, id.as_str(), kind);
            }
        });

        let logs_weak = window.as_weak();
        let logs_runtime = ctx.runtime.clone();
        let logs_handles = handles.clone();
        window.on_docker_open_logs(move |id| {
            if let Some(w) = logs_weak.upgrade() {
                let key = w.get_docker_active_host().to_string();
                super::docker::fetch_logs(&w, &logs_runtime, &logs_handles, &key, id.as_str());
            }
        });

        let terminal_weak = window.as_weak();
        window.on_docker_open_terminal(move |id| {
            if let Some(w) = terminal_weak.upgrade() {
                let key = w.get_docker_active_host().to_string();
                super::docker::open_terminal(&w, &key, id.as_str());
            }
        });

        let filter_weak = window.as_weak();
        window.on_docker_filter_changed(move |text| {
            if let Some(w) = filter_weak.upgrade() {
                w.set_docker_filter(text.clone());
                super::docker::set_filter(&w, text.as_str());
            }
        });

        let switch_weak = window.as_weak();
        let switch_runtime = ctx.runtime.clone();
        let switch_handles = handles.clone();
        window.on_docker_switch_host(move |key| {
            if let Some(w) = switch_weak.upgrade() {
                super::docker::select_host(
                    &w,
                    &switch_runtime,
                    &switch_handles,
                    key.as_str(),
                );
            }
        });

        let service_weak = window.as_weak();
        let service_runtime = ctx.runtime.clone();
        let service_handles = handles.clone();
        window.on_docker_toggle_service(move |start| {
            if let Some(w) = service_weak.upgrade() {
                let key = w.get_docker_active_host().to_string();
                if !key.is_empty() {
                    super::docker::toggle_service(
                        &w,
                        &service_runtime,
                        &service_handles,
                        &key,
                        start,
                    );
                }
            }
        });

        let close_host_weak = window.as_weak();
        let close_host_store = store.clone();
        window.on_docker_close_host(move |key| {
            if let Some(w) = close_host_weak.upgrade() {
                super::docker::close_host(&w, &close_host_store.borrow(), key.as_str());
            }
        });

        let welcome_weak = window.as_weak();
        let welcome_store = store.clone();
        window.on_docker_select_welcome(move || {
            if let Some(w) = welcome_weak.upgrade() {
                super::docker::goto_welcome(&w, &welcome_store.borrow());
            }
        });

        let open_saved_weak = window.as_weak();
        let open_saved_store = store.clone();
        let open_saved_runtime = ctx.runtime.clone();
        let open_saved_handles = handles.clone();
        let open_saved_tabs = tabs_model.clone();
        let open_saved_statuses = tab_statuses.clone();
        window.on_docker_open_saved_session(move |id| {
            if let Some(w) = open_saved_weak.upgrade() {
                super::docker::open_saved_session(
                    &w,
                    &open_saved_store.borrow(),
                    &open_saved_runtime,
                    &open_saved_handles,
                    &open_saved_tabs,
                    &open_saved_statuses,
                    id.as_str(),
                );
            }
        });

        let group_weak = window.as_weak();
        let group_store = store.clone();
        window.on_docker_toggle_group(move |group| {
            if let Some(w) = group_weak.upgrade() {
                super::docker::toggle_group(&w, &mut group_store.borrow_mut(), group.as_str());
            }
        });

        let blocker_terminal_weak = window.as_weak();
        window.on_docker_blocker_terminal(move || {
            if let Some(w) = blocker_terminal_weak.upgrade() {
                let host = w.get_docker_blocker_host().to_string();
                super::docker::focus_host_terminal(&w, &host);
            }
        });

        let blocker_settings_weak = window.as_weak();
        window.on_docker_blocker_settings(move || {
            if let Some(w) = blocker_settings_weak.upgrade()
                && let Ok(sw) = sub_windows::ensure_settings(&w)
            {
                sw.set_ifd_page("docker".into());
                settings_window::open_settings_window(&w, &sw);
                w.set_interface_open(true);
            }
        });

        let blocker_retry_weak = window.as_weak();
        let blocker_retry_runtime = ctx.runtime.clone();
        let blocker_retry_handles = handles.clone();
        let blocker_retry_tabs = tabs_model.clone();
        let blocker_retry_statuses = tab_statuses.clone();
        let blocker_retry_store = store.clone();
        window.on_docker_blocker_retry(move || {
            if let Some(w) = blocker_retry_weak.upgrade() {
                super::docker::retry_blocker(
                    &w,
                    &blocker_retry_runtime,
                    &blocker_retry_handles,
                    &blocker_retry_tabs,
                    &blocker_retry_statuses,
                    &blocker_retry_store.borrow(),
                );
            }
        });

        let blocker_dismiss_weak = window.as_weak();
        window.on_docker_blocker_dismiss(move || {
            if let Some(w) = blocker_dismiss_weak.upgrade() {
                super::docker::dismiss_blocker(&w);
            }
        });

        let cred_open_weak = window.as_weak();
        let cred_open_store = store.clone();
        window.on_docker_configure_credentials(move |session_id| {
            if let Some(w) = cred_open_weak.upgrade() {
                super::docker::open_credentials(&w, &cred_open_store.borrow(), session_id.as_str());
            }
        });

        let cred_save_weak = window.as_weak();
        let cred_save_store = store.clone();
        let cred_save_runtime = ctx.runtime.clone();
        let cred_save_handles = handles.clone();
        let cred_save_tabs = tabs_model.clone();
        let cred_save_statuses = tab_statuses.clone();
        window.on_docker_save_credentials(move |session_id, mode, user, password| {
            if let Some(w) = cred_save_weak.upgrade() {
                let saved = {
                    let mut store = cred_save_store.borrow_mut();
                    super::docker::save_credentials(
                        &w,
                        &mut store,
                        session_id.as_str(),
                        mode.as_str(),
                        user.as_str(),
                        password.as_str(),
                    )
                };
                if saved {
                    super::docker::refresh_forced(
                        &w,
                        &cred_save_runtime,
                        &cred_save_handles,
                        &cred_save_tabs,
                        &cred_save_statuses,
                        &cred_save_store.borrow(),
                    );
                }
            }
        });

        let cred_test_weak = window.as_weak();
        let cred_test_store = store.clone();
        let cred_test_runtime = ctx.runtime.clone();
        let cred_test_handles = handles.clone();
        let cred_test_statuses = tab_statuses.clone();
        window.on_docker_test_credentials(move |session_id, mode, user, password| {
            if let Some(w) = cred_test_weak.upgrade() {
                super::docker::test_credentials(
                    &w,
                    &cred_test_runtime,
                    &cred_test_handles,
                    &cred_test_statuses,
                    &cred_test_store.borrow(),
                    super::docker::CredentialInput {
                        session_id: session_id.as_str(),
                        mode: mode.as_str(),
                        user: user.as_str(),
                        password: password.as_str(),
                    },
                );
            }
        });

        let cred_clear_weak = window.as_weak();
        let cred_clear_store = store.clone();
        let cred_clear_runtime = ctx.runtime.clone();
        let cred_clear_handles = handles.clone();
        let cred_clear_tabs = tabs_model.clone();
        let cred_clear_statuses = tab_statuses.clone();
        window.on_docker_clear_credentials(move |session_id| {
            if let Some(w) = cred_clear_weak.upgrade() {
                {
                    let mut store = cred_clear_store.borrow_mut();
                    super::docker::clear_credentials(&w, &mut store, session_id.as_str());
                }
                super::docker::refresh_forced(
                    &w,
                    &cred_clear_runtime,
                    &cred_clear_handles,
                    &cred_clear_tabs,
                    &cred_clear_statuses,
                    &cred_clear_store.borrow(),
                );
            }
        });

        let cred_dismiss_weak = window.as_weak();
        window.on_docker_dismiss_credentials(move || {
            if let Some(w) = cred_dismiss_weak.upgrade() {
                super::docker::dismiss_credentials(&w);
            }
        });
    }

    // Recompute the sidebar whenever the active tab changes (fired from Slint's
    // `changed active-tab-id`).
    {
        let weak = window.as_weak();
        let statuses = tab_statuses.clone();
        let monitor_handles = handles.clone();
        let monitor_store = store.clone();
        let monitor_tab = Rc::new(Cell::new(String::new()));
        let monitor_all = Rc::new(Cell::new(false));
        let reclaim_bufs = bufs.clone();
        window.on_refresh_sidebar(move || {
            let Some(w) = weak.upgrade() else { return };
            // 活动标签可能变了：清掉 seq 缓存，强制下一趟走全量刷新，
            // 否则切回看过的标签会被 `stats_seq` 门控跳过、界面留着上一个标签的数据。
            super::sidebar::invalidate_sidebar_cache();
            // 远端监控默认只开活动标签：3 条 exec 通道（750ms/1.5s/2.5s）在非活动标签
            // 上纯属浪费。MCP 启用时保持全量 —— 它的工具可能读任意标签的状态。
            let active = w.get_active_tab_id().to_string();
            let all = monitor_store.borrow().mcp_enabled();
            if all != monitor_all.get() || monitor_tab.replace(active.clone()) != active {
                monitor_all.set(all);
                if let Ok(handles) = monitor_handles.try_borrow() {
                    for (id, handle) in handles.iter() {
                        handle.set_resource_monitoring(all || id == &active);
                    }
                } else {
                    // 句柄表正被别处借用：清掉记录，下一次 refresh 再试。
                    monitor_tab.set(String::new());
                }
            }
            // 每个会话按自己的两条曲线跨度决定采样间隔（任一要 15 秒 → 1 s）。
            if let (Ok(handles), Ok(map)) = (monitor_handles.try_borrow(), statuses.lock()) {
                for (id, handle) in handles.iter() {
                    let interval = map
                        .get(id)
                        .map(|st| {
                            super::resource_ui::base_step(
                                st.cpu_span_secs,
                                st.net_span_secs,
                                st.mem_span_secs,
                            )
                        })
                        .unwrap_or(2);
                    handle.set_sample_interval(interval);
                }
            }
            refresh_sidebar(&w, &statuses);
            // 所有「活动标签变化」都汇到这里：回收不可见标签的 span/emoji 模型。
            crate::app::terminal_ui::reclaim_offscreen_terminal_models(&w, &reclaim_bufs);
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
                s.save_logging();
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
                // 各分屏的标签条渲染的是 `tabs_model` 的**快照**（`app.slint` 的
                // `pane.tabs`）—— 只改顶层模型不会让它们跟着换语言，所以按 pane 再传播
                // 一遍（与 `session_event.rs` 更新连接状态时用的是同一套做法）。
                let panes = w.get_panes();
                if let Some(pm) = panes
                    .as_any()
                    .downcast_ref::<VecModel<crate::ui::PaneInfo>>()
                {
                    for pi in 0..pm.row_count() {
                        let Some(pane) = pm.row_data(pi) else { continue };
                        let Some(tm) =
                            pane.tabs.as_any().downcast_ref::<VecModel<crate::ui::TabInfo>>()
                        else {
                            continue;
                        };
                        for ti in 0..tm.row_count() {
                            if let Some(mut row) = tm.row_data(ti)
                                && row.id.as_str() == "welcome"
                            {
                                row.title_len = tab_title_len(t("新标签页", "New tab"));
                                row.title = t("新标签页", "New tab").into();
                                tm.set_row_data(ti, row);
                            }
                        }
                    }
                }
                w.set_lang_en(crate::i18n::is_en());
                w.set_span_labels(span_labels());
                w.invoke_refresh_sidebar();
            }
        });
    }

    // 侧栏 + Docker 概览 500ms 定时拉取：刷屏时 UI 线程被终端渲染占满，
    // 事件驱动的全量刷新会排在渲染任务后面（看起来卡顿）。采集只写
    // `TabStatus` 快照，这里按固定节奏应用；Docker 容器列表按 10s 节流。
    {
        let weak = window.as_weak();
        let statuses = tab_statuses.clone();
        let docker_last = Rc::new(Cell::new(std::time::Instant::now()));
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(500),
            move || {
                if let Some(w) = weak.upgrade() {
                    refresh_sidebar(&w, &statuses);
                    // Docker 页可见时：曲线跟快照每拍对齐侧栏，容器列表 10s 一次。
                    if w.get_nav_page() == 3 {
                        super::docker::tick_monitor(&w);
                        if docker_last.get().elapsed() >= std::time::Duration::from_secs(10) {
                            docker_last.set(std::time::Instant::now());
                            w.invoke_refresh_docker();
                        }
                    }
                }
            },
        );
        std::mem::forget(timer);
    }

    // Theme toggle: flip dark ↔ light, persist the preference, and re-render
    // every open terminal with the new ANSI palette so historical output is
    // also recoloured (not just new output).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_theme = bufs.clone();
        window.on_toggle_theme(move || {
            let Some(w) = weak.upgrade() else { return };
            let next_dark = !w.global::<Theme>().get_dark();
            // 等同于在「壁纸」里选「深色 / 浅色」：自定义壁纸也会被内置图覆盖，
            // 主题色与光标跟着切到新壁纸的互补色。
            crate::app::settings::appearance::apply_builtin_theme(
                &w,
                &store,
                &bufs_theme,
                &sub_windows::proc_weak(),
                next_dark,
            );
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_toggle_quick_sidebar(move || {
            let Some(w) = weak.upgrade() else { return };
            let next = !store.borrow().quick_commands_as_sidebar();
            crate::app::settings::layout::apply_quick_commands_as_sidebar(&w, &store, next);
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
        window.on_select_net_iface(move |iface: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let active = w.get_active_tab_id().to_string();
            if let Some(st) = statuses.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&active) {
                st.selected_iface = iface.to_string();
                st.net_hist.clear(); // 下一拍 push_ring 按当前跨度用首值重建
            }
            refresh_sidebar(&w, &statuses);
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
        s.save_logging();
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
                    s.save_logging();
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
    wire_update_check(&window, &ctx);

    wire_tab_callbacks(&window, &ctx);
    wire_sftp_callbacks(&window, &ctx);
    key_input::wire_key_input(&window, &ctx);

    // --- Window activity, for idle-CPU throttling (#127) ----------------
    // Idle terminals shouldn't burn CPU: pause the sampler when the window is
    // minimized / occluded, throttle it when it's merely unfocused, and stop the
    // cursor blink whenever the window isn't focused (mirrors what Tabby / Windows
    // Terminal do). The winit event handler below updates this; the blink reads
    // Theme.window-focused.
    let activity = Rc::new(std::cell::Cell::new(WinActivity::Active));
    // 工作集裁剪只在整个应用持续隐藏、没有会话与传输时执行；状态在 UI 线程读取。
    {
        let obs_weak = window.as_weak();
        let obs_handles = handles.clone();
        let obs_activity = activity.clone();
        let cleanup_weak = window.as_weak();
        super::mem::spawn_idle_observer(
            move || {
                let Some(win) = obs_weak.upgrade() else {
                    // 主窗口已销毁：按「仍有活动」处理，退出前不做裁剪。
                    return super::mem::TrimSnapshot {
                        activity: obs_activity.get(),
                        minimized: true,
                        has_visible_window: true,
                        live_sessions: true,
                        active_transfers: true,
                    };
                };
                super::mem::TrimSnapshot {
                    activity: obs_activity.get(),
                    // 与事件链互为兜底：`Resized(0,0)` 能到，但顺序不是契约。
                    minimized: win
                        .window()
                        .with_winit_window(|ww| ww.is_minimized())
                        .flatten()
                        .unwrap_or(false),
                    // 主窗口的可见性由 activity / minimized 表达。
                    has_visible_window: sub_windows::any_visible(),
                    live_sessions: !obs_handles.borrow().is_empty(),
                    active_transfers: win.get_has_active_transfers(),
                }
            },
            move || {
                if let Some(w) = cleanup_weak.upgrade() {
                    crate::app::terminal_ui::reclaim_all_terminal_models(&w);
                }
            },
        );
    }
    // 激进档：首帧与设置播种完成后再把工作集转入待机。
    super::mem::trim_after_startup();
    // Once the user confirms shutdown, every subsequent native/custom close
    // request must pass through without reopening the modal. Windows Installer
    // and Restart Manager may issue more than one close request while replacing
    // the executable (#267).
    let exit_confirmed = Rc::new(Cell::new(false));

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
        let ev_minimized = Rc::new(Cell::new(false));
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
                    tracing::info!("startup: first window event");
                    if let Some(win) = weak.upgrade() {
                        apply_window_chrome(win.window());
                    }
                }
                // Recompute window activity, push it to the shared cell, and update
                // Theme.window-focused (gates the cursor blink) (#127).
                //
                // `minimized` 同时参考 `is_minimized()`：`Resized(0,0)` 能到应用层，
                // 但 Windows 不发 `Occluded`，两个信号一起用更稳。
                let apply_activity = |focused: bool, minimized: bool, occluded: bool| {
                    let win = weak.upgrade();
                    let minimized = minimized
                        || win
                            .as_ref()
                            .and_then(|w| w.window().with_winit_window(|ww| ww.is_minimized()))
                            .flatten()
                            .unwrap_or(false);
                    let act = if minimized || occluded {
                        WinActivity::Hidden
                    } else if focused {
                        WinActivity::Active
                    } else {
                        WinActivity::Background
                    };
                    let prev = ev_activity.get();
                    ev_activity.set(act);
                    let was_minimized = ev_minimized.replace(minimized);
                    if let Some(win) = win {
                        win.set_window_focused(act == WinActivity::Active);
                        // 恢复分两路：activity 从 Hidden 出来，或最小化标记刚刚解除。
                        // 后者兜住「事件没送到但窗口确实被最小化」的情况。
                        let became_visible = (prev == WinActivity::Hidden
                            && act != WinActivity::Hidden)
                            || (was_minimized && !minimized);
                        if became_visible {
                            win.set_terminal_restore_cover(true);
                            let weak2 = weak.clone();
                            slint::Timer::single_shot(
                                std::time::Duration::from_millis(120),
                                move || {
                                    if let Some(w) = weak2.upgrade() {
                                        w.set_terminal_restore_cover(false);
                                        // 深度空闲可能清过渲染模型，恢复时补一次重建。
                                        w.invoke_refresh_sidebar();
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
                        // 非 Windows 平台只有这里能告诉我们落点（上游 0aeba62）；
                        // Windows 会忽略它、改问系统，因为 OLE 拖放期间指针事件被抑制。
                        if let Some(win) = weak.upgrade() {
                            handle_file_drop(&win, &sh, path.clone(), last_cursor_logical);
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
                        // 设置页现在是**独立窗口**（不叠在这个窗口里）→ 不再需要"设置打开时
                        // 别把滚轮喂给终端"的补丁：滚轮按指针位置由系统派发给对应窗口。
                        // 原先的 `macos_terminal_wheel_can_target_terminal(get_interface_open())`
                        // 守卫连同它的单测一起删掉了（否则设置开着时主窗口反而滚不动）。
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
                                    tracing::debug!(
                                        "[WINDOW_SIZE] skipped persisted-size restore on Wayland"
                                    );
                                } else if let Some(preferred) = ev_pending_window_size_restore.get()
                                    && let Some(target) =
                                        clamp_window_size_to_monitor(win.window(), Some(preferred))
                                {
                                    tracing::debug!(
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
                                tracing::debug!(
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
                                        tracing::debug!(
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
                                            tracing::debug!(
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
                        if let Some(win) = weak.upgrade() {
                            begin_shutdown(&win, &ev_store, &close_handles, &sh);
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
            let Some(w) = weak.upgrade() else { return };
            w.set_confirm_close_open(false);
            begin_shutdown(&w, &cc_store, &close_handles, &close_sftp_handles);
        });
    }

    wire_window_chrome(&window, &handles, &store, &exit_confirmed, &sftp_handles);

    // 尺寸已经在建窗前设好，事件驱动的恢复重试不必再跑：留着会在首帧再 request 一次同尺寸，
    // 多出一次 Resized 与重绘。门在这里直接关掉；只有配置里没有尺寸（首次运行）才留给重试。
    if pre_placed.is_some() {
        pending_window_size_restore.set(None);
        window_size_tracking_ready.set(true);
    }

    // 进程内 MCP server：配置里启用（或 debug 构建给了 RUDDER_MCP_DEV_TOKEN）就起监听。
    // UI 桥先注册 —— 工具调用随时可能进来；快照在启用但没令牌时会当场生成并落盘。
    {
        mcp_ui::install(&window, &ctx);
        let snapshot = {
            let mut store_ref = store.borrow_mut();
            crate::mcp::McpConfigSnapshot::from_store(&mut store_ref)
        };
        crate::mcp::set_context(ctx.runtime.handle(), mcp_ui::app_handles(&ctx));
        crate::mcp::start(
            ctx.runtime.handle(),
            snapshot,
            mcp_ui::app_handles(&ctx),
        );
    }

    let run_result = window.run();
    // 事件循环已退出：MCP 服务不再可能操作界面，先停服务、摘掉 UI 桥，再收尾。
    crate::mcp::stop();
    mcp_ui::uninstall();
    // 退出前把防抖窗口内未落盘的设置改动写出去：事件循环一停，挂起的改动就没机会落盘了。
    store.borrow_mut().flush_logging();
    if let Err(err) = run_result {
        // GPU 档起不来（首帧渲染失败 / 运行中设备丢失）时降级成软件渲染并重拉自己；
        // 重拉成功就安静退出，`None` 表示不用报错给上层。
        if let Some(fatal) = recover_render_failure(&store, err) {
            return Err(fatal);
        }
    }
    // 事件循环已退出、配置已落盘；在 window / ctx 析构与 CRT 收尾之前直接终止进程，
    // 避免 Runtime::drop 等 blocking 任务、以及 ExitProcess 在 DLL detach 阶段卡死。
    tracing::warn!("shutdown: event loop exited, terminating process");
    exit_process_now()
}
