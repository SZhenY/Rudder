use super::*;

pub(super) fn wire_update_links(window: &AppWindow) {
    // 「自动更新」对话框里「当前版本 → 新版本」那行的左半边：编译期版本，写一次就够。
    window.set_update_current_version(format!("v{}", env!("CARGO_PKG_VERSION")).into());
    // "Download" on the banner opens the latest-release page in the browser.
    window.on_open_update_url(move || {
        let url = "https://github.com/SZhenY/Rudder/releases/latest";
        #[cfg(windows)]
        let _ = std::process::Command::new("explorer").arg(url).spawn();
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg(url).spawn();
        #[cfg(all(not(windows), not(target_os = "macos")))]
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    });
    // The open-source link in the About dialog opens the project page.
    window.on_open_repo(move || {
        let url = "https://github.com/SZhenY/Rudder";
        #[cfg(windows)]
        let _ = std::process::Command::new("explorer").arg(url).spawn();
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg(url).spawn();
        #[cfg(all(not(windows), not(target_os = "macos")))]
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    });
    // Query the GitHub releases API on a background thread; if a newer version
    // exists, flip the banner on. Best-effort: any network/parse error is
    // silently ignored and the app keeps working on the current version.
    //
    // 三件事都听设置：「自动检查更新」开关（关掉就完全不发请求，#184）、
    // 「更新通道」（只提示正式版 / 只提示预发布 / 全通道最新，见
    // `self_updater::fetch_channel_release`）、「检查频率」（每次启动 / 每天一次）。
}
