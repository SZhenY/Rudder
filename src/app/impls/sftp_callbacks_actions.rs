use super::*;

pub(super) fn wire_sftp_actions(window: &AppWindow, ctx: &AppContext) {
    let sftp_handles = ctx.sftp_handles.clone();

    // Context menu → 查看 (read-only) / 编辑 (editable). Both load the file's
    // text into the built-in editor instead of an external app (#70).
    // SFTP remote-to-remote copy (#203): stage through a local temp directory,
    // then upload into the target session's current SFTP directory.
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_copy_to_target(
            move |tab_id: SharedString, remote_path: SharedString, target_id: SharedString| {
                let Some(w) = weak.upgrade() else { return };
                let paths = vec![remote_path.to_string()];
                let dirs = terminal_sftp_paths(&w);
                let target_dir = dirs
                    .get(target_id.as_str())
                    .cloned()
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| "/".to_string());
                if let Ok(handles) = sftp_handles.lock() {
                    let Some(src) = handles.get(tab_id.as_str()) else {
                        return;
                    };
                    let Some(dst) = handles.get(target_id.as_str()) else {
                        return;
                    };
                    src.copy_to(paths, dst.commands.clone(), target_dir);
                    w.set_download_open(true);
                }
            },
        );
    }
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_copy_selected_to_target(
            move |tab_id: SharedString, target_id: SharedString| {
                let Some(w) = weak.upgrade() else { return };
                let terminals = w.get_terminals();
                let Some(tm) = terminals.as_any().downcast_ref::<VecModel<TerminalState>>() else {
                    return;
                };
                let paths = collect_sftp_selected(tm, tab_id.as_str());
                if paths.is_empty() {
                    return;
                }
                let dirs = terminal_sftp_paths(&w);
                let target_dir = dirs
                    .get(target_id.as_str())
                    .cloned()
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| "/".to_string());
                if let Ok(handles) = sftp_handles.lock() {
                    let Some(src) = handles.get(tab_id.as_str()) else {
                        return;
                    };
                    let Some(dst) = handles.get(target_id.as_str()) else {
                        return;
                    };
                    src.copy_to(paths, dst.commands.clone(), target_dir);
                    w.set_download_open(true);
                }
                clear_sftp_selection(tm, tab_id.as_str());
            },
        );
    }
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_view(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock()
                && let Some(h) = handles.get(tab_id.as_str())
            {
                h.read_text(path.to_string(), false);
            }
        });
    }
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_edit(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock()
                && let Some(h) = handles.get(tab_id.as_str())
            {
                h.read_text(path.to_string(), true);
            }
        });
    }
    // Open / edit with an external program (#81): download to a temp file and
    // hand it to the OS default app. Edit mode watches the temp copy and
    // re-uploads on every change.
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_open_external(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock()
                && let Some(h) = handles.get(tab_id.as_str())
            {
                h.open_temp(path.to_string(), false);
            }
        });
    }
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_edit_external(move |tab_id: SharedString, path: SharedString| {
            if let Ok(handles) = sftp_handles.lock()
                && let Some(h) = handles.get(tab_id.as_str())
            {
                h.open_temp(path.to_string(), true);
            }
        });
    }

    // Context-menu extensions (#69): one prompt dialog covers rename / chmod /
    // mkdir / touch; copy-path goes straight to the system clipboard.
    {
        let sftp_handles = sftp_handles.clone();
        window.on_sftp_prompt_submit(
            move |tab_id: SharedString,
                  kind: SharedString,
                  target: SharedString,
                  value: SharedString| {
                let value = value.to_string();
                let value = value.trim();
                if value.is_empty() {
                    return;
                }
                let target = target.to_string();
                let handles = match sftp_handles.lock() {
                    Ok(h) => h,
                    Err(_) => return,
                };
                let Some(h) = handles.get(tab_id.as_str()) else {
                    return;
                };
                match kind.as_str() {
                    "rename" => {
                        let to =
                            format!("{}/{}", parent_path(&target).trim_end_matches('/'), value);
                        h.rename(target, to);
                    }
                    "mkdir" => {
                        h.mkdir(format!("{}/{}", target.trim_end_matches('/'), value));
                    }
                    "touch" => {
                        h.touch(format!("{}/{}", target.trim_end_matches('/'), value));
                    }
                    _ => {}
                }
            },
        );
    }
    {
        window.on_sftp_copy_path(move |path: SharedString| {
            clipboard_set_text(path.to_string());
        });
    }

    // Visual chmod dialog (#84): decompose the current mode into nine bools on
    // open, recompose on apply (Slint has no bitwise ops).
    {
        let weak = window.as_weak();
        window.on_sftp_chmod_open(
            move |tab: SharedString, path: SharedString, name: SharedString, mode: i32| {
                let Some(w) = weak.upgrade() else { return };
                let m = mode as u32;
                w.set_chmod_tab(tab);
                w.set_chmod_path(path);
                w.set_chmod_name(name);
                w.set_chmod_or(m & 0o400 != 0);
                w.set_chmod_ow(m & 0o200 != 0);
                w.set_chmod_ox(m & 0o100 != 0);
                w.set_chmod_gr(m & 0o040 != 0);
                w.set_chmod_gw(m & 0o020 != 0);
                w.set_chmod_gx(m & 0o010 != 0);
                w.set_chmod_tr(m & 0o004 != 0);
                w.set_chmod_tw(m & 0o002 != 0);
                w.set_chmod_tx(m & 0o001 != 0);
                w.set_chmod_open(true);
            },
        );
    }
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_sftp_chmod_apply(move || {
            let Some(w) = weak.upgrade() else { return };
            let mode = (w.get_chmod_or() as u32) << 8
                | (w.get_chmod_ow() as u32) << 7
                | (w.get_chmod_ox() as u32) << 6
                | (w.get_chmod_gr() as u32) << 5
                | (w.get_chmod_gw() as u32) << 4
                | (w.get_chmod_gx() as u32) << 3
                | (w.get_chmod_tr() as u32) << 2
                | (w.get_chmod_tw() as u32) << 1
                | (w.get_chmod_tx() as u32);
            let path = w.get_chmod_path().to_string();
            let tab = w.get_chmod_tab().to_string();
            if let Ok(handles) = sftp_handles.lock()
                && let Some(h) = handles.get(&tab)
            {
                h.chmod(path, mode);
            }
        });
    }

    // Rebuild the editor's line-number gutter after each edit (#81). The text
    // comes straight from the TextInput so we don't re-read the property.
    {
        let weak = window.as_weak();
        window.on_editor_recount(move |text: SharedString| {
            if let Some(w) = weak.upgrade() {
                w.set_editor_line_numbers(line_numbers_for(text.as_str()).into());
            }
        });
    }
}
