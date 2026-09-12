//! 传输页：SFTP 跟随 cd / 总是询问保存位置。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Store, persist};
use crate::config::fresh_config;
use crate::ui::AppWindow;

/// 播种 + 注册持久化回调。
pub(crate) fn bind(w: &AppWindow, store: &Store, follow_flag: &Arc<AtomicBool>) {
    // SFTP 跟随 cd：泵线程读的是 `AppContext.sftp_follow_cd` 这个原子标志，而不是
    // 配置。两处都要写，否则"界面变了、行为没变"。
    w.set_sftp_follow_cd(store.borrow().sftp_follow_cd());
    {
        let store = store.clone();
        let flag = follow_flag.clone();
        w.on_set_sftp_follow_cd(move |follow| {
            flag.store(follow, Ordering::Relaxed);
            persist(&store, |s| s.set_sftp_follow_cd(follow));
        });
    }

    // 总是询问保存位置（#87）：下载处理器实时读取该窗口属性，所以只需播种 + 持久化。
    w.set_download_always_ask(store.borrow().download_always_ask());
    {
        let store = store.clone();
        w.on_set_download_always_ask(move |ask| {
            persist(&store, |s| s.set_download_always_ask(ask));
        });
    }
}

/// 「还原本页默认」：替换传输域为出厂默认，再走与 `bind` 相同的落点。
///
/// 注意 `sftp_follow_cd` 的存储字段是取反的 `sftp_no_follow_cd`。
pub(crate) fn reset(w: &AppWindow, store: &Store, follow_flag: &Arc<AtomicBool>) {
    let d = fresh_config();
    let follow_cd = !d.transfer.sftp_no_follow_cd;

    persist(store, |s| {
        s.set_sftp_follow_cd(follow_cd);
        s.set_download_always_ask(d.transfer.download_always_ask);
    });

    // 泵线程读的是原子标志：只改配置与控件的话，已打开会话仍按旧值跟随 cd。
    follow_flag.store(follow_cd, Ordering::Relaxed);
    w.set_sftp_follow_cd(follow_cd);
    w.set_download_always_ask(d.transfer.download_always_ask);
}
