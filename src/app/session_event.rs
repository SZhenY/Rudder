use super::render_tickets::request_tab_render_from_ui;
use super::*;

pub(crate) struct SessionResources<'a> {
    pub(crate) bufs: &'a TermBuffers,
    pub(crate) gates: &'a RenderGates,
    pub(crate) statuses: &'a TabStatuses,
    pub(crate) local: &'a LocalSnap,
    pub(crate) local_net_hist: &'a NetHist,
}

pub(super) fn apply_session_event_to_window<'a>(
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
            if let Ok(mut s) = statuses.lock()
            && let Some(st) = s.get_mut(tab_id)
            {
                st.state = 2;
            }
            // 断开后**立刻**释放回滚历史（内存大头：默认 5 000 行 ≈ 14 MB，开了大回滚
            // 可达 GB 级）。断开的会话不再产生输出，留着没有任何用处；可见屏幕保留 ——
            // 上面的断线提示与断线前的内容仍在，代价是滚不回历史了。所有会话类型
            // （ssh / serial / telnet / local）都走这一个入口。
            if let Ok(bufs) = bufs.lock()
                && let Some(handle) = bufs.get(tab_id)
            {
                let mut b = handle.lock().unwrap_or_else(|e| e.into_inner());
                b.release_history_keep_screen();
            }
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
    }
}
/// 传输行的进度文案（state：0 传输中 / 1 完成 / 2 失败 / 3 准备中 / 4 已取消）。
fn transfer_detail(state: u8, msg: &str, transferred: u64, total: u64) -> String {
    match state {
        // 失败时优先显示服务端给的原因，没有才用通用文案。
        2 => {
            if msg.is_empty() {
                t("失败", "Failed").to_string()
            } else {
                msg.to_string()
            }
        }
        1 => t("已完成", "Done").to_string(),
        // 远端准备阶段（如 tar 打包）还没开始传字节（#100）。
        3 => t("文件准备中", "Preparing...").to_string(),
        // 用户取消（#100）。
        4 => t("已取消", "Cancelled").to_string(),
        _ => {
            if total > 0 {
                format!("{}/{}", format_size(transferred), format_size(total))
            } else {
                format_size(transferred)
            }
        }
    }
}

/// 进度条比例：完成态直接 1.0（否则会永远停在 99%）；`total == 0` 时不能做除数。
fn transfer_percent(state: u8, transferred: u64, total: u64) -> f32 {
    if state == 1 {
        1.0
    } else if total > 0 {
        (transferred as f32 / total as f32).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// 写入/更新一行传输记录，返回**是否还有活跃任务**（驱动工具栏呼吸灯）。
///
/// 新行插到队首（最新在上）；超过上限时保留队首、淘汰队尾 —— 方向写反就会把刚完成的
/// 那条挤掉，用户再也看不到它。
fn upsert_transfer_row(model: &VecModel<TransferInfo>, rec: TransferInfo) -> bool {
    const MAX_TRANSFER_ROWS: usize = 200;
    let mut found = None;
    for i in 0..model.row_count() {
        if let Some(row) = model.row_data(i)
            && row.id.as_str() == rec.id.as_str()
        {
            found = Some(i);
            break;
        }
    }
    match found {
        Some(i) => model.set_row_data(i, rec),
        None => {
            model.insert(0, rec); // newest at top
            if model.row_count() > MAX_TRANSFER_ROWS {
                let keep: Vec<TransferInfo> = (0..MAX_TRANSFER_ROWS)
                    .filter_map(|i| model.row_data(i))
                    .collect();
                model.set_vec(keep);
            }
        }
    }
    // 呼吸灯：还有传输中(0)或准备中(3)的就算活跃。
    (0..model.row_count()).any(|i| {
        model
            .row_data(i)
            .map(|r| r.state == 0 || r.state == 3)
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, state: i32) -> TransferInfo {
        TransferInfo {
            id: id.into(),
            name: format!("{id}.bin").into(),
            detail: String::new().into(),
            percent: 0.0,
            state,
            is_upload: true,
        }
    }

    #[test]
    fn detail_uses_the_server_message_on_failure() {
        assert_eq!(transfer_detail(2, "", 0, 0), t("失败", "Failed"));
        assert_eq!(
            transfer_detail(2, "permission denied", 0, 0),
            "permission denied",
            "有服务端原因就用原因"
        );
        assert_eq!(transfer_detail(1, "", 0, 0), t("已完成", "Done"));
        assert_eq!(transfer_detail(3, "", 0, 0), t("文件准备中", "Preparing..."));
        assert_eq!(transfer_detail(4, "", 0, 0), t("已取消", "Cancelled"));
    }

    #[test]
    fn detail_shows_bytes_while_transferring() {
        assert_eq!(
            transfer_detail(0, "", 1536, 4096),
            format!("{}/{}", format_size(1536), format_size(4096))
        );
        assert_eq!(
            transfer_detail(0, "", 1536, 0),
            format_size(1536),
            "不知道总量时只显示已传"
        );
    }

    /// 完成态必须直接给 1.0：靠 `transferred/total` 算的话，最后一段字节没到的任务会停在 99%。
    #[test]
    fn percent_is_one_when_done_and_zero_when_total_is_unknown() {
        assert_eq!(transfer_percent(1, 10, 100), 1.0);
        assert_eq!(transfer_percent(0, 0, 0), 0.0, "总量未知不能做除数");
        assert_eq!(transfer_percent(0, 50, 100), 0.5);
        assert_eq!(transfer_percent(0, 150, 100), 1.0, "超过 100% 要夹住");
    }

    /// 新任务插到队首，并按状态给出"还有活跃任务"（呼吸灯：只要有 0 或 3 就亮）。
    #[test]
    fn upsert_inserts_at_the_top_and_reports_activity() {
        let m = VecModel::from(vec![rec("old", 1)]);
        assert!(upsert_transfer_row(&m, rec("new", 0)), "传输中 -> 呼吸灯该亮");
        assert_eq!(m.row_count(), 2);
        assert_eq!(m.row_data(0).unwrap().id.as_str(), "new", "最新的在最上面");

        // 全是终态（完成 1 / 失败 2 / 取消 4）时不该再亮。
        let settled = VecModel::from(vec![rec("a", 1), rec("b", 2), rec("c", 4)]);
        assert!(
            !upsert_transfer_row(&settled, rec("d", 1)),
            "只剩终态 -> 呼吸灯熄灭"
        );
        let preparing = VecModel::from(vec![rec("a", 1)]);
        assert!(upsert_transfer_row(&preparing, rec("b", 3)), "准备中也算活跃");
    }

    /// 同一个 id 再来一次是原地替换（不新增行）—— 同一次传输的进度更新路径。
    #[test]
    fn upsert_replaces_the_same_id_in_place() {
        let m = VecModel::from(vec![rec("a", 0), rec("b", 1)]);
        assert!(upsert_transfer_row(&m, rec("a", 3)));
        assert_eq!(m.row_count(), 2, "不新增行");
        assert_eq!(m.row_data(0).unwrap().id.as_str(), "a", "位置不变");
        assert_eq!(m.row_data(0).unwrap().state, 3, "状态已更新");
    }

    /// 超过 200 行时保留队首、淘汰队尾 —— 方向写反就会把刚完成的挤掉。
    #[test]
    fn upsert_evicts_oldest_rows_past_the_cap() {
        let m = VecModel::from(Vec::new());
        for i in 0..201 {
            upsert_transfer_row(&m, rec(&format!("t{i}"), 1));
        }
        assert_eq!(m.row_count(), 200, "上限 200 行");
        let ids: Vec<String> = (0..m.row_count())
            .filter_map(|i| m.row_data(i))
            .map(|r| r.id.to_string())
            .collect();
        assert_eq!(ids[0], "t200", "最新的还在最上面");
        assert!(!ids.contains(&"t0".to_string()), "最旧的被淘汰");
    }
}
