use super::*;

pub(super) fn wire_seed_callbacks(window: &AppWindow, ctx: &AppContext) {
    let AppContext {
        store,
        bufs,
        layout,
        panes_model,
        splitters_model,
        terminals_model,
        tabs_model,
        sessions_model,
        content_size,
        sftp_follow_cd,
        ..
    } = ctx;

    {
        let weak = window.as_weak();
        let r_store = store.clone();
        let r_bufs = bufs.clone();
        let r_refs = ResetRefs {
            sftp_follow_cd: sftp_follow_cd.clone(),
            // 外观页还原可能改变深浅色（默认壁纸是暗色的），已打开的进程监视窗
            // 要跟着换肤 —— 与用户手动选壁纸走同一条同步路径。
        };
        window.on_reset_page(move |page: slint::SharedString| {
            let Some(w) = weak.upgrade() else { return };
            reset_page(&w, &r_store, &r_bufs, &r_refs, page.as_str());
        });
    }

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
            s.save_logging();
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
            s.save_logging();
            if let Some(w) = weak.upgrade() {
                w.set_wsl_profiles(wsl_profile_model(&s));
                sync_sessions_to_model(&s, &sessions_model);
            }
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
                    &layout,
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }
        });
    }

    // Per-session SFTP state: collapse + sizes live in each tab's TerminalState so
    // split panes / other tabs each keep their own (resizing/collapsing one no
    // longer bleeds onto the rest) (#v0.5).
    {
        let terminals_model = terminals_model.clone();
        window.on_set_pane_sftp_collapsed(move |tab_id: SharedString, v: bool| {
            let id = tab_id.to_string();
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_collapsed = v);
            // 折叠 5 秒后断开；窗口内展开取消。
            if v {
                crate::sftp::schedule_sftp_release(&id);
            } else {
                crate::sftp::cancel_sftp_release(&id);
            }
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
}
