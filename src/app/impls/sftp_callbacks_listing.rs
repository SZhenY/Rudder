use super::*;

pub(super) fn wire_sftp_listing(window: &AppWindow, ctx: &AppContext) {
    let sftp_handles = ctx.sftp_handles.clone();
    let sftp_last_cwd = ctx.sftp_last_cwd.clone();

    // Refresh the current directory listing.
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_refresh(move |tab_id: SharedString, path: SharedString| {
            let tab_id = tab_id.to_string();
            let path = path.to_string();
            // 合并同一帧内多 pane 的重复刷新。
            let now = std::time::Instant::now();
            let duplicate = LAST_SFTP_REFRESH.with(|last| {
                let mut last = last.borrow_mut();
                last.retain(|_, (_, at)| {
                    now.duration_since(*at) < std::time::Duration::from_secs(2)
                });
                if let Some((prev_path, prev_at)) = last.get(&tab_id)
                    && prev_path == &path
                    && now.duration_since(*prev_at) < std::time::Duration::from_millis(50)
                {
                    return true;
                }
                last.insert(tab_id.clone(), (path.clone(), now));
                false
            });
            if duplicate {
                return;
            }
            // 懒启动会话：展开面板/手动刷新是第一次真正需要 SFTP 的时机。
            crate::sftp::want_sftp(&tab_id, Some(path.clone()));
            if let Ok(handles) = sftp_handles.lock()
                && let Some(h) = handles.get(&tab_id)
            {
                // Refresh re-syncs the left tree too, not just the file list (#189).
                h.refresh_dir(path);
            }
            // Reset the multi-select counter to match the new (empty) selection.
            // `refresh_dir` replaces the listing with freshly loaded, unchecked
            // entries, but without this the toolbar kept showing the stale
            // count (e.g. "2") and the batch download/delete buttons stayed
            // visible until the user toggled a checkbox to force a recount.
            if let Some(w) = weak.upgrade() {
                let terminals = w.get_terminals();
                if let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() {
                    clear_sftp_selection(tm, &tab_id);
                }
            }
        });
    }

    // Toggle tree node expand/collapse and navigate to that directory.
    {
        let sftp_handles = sftp_handles.clone();
        let sftp_last_cwd = sftp_last_cwd.clone();
        window.on_sftp_tree_expand(move |tab_id: SharedString, path: SharedString| {
            let tab_id = tab_id.to_string();
            let path = path.to_string();
            // Forget the followed cwd (see on_sftp_navigate): tree navigation
            // must never permanently disable cd-follow.
            if let Ok(mut cwd) = sftp_last_cwd.lock() {
                cwd.remove(&tab_id);
            }
            if let Ok(handles) = sftp_handles.lock()
                && let Some(h) = handles.get(&tab_id)
            {
                h.toggle_tree_node(path.clone());
                h.list_dir(path);
            }
        });
    }

    // Context menu → 删除 a remote file. The irreversible-delete confirmation
    // (#28) is handled by the in-app ConfirmDialog in the UI layer, so by the
    // time this fires the user has already confirmed.
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_delete(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock()
                && let Some(h) = handles.get(tab_id.as_str())
            {
                h.delete(path.to_string());
            }
        });
    }

    // SFTP file-list sorting (#248): click a header to cycle asc -> desc -> default.
    {
        let weak = window.as_weak();
        window.on_sftp_sort_request(move |tab_id: SharedString, key: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let terminals = w.get_terminals();
            let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                return;
            };
            update_terminal_row(tm, tab_id.as_str(), |row| {
                let key = key.to_string();
                let next_dir = if row.sftp_sort_key.as_str() != key || row.sftp_sort_dir == 0 {
                    1
                } else if row.sftp_sort_dir > 0 {
                    -1
                } else {
                    0
                };
                let next_key = if next_dir == 0 { String::new() } else { key };
                row.sftp_entries =
                    sorted_sftp_entries_from_model(&row.sftp_entries, &next_key, next_dir);
                row.sftp_sort_key = next_key.into();
                row.sftp_sort_dir = next_dir;
            });
        });
    }
    {
        let weak = window.as_weak();
        window.on_sftp_clear_sort(move |tab_id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let terminals = w.get_terminals();
            let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                return;
            };
            update_terminal_row(tm, tab_id.as_str(), |row| {
                row.sftp_entries = sorted_sftp_entries_from_model(&row.sftp_entries, "", 0);
                row.sftp_sort_key = "".into();
                row.sftp_sort_dir = 0;
            });
        });
    }

    // SFTP multi-select: toggle a row's checkbox + recount (#100).
    {
        let weak = window.as_weak();
        window.on_sftp_toggle_select(move |tab_id: SharedString, idx: i32| {
            let Some(w) = weak.upgrade() else { return };
            let terminals = w.get_terminals();
            let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                return;
            };
            for ti in 0..tm.row_count() {
                let Some(row) = tm.row_data(ti) else { continue };
                if row.id.as_str() != tab_id.as_str() {
                    continue;
                }
                if let Some(em) = row
                    .sftp_entries
                    .as_any()
                    .downcast_ref::<VecModel<SftpEntry>>()
                {
                    let i = idx as usize;
                    if let Some(mut e) = em.row_data(i) {
                        e.selected = !e.selected;
                        em.set_row_data(i, e);
                    }
                    let mut n = 0;
                    for ei in 0..em.row_count() {
                        if em.row_data(ei).map(|x| x.selected).unwrap_or(false) {
                            n += 1;
                        }
                    }
                    let mut r = row.clone();
                    r.sftp_selected_count = n;
                    tm.set_row_data(ti, r);
                }
                break;
            }
        });
    }
    // SFTP multi-select: download all checked entries into one folder (#100).
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_download_selected(move |tab_id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let terminals = w.get_terminals();
            let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                return;
            };
            let paths = collect_sftp_selected(tm, tab_id.as_str());
            if paths.is_empty() {
                return;
            }
            // Single selection downloads as a plain file (no compression, #100.3);
            // multiple selections are tar-packed into one archive on the remote
            // (#100.2) — this also avoids the concurrent-transfer races (#100.1).
            let single = paths.len() == 1;
            let remote_dir = active_sftp_path(&w, tab_id.as_str());
            let names: Vec<String> = paths
                .iter()
                .map(|p| {
                    p.trim_end_matches('/')
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(p)
                        .to_string()
                })
                .collect();
            let preset = w.get_download_dir().to_string();
            let always_ask = w.get_download_always_ask();
            if !always_ask && !preset.is_empty() {
                if let Ok(handles) = sftp_handles.lock()
                    && let Some(h) = handles.get(tab_id.as_str())
                {
                    if single {
                        if let Some(conflict) = choose_download_conflict(&paths[0], &preset) {
                            h.download(paths[0].clone(), preset.clone(), conflict);
                        }
                    } else {
                        h.download_archive(remote_dir.clone(), names.clone(), preset.clone());
                    }
                }
                w.set_download_open(true);
            } else {
                let sftp_handles = sftp_handles.clone();
                let weak2 = weak.clone();
                let tab = tab_id.to_string();
                std::thread::spawn(move || {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        let dir = dir.to_string_lossy().to_string();
                        if let Ok(handles) = sftp_handles.lock()
                            && let Some(h) = handles.get(&tab)
                        {
                            if single {
                                if let Some(conflict) = choose_download_conflict(&paths[0], &dir) {
                                    h.download(paths[0].clone(), dir.clone(), conflict);
                                }
                            } else {
                                h.download_archive(remote_dir.clone(), names.clone(), dir.clone());
                            }
                        }
                        let _ = weak2.upgrade_in_event_loop(|w| w.set_download_open(true));
                    }
                });
            }
            clear_sftp_selection(tm, tab_id.as_str());
        });
    }
    // SFTP multi-select: delete all checked entries (confirmed in the UI) (#100).
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_delete_selected(move |tab_id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let terminals = w.get_terminals();
            let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                return;
            };
            let paths = collect_sftp_selected(tm, tab_id.as_str());
            if paths.is_empty() {
                return;
            }
            if let Ok(handles) = sftp_handles.lock()
                && let Some(h) = handles.get(tab_id.as_str())
            {
                for p in &paths {
                    h.delete(p.clone());
                }
            }
            clear_sftp_selection(tm, tab_id.as_str());
        });
    }
}
