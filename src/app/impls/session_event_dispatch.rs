use super::*;

pub(crate) fn apply_session_event_to_window<'a>(
    win: &AppWindow,
    tab_id: &str,
    event: SessionEvent,
    res: &'a SessionResources<'a>,
) {
    let SessionResources {
        bufs,
        gates,
        statuses,
        local,
        local_net_hist,
    } = res;
    let tabs_rc = win.get_tabs();
    let terminals_rc = win.get_terminals();
    // `ModelRc::as_any` lets us downcast to the concrete `VecModel<T>`.
    // Downgrade instead of `expect()`: this runs for every session event, and
    // release builds use panic = "abort", so an unexpected model type would
    // kill the client rather than drop one event. The `panes` lookups further
    // down already degrade the same way.
    let Some(tabs) = tabs_rc.as_any().downcast_ref::<VecModel<TabInfo>>() else {
        return;
    };
    let Some(terminals) = terminals_rc
        .as_any()
        .downcast_ref::<VecModel<TerminalState>>()
    else {
        return;
    };

    let update_terminal = |mutator: &dyn Fn(&mut TerminalState)| {
        for i in 0..terminals.row_count() {
            if let Some(mut row) = terminals.row_data(i)
                && row.id.as_str() == tab_id
            {
                mutator(&mut row);
                terminals.set_row_data(i, row);
                break;
            }
        }
    };
    let update_tab = |mutator: &dyn Fn(&mut TabInfo)| {
        for i in 0..tabs.row_count() {
            if let Some(mut row) = tabs.row_data(i)
                && row.id.as_str() == tab_id
            {
                mutator(&mut row);
                tabs.set_row_data(i, row);
                break;
            }
        }
        // The per-pane tab strips (v0.5 split panes) render snapshots copied from
        // `tabs_model`, so they don't track this change on their own — propagate
        // it into each pane's tab sub-model too (e.g. so the connected dot turns
        // green without needing a tab switch).
        let panes = win.get_panes();
        if let Some(pm) = panes.as_any().downcast_ref::<VecModel<PaneInfo>>() {
            for pi in 0..pm.row_count() {
                let Some(pane) = pm.row_data(pi) else {
                    continue;
                };
                let Some(tm) = pane.tabs.as_any().downcast_ref::<VecModel<TabInfo>>() else {
                    continue;
                };
                for ti in 0..tm.row_count() {
                    if let Some(mut row) = tm.row_data(ti)
                        && row.id.as_str() == tab_id
                    {
                        mutator(&mut row);
                        tm.set_row_data(ti, row);
                        break;
                    }
                }
            }
        }
    };

    match event {
        SessionEvent::Status(status) => {
            update_terminal(&|t| t.status = status.clone().into());
        }
        SessionEvent::Output(chunk) => {
            // Synthetic Output (disconnect hint, editor error, …) — rare, already
            // on the UI thread. Live shell output is ingested on the pump thread.
            let _ = ingest_terminal_output(bufs, tab_id, chunk.as_bytes());
            request_tab_render_from_ui(win.as_weak(), tab_id, bufs, gates);
        }
        SessionEvent::Connected => {
            update_tab(&|t| t.connected = true);
            update_terminal(&|t| t.status = crate::i18n::t("已连接", "Connected").into());
            // 连接状态弹窗：只认自己跟踪的那条 tab（连接失败后 phase 会是 2，
            // 再成功时重新置 1 也合理 —— 比如重连成功）。
            if win.get_conn_popup_tab().as_str() == tab_id {
                win.set_conn_popup_phase(1);
            }
            // Poisoned lock → skip the status update rather than abort the
            // process (release builds use panic = "abort").
            if let Ok(mut s) = statuses.lock()
                && let Some(st) = s.get_mut(tab_id)
            {
                st.state = 1;
            }
            if win.get_active_tab_id().as_str() == tab_id {
                refresh_sidebar(win, statuses, local, local_net_hist);
            }
        }
        SessionEvent::Closed(reason) => {
            // Print the hint into the terminal itself (FinalShell-style), via a
            // synthetic Output event so it reuses the normal render path (#79).
            apply_session_event_to_window(
                win,
                tab_id,
                SessionEvent::Output(format!(
                    "\r\n\x1b[31m{}\x1b[0m\r\n",
                    crate::i18n::t(
                        "连接已断开,按 Enter 重新连接",
                        "Disconnected — press Enter to reconnect"
                    )
                )),
                &SessionResources {
                    bufs,
                    gates,
                    statuses,
                    local,
                    local_net_hist,
                },
            );
            update_tab(&|t| t.connected = false);
            update_terminal(&|t| {
                t.status = format!("{} — {reason}", crate::i18n::t("已断开", "Disconnected")).into()
            });
            // 连接状态弹窗：只有**从未成功过**才判失败（phase 仍是 0）。
            // 连上之后掉线走终端里的红色提示，不弹失败框。
            if win.get_conn_popup_tab().as_str() == tab_id && win.get_conn_popup_phase() == 0 {
                win.set_conn_popup_detail(reason.clone().into());
                win.set_conn_popup_phase(2);
            }
            if let Ok(mut s) = statuses.lock()
                && let Some(st) = s.get_mut(tab_id)
            {
                st.state = 2;
            }
            // 注意：意外断线（网络 / 远端挂掉）**保留**回滚历史 —— 用户正要看断线前发生了什么，
            // 此时清掉是最不该做的事。释放只发生在用户**主动关标签页**时
            // （`tab_callbacks.rs` 的 pane-tab-closed）。
            if win.get_active_tab_id().as_str() == tab_id {
                refresh_sidebar(win, statuses, local, local_net_hist);
            }
        }
        SessionEvent::ResourceStats {
            cpu_percent,
            mem_used_kib,
            mem_total_kib,
            swap_used_kib,
            swap_total_kib,
            net,
            disks,
            sys,
        } => {
            if let Ok(mut s) = statuses.lock()
                && let Some(st) = s.get_mut(tab_id)
            {
                st.cpu = cpu_percent;
                st.mem_used_kib = mem_used_kib;
                st.mem_total_kib = mem_total_kib;
                st.swap_used_kib = swap_used_kib;
                st.swap_total_kib = swap_total_kib;
                st.net = net;
                st.disks = disks;
                if let Some(sys) = *sys {
                    st.sys = sys;
                }
                st.last_stats_at = Some(std::time::Instant::now());
                // A sample means the channel is alive → treat as connected.
                if st.state != 1 {
                    st.state = 1;
                }
                // Append the selected interface's total rate to its sparkline.
                let (_, rx, tx) = selected_iface(st);
                push_ring(&mut st.net_hist, (rx + tx) as f32);
            }
            if win.get_active_tab_id().as_str() == tab_id {
                refresh_sidebar(win, statuses, local, local_net_hist);
            }
        }
        SessionEvent::ProcessStats {
            current_user,
            procs,
        } => {
            if let Ok(mut s) = statuses.lock()
                && let Some(st) = s.get_mut(tab_id)
            {
                if !current_user.is_empty() {
                    st.user = current_user;
                }
                st.procs = procs;
            }
            if win.get_active_tab_id().as_str() == tab_id {
                refresh_sidebar(win, statuses, local, local_net_hist);
            }
        }
        // --- SFTP events ---------------------------------------------------
        SessionEvent::CwdChanged(path) => {
            // Just update the displayed path; the pump thread already sent
            // SftpCommand::ListDir so a SftpEntries event is inbound.
            update_terminal(&|t| {
                t.sftp_path = path.clone().into();
                t.sftp_loading = true;
            });
        }
        SessionEvent::SftpEntries { path, entries } => {
            let mut slint_entries: Vec<SftpEntry> = entries
                .iter()
                .map(|e| SftpEntry {
                    name: e.name.clone().into(),
                    full_path: e.full_path.clone().into(),
                    is_dir: e.is_dir,
                    size: if e.is_dir {
                        "".into()
                    } else {
                        format_size(e.size).into()
                    },
                    size_bytes: e.size as f32,
                    modified: format_mtime(e.modified).into(),
                    modified_ts: e.modified as f32,
                    mode: (e.mode & 0o7777) as i32,
                    // 「类型 / 权限」两列（上游 ed7f231）：都由完整 mode 位算出，不需要
                    // 额外往返。
                    permissions: format_permissions(e.mode_full).into(),
                    entry_type: file_type_label(e.mode_full).into(),
                    selected: false,
                })
                .collect();
            let (sort_key, sort_dir) = (0..terminals.row_count())
                .find_map(|i| {
                    let row = terminals.row_data(i)?;
                    (row.id.as_str() == tab_id)
                        .then(|| (row.sftp_sort_key.to_string(), row.sftp_sort_dir))
                })
                .unwrap_or_default();
            sort_sftp_entries(&mut slint_entries, &sort_key, sort_dir);
            let model = ModelRc::from(std::rc::Rc::new(VecModel::from(slint_entries)));
            update_terminal(&|t| {
                t.sftp_path = path.clone().into();
                t.sftp_entries = model.clone();
                t.sftp_loading = false;
            });
        }
        SessionEvent::SftpStatus(msg) => {
            update_terminal(&|t| t.sftp_status = msg.clone().into());
        }
        SessionEvent::SftpError(msg) => {
            // Show the reason and stop the spinner; leave the current listing in
            // place so a failed navigation doesn't blank the panel (#112).
            update_terminal(&|t| {
                t.sftp_status = msg.clone().into();
                t.sftp_loading = false;
            });
        }
        SessionEvent::SftpFileText {
            path,
            name,
            content,
            edit,
            error,
        } => {
            if error.is_empty() {
                // Open the built-in viewer/editor (#70).
                win.set_editor_line_numbers(line_numbers_for(&content).into());
                win.set_editor_path(path.into());
                win.set_editor_name(name.into());
                // 记下归属标签页：之后保存 / 关闭都写到这个会话，而不是"当前活动标签页"
                // （开着编辑器切标签再 Ctrl+S 不能写错会话，上游 eafd513）。
                win.set_editor_tab_id(tab_id.into());
                win.set_editor_content(content.into());
                win.set_editor_readonly(!edit);
                win.set_editor_dirty(false);
                // Fresh find/replace state per open (#287).
                win.set_editor_find_query("".into());
                win.set_editor_replace_text("".into());
                win.set_editor_match_count(0);
                win.set_editor_find_position(-1);
                win.set_editor_open(true);
            } else {
                // Couldn't open as text. The SFTP status line alone is easy to
                // miss (looks like "nothing happened"), so also print the reason
                // into the terminal via a synthetic Output event (#70).
                apply_session_event_to_window(
                    win,
                    tab_id,
                    SessionEvent::Output(format!(
                        "\r\n[rudder] {} {}: {}\r\n",
                        crate::i18n::t("无法打开", "Cannot open"),
                        name,
                        error
                    )),
                    &SessionResources {
                        bufs,
                        gates,
                        statuses,
                        local,
                        local_net_hist,
                    },
                );
                update_terminal(&|t| t.sftp_status = error.clone().into());
            }
        }
        SessionEvent::SftpTreeUpdate(nodes) => {
            let slint_nodes: Vec<SftpTreeNode> = nodes
                .iter()
                .map(|n| SftpTreeNode {
                    path: n.path.clone().into(),
                    name: n.name.clone().into(),
                    depth: n.depth as i32,
                    expanded: n.expanded,
                    has_children: n.has_children,
                })
                .collect();
            let model = ModelRc::from(std::rc::Rc::new(VecModel::from(slint_nodes)));
            update_terminal(&|t| t.sftp_tree_nodes = model.clone());
        }
        SessionEvent::SftpTransfer {
            id,
            name,
            is_upload,
            transferred,
            total,
            state,
            msg,
        } => {
            let detail = transfer_detail(state, msg.as_str(), transferred, total);
            let percent = transfer_percent(state, transferred, total);
            let rec = TransferInfo {
                id: id.clone().into(),
                name: name.into(),
                detail: detail.into(),
                percent,
                state: state as i32,
                is_upload,
            };
            if let Some(model) = win
                .get_transfers()
                .as_any()
                .downcast_ref::<VecModel<TransferInfo>>()
            {
                let has_active = upsert_transfer_row(model, rec);
                win.set_has_active_transfers(has_active);
            }
        }
        SessionEvent::HostKeyPrompt {
            host,
            port,
            key_type,
            fingerprint,
            changed,
            responder,
        } => {
            enqueue_hostkey_prompt(win, host, port, key_type, fingerprint, changed, responder);
        }
        SessionEvent::CredentialPrompt {
            session_id,
            host,
            user,
            need_user,
            need_password,
            responder,
        } => {
            enqueue_cred_prompt(
                win,
                session_id,
                host,
                user,
                need_user,
                need_password,
                responder,
            );
        }
        SessionEvent::MfaPrompt {
            session_id,
            host,
            prompt,
            echo,
            responder,
        } => {
            enqueue_mfa_prompt(win, session_id, host, prompt, echo, responder);
        }
        SessionEvent::CommandRan(cmd) => {
            // A command typed directly in the terminal, captured via the shell
            // hook (#113). Record it in the same command-box history, reusing the
            // de-dup/move-to-end logic, and refresh the model.
            HISTORY_STORE.with(|s| {
                if let Some(store) = s.borrow().as_ref() {
                    {
                        let mut st = store.borrow_mut();
                        st.push_command_history(cmd);
                    }
                    // 同上：命令历史走防抖出口。
                    crate::app::settings::debounced_save(store);
                    win.set_command_history(history_model(&store.borrow()));
                }
            });
        }
        SessionEvent::TunnelUpdate(rows) => {
            let items = rows
                .into_iter()
                .map(|r| TunnelInfo {
                    id: r.id.into(),
                    name: r.name.into(),
                    kind: r.kind.clone().into(),
                    bind: format!("{}:{}", r.bind_addr, r.bind_port).into(),
                    target: if r.kind == "dynamic" {
                        "SOCKS5".into()
                    } else if r.host.is_empty() || r.host_port == 0 {
                        "".into()
                    } else {
                        format!("{}:{}", r.host, r.host_port).into()
                    },
                    status: r.status.into(),
                    active: r.active,
                })
                .collect::<Vec<_>>();
            update_terminal(&|t| {
                t.tunnels = ModelRc::from(std::rc::Rc::new(VecModel::from(items.clone())));
            });
        }
    }
}
