use super::*;

pub(super) fn wire_session_search(window: &AppWindow, ctx: &AppContext, scope: &SessionEditScope) {
    let AppContext {
        store,
        sessions_model,
        ..
    } = ctx;
    let edit_forwards = scope.edit_forwards.clone();
    let edit_triggers = scope.edit_triggers.clone();

    // Rebuild the session list as the user edits the Quick Connect search.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_host_search_changed(move |query| {
            if let Some(window) = weak.upgrade() {
                let query = if query.trim().is_empty() {
                    SharedString::new()
                } else {
                    query
                };
                window.set_host_search_query(query.clone());
                sync_sessions_to_model_with_filter(
                    &store.borrow(),
                    &sessions_model,
                    query.as_str(),
                );
            }
        });
    }

    // New session -> open dialog with blank draft.
    let weak = window.as_weak();
    let ef_new = edit_forwards.clone();
    let et_new = edit_triggers.clone();
    let store_ng = store.clone();
    window.on_new_session_clicked(move || {
        if let Some(w) = weak.upgrade() {
            *ef_new.borrow_mut() = vec![blank_forward_draft()];
            *et_new.borrow_mut() = vec![blank_trigger_draft()];
            w.set_session_groups(session_groups_model(&store_ng.borrow()));
            w.set_dialog_forwards(forward_model(&ef_new.borrow()));
            w.set_dialog_triggers(trigger_model(&et_new.borrow()));
            let empty = Session::new_empty();
            let (jump_labels, jump_ids, jump_idx) =
                jump_candidates(&store_ng.borrow(), &empty.id, "");
            w.set_jump_choices(jump_labels);
            w.set_jump_ids(jump_ids);
            w.set_dialog_jump_index(jump_idx);
            w.set_dialog_id(empty.id.into());
            w.set_dialog_name("".into());
            w.set_dialog_host("".into());
            w.set_dialog_port("22".into());
            // No default username (#110): leaving it blank makes the connect-time
            // prompt ask for it, Xshell-style.
            w.set_dialog_user("".into());
            w.set_dialog_auth("password".into());
            w.set_dialog_password("".into());
            w.set_dialog_key_path("".into());
            w.set_dialog_key_inline("".into());
            w.set_dialog_key_inline_mode(false);
            w.set_dialog_test_status("".into());
            w.set_dialog_proxy_type("none".into());
            w.set_dialog_proxy_hostport("".into());
            w.set_dialog_proxy_user("".into());
            w.set_dialog_proxy_password("".into());
            w.set_dialog_group("".into());
            w.set_dialog_kind("ssh".into());
            w.set_dialog_serial_port("".into());
            w.set_dialog_baud("115200".into());
            w.set_dialog_data_bits("8".into());
            w.set_dialog_stop_bits("1".into());
            w.set_dialog_parity("none".into());
            w.set_dialog_flow("none".into());
            w.set_dialog_encoding("UTF-8".into());
            w.set_dialog_disable_shell_integration(false);
            w.set_dialog_note("".into());
            w.set_dialog_editing(false);
            w.set_dialog_open(true);
        }
    });

    // Import hosts from ~/.ssh/config -> add them as sessions (skipping dups).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_import_ssh_config(move || {
            let hosts = crate::ssh::ssh_config::parse_default();
            let mut added = 0usize;
            if hosts.is_empty() {
                if let Some(w) = weak.upgrade() {
                    w.set_ssh_import_hint(
                        t("未找到 ~/.ssh/config", "no ~/.ssh/config found").into(),
                    );
                }
                return;
            }
            {
                let mut s = store.borrow_mut();
                for h in hosts {
                    // Skip if a session already has this alias, or the same
                    // host + user pair.
                    let dup = s
                        .sessions()
                        .iter()
                        .any(|x| x.name == h.alias || (x.host == h.hostname && x.user == h.user));
                    if dup {
                        continue;
                    }
                    let auth = if h.identity_file.is_empty() {
                        AuthMethod::Password
                    } else {
                        AuthMethod::Key
                    };
                    s.upsert(Session {
                        name: h.alias,
                        host: h.hostname,
                        port: h.port,
                        user: if h.user.is_empty() {
                            "root".into()
                        } else {
                            h.user
                        },
                        auth,
                        private_key_path: h.identity_file,
                        ..Session::new_empty()
                    });
                    added += 1;
                }
                if added > 0 {
                    s.save_logging();
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let hint = if added > 0 {
                    format!("{} {}", t("已导入", "imported"), added)
                } else {
                    t("没有新主机可导入", "no new hosts to import").to_string()
                };
                w.set_ssh_import_hint(hint.into());
            }
        });
    }

    // Export all sessions to a portable JSON file (issue #46). Passwords are
    // obfuscated with the built-in export key; host/user/port stay plaintext.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_export_sessions(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_file_name("rudder-connections.json")
                .add_filter("JSON", &["json"])
                .save_file()
            {
                let res = store.borrow().export_to(&path);
                if let Some(w) = weak.upgrade() {
                    let hint = match res {
                        Ok(n) => format!("{} {}", t("已导出连接", "exported"), n),
                        Err(e) => format!("{}: {}", t("导出失败", "export failed"), e),
                    };
                    w.set_ssh_import_hint(hint.into());
                }
            }
        });
    }

    // Batch-import connections from pasted text (#150). One per line:
    // `host|port|user|password|name` (trailing fields optional).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_batch_import_confirm(move |text: SharedString| {
            let parsed = parse_batch_import(text.as_str());
            let total = parsed.len();
            let mut added = 0usize;
            {
                let mut s = store.borrow_mut();
                for sess in parsed {
                    // Skip a host/user/port we already have.
                    let dup = s
                        .sessions()
                        .iter()
                        .any(|x| x.host == sess.host && x.user == sess.user && x.port == sess.port);
                    if dup {
                        continue;
                    }
                    s.upsert(sess);
                    added += 1;
                }
                if added > 0 {
                    s.save_logging();
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                let hint = if total == 0 {
                    t("没有可导入的连接", "nothing to import").to_string()
                } else if added > 0 {
                    format!("{} {}/{}", t("已导入", "imported"), added, total)
                } else {
                    t("没有新连接可导入(已存在)", "no new connections (all exist)").to_string()
                };
                w.set_ssh_import_hint(hint.into());
            }
        });
    }

    // Import sessions from a portable JSON file (issue #46).
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_import_sessions(move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
            {
                let res = store.borrow_mut().import_from(&path);
                if let Some(w) = weak.upgrade() {
                    let hint = match res {
                        Ok((added, skipped)) => {
                            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
                            format!(
                                "{} {} / {} {}",
                                t("已导入", "imported"),
                                added,
                                t("跳过重复", "skipped"),
                                skipped
                            )
                        }
                        Err(e) => format!("{}: {}", t("导入失败", "import failed"), e),
                    };
                    w.set_ssh_import_hint(hint.into());
                }
            }
        });
    }
}
