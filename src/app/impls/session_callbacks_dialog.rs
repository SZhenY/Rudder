use super::*;

pub(super) fn wire_session_dialog(window: &AppWindow, ctx: &AppContext, scope: &SessionEditScope) {
    let AppContext {
        store,
        sessions_model,
        runtime,
        ..
    } = ctx;
    let edit_forwards = scope.edit_forwards.clone();
    let edit_triggers = scope.edit_triggers.clone();

    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        let edit_forwards = edit_forwards.clone();
        let edit_triggers = edit_triggers.clone();
        window.on_session_dialog_submit(move |draft: SessionDraft| {
            let id = draft.id.to_string();
            let forwards = match validated_port_forwards(&edit_forwards.borrow()) {
                Ok(forwards) => forwards,
                Err(message) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                    return;
                }
            };
            // Triggers keep their saved response when the box is left blank,
            // so the previously stored secrets must be passed along (#212).
            let saved_responses: Vec<Secret> = store
                .borrow()
                .get(&id)
                .map(|s| s.triggers.iter().map(|t| t.response.clone()).collect())
                .unwrap_or_default();
            let triggers = match validated_triggers(&edit_triggers.borrow(), &saved_responses) {
                Ok(triggers) => triggers,
                Err(message) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                    return;
                }
            };
            // The edit dialog never echoes the real password (issue #10): a blank
            // field while editing means "keep the existing password" rather than
            // "clear it".  Only overwrite when the user actually typed something.
            let password = if draft.password.is_empty() {
                store
                    .borrow()
                    .get(&id)
                    .map(|s| s.password.clone())
                    .unwrap_or_default()
            } else {
                Secret::new(draft.password.to_string())
            };
            let private_key_inline = if draft.private_key_inline_mode {
                if draft.private_key_inline.is_empty() {
                    store
                        .borrow()
                        .get(&id)
                        .map(|s| s.private_key_inline.clone())
                        .unwrap_or_default()
                } else {
                    Secret::new(draft.private_key_inline.to_string())
                }
            } else {
                Secret::default()
            };
            let private_key_path = if draft.private_key_inline_mode {
                String::new()
            } else {
                draft.private_key_path.to_string().replace('\\', "/")
            };
            let kind = crate::config::SessionKind::from_str(draft.kind.as_ref());
            // Auto-name: serial → port label; otherwise user@host, or just the
            // host when no username was given (#110).
            let auto_name = match kind {
                crate::config::SessionKind::Serial => {
                    format!("{} @{}", draft.serial_port, draft.baud_rate)
                }
                _ if draft.user.trim().is_empty() => draft.host.to_string(),
                _ => format!("{}@{}", draft.user, draft.host),
            };
            // Telnet defaults to port 23, SSH to 22; serial ignores port.
            let default_port = if kind == crate::config::SessionKind::Telnet {
                23
            } else {
                22
            };
            // 代理凭据：地址框里的 userinfo 与两个独立字段合成一份，
            // 密码统一落到 `proxy_pass`（加密存储），URL 里不再留明文。
            let (proxy, proxy_user, proxy_pass) = {
                let existing_session = store.borrow().get(&id).cloned();
                let (proxy_type, hostport) = split_proxy(draft.proxy.as_ref());
                compose_proxy(
                    &proxy_type,
                    &hostport,
                    draft.proxy_user.as_ref(),
                    draft.proxy_password.as_ref(),
                    existing_session.as_ref(),
                )
            };
            let new_session = Session {
                id,
                name: if draft.name.is_empty() {
                    auto_name
                } else {
                    draft.name.to_string()
                },
                host: draft.host.to_string(),
                port: if draft.port <= 0 {
                    default_port
                } else {
                    draft.port as u16
                },
                user: draft.user.to_string(),
                auth: AuthMethod::from_str(draft.auth.as_ref()),
                password,
                // Store the key path with forward slashes uniformly.
                private_key_path,
                private_key_inline,
                proxy,
                proxy_user,
                proxy_pass,
                last_used: None,
                // 分组名先归一空白：输入框里手打的 " Prod " 不该带空白进配置
                // （与 `normalize_move_group` / `add_group` 同一口径）。
                // ⚠️ 这段 draft→Session 与 `session_models::session_from_draft` 是两份重复实现。
                group: draft.group.trim().to_string(),
                kind,
                local_distribution: String::new(),
                local_working_dir: String::new(),
                serial_port: draft.serial_port.to_string(),
                baud_rate: if draft.baud_rate <= 0 {
                    115_200
                } else {
                    draft.baud_rate as u32
                },
                data_bits: draft.data_bits as u8,
                stop_bits: draft.stop_bits as u8,
                parity: draft.parity.to_string(),
                flow_control: draft.flow_control.to_string(),
                encoding: draft.encoding.to_string(),
                forwards,
                triggers,
                disable_shell_integration: draft.disable_shell_integration,
                note: draft.note.to_string(),
                jump_session_id: draft.jump_session_id.to_string(),
            };
            {
                let mut s = store.borrow_mut();
                s.upsert(new_session);
                if let Err(err) = s.save() {
                    tracing::warn!("failed to save config: {err:#}");
                }
            }
            sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
            if let Some(w) = weak.upgrade() {
                w.set_dialog_open(false);
            }
        });
    }

    // Test connection from the session dialog. SSH tests use the same handshake,
    // host-key verification, proxy/jump routing, and authentication as a real
    // terminal connection (#276). Telnet and serial retain reachability tests.
    {
        let weak = window.as_weak();
        let runtime = runtime.clone();
        let store = store.clone();
        let edit_forwards = edit_forwards.clone();
        window.on_session_dialog_test(move |draft: SessionDraft| {
            let kind = draft.kind.to_string();
            if kind == "serial" {
                let port_name = draft.serial_port.to_string();
                let baud = if draft.baud_rate <= 0 {
                    115_200
                } else {
                    draft.baud_rate as u32
                };
                let weak_done = weak.clone();
                runtime.spawn(async move {
                    let message = match tokio::task::spawn_blocking(move || {
                        serialport::new(&port_name, baud)
                            .timeout(std::time::Duration::from_millis(800))
                            .open()
                    })
                    .await
                    {
                        Ok(Ok(_)) => t("连接正常", "Connection OK").to_string(),
                        Ok(Err(e)) => format!("{}: {e}", t("连接失败", "Connection failed")),
                        Err(e) => format!("{}: {e}", t("连接失败", "Connection failed")),
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_done.upgrade() {
                            w.set_dialog_test_status(message.into());
                        }
                    });
                });
                return;
            }

            let existing = store.borrow().get(draft.id.as_str()).cloned();
            let forwards = match validated_port_forwards(&edit_forwards.borrow()) {
                Ok(forwards) => forwards,
                Err(message) => {
                    if let Some(w) = weak.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                    return;
                }
            };
            // Triggers are irrelevant to an authentication probe (and must not
            // block it with a validation error), so start from an empty set.
            let session = session_from_draft(&draft, existing.as_ref(), forwards, Vec::new());
            let weak_done = weak.clone();

            if kind == "ssh" {
                let jump = resolve_jump(&store, &session);
                let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
                runtime.spawn(async move {
                    let mut test = Box::pin(test_session_auth(session, jump, events_tx));
                    // sender 被丢弃后 `recv()` 会**立刻**再次返回 `None`。原来这里写
                    // `continue`，于是 `loop` 变成忙循环空转烧 CPU（C2.6）。改成用一个
                    // 守卫把该分支停用；`loop` 是表达式，不能直接 `break`（需要一个值）。
                    let mut events_open = true;
                    let result = loop {
                        tokio::select! {
                            result = &mut test => break result,
                            event = events_rx.recv(), if events_open => {
                                let Some(event) = event else {
                                    events_open = false;
                                    continue;
                                };
                                if matches!(
                                    event,
                                    SessionEvent::HostKeyPrompt { .. }
                                        | SessionEvent::CredentialPrompt { .. }
                                        | SessionEvent::MfaPrompt { .. }
                                ) {
                                    let weak_prompt = weak_done.clone();
                                    let _ = slint::invoke_from_event_loop(move || {
                                        let Some(w) = weak_prompt.upgrade() else { return };
                                        match event {
                                            SessionEvent::HostKeyPrompt {
                                                host,
                                                port,
                                                key_type,
                                                fingerprint,
                                                changed,
                                                responder,
                                            } => enqueue_hostkey_prompt(
                                                &w,
                                                host,
                                                port,
                                                key_type,
                                                fingerprint,
                                                changed,
                                                responder,
                                            ),
                                            SessionEvent::CredentialPrompt {
                                                session_id,
                                                host,
                                                user,
                                                need_user,
                                                need_password,
                                                responder,
                                            } => enqueue_cred_prompt(
                                                &w,
                                                session_id,
                                                host,
                                                user,
                                                need_user,
                                                need_password,
                                                responder,
                                            ),
                                            SessionEvent::MfaPrompt {
                                                session_id,
                                                host,
                                                prompt,
                                                echo,
                                                responder,
                                            } => enqueue_mfa_prompt(
                                                &w,
                                                session_id,
                                                host,
                                                prompt,
                                                echo,
                                                responder,
                                            ),
                                            _ => {}
                                        }
                                    });
                                }
                            }
                        }
                    };
                    let message = match result {
                        Ok(()) => t("连接正常", "Connection OK").to_string(),
                        Err(e) => format!("{}: {e:#}", t("连接失败", "Connection failed")),
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_done.upgrade() {
                            w.set_dialog_test_status(message.into());
                        }
                    });
                });
                return;
            }

            let host = session.host;
            let port = session.port;
            runtime.spawn(async move {
                let target = format!("{host}:{port}");
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    tokio::net::TcpStream::connect((host.as_str(), port)),
                )
                .await;
                let message = match result {
                    Ok(Ok(_)) => t("连接正常", "Connection OK").to_string(),
                    Ok(Err(e)) => format!("{}: {e}", t("连接失败", "Connection failed")),
                    Err(_) => format!("{}: {target}", t("连接超时", "Connection timed out")),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_done.upgrade() {
                        w.set_dialog_test_status(message.into());
                    }
                });
            });
        });
    }

    // Cancel dialog.
    {
        let weak = window.as_weak();
        window.on_session_dialog_cancel(move || {
            if let Some(w) = weak.upgrade() {
                w.set_dialog_open(false);
            }
        });
    }

    // Private-key file picker: pick the private key and store its path with
    // forward-slash separators (uniform across Windows/Linux; russh accepts them).
    {
        let weak = window.as_weak();
        window.on_session_dialog_pick_key(move || {
            let mut dialog =
                rfd::FileDialog::new().set_title(t("选择私钥文件", "Choose private key file"));
            // OpenSSH's standard macOS key names (id_ed25519, id_rsa, …) have
            // no extension. A native macOS extension filter makes those files
            // visible but disabled, so leave the picker unfiltered there (#325).
            // Other platforms retain the narrower existing filter.
            #[cfg(not(target_os = "macos"))]
            {
                dialog =
                    dialog.add_filter(t("SSH 私钥", "SSH private keys"), &["ppk", "pem", "key"]);
            }
            // Start in ~/.ssh if it exists.
            if let Some(home) = directories::UserDirs::new().map(|u| u.home_dir().join(".ssh"))
                && home.is_dir()
            {
                dialog = dialog.set_directory(home);
            }
            if let Some(file) = dialog.pick_file() {
                let path = file.to_string_lossy().replace('\\', "/");
                if let Some(w) = weak.upgrade() {
                    w.set_dialog_key_path(path.into());
                }
            }
        });
    }
}
