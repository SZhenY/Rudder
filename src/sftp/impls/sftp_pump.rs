use super::*;

pub(crate) async fn run_sftp(
    session: Session,
    jump: Option<Session>,
    mut commands: UnboundedReceiver<SftpCommand>,
    self_tx: UnboundedSender<SftpCommand>,
    events: UnboundedSender<SessionEvent>,
) -> Result<()> {
    let _ = events.send(SessionEvent::SftpStatus(
        t("SFTP 连接中...", "SFTP connecting...").into(),
    ));

    // Open a dedicated SSH connection for SFTP. Share the terminal's config so
    // keepalive (#160) and legacy algorithm fallbacks (#172) can't drift apart.
    let config = crate::ssh::ssh_client_config();

    let addr = format!("{}:{}", session.host, session.port);
    // Isolate external-editor files by connection and keep the server in the
    // visible local filename. Different hosts (or concurrent edit sessions)
    // can therefore open the same remote basename without sharing one temp
    // file or editor document (#318).
    let external_edit_prefix = sanitize_filename(&session.host);
    // 随机后缀 + 0700 + 创建即独占。老写法是 `/tmp/rudder/<host>-<port>-<uuid>`：
    // 叶子名虽然随机，父目录 `/tmp/rudder` 是固定名字，共享机器上别人可以先在那里
    // 摆一个符号链接，把随后写下的远端文件内容引到任意位置。
    let external_edit_dir = crate::config::create_private_temp_dir(&format!(
        "rudder-sftp-{}-{}",
        external_edit_prefix, session.port
    ))?;
    // Keep the jump-host connection alive for the whole SFTP session — the
    // direct-tcpip tunnel rides on it (#211). Declared here so it lives to the
    // end of the function; `_`-prefixed so it isn't flagged unused.
    let mut _jump_keepalive;
    // Tunnel through an SSH jump host (#211), the same proxy as the shell (#7),
    // or connect directly.
    let mut handle = match &jump {
        Some(j) => {
            let (h, jh) = crate::ssh::connect_target_via_jump(
                j,
                &session.host,
                session.port,
                config.clone(),
                sftp_handler(&session, &events),
                &events,
                // SFTP 在 shell 连上之后才连，主机密钥那时已被记住。
                false,
            )
            .await
            .with_context(|| format!("sftp connect {} via jump failed", addr))?;
            _jump_keepalive = Some(jh);
            h
        }
        None => {
            _jump_keepalive = None;
            match crate::ssh::proxy::resolve_session(&session) {
                Some(p) => {
                    let stream = crate::ssh::proxy::connect(&p, &session.host, session.port)
                        .await
                        .with_context(|| format!("sftp proxy connect {} failed", addr))?;
                    client::connect_stream(config.clone(), stream, sftp_handler(&session, &events))
                        .await
                        .with_context(|| format!("sftp connect {} failed", addr))?
                }
                None => client::connect(
                    config.clone(),
                    addr.as_str(),
                    sftp_handler(&session, &events),
                )
                .await
                .with_context(|| format!("sftp connect {} failed", addr))?,
            }
        }
    };

    // Resolve missing username/password (shares the shell's prompt; the UI
    // de-dupes by session id so SFTP doesn't prompt a second time) (#110).
    let (user, password) = match crate::ssh::resolve_credentials(&session, &events).await {
        Some(c) => c,
        None => return Err(anyhow!(t("已取消登录", "login cancelled"))),
    };

    // --- Authenticate (same method as the shell session) -------------------
    let authed = match session.auth {
        AuthMethod::Password => {
            let mut ok = handle
                .authenticate_password(&user, password.as_str())
                .await
                .context("sftp password auth failed")?
                .success();
            if !ok {
                // Match the shell session's fallback: russh can hang if a second
                // auth method is attempted on the same failed handle, so reconnect
                // before trying keyboard-interactive (#86, #186).
                let _ = handle.disconnect(Disconnect::ByApplication, "", "").await;
                handle = match &jump {
                    Some(j) => {
                        let (h, jh) = crate::ssh::connect_target_via_jump(
                            j,
                            &session.host,
                            session.port,
                            config.clone(),
                            sftp_handler(&session, &events),
                            &events,
                            false,
                        )
                        .await
                        .with_context(|| format!("sftp reconnect {} via jump failed", addr))?;
                        _jump_keepalive = Some(jh);
                        h
                    }
                    None => {
                        _jump_keepalive = None;
                        match crate::ssh::proxy::resolve_session(&session) {
                            Some(p) => {
                                let stream =
                                    crate::ssh::proxy::connect(&p, &session.host, session.port)
                                        .await
                                        .with_context(|| {
                                            format!("sftp proxy reconnect {} failed", addr)
                                        })?;
                                client::connect_stream(
                                    config.clone(),
                                    stream,
                                    sftp_handler(&session, &events),
                                )
                                .await
                                .with_context(|| format!("sftp reconnect {} failed", addr))?
                            }
                            None => client::connect(
                                config.clone(),
                                addr.as_str(),
                                sftp_handler(&session, &events),
                            )
                            .await
                            .with_context(|| format!("sftp reconnect {} failed", addr))?,
                        }
                    }
                };
                ok = crate::ssh::keyboard_interactive_auth(
                    &mut handle,
                    &user,
                    password.as_str(),
                    &session.id,
                    &session.host,
                    &events,
                )
                .await
                .context("sftp keyboard-interactive auth failed")?;
            }
            ok
        }
        AuthMethod::KeyboardInteractive => crate::ssh::keyboard_interactive_auth(
            &mut handle,
            &user,
            password.as_str(),
            &session.id,
            &session.host,
            &events,
        )
        .await
        .context("sftp keyboard-interactive auth failed")?,
        AuthMethod::Key => {
            // An encrypted private key needs its passphrase; reuse the session's
            // password field for it (empty = unencrypted), exactly like the shell
            // session does — otherwise a passphrase-protected key authenticates the
            // shell but fails SFTP with "the key is encrypted" (#133).
            let pass = password.as_str();
            let keypair = crate::ssh::load_session_private_key(&session, pass)?;
            // RSA keys need an explicit SHA-2 hash; other key types don't.
            let hash = keypair.algorithm().is_rsa().then_some(HashAlg::Sha256);
            let key_with_hash = PrivateKeyWithHashAlg::new(Arc::new(keypair), hash);
            handle
                .authenticate_publickey(&user, key_with_hash)
                .await
                .context("sftp publickey auth failed")?
                .success()
        }
    };

    if !authed {
        return Err(anyhow!(t("SFTP 认证失败", "SFTP authentication failed")));
    }

    // --- Open the sftp subsystem channel -----------------------------------
    let channel = handle
        .channel_open_session()
        .await
        .context("open sftp channel")?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("request sftp subsystem")?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .context("sftp handshake")?;
    // Share the session + connection so transfers can run on their own task,
    // leaving the command loop free to list/switch directories meanwhile (#116-2).
    let sftp = std::sync::Arc::new(sftp);
    let handle = std::sync::Arc::new(handle);

    // Per-transfer cancel flags, keyed by transfer id. A download task registers
    // its flag here; a CancelTransfer command flips it; the task removes it on
    // exit (#100 cancel download).
    let cancels: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Resolve the home directory and do an initial listing.
    let home = sftp
        .canonicalize(".")
        .await
        .unwrap_or_else(|_| "/".to_string());
    let _ = events.send(SessionEvent::SftpStatus(format!(
        "{} {}...",
        t("SFTP 加载", "SFTP loading"),
        home
    )));
    match list_dir_impl(&sftp, &home).await {
        Ok(entries) => {
            let _ = events.send(SessionEvent::SftpEntries {
                path: home.clone(),
                entries,
            });
            let _ = events.send(SessionEvent::SftpStatus(home.clone()));
        }
        Err(e) => {
            let _ = events.send(SessionEvent::SftpError(list_error_msg(&home, &e)));
        }
    }

    // --- Directory tree initialization -------------------------------------
    // tree_dirs: path -> [(child_name, child_full_path)] for directories only
    // tree_expanded: set of paths currently shown as expanded
    let mut tree_dirs: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();
    let mut tree_expanded: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Fetch root "/" subdirs, then expand path down to home.
    let root_dirs = list_dirs_only_impl(&sftp, "/").await.unwrap_or_default();
    tree_dirs.insert("/".to_string(), root_dirs);
    tree_expanded.insert("/".to_string());

    // Walk each path segment from "/" toward home, expanding as we go.
    if home != "/" {
        let mut current = "/".to_string();
        for segment in home.trim_start_matches('/').split('/') {
            if segment.is_empty() {
                continue;
            }
            let child = format!("{}/{}", current.trim_end_matches('/'), segment);
            // Only expand if this child appeared in the parent listing.
            let found = tree_dirs
                .get(&current)
                .map(|c| c.iter().any(|(_, p)| p == &child))
                .unwrap_or(false);
            if !found {
                break;
            }
            let dirs = list_dirs_only_impl(&sftp, &child).await.unwrap_or_default();
            tree_dirs.insert(child.clone(), dirs);
            tree_expanded.insert(child.clone());
            current = child;
        }
    }
    {
        let mut nodes = Vec::new();
        build_tree_nodes("/", 0, &tree_expanded, &tree_dirs, &mut nodes);
        let _ = events.send(SessionEvent::SftpTreeUpdate(nodes));
    }

    // --- Command loop -------------------------------------------------------
    while let Some(cmd) = commands.recv().await {
        match cmd {
            SftpCommand::Close => break,

            SftpCommand::ListDir(path) => {
                emit_panel_dir(&sftp, &events, &path).await;
            }

            // MCP 的三条直连命令：不经过界面、不产生面板事件，结果直接回给调用方。
            SftpCommand::McpList { path, reply } => {
                let result = list_dir_impl(&sftp, &path)
                    .await
                    .map_err(|err| format!("{err:#}"));
                let _ = reply.send(result);
            }
            // ── MCP 变更类：带回执。只刷面板列表、不动左树（下次 Refresh 即修正） ──
            SftpCommand::McpMkDir { path, reply } => {
                let result = sftp
                    .create_dir(&path)
                    .await
                    .map_err(|err| format!("mkdir {path}: {err}"));
                if result.is_ok() {
                    refresh_panel_dir(&sftp, &events, &parent_dir(&path)).await;
                }
                let _ = reply.send(result);
            }
            SftpCommand::McpRemove {
                path,
                recursive,
                reply,
            } => {
                let result = async {
                    // 目录还是文件得先看权限位：SFTP 的 remove_file 删不掉目录。
                    let is_dir = sftp
                        .metadata(&path)
                        .await
                        .ok()
                        .and_then(|meta| meta.permissions)
                        .map(|mode| mode & 0o170_000 == 0o040_000)
                        .unwrap_or(false);
                    if !is_dir {
                        return sftp
                            .remove_file(&path)
                            .await
                            .map_err(|err| format!("remove {path}: {err}"));
                    }
                    if recursive {
                        remove_dir_recursive(&sftp, &path)
                            .await
                            .map_err(|err| format!("{err:#}"))
                    } else {
                        sftp.remove_dir(&path).await.map_err(|err| {
                            format!(
                                "remove dir {path}: {err} \
                                 (pass recursive=true to delete a non-empty directory)"
                            )
                        })
                    }
                }
                .await;
                if result.is_ok() {
                    refresh_panel_dir(&sftp, &events, &parent_dir(&path)).await;
                }
                let _ = reply.send(result);
            }
            SftpCommand::McpRename { from, to, reply } => {
                let result = sftp
                    .rename(&from, &to)
                    .await
                    .map_err(|err| format!("rename {from} -> {to}: {err}"));
                if result.is_ok() {
                    refresh_panel_dir(&sftp, &events, &parent_dir(&from)).await;
                    if parent_dir(&from) != parent_dir(&to) {
                        refresh_panel_dir(&sftp, &events, &parent_dir(&to)).await;
                    }
                }
                let _ = reply.send(result);
            }
            SftpCommand::McpChmod { path, mode, reply } => {
                let attrs = FileAttributes {
                    permissions: Some(mode),
                    ..Default::default()
                };
                let result = sftp
                    .set_metadata(&path, attrs)
                    .await
                    .map_err(|err| format!("chmod {path}: {err}"));
                if result.is_ok() {
                    refresh_panel_dir(&sftp, &events, &parent_dir(&path)).await;
                }
                let _ = reply.send(result);
            }
            SftpCommand::McpDownloadAuto { remote, dir, reply } => {
                // 与界面手动下载同源：流式分块 + 进度事件 + 可取消。整块 `read` 既把
                // 整个文件塞进内存，又在传输期间占住命令循环（列目录 / 切目录都得排队）。
                let sftp = sftp.clone();
                let handle = handle.clone();
                let events = events.clone();
                let id = Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                // 传输列表的「取消」只按 id 广播给所有句柄，不登记就没人响应 (#100)。
                cancels
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id.clone(), cancel.clone());
                let cancels_done = cancels.clone();
                tokio::spawn(async move {
                    let result = async {
                        // 先量尺寸：超限要在开传之前拒掉，而不是下完再丢。
                        let size = sftp
                            .metadata(&remote)
                            .await
                            .map_err(|err| format!("stat {remote}: {err}"))?
                            .size
                            .unwrap_or(0);
                        if size > crate::sftp::MCP_TRANSFER_LIMIT as u64 {
                            return Err(format!(
                                "{remote} is larger than the {} MiB MCP limit; use the SFTP panel",
                                crate::sftp::MCP_TRANSFER_LIMIT / (1024 * 1024)
                            ));
                        }
                        tokio::fs::create_dir_all(&dir)
                            .await
                            .map_err(|err| format!("create {}: {err}", dir.display()))?;
                        let requested = download_target_path(&remote, &dir.to_string_lossy());
                        // MCP 没有「覆盖 / 保留两者」这个选项，一律避让成 `name (1).ext`。
                        let candidate = available_download_path(&requested);
                        let renamed = candidate != requested;
                        let name = candidate
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| base_name(&remote));
                        let local = candidate.to_string_lossy().to_string();
                        match download_impl(&handle, &remote, &local, &name, &id, &events, &cancel)
                            .await
                        {
                            Ok(true) => {
                                // `download_impl` 不回报字节数，落盘长度才是实际写入量。
                                let bytes = tokio::fs::metadata(&local)
                                    .await
                                    .map(|m| m.len())
                                    .unwrap_or(size);
                                Ok((candidate, renamed, bytes))
                            }
                            // 取消已由 `download_impl` 发过 state 4，再补 state 2 会把它
                            // 在列表里翻成「失败」。
                            Ok(false) => Err(format!("download of {remote} was cancelled")),
                            Err(err) => {
                                emit_transfer(TransferEmit {
                                    events: &events,
                                    id: &id,
                                    name: &name,
                                    is_upload: false,
                                    transferred: 0,
                                    total: 0,
                                    state: 2,
                                    msg: &err.to_string(),
                                });
                                Err(format!("{err:#}"))
                            }
                        }
                    }
                    .await;
                    let _ = reply.send(result);
                    cancels_done
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                });
            }
            SftpCommand::McpUpload {
                local,
                remote,
                reply,
            } => {
                // 同上下载：走界面上传的流水线实现，进度与取消都与手动上传一致。
                let handle = handle.clone();
                let events = events.clone();
                let id = Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                cancels
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id.clone(), cancel.clone());
                let cancels_done = cancels.clone();
                tokio::spawn(async move {
                    let result = async {
                        let size = tokio::fs::metadata(&local)
                            .await
                            .map_err(|err| format!("stat {}: {err}", local.display()))?
                            .len();
                        if size > crate::sftp::MCP_TRANSFER_LIMIT as u64 {
                            return Err(format!(
                                "{} is larger than the {} MiB MCP limit; use the SFTP panel",
                                local.display(),
                                crate::sftp::MCP_TRANSFER_LIMIT / (1024 * 1024)
                            ));
                        }
                        let name = local
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "upload".to_string());
                        match upload_pipelined(
                            &handle, &local, &remote, &name, &id, &events, &cancel,
                        )
                        .await
                        {
                            Ok(true) => Ok(size),
                            Ok(false) => {
                                Err(format!("upload of {} was cancelled", local.display()))
                            }
                            Err(err) => {
                                emit_transfer(TransferEmit {
                                    events: &events,
                                    id: &id,
                                    name: &name,
                                    is_upload: true,
                                    transferred: 0,
                                    total: 0,
                                    state: 2,
                                    msg: &err.to_string(),
                                });
                                Err(format!("{err:#}"))
                            }
                        }
                    }
                    .await;
                    let _ = reply.send(result);
                    cancels_done
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                });
            }

            SftpCommand::RefreshDir(path) => {
                // File panel — same as ListDir（共用 emit_panel_dir）。
                emit_panel_dir(&sftp, &events, &path).await;
                // Tree — re-fetch every currently-expanded directory so deleted /
                // created folders sync without a reconnect (#189). Stale entries
                // whose parent no longer lists them are simply never walked by
                // build_tree_nodes, so they drop out on the rebuild.
                let expanded: Vec<String> = tree_expanded.iter().cloned().collect();
                for dir in expanded {
                    let dirs = list_dirs_only_impl(&sftp, &dir).await.unwrap_or_default();
                    tree_dirs.insert(dir, dirs);
                }
                emit_tree(&tree_dirs, &tree_expanded, &events);
            }

            SftpCommand::ToggleTreeNode(path) => {
                if tree_expanded.contains(&path) {
                    // Collapse this node and all descendants.
                    let prefix = format!("{}/", path.trim_end_matches('/'));
                    tree_expanded.retain(|p| p != &path && !p.starts_with(&prefix));
                } else {
                    // Expand: fetch children if not yet cached.
                    if !tree_dirs.contains_key(&path) {
                        let dirs = list_dirs_only_impl(&sftp, &path).await.unwrap_or_default();
                        tree_dirs.insert(path.clone(), dirs);
                    }
                    tree_expanded.insert(path.clone());
                }
                let mut nodes = Vec::new();
                build_tree_nodes("/", 0, &tree_expanded, &tree_dirs, &mut nodes);
                let _ = events.send(SessionEvent::SftpTreeUpdate(nodes));
            }

            SftpCommand::Download {
                remote,
                local_dir,
                conflict,
            } => {
                // Run on its own task so the command loop stays free to list /
                // switch directories during the transfer (#116-2).
                let sftp = sftp.clone();
                let handle = handle.clone();
                let events = events.clone();
                // Register a cancel flag up-front under the file id, so a
                // CancelTransfer arriving mid-download can flip it (#100).
                let file_id = Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                // A poisoned mutex only marks the map as suspect — the entries
                // themselves are fine, so take them instead of panicking. Under
                // `panic = "abort"` (release) the old `unwrap()` would have taken
                // the whole client down; the other `cancels` uses below already
                // recover this way.
                cancels
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(file_id.clone(), cancel.clone());
                let cancels_done = cancels.clone();
                tokio::spawn(async move {
                    // A directory target → recursively mirror the whole tree (#50).
                    let is_dir = sftp
                        .metadata(&remote)
                        .await
                        .ok()
                        .map(|m| (m.permissions.unwrap_or(0) & 0o170_000) == 0o040_000)
                        .unwrap_or(false);
                    if is_dir {
                        let dirname = base_name(&remote);
                        // #100.3: an empty folder downloads nothing — just say so
                        // rather than silently creating an empty local directory.
                        let empty = list_dir_impl(&sftp, &remote)
                            .await
                            .map(|e| e.is_empty())
                            .unwrap_or(false);
                        if empty {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("空文件夹", "Empty folder"),
                                dirname
                            )));
                            return;
                        }
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{} {}/...",
                            t("下载文件夹", "Downloading folder"),
                            dirname
                        )));
                        match download_dir(
                            &sftp, &handle, &remote, &local_dir, &events, &file_id, &cancel,
                        )
                        .await
                        {
                            Ok(_) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("下载完成", "Downloaded"),
                                    dirname
                                )));
                            }
                            Err(e) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("下载失败", "Download failed")
                                )));
                            }
                        }
                    } else {
                        // Sanitize the server-supplied name before it touches the local
                        // filesystem (#26): a malicious server could otherwise craft a
                        // name with traversal, shell-special chars or a Windows reserved
                        // device name to write outside the chosen dir or hit a device.
                        let target = download_target_path(&remote, &local_dir);
                        // "Keep both" writes to a numbered sibling so the
                        // existing local file survives the download.
                        let local_path = if conflict == DownloadConflict::KeepBoth {
                            available_download_path(&target)
                        } else {
                            target
                        };
                        let filename = local_path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| base_name(&remote));
                        let local_path = local_path.to_string_lossy().to_string();
                        let id = file_id.clone();
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{} {}...",
                            t("下载", "Downloading"),
                            filename
                        )));
                        match download_impl(
                            &handle,
                            &remote,
                            &local_path,
                            &filename,
                            &id,
                            &events,
                            &cancel,
                        )
                        .await
                        {
                            Ok(true) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("下载完成", "Downloaded"),
                                    filename
                                )));
                            }
                            Ok(false) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("已取消", "Cancelled"),
                                    filename
                                )));
                            }
                            Err(e) => {
                                emit_transfer(TransferEmit {
                                    events: &events,
                                    id: &id,
                                    name: &filename,
                                    is_upload: false,
                                    transferred: 0,
                                    total: 0,
                                    state: 2,
                                    msg: &e.to_string(),
                                });
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("下载失败", "Download failed")
                                )));
                            }
                        }
                    }
                    cancels_done
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&file_id);
                });
            }

            SftpCommand::DownloadArchive {
                remote_dir,
                names,
                local_dir,
            } => {
                // #100: multi-select download. Instead of N concurrent transfers
                // (which raced and dropped files), tar everything into ONE archive
                // on the remote, pull that single file, then delete the temp.
                let sftp = sftp.clone();
                let handle = handle.clone();
                let events = events.clone();
                // Register a cancel flag up-front so CancelTransfer can flip it (#100).
                let id = Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                cancels
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id.clone(), cancel.clone());
                let cancels_done = cancels.clone();
                tokio::spawn(async move {
                    let n = names.len();
                    let tmp = format!("/tmp/meatshell-{}.tar", Uuid::new_v4());
                    // Name the archive after the first item's stem, per the user:
                    // 11.txt → "11等文件.tar". Sanitize since names come from the server.
                    let first = names.first().map(|s| s.as_str()).unwrap_or("download");
                    let stem = first
                        .rsplit_once('.')
                        .map(|(a, _)| a)
                        .filter(|a| !a.is_empty())
                        .unwrap_or(first);
                    let arc_name =
                        sanitize_filename(&format!("{}{}.tar", stem, t("等文件", "-and-more")));
                    let local_path = format!("{}/{}", local_dir.trim_end_matches('/'), arc_name);
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{} {} {}...",
                        t("打包下载", "Archiving"),
                        n,
                        t("项", "items")
                    )));
                    // Show a "preparing" row in the transfer panel right away so a
                    // big selection isn't a silent wait while tar runs (#100). The
                    // download then reuses this same id, so the row turns into the
                    // live progress bar once bytes start flowing.
                    emit_transfer(TransferEmit {
                        events: &events,
                        id: &id,
                        name: &arc_name,
                        is_upload: false,
                        transferred: 0,
                        total: 0,
                        state: 3,
                        msg: "",
                    });
                    // Plain tar (no gzip): the user prefers speed over a smaller file.
                    // Server-supplied names are untrusted → quote every argument.
                    let mut cmd =
                        format!("tar -cf {} -C {}", sh_quote(&tmp), sh_quote(&remote_dir));
                    for nm in &names {
                        cmd.push(' ');
                        cmd.push_str(&sh_quote(nm));
                    }
                    let _ = &sftp; // listing session kept alive; transfer uses `handle`
                    let res: Result<bool> = async {
                        let st = exec_remote(&handle, &cmd).await.context("tar on remote")?;
                        if st != 0 {
                            return Err(anyhow!(t("远端 tar 打包失败", "remote tar failed")));
                        }
                        download_impl(&handle, &tmp, &local_path, &arc_name, &id, &events, &cancel)
                            .await
                    }
                    .await;
                    // Best-effort cleanup of the remote temp tar — success, failure
                    // or cancel all reach here, so no junk is left on the server (#100).
                    let _ = exec_remote(&handle, &format!("rm -f {}", sh_quote(&tmp))).await;
                    match res {
                        Ok(true) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("下载完成", "Downloaded"),
                                arc_name
                            )));
                        }
                        Ok(false) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("已取消", "Cancelled"),
                                arc_name
                            )));
                        }
                        Err(e) => {
                            emit_transfer(TransferEmit {
                                events: &events,
                                id: &id,
                                name: &arc_name,
                                is_upload: false,
                                transferred: 0,
                                total: 0,
                                state: 2,
                                msg: &e.to_string(),
                            });
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}",
                                t("下载失败", "Download failed")
                            )));
                        }
                    }
                    cancels_done
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                });
            }

            SftpCommand::CancelTransfer(id) => {
                if let Some(flag) = cancels.lock().unwrap_or_else(|e| e.into_inner()).get(&id) {
                    flag.store(true, Ordering::Relaxed);
                }
            }

            SftpCommand::Upload {
                local,
                remote_dir,
                cleanup_after,
            } => {
                // Run on its own task so the command loop stays free to list /
                // switch directories during the transfer (#116-2).
                let sftp = sftp.clone();
                let handle = handle.clone();
                let events = events.clone();
                // Register a cancel flag up-front under the file id so a
                // CancelTransfer arriving mid-upload can flip it (#100).
                let up_id = Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                // See the Download arm above: recover a poisoned map rather
                // than aborting the process.
                cancels
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(up_id.clone(), cancel.clone());
                let cancels_done = cancels.clone();
                tokio::spawn(async move {
                    // A directory source → recursively upload the whole tree (#50).
                    let is_dir = tokio::fs::metadata(&local)
                        .await
                        .map(|m| m.is_dir())
                        .unwrap_or(false);
                    if is_dir {
                        let dirname = match local_file_name_utf8(&local) {
                            Ok(name) => name,
                            Err(e) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("上传失败", "Upload failed")
                                )));
                                if let Some(path) = cleanup_after.as_deref() {
                                    cleanup_import_path(path).await;
                                }
                                cancels_done
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .remove(&up_id);
                                return;
                            }
                        };
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{} {}/...",
                            t("上传文件夹", "Uploading folder"),
                            dirname
                        )));
                        let res = upload_dir(
                            &handle,
                            &sftp,
                            &local,
                            &remote_dir,
                            &events,
                            &up_id,
                            &cancel,
                        )
                        .await;
                        if let Ok(entries) = list_dir_impl(&sftp, &remote_dir).await {
                            let _ = events.send(SessionEvent::SftpEntries {
                                path: remote_dir.clone(),
                                entries,
                            });
                        }
                        match res {
                            Ok(_) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("上传完成", "Uploaded"),
                                    dirname
                                )));
                            }
                            Err(e) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("上传失败", "Upload failed")
                                )));
                            }
                        }
                    } else {
                        let filename = match local_file_name_utf8(&local) {
                            Ok(name) => name,
                            Err(e) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("上传失败", "Upload failed")
                                )));
                                if let Some(path) = cleanup_after.as_deref() {
                                    cleanup_import_path(path).await;
                                }
                                cancels_done
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .remove(&up_id);
                                return;
                            }
                        };
                        let remote_path =
                            format!("{}/{}", remote_dir.trim_end_matches('/'), filename);
                        let id = up_id.clone();
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{} {}...",
                            t("上传", "Uploading"),
                            filename
                        )));
                        match upload_pipelined(
                            &handle,
                            &local,
                            &remote_path,
                            &filename,
                            &id,
                            &events,
                            &cancel,
                        )
                        .await
                        {
                            Ok(true) => {
                                if let Ok(entries) = list_dir_impl(&sftp, &remote_dir).await {
                                    let _ = events.send(SessionEvent::SftpEntries {
                                        path: remote_dir.clone(),
                                        entries,
                                    });
                                }
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("上传完成", "Uploaded"),
                                    filename
                                )));
                            }
                            Ok(false) => {
                                // Refresh the listing so the removed partial file disappears.
                                if let Ok(entries) = list_dir_impl(&sftp, &remote_dir).await {
                                    let _ = events.send(SessionEvent::SftpEntries {
                                        path: remote_dir.clone(),
                                        entries,
                                    });
                                }
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {}",
                                    t("已取消", "Cancelled"),
                                    filename
                                )));
                            }
                            Err(e) => {
                                emit_transfer(TransferEmit {
                                    events: &events,
                                    id: &id,
                                    name: &filename,
                                    is_upload: true,
                                    transferred: 0,
                                    total: 0,
                                    state: 2,
                                    msg: &e.to_string(),
                                });
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("上传失败", "Upload failed")
                                )));
                            }
                        }
                    }
                    if let Some(path) = cleanup_after.as_deref() {
                        cleanup_import_path(path).await;
                    }
                    cancels_done
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&up_id);
                });
            }

            SftpCommand::CopyTo {
                remotes,
                target,
                target_dir,
            } => {
                let sftp = sftp.clone();
                let handle = handle.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    let label = format!("{} {}", remotes.len(), t("项", "items"));
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{} {}...",
                        t("复制到其他会话", "Copying to another session"),
                        label
                    )));
                    for remote in remotes {
                        match stage_remote_for_copy(&sftp, &handle, &remote, &events).await {
                            Ok((local, cleanup_root)) => {
                                let _ = target.send(SftpCommand::Upload {
                                    local,
                                    remote_dir: target_dir.clone(),
                                    cleanup_after: Some(cleanup_root),
                                });
                            }
                            Err(e) => {
                                let _ = events.send(SessionEvent::SftpStatus(format!(
                                    "{}: {e}",
                                    t("复制失败", "Copy failed")
                                )));
                            }
                        }
                    }
                });
            }

            SftpCommand::Delete(path) => {
                let filename = base_name(&path);
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("删除", "Deleting"),
                    filename
                )));
                // Directories are removed recursively (a plain remove_dir only
                // works on an empty dir, so an uploaded folder couldn't be
                // deleted); files via remove_file.
                let is_dir = sftp
                    .metadata(&path)
                    .await
                    .ok()
                    .map(|m| (m.permissions.unwrap_or(0) & 0o170_000) == 0o040_000)
                    .unwrap_or(false);
                let res: Result<()> = if is_dir {
                    remove_dir_recursive(&sftp, &path).await
                } else {
                    sftp.remove_file(&path)
                        .await
                        .map(|_| ())
                        .map_err(|e| anyhow::anyhow!("{e}"))
                };
                match res {
                    Ok(_) => {
                        let parent = parent_dir(&path);
                        if let Ok(entries) = list_dir_impl(&sftp, &parent).await {
                            let _ = events.send(SessionEvent::SftpEntries {
                                path: parent.clone(),
                                entries,
                            });
                        }
                        // Keep the left directory tree in sync (#189): drop the
                        // deleted folder and any cached descendants, then re-list
                        // the parent's sub-dirs so the deleted node disappears
                        // without needing a reconnect.
                        let prefix = format!("{}/", path.trim_end_matches('/'));
                        tree_dirs.retain(|p, _| p != &path && !p.starts_with(&prefix));
                        tree_expanded.retain(|p| p != &path && !p.starts_with(&prefix));
                        sync_tree_dir(&sftp, &parent, &mut tree_dirs).await;
                        emit_tree(&tree_dirs, &tree_expanded, &events);
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已删除", "Deleted"),
                            filename
                        )));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("删除失败", "Delete failed")
                        )));
                    }
                }
            }

            SftpCommand::Rename { from, to } => {
                let refresh = parent_dir(&from);
                match sftp.rename(&from, &to).await {
                    Ok(_) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已重命名", "Renamed"),
                            base_name(&to)
                        )));
                        // Sync the left tree (#189): drop the old name + cached
                        // descendants, then re-list both the source and the
                        // destination parent (rename can also move across dirs).
                        let prefix = format!("{}/", from.trim_end_matches('/'));
                        tree_dirs.retain(|p, _| p != &from && !p.starts_with(&prefix));
                        tree_expanded.retain(|p| p != &from && !p.starts_with(&prefix));
                        sync_tree_dir(&sftp, &refresh, &mut tree_dirs).await;
                        let to_parent = parent_dir(&to);
                        if to_parent != refresh {
                            sync_tree_dir(&sftp, &to_parent, &mut tree_dirs).await;
                        }
                        emit_tree(&tree_dirs, &tree_expanded, &events);
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("重命名失败", "Rename failed")
                        )));
                    }
                }
                if let Ok(entries) = list_dir_impl(&sftp, &refresh).await {
                    let _ = events.send(SessionEvent::SftpEntries {
                        path: refresh,
                        entries,
                    });
                }
            }

            SftpCommand::Chmod { path, mode } => {
                let refresh = parent_dir(&path);
                let attrs = FileAttributes {
                    permissions: Some(mode),
                    ..Default::default()
                };
                match sftp.set_metadata(&path, attrs).await {
                    Ok(_) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {} → {:o}",
                            t("已修改权限", "Permissions changed"),
                            base_name(&path),
                            mode
                        )));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("修改权限失败", "chmod failed")
                        )));
                    }
                }
                if let Ok(entries) = list_dir_impl(&sftp, &refresh).await {
                    let _ = events.send(SessionEvent::SftpEntries {
                        path: refresh,
                        entries,
                    });
                }
            }

            SftpCommand::MkDir(path) => {
                let refresh = parent_dir(&path);
                match sftp.create_dir(&path).await {
                    Ok(_) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已新建文件夹", "Folder created"),
                            base_name(&path)
                        )));
                        // Show the new folder in the left tree too (#189).
                        sync_tree_dir(&sftp, &refresh, &mut tree_dirs).await;
                        emit_tree(&tree_dirs, &tree_expanded, &events);
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("新建文件夹失败", "Create folder failed")
                        )));
                    }
                }
                if let Ok(entries) = list_dir_impl(&sftp, &refresh).await {
                    let _ = events.send(SessionEvent::SftpEntries {
                        path: refresh,
                        entries,
                    });
                }
            }

            SftpCommand::TouchFile(path) => {
                let refresh = parent_dir(&path);
                // create() truncates if the file exists, so refuse to clobber.
                let exists = sftp.metadata(&path).await.is_ok();
                if exists {
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{}: {}",
                        t("文件已存在", "File already exists"),
                        base_name(&path)
                    )));
                } else {
                    match sftp.create(&path).await {
                        Ok(_) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("已新建文件", "File created"),
                                base_name(&path)
                            )));
                        }
                        Err(e) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}",
                                t("新建文件失败", "Create file failed")
                            )));
                        }
                    }
                }
                if let Ok(entries) = list_dir_impl(&sftp, &refresh).await {
                    let _ = events.send(SessionEvent::SftpEntries {
                        path: refresh,
                        entries,
                    });
                }
            }

            SftpCommand::UploadEdited { local, remote } => {
                let sftp = sftp.clone();
                let handle = handle.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    let filename = base_name(&remote);
                    let remote_dir = parent_dir(&remote);
                    let id = Uuid::new_v4().to_string();
                    let no_cancel = Arc::new(AtomicBool::new(false));
                    let result = upload_pipelined(
                        &handle, &local, &remote, &filename, &id, &events, &no_cancel,
                    )
                    .await;
                    match result {
                        Ok(true) => {
                            if let Ok(entries) = list_dir_impl(&sftp, &remote_dir).await {
                                let _ = events.send(SessionEvent::SftpEntries {
                                    path: remote_dir,
                                    entries,
                                });
                            }
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("已上传修改", "Re-uploaded changes"),
                                filename
                            )));
                        }
                        Ok(false) => {}
                        Err(e) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}",
                                t("上传修改失败", "Failed to re-upload changes")
                            )));
                        }
                    }
                });
            }

            SftpCommand::OpenTemp { remote, edit } => {
                // Sanitize the remote-controlled name before it becomes a local
                // file path that we later hand to the OS "open" call.
                let filename = sanitize_filename(&base_name(&remote));
                let tmp_dir = external_edit_dir.clone();
                let _ = tokio::fs::create_dir_all(&tmp_dir).await;
                let local = tmp_dir.join(external_edit_local_name(
                    &external_edit_prefix,
                    &filename,
                    &Uuid::new_v4().to_string(),
                ));
                let local_str = local.to_string_lossy().to_string();
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("打开", "Opening"),
                    filename
                )));
                let xid = Uuid::new_v4().to_string();
                let no_cancel = Arc::new(AtomicBool::new(false));
                match download_impl(
                    &handle, &remote, &local_str, &filename, &xid, &events, &no_cancel,
                )
                .await
                {
                    Ok(_) => {
                        open_with_os(&local_str);
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            if edit {
                                t("已打开编辑", "Opened for editing")
                            } else {
                                t("已打开", "Opened")
                            },
                            filename
                        )));
                        if edit {
                            spawn_edit_watcher(self_tx.clone(), local_str, remote.clone());
                        }
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("打开失败", "Open failed")
                        )));
                    }
                }
            }
            SftpCommand::ReadText { remote, edit } => {
                let name = base_name(&remote);
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("打开", "Opening"),
                    name
                )));
                let (content, error) = match read_text_guarded(&sftp, &remote).await {
                    Ok(text) => (text, String::new()),
                    Err(msg) => (String::new(), msg),
                };
                let _ = events.send(SessionEvent::SftpFileText {
                    path: remote,
                    name,
                    content,
                    edit,
                    error,
                });
            }
            SftpCommand::WriteText { remote, content } => {
                let name = base_name(&remote);
                match write_text_file(&sftp, &remote, &content).await {
                    Ok(_) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已保存", "Saved"),
                            name
                        )));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e:#}",
                            t("保存失败", "Save failed")
                        )));
                    }
                }
            }
        }
    }

    let _ = handle
        .disconnect(Disconnect::ByApplication, "bye", "")
        .await;

    // Clean up this session's external-editor scratch dir. Every connection
    // created a fresh UUID dir under $TMPDIR/rudder that previously lived on
    // until reboot — holding whatever remote files the user had opened
    // (#external-edit-cleanup).
    let _ = tokio::fs::remove_dir_all(&external_edit_dir).await;

    Ok(())
}
