//! 同步页：会话同步上传开关 + WebDAV 配置同步（手动上传/下载）。
//!
//! 该页按规格**没有「还原本页默认」**（全是账户与服务器配置），所以只有 `bind`。
//! WebDAV 的传送是手动触发的 —— 刻意不在启动时自动跑，避免误把本机配置推到远端。

use std::rc::Rc;

use slint::{ComponentHandle, SharedString, VecModel};

use super::{Store, persist, register_ui_handles, with_ui_handles};
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
    // WebDAV 的网络请求在后台线程跑，回填时要在 UI 线程拿到同一个 store 与会话模型
    // （两者都是 `Rc`，捕获不进 `Send` 闭包）。
    register_ui_handles(store, sessions_model);
    {
        let store = store.clone();
        window.on_set_sync_upload_enabled(move |v| {
            persist(&store, |s| {
                s.set_sync_upload(v);
            });
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_save_webdav_settings(
            move |enabled: bool,
                  url: SharedString,
                  username: SharedString,
                  password: SharedString,
                  remote_path: SharedString,
                  accept_invalid_certs: bool| {
                let password = effective_password(password.as_str(), store.borrow().webdav_password());
                persist(&store, |s| {
                    s.set_webdav_settings(
                        enabled,
                        url.to_string(),
                        username.to_string(),
                        password,
                        remote_path.to_string(),
                        accept_invalid_certs,
                    );
                });
                // 存完就把输入框清空：口令不再以明文留在界面层里。
                if let Some(w) = weak.upgrade() {
                    w.set_webdav_password(String::new().into());
                }
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
            let remote_path = w.get_webdav_remote_path().to_string();
            let accept_invalid_certs = w.get_webdav_accept_invalid_certs();
            let password =
                effective_password(w.get_webdav_password().as_str(), store.borrow().webdav_password());
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
                    w.set_webdav_status(format!("{}: {}", t("上传失败", "upload failed"), e).into());
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
            let password =
                effective_password(w.get_webdav_password().as_str(), store.borrow().webdav_password());
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
                                    sync_sessions_for_window(&weak, &store.borrow(), sessions_model);
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

    {
        let s = store.borrow();
        window.set_webdav_enabled(s.webdav_enabled());
        window.set_webdav_url(s.webdav_url().into());
        window.set_webdav_username(s.webdav_username().into());
        // **刻意不回填口令**：回填等于把明文复制一份进界面层常驻（UI 里既没有零化，
        // 也没有 Debug 屏蔽）。框里留空按"沿用已存口令"处理，见 `effective_password`；
        // 保存 / 上传 / 下载之后也会把框清空。
        window.set_webdav_password(String::new().into());
        window.set_webdav_remote_path(s.webdav_remote_path().into());
        window.set_webdav_accept_invalid_certs(s.webdav_accept_invalid_certs());
        window.set_webdav_status(String::new().into());
    }

    window.set_sync_upload_enabled(store.borrow().sync_upload());
}
/// 口令框留空 = 沿用已存的口令。
///
/// 界面刻意**不**回填已存口令（那等于把明文复制进界面层常驻），所以空值必须解释成
/// "保持原样" —— 否则用户只是点一下上传 / 下载就会把口令清掉。
fn effective_password(typed: &str, stored: &str) -> String {
    if typed.trim().is_empty() {
        stored.to_string()
    } else {
        typed.to_string()
    }
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

    /// 口令框留空要沿用已存口令 —— 否则点一下上传就会把口令清掉；
    /// 空白（含空格）也算留空。
    #[test]
    fn blank_password_field_keeps_the_stored_one() {
        assert_eq!(effective_password("", "stored-token"), "stored-token");
        assert_eq!(effective_password("   ", "stored-token"), "stored-token");
        assert_eq!(effective_password("typed", "stored-token"), "typed");
        assert_eq!(effective_password("typed", ""), "typed");
    }

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
