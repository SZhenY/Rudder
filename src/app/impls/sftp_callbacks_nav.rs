use super::*;

pub(super) fn wire_sftp_nav(window: &AppWindow, ctx: &AppContext) {
    let sftp_handles = ctx.sftp_handles.clone();
    let sftp_last_cwd = ctx.sftp_last_cwd.clone();

    // Navigate to a remote path (or ".." to go up one level).
    {
        let sftp_handles = sftp_handles.clone();
        let sftp_last_cwd = sftp_last_cwd.clone();
        let weak = window.as_weak();
        window.on_sftp_navigate(move |tab_id: SharedString, path: SharedString| {
            let tab_id = tab_id.to_string();
            // A pasted path may carry trailing whitespace / newline (#54).
            let path = path.trim();
            let resolved = if path == ".." {
                let current = weak.upgrade().and_then(|w| {
                    let terminals_rc = w.get_terminals();
                    let terminals = terminals_rc
                        .as_any()
                        .downcast_ref::<VecModel<TerminalState>>()?;
                    for i in 0..terminals.row_count() {
                        if let Some(row) = terminals.row_data(i)
                            && row.id.as_str() == tab_id
                        {
                            return Some(row.sftp_path.to_string());
                        }
                    }
                    None
                });
                parent_path(&current.unwrap_or_else(|| "/".to_string()))
            } else {
                path.to_string()
            };
            // Forget the followed cwd so the next OSC 7 — even at an unchanged
            // directory — snaps the panel back to the shell's cwd; manual
            // navigation never permanently disables cd-follow.
            // Poisoned lock → skip (same degrade-everywhere rule as the other
            // handles lookups in this file: panic = "abort" in release).
            if let Ok(mut cwd) = sftp_last_cwd.lock() {
                cwd.remove(&tab_id);
            }
            let mut listing_replaced = false;
            // 懒启动会话：手动导航同样视为"要用 SFTP"。
            crate::sftp::want_sftp(&tab_id, Some(resolved.clone()));
            if let Ok(handles) = sftp_handles.lock()
                && let Some(h) = handles.get(&tab_id)
            {
                h.list_dir(resolved);
                listing_replaced = true;
            }
            // Same stale-counter fix as refresh: the checked entries belong to
            // the directory we just left, so drop the selection with them.
            if listing_replaced && let Some(w) = weak.upgrade() {
                let terminals = w.get_terminals();
                if let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() {
                    clear_sftp_selection(tm, &tab_id);
                }
            }
        });
    }

    // Download a remote file.  If a download folder is preset in settings, save
    // straight there; otherwise fall back to a native folder picker.
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_download(move |tab_id: SharedString, remote_path: SharedString| {
            let tab_id = tab_id.to_string();
            let remote_path = remote_path.to_string();
            // If the user has checked 2+ entries, ANY download (right-click,
            // row button or the toolbar) packs the whole checked set into one
            // archive (#100) — this matches "download these together". A single
            // checked item (or none) downloads the clicked file as-is.
            let (arc_dir, arc_names) = weak
                .upgrade()
                .and_then(|w| {
                    let terminals = w.get_terminals();
                    let tm = terminals
                        .as_any()
                        .downcast_ref::<VecModel<TerminalState>>()?;
                    let paths = collect_sftp_selected(tm, &tab_id);
                    if paths.len() >= 2 {
                        let dir = active_sftp_path(&w, &tab_id);
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
                        clear_sftp_selection(tm, &tab_id);
                        Some((dir, names))
                    } else {
                        None
                    }
                })
                .map(|(d, n)| (Some(d), n))
                .unwrap_or((None, Vec::new()));
            // "Always ask" (#87) forces the folder picker, ignoring the preset.
            let (preset, always_ask) = weak
                .upgrade()
                .map(|w| {
                    (
                        w.get_download_dir().to_string(),
                        w.get_download_always_ask(),
                    )
                })
                .unwrap_or_default();
            if !always_ask && !preset.is_empty() {
                if let Ok(handles) = sftp_handles.lock()
                    && let Some(h) = handles.get(&tab_id)
                {
                    if let Some(ref dir) = arc_dir {
                        h.download_archive(dir.clone(), arc_names.clone(), preset);
                    } else if let Some(conflict) = choose_download_conflict(&remote_path, &preset) {
                        h.download(remote_path, preset, conflict);
                    }
                    // Pop the transfers panel so progress is visible (user
                    // request: any download opens the download popup).
                    if let Some(w) = weak.upgrade() {
                        w.set_download_open(true);
                    }
                }
                return;
            }
            let sftp_handles = sftp_handles.clone();
            let weak = weak.clone();
            std::thread::spawn(move || {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    let local_dir = dir.to_string_lossy().to_string();
                    if let Ok(handles) = sftp_handles.lock()
                        && let Some(h) = handles.get(&tab_id)
                    {
                        if let Some(ref rdir) = arc_dir {
                            h.download_archive(rdir.clone(), arc_names.clone(), local_dir);
                        } else if let Some(conflict) =
                            choose_download_conflict(&remote_path, &local_dir)
                        {
                            h.download(remote_path, local_dir, conflict);
                        }
                    }
                    let _ = weak.upgrade_in_event_loop(|w| w.set_download_open(true));
                }
            });
        });
    }

    // Upload a local file into the current remote directory.
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_upload_clicked(
            move |tab_id: SharedString, remote_dir: SharedString, folder: bool| {
                let tab_id = tab_id.to_string();
                let remote_dir = remote_dir.to_string();
                let sftp_handles = sftp_handles.clone();
                // Session-sync upload (#sync): when both the sync toggle and the
                // "sync upload" setting are on, mirror the upload to every other
                // online session — each into *that session's own* current SFTP
                // directory (paths differ between sessions, e.g. /home/jeff vs
                // /home/root, so the active session's path can't be reused).
                // Gather targets on the UI thread (Slint models aren't Send).
                let sync_targets: Vec<(String, String)> = weak
                    .upgrade()
                    .filter(|w| w.get_sync_input() && w.get_sync_upload_enabled())
                    .map(|w| {
                        let paths = terminal_sftp_paths(&w);
                        let handles = sftp_handles.lock().ok();
                        handles
                            .iter()
                            .flat_map(|h| h.keys())
                            .filter(|id| *id != &tab_id)
                            .filter_map(|id| paths.get(id).map(|dir| (id.clone(), dir.clone())))
                            .filter(|(_, dir)| !dir.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                std::thread::spawn(move || {
                    // The remote SFTP upload handles a file or a whole directory;
                    // only the local picker differs (#85). Folder uploads one dir;
                    // file mode allows selecting several at once.
                    let locals: Vec<std::path::PathBuf> = if folder {
                        rfd::FileDialog::new()
                            .pick_folder()
                            .map(|p| vec![p])
                            .unwrap_or_default()
                    } else {
                        rfd::FileDialog::new()
                            .pick_files()
                            .map(|v| v.into_iter().collect())
                            .unwrap_or_default()
                    };
                    if locals.is_empty() {
                        return;
                    }
                    if let Ok(handles) = sftp_handles.lock() {
                        if let Some(h) = handles.get(&tab_id) {
                            for local in &locals {
                                h.upload(local.clone(), remote_dir.clone());
                            }
                        }
                        // Mirror to the other online sessions, each into its own
                        // current SFTP directory.
                        for (id, dir) in &sync_targets {
                            if let Some(h) = handles.get(id) {
                                for local in &locals {
                                    h.upload(local.clone(), dir.clone());
                                }
                            }
                        }
                    }
                });
            },
        );
    }
}
