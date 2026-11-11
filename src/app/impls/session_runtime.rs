use super::render_tickets::{request_tab_render, wait_for_ui_flush};
use super::*;

/// 该会话要用的跳板机 id；不是 SSH / 没配跳板机 / 自引用 / 全空白 -> None。
///
/// 抽成纯函数的原因：判定错了是**安全问题** —— 本该经跳板机的会话会静默直连目标机
/// （审计链路与网络边界同时失效）；反过来自引用没拦住则会拿自己当跳板。
///
/// 返回值**已 trim**：调用方拿它去 `store.get(id)` 精确查表，而配置里可能留着带空白的
/// id（早期版本写进去的）。以前是判空用 `trim()`、查表用原始串 —— `" b "` 匹配不到任何
/// 会话，于是静默直连目标机。自引用那条也一并对齐（`" a "` 同样要拦住）。
fn jump_target(session: &Session) -> Option<&str> {
    let id = session.jump_session_id.trim();
    if session.kind != SessionKind::Ssh || id.is_empty() || id == session.id.trim() {
        return None;
    }
    Some(id)
}

pub(crate) fn resolve_jump(store: &Rc<RefCell<ConfigStore>>, session: &Session) -> Option<Session> {
    let target = jump_target(session)?;
    store.borrow().get(target).cloned()
}

pub(crate) fn start_session_in_tab(tab_id: &str, session: Session, ctx: &ConnectCtx) {
    let has_sftp = session.kind == SessionKind::Ssh;
    let (initial_cols, initial_rows) = *ctx.last_term_size.lock().unwrap_or_else(|e| e.into_inner());
    // Resolve the optional SSH jump host now (on the UI thread, where the store
    // lives) so the owned Session can be handed to the worker threads (#211).
    let jump = resolve_jump(&ctx.store, &session);
    let (handle, rx) = match session.kind {
        SessionKind::Ssh => spawn_session(
            ctx.runtime.handle(),
            session.clone(),
            jump.clone(),
            initial_cols,
            initial_rows,
            ctx.trust_unknown_host_key,
        ),
        SessionKind::Serial => crate::terminal::serial::spawn_serial_session(
            ctx.runtime.handle(),
            session.clone(),
        ),
        SessionKind::Telnet => crate::terminal::telnet::spawn_telnet_session(
            ctx.runtime.handle(),
            session.clone(),
            initial_cols,
            initial_rows,
        ),
        SessionKind::Local => crate::terminal::local::spawn_local_session(
            ctx.runtime.handle(),
            session.clone(),
            initial_cols,
            initial_rows,
        ),
    };
    let terminal_reply_tx = handle.commands.clone();
    // 连接后先不开远端监控：默认只有活动标签需要（由 `refresh-sidebar` 处理器打开），
    // 非活动标签的 3 条 exec 通道（750ms/1.5s/2.5s）纯属浪费。MCP 启用时该处理器
    // 会保持全量打开，避免它的工具读到未更新的标签状态。
    handle.set_resource_monitoring(false);
    ctx.handles.borrow_mut().insert(tab_id.to_string(), handle);

    // 文件面板默认折叠的会话：先不建第二条 SSH 连接，等面板展开/手动刷新再连。
    // 默认展开的会话保持原行为（连上 PTY 后立即建 SFTP），用户无感。
    let lazy_sftp = has_sftp && ctx.store.borrow().collapse_sftp_default();
    let sftp_ready_tx = if has_sftp {
        let (sftp_tx, sftp_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        // 事件 pump 延迟到首次用到 SFTP 时才起线程。
        type SftpRx = tokio::sync::mpsc::UnboundedReceiver<SessionEvent>;
        let pump: crate::sftp::SftpPumpStarter = {
            let ctx_weak = ctx.weak.clone();
            let ctx_bufs = ctx.bufs.clone();
            let ctx_tid = tab_id.to_string();
            let ctx_statuses = ctx.tab_statuses.clone();
            let ctx_local = ctx.local_snap.clone();
            let ctx_net = ctx.local_net_hist.clone();
            let ctx_gates = ctx.render_gates.clone();
            std::sync::Arc::new(move |mut rx: SftpRx| {
                let weak_sftp = ctx_weak.clone();
                let bufs_sftp = ctx_bufs.clone();
                let tab_id_sftp = ctx_tid.clone();
                let statuses_sftp = ctx_statuses.clone();
                let local_sftp = ctx_local.clone();
                let net_sftp = ctx_net.clone();
                let gates_sftp = ctx_gates.clone();
                std::thread::spawn(move || {
                    let mut drained: Vec<SessionEvent> = Vec::new();
                    loop {
                        match rx.blocking_recv() {
                            None => break,
                            Some(first) => drained.push(first),
                        }
                        const SFTP_DRAIN_CAP: usize = 256;
                        while drained.len() < SFTP_DRAIN_CAP {
                            match rx.try_recv() {
                                Ok(evt) => drained.push(evt),
                                Err(_) => break,
                            }
                        }
                        let ui_batch: Vec<SessionEvent> = std::mem::take(&mut drained);
                        if ui_batch.is_empty() {
                            continue;
                        }
                        let weak_s = weak_sftp.clone();
                        let tid = tab_id_sftp.clone();
                        let bufs_s = bufs_sftp.clone();
                        let st_s = statuses_sftp.clone();
                        let lc_s = local_sftp.clone();
                        let nh_s = net_sftp.clone();
                        let gates_s = gates_sftp.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(win) = weak_s.upgrade() {
                                for sftp_evt in ui_batch {
                                    apply_session_event_to_window(
                                        &win,
                                        &tid,
                                        sftp_evt,
                                        &SessionResources {
                                            bufs: &bufs_s,
                                            gates: &gates_s,
                                            statuses: &st_s,
                                            local: &lc_s,
                                            local_net_hist: &nh_s,
                                        },
                                    );
                                }
                            }
                        });
                    }
                });
            })
        };
        if lazy_sftp {
            crate::sftp::register_sftp_launcher(crate::sftp::SftpLaunchParams {
                tab_id: tab_id.to_string(),
                runtime: ctx.runtime.handle().clone(),
                handles: ctx.sftp_handles.clone(),
                last_cwd: ctx.sftp_last_cwd.clone(),
                session,
                jump,
                events: sftp_tx,
                events_rx: sftp_rx,
                pump_starter: pump,
            });
            None
        } else {
            pump(sftp_rx);
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
            let sftp_runtime = ctx.runtime.clone();
            let sftp_task_runtime = sftp_runtime.clone();
            let sftp_handles = ctx.sftp_handles.clone();
            let sftp_tab_id = tab_id.to_string();
            sftp_runtime.spawn(async move {
                if ready_rx.await.is_err() {
                    return;
                }
                tokio::task::yield_now().await;
                let sftp_handle = spawn_sftp(sftp_task_runtime.handle(), session, jump, sftp_tx);
                if let Ok(mut handles) = sftp_handles.lock() {
                    handles.insert(sftp_tab_id, sftp_handle);
                }
            });
            Some(ready_tx)
        }
    } else {
        None
    };

    // --- Shell event pump (dedicated thread) ---
    {
        let weak_inner = ctx.weak.clone();
        let bufs_thread = ctx.bufs.clone();
        let sftp_handles_pump = ctx.sftp_handles.clone();
        let sftp_last_cwd_pump = ctx.sftp_last_cwd.clone();
        let rt_pump = ctx.runtime.clone();
        let tab_id_pump = tab_id.to_string();
        let statuses_pump = ctx.tab_statuses.clone();
        let local_pump = ctx.local_snap.clone();
        let net_pump = ctx.local_net_hist.clone();
        let follow_cd_pump = ctx.sftp_follow_cd.clone();
        let render_gates_pump = ctx.render_gates.clone();
        std::thread::spawn(move || {
            let mut shell_rx = rx;
            let mut sftp_ready_tx = sftp_ready_tx;
            let lazy_sftp_pump = lazy_sftp;
            let mut cwd_debounce: Option<tokio::task::JoinHandle<()>> = None;
            // Reusable scratch so a fast firehose doesn't reallocate every batch.
            let mut drained: Vec<SessionEvent> = Vec::new();
            // This survives drain batches, so a stream of small events cannot
            // evade the frame checkpoint merely because of thread timing.
            let mut ingested_since_checkpoint = 0usize;
            loop {
                // Block for the first event, then sweep up everything else that's
                // already queued. A burst — e.g. `tail -f` on a busy log (#171) —
                // then collapses into ONE invoke_from_event_loop and (after merging
                // adjacent Output below) ONE vt100 ingest + render, instead of one
                // UI task per chunk flooding the event loop and freezing the app.
                match shell_rx.blocking_recv() {
                    None => break,
                    Some(first) => drained.push(first),
                }
                // Cap the sweep so an unending stream still yields to the renderer
                // between batches (keeps the UI live rather than starved).
                const DRAIN_CAP: usize = 2048;
                while drained.len() < DRAIN_CAP {
                    match shell_rx.try_recv() {
                        Ok(evt) => drained.push(evt),
                        Err(_) => break,
                    }
                }

                // Run CwdChanged side-effects here (off the UI thread), drop the
                // swallowed ones, and concatenate runs of Output into a single chunk
                // so the UI parses + renders the whole burst once.
                let mut ui_batch: Vec<SessionEvent> = Vec::with_capacity(drained.len());
                for evt in drained.drain(..) {
                    match evt {
                        SessionEvent::Connected => {
                            if let Some(ready) = sftp_ready_tx.take() {
                                let _ = ready.send(());
                            }
                            if lazy_sftp_pump {
                                crate::sftp::sftp_launcher_connected(&tab_id_pump);
                            }
                            ui_batch.push(SessionEvent::Connected);
                        }
                        SessionEvent::CwdChanged(cwd) => {
                            // Shared map (not a thread-local) so manual SFTP
                            // navigation can clear the entry — then the very next
                            // OSC 7, same directory or not, snaps the panel back to
                            // the shell's cwd. Unchanged repeats (every prompt
                            // re-emits OSC 7) are ignored (#59).
                            let changed = match sftp_last_cwd_pump.lock() {
                                Ok(mut m) => {
                                    m.insert(tab_id_pump.clone(), cwd.clone()).as_deref()
                                        != Some(cwd.as_str())
                                }
                                Err(_) => false,
                            };
                            // Swallow when follow-cd is off: forwarding it would set
                            // sftp_loading without any ListDir to clear it (the #59
                            // stuck-"loading" trap).
                            if !changed
                                || !follow_cd_pump.load(std::sync::atomic::Ordering::Relaxed)
                            {
                                continue;
                            }
                            if let Some(prev) = cwd_debounce.take() {
                                prev.abort();
                            }
                            let cwd_spawn = cwd.clone();
                            let sftp_h = sftp_handles_pump.clone();
                            let tid = tab_id_pump.clone();
                            cwd_debounce = Some(rt_pump.spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                if let Ok(handles) = sftp_h.lock()
                                    && let Some(h) = handles.get(&tid)
                                {
                                    h.list_dir(cwd_spawn);
                                }
                            }));
                            ui_batch.push(SessionEvent::CwdChanged(cwd));
                        }
                        SessionEvent::Output(chunk) => {
                            // Merge with the immediately preceding Output so the
                            // whole run is one vt100 ingest + one render. Only
                            // *adjacent* chunks merge, so byte order (and any
                            // interleaved event) is preserved exactly. Cap the
                            // merged size so one batch can't monopolize the UI
                            // thread for hundreds of ms (#209).
                            if let Some(SessionEvent::Output(prev)) = ui_batch.last_mut() {
                                if prev.len() + chunk.len() <= OUTPUT_MERGE_BYTE_CAP {
                                    prev.push_str(&chunk);
                                } else {
                                    ui_batch.push(SessionEvent::Output(chunk));
                                }
                            } else {
                                ui_batch.push(SessionEvent::Output(chunk));
                            }
                        }
                        other => ui_batch.push(other),
                    }
                }
                if ui_batch.is_empty() {
                    continue;
                }

                // Ingest terminal output on this pump thread (not the UI thread).
                // Keep each Output event atomic: TermBuffer detects full-screen
                // redraw sequences within one ingest call, so artificial byte
                // splits could corrupt scrollback when they bisect such a refresh.
                let mut remaining_output_bytes: usize = ui_batch
                    .iter()
                    .map(|event| match event {
                        SessionEvent::Output(chunk) => chunk.len(),
                        _ => 0,
                    })
                    .sum();
                let has_immediate_ui_events = ui_batch.iter().any(event_requires_immediate_ui);
                let mut dirty_since_request = false;
                let mut ui_only: Vec<SessionEvent> = Vec::with_capacity(ui_batch.len());
                for evt in ui_batch {
                    match evt {
                        SessionEvent::Output(chunk) => {
                            let chunk_len = chunk.len();
                            let reply = ingest_terminal_output(
                                &bufs_thread,
                                &tab_id_pump,
                                chunk.as_bytes(),
                            );
                            if !reply.is_empty() {
                                let _ = terminal_reply_tx.send(SessionCommand::RawInput(reply));
                            }
                            remaining_output_bytes =
                                remaining_output_bytes.saturating_sub(chunk_len);
                            dirty_since_request = true;

                            if record_ingested_chunk(chunk_len, &mut ingested_since_checkpoint) {
                                let ticket = request_tab_render(
                                    weak_inner.clone(),
                                    &tab_id_pump,
                                    &bufs_thread,
                                    &render_gates_pump,
                                );
                                dirty_since_request = false;

                                // The event channel is intentionally unbounded
                                // today. Waiting while a large backlog exists would
                                // only move bytes from the terminal buffer into that
                                // channel and inflate memory, so catch up first and
                                // pace once the stream's tail is within reach.
                                if !has_immediate_ui_events
                                    && remaining_output_bytes <= PACED_LOCAL_BACKLOG_LIMIT
                                    && shell_rx.len() <= PACED_QUEUE_EVENT_LIMIT
                                {
                                    wait_for_ui_flush(ticket);
                                }
                            }
                        }
                        other => ui_only.push(other),
                    }
                }

                if dirty_since_request {
                    let _ = request_tab_render(
                        weak_inner.clone(),
                        &tab_id_pump,
                        &bufs_thread,
                        &render_gates_pump,
                    );
                }

                if ui_only.is_empty() {
                    continue;
                }

                let weak_evt = weak_inner.clone();
                let tid = tab_id_pump.clone();
                let bufs_evt = bufs_thread.clone();
                let st_evt = statuses_pump.clone();
                let lc_evt = local_pump.clone();
                let nh_evt = net_pump.clone();
                let gates_evt = render_gates_pump.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(win) = weak_evt.upgrade() {
                        for evt in ui_only {
                            apply_session_event_to_window(
                                &win,
                                &tid,
                                evt,
                                &SessionResources {
                                    bufs: &bufs_evt,
                                    gates: &gates_evt,
                                    statuses: &st_evt,
                                    local: &lc_evt,
                                    local_net_hist: &nh_evt,
                                },
                            );
                        }
                    }
                });
            }
        });
    }
}
#[cfg(test)]
#[path = "session_runtime_tests.rs"] mod session_runtime_tests;
