use super::*;

pub(super) fn wire_sync_transfer(window: &AppWindow, store: &Store) {
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_webdav_upload(move || {
            let Some(w) = weak.upgrade() else { return };
            let enabled = w.get_webdav_enabled();
            let url = w.get_webdav_url().to_string();
            let username = w.get_webdav_username().to_string();
            let remote_path = w.get_webdav_remote_path().to_string();
            let accept_invalid_certs = w.get_webdav_accept_invalid_certs();
            let password = effective_password(
                w.get_webdav_password().as_str(),
                store.borrow().webdav_password(),
            );
            {
                persist(&store, |s| {
                    s.set_webdav_settings(
                        enabled,
                        url.clone(),
                        username.clone(),
                        password.clone(),
                        remote_path.clone(),
                        accept_invalid_certs,
                    );
                });
            }
            // 输入框不再留着明文口令（已经存进配置了）。
            w.set_webdav_password(String::new().into());
            if !enabled {
                w.set_webdav_status(t("请先启用 WebDAV 同步", "enable WebDAV sync first").into());
                return;
            }
            // 导出在 UI 线程做（store 是 Rc<RefCell<..>>），但只是内存里的序列化，很快；
            // 网络请求挪到后台线程：超时 20 秒、失败还会重试，以前会把整个界面冻住。
            let (json, count) = match store.borrow().export_json() {
                Ok(v) => v,
                Err(e) => {
                    w.set_webdav_status(
                        format!("{}: {}", t("上传失败", "upload failed"), e).into(),
                    );
                    return;
                }
            };
            w.set_webdav_status(t("正在上传…", "Uploading…").into());
            let weak = weak.clone();
            std::thread::spawn(move || {
                let res = webdav_put_json(
                    &url,
                    &remote_path,
                    &username,
                    &password,
                    accept_invalid_certs,
                    json,
                );
                let msg = match res {
                    Ok(()) => format!("{} {}", t("已上传连接", "uploaded connections"), count),
                    Err(e) => format!("{}: {}", t("上传失败", "upload failed"), e),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_webdav_status(msg.into());
                    }
                });
            });
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_webdav_download(move || {
            let Some(w) = weak.upgrade() else { return };
            let enabled = w.get_webdav_enabled();
            let url = w.get_webdav_url().to_string();
            let username = w.get_webdav_username().to_string();
            let remote_path = w.get_webdav_remote_path().to_string();
            let accept_invalid_certs = w.get_webdav_accept_invalid_certs();
            let password = effective_password(
                w.get_webdav_password().as_str(),
                store.borrow().webdav_password(),
            );
            {
                persist(&store, |s| {
                    s.set_webdav_settings(
                        enabled,
                        url.clone(),
                        username.clone(),
                        password.clone(),
                        remote_path.clone(),
                        accept_invalid_certs,
                    );
                });
            }
            // 输入框不再留着明文口令（已经存进配置了）。
            w.set_webdav_password(String::new().into());
            if !enabled {
                w.set_webdav_status(t("请先启用 WebDAV 同步", "enable WebDAV sync first").into());
                return;
            }
            // 网络在后台线程；只有「写进 store + 刷新会话列表」回到 UI 线程做。
            // 回填闭包必须是 `Send`，所以 store 与模型都不能捕获 —— 走 `with_ui_handles` 取。
            w.set_webdav_status(t("正在下载…", "Downloading…").into());
            let weak = weak.clone();
            std::thread::spawn(move || {
                let fetched = webdav_get_json(
                    &url,
                    &remote_path,
                    &username,
                    &password,
                    accept_invalid_certs,
                );
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else { return };
                    let msg = match fetched {
                        Ok(json) => with_ui_handles(|store, sessions_model| {
                            // 先把结果取出来，别写成 `match store.borrow_mut()…`：
                            // 那样 `RefMut` 临时值会活到 match 结束，成功分支里的
                            // `store.borrow()` 会直接 BorrowMutError 崩掉。
                            let imported = store.borrow_mut().import_json(&json);
                            match imported {
                                Ok((added, skipped)) => {
                                    sync_sessions_for_window(
                                        &weak,
                                        &store.borrow(),
                                        sessions_model,
                                    );
                                    download_status_msg(added, skipped)
                                }
                                Err(e) => format!("{}: {}", t("下载失败", "download failed"), e),
                            }
                        })
                        .unwrap_or_else(|| {
                            format!(
                                "{}: {}",
                                t("下载失败", "download failed"),
                                t("内部状态未就绪", "internal state not ready")
                            )
                        }),
                        Err(e) => format!("{}: {}", t("下载失败", "download failed"), e),
                    };
                    w.set_webdav_status(msg.into());
                });
            });
        });
    }
}
