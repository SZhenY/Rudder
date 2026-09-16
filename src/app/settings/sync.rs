//! 同步页：会话同步上传开关 + WebDAV 配置同步（手动上传/下载）。
//!
//! 该页按规格**没有「还原本页默认」**（全是账户与服务器配置），所以只有 `bind`。
//! WebDAV 的传送是手动触发的 —— 刻意不在启动时自动跑，避免误把本机配置推到远端。

use std::rc::Rc;

use slint::{ComponentHandle, SharedString, VecModel};

use super::{Store, persist};
use crate::app::sync_sessions_for_window;
use crate::app::webdav::{webdav_get_json, webdav_put_json};
use crate::i18n::t;
use crate::ui::{AppWindow, SessionInfo};

/// 播种 + 注册回调。
///
/// `sessions_model` 用于 WebDAV 下载后刷新欢迎页的会话列表。传 `&Rc<..>` 而不是
/// `&VecModel<..>`：`VecModel` 不是 `Clone`，对引用调 `.clone()` 得到的还是引用，
/// 被 `move` 闭包捕获会逃逸出函数体。
pub(crate) fn bind(window: &AppWindow, store: &Store, sessions_model: &Rc<VecModel<SessionInfo>>) {
    {
        let store = store.clone();
        window.on_set_sync_upload_enabled(move |v| {
            persist(&store, |s| {
                s.set_sync_upload(v);
            });
        });
    }

    {
        let store = store.clone();
        window.on_save_webdav_settings(
            move |enabled: bool,
                  url: SharedString,
                  username: SharedString,
                  password: SharedString,
                  remote_path: SharedString,
                  accept_invalid_certs: bool| {
                persist(&store, |s| {
                    s.set_webdav_settings(
                        enabled,
                        url.to_string(),
                        username.to_string(),
                        password.to_string(),
                        remote_path.to_string(),
                        accept_invalid_certs,
                    );
                });
            },
        );
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_webdav_upload(move || {
            let Some(w) = weak.upgrade() else { return };
            let enabled = w.get_webdav_enabled();
            let url = w.get_webdav_url().to_string();
            let username = w.get_webdav_username().to_string();
            let password = w.get_webdav_password().to_string();
            let remote_path = w.get_webdav_remote_path().to_string();
            let accept_invalid_certs = w.get_webdav_accept_invalid_certs();
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
            if !enabled {
                w.set_webdav_status(t("请先启用 WebDAV 同步", "enable WebDAV sync first").into());
                return;
            }
            let res = store.borrow().export_json().and_then(|(json, count)| {
                webdav_put_json(
                    &url,
                    &remote_path,
                    &username,
                    &password,
                    accept_invalid_certs,
                    json,
                )
                .map(|_| count)
            });
            let msg = match res {
                Ok(n) => format!("{} {}", t("已上传连接", "uploaded connections"), n),
                Err(e) => format!("{}: {}", t("上传失败", "upload failed"), e),
            };
            w.set_webdav_status(msg.into());
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_webdav_download(move || {
            let Some(w) = weak.upgrade() else { return };
            let enabled = w.get_webdav_enabled();
            let url = w.get_webdav_url().to_string();
            let username = w.get_webdav_username().to_string();
            let password = w.get_webdav_password().to_string();
            let remote_path = w.get_webdav_remote_path().to_string();
            let accept_invalid_certs = w.get_webdav_accept_invalid_certs();
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
            if !enabled {
                w.set_webdav_status(t("请先启用 WebDAV 同步", "enable WebDAV sync first").into());
                return;
            }
            let res = webdav_get_json(
                &url,
                &remote_path,
                &username,
                &password,
                accept_invalid_certs,
            )
            .and_then(|json| store.borrow_mut().import_json(&json));
            let msg = match res {
                Ok((added, skipped)) => {
                    sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
                    download_status_msg(added, skipped)
                }
                Err(e) => format!("{}: {}", t("下载失败", "download failed"), e),
            };
            w.set_webdav_status(msg.into());
        });
    }

    {
        let s = store.borrow();
        window.set_webdav_enabled(s.webdav_enabled());
        window.set_webdav_url(s.webdav_url().into());
        window.set_webdav_username(s.webdav_username().into());
        window.set_webdav_password(s.webdav_password().into());
        window.set_webdav_remote_path(s.webdav_remote_path().into());
        window.set_webdav_accept_invalid_certs(s.webdav_accept_invalid_certs());
        window.set_webdav_status(String::new().into());
    }

    window.set_sync_upload_enabled(store.borrow().sync_upload());
}
/// 下载结果的提示文案：「已导入 N, 跳过 M」。
fn download_status_msg(added: usize, skipped: usize) -> String {
    format!(
        "{} {}, {} {}",
        t("已导入", "imported"),
        added,
        t("跳过", "skipped"),
        skipped
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个计数**不能取错位**：写反了用户会把合并结果看反（以为没导入成功）。
    #[test]
    fn download_status_reports_both_counts_in_order() {
        let msg = download_status_msg(3, 5);
        assert!(msg.contains('3'), "缺导入数：{msg}");
        assert!(msg.contains('5'), "缺跳过数：{msg}");
        let first = msg.find('3').unwrap();
        let second = msg.find('5').unwrap();
        assert!(first < second, "格式是「已导入 N, 跳过 M」：{msg}");
        assert_eq!(
            download_status_msg(0, 0).matches('0').count(),
            2,
            "两个 0 都要出现"
        );
    }
}
