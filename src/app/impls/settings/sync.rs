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
#[path = "sync_bind_callbacks.rs"] mod sync_bind_callbacks;
#[path = "sync_bind_transfer.rs"] mod sync_bind_transfer;

pub(crate) fn bind(window: &AppWindow, store: &Store, sessions_model: &Rc<VecModel<SessionInfo>>) {
    // WebDAV 的网络请求在后台线程跑，回填时要在 UI 线程拿到同一个 store 与会话模型
    // （两者都是 `Rc`，捕获不进 `Send` 闭包）。
    register_ui_handles(store, sessions_model);
    sync_bind_callbacks::wire_sync_settings(window, store);
    sync_bind_transfer::wire_sync_transfer(window, store);

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
