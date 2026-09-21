//! In-app update check (#48).

use std::rc::Rc;

use slint::{ComponentHandle as _, ModelRc, SharedString, VecModel};

use crate::ui::{AppWindow, TransferInfo};

use super::settings::{Store, persist};
use super::{AppContext, parse_version, DEP_VERSIONS};

/// 「检查频率」对应的最小间隔；`None` = 每次启动都查（`startup`）。
///
/// 月/半年/年按月与年的常用近似（30 / 182 / 365 天）—— 目的只是"别太频繁"，
/// 不需要日历级精确。
pub(crate) fn check_interval_secs(frequency: &str) -> Option<i64> {
    const DAY: i64 = 24 * 60 * 60;
    match frequency {
        "daily" => Some(DAY),
        "weekly" => Some(7 * DAY),
        "monthly" => Some(30 * DAY),
        "semiannual" => Some(182 * DAY),
        "yearly" => Some(365 * DAY),
        // `startup` 以及任何未知值：每次启动都查。
        _ => None,
    }
}

/// 「自动更新」对话框上那个通道徽章：被提示的版本是**测试版**（`-betaN` 等）就写「测试版」，
/// 否则「正式版」。按 tag 名判定，与更新通道的筛选同一套规则（`is_test_build`）。
fn version_badge(tag: &str) -> String {
    let is_test = crate::app::parse_version(tag).is_some_and(|v| crate::app::is_test_build(&v));
    if is_test {
        crate::i18n::t("测试版", "Beta").to_string()
    } else {
        crate::i18n::t("正式版", "Stable").to_string()
    }
}

/// 当前 Unix 秒；系统时钟异常时退化为 0（调用方按"还没查过"处理）。
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 「上次检查时间」那一行显示的本机时间（`2026/09/21 10:14:58`）；0 → 空串。
pub(crate) fn format_last_check(unix: i64) -> String {
    if unix <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y/%m/%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

/// 记下「这次检查发生的时间」：写配置（带去抖）+ 刷新界面那一行。
///
/// 在**发起**检查之前、UI 线程上调用 —— 时间戳即"上次检查时间"，界面显示的就是它；
/// `daily` 的节流也据此判断。放在这里而不是等结果回来，是因为配置句柄是
/// `Rc<RefCell<..>>`（只能待在 UI 线程），而检查跑在后台线程上。
fn mark_check_started(window: &AppWindow, store: &Store) {
    let now = now_unix();
    persist(store, |s| s.set_update_last_check(now));
    window.set_update_last_check(format_last_check(now).into());
}

/// Wire the in-app update-check banner (#48): download opens the releases page.
pub(crate) fn wire_update_check(window: &AppWindow, ctx: &AppContext) {
    // 「自动更新」对话框里「当前版本 → 新版本」那行的左半边：编译期版本，写一次就够。
    window.set_update_current_version(format!("v{}", env!("CARGO_PKG_VERSION")).into());
    let store = &ctx.store;
    let sftp_handles = &ctx.sftp_handles;
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
    {
        let (enabled, channel, frequency, last) = {
            let st = store.borrow();
            (
                st.update_check_enabled(),
                st.update_channel().to_string(),
                st.update_frequency().to_string(),
                st.update_last_check(),
            )
        };
        let now = now_unix();
        // 距上次检查不足所选频率的最小间隔就跳过这次启动（`startup` = 不节流）。
        let due = match check_interval_secs(&frequency) {
            None => true,
            Some(secs) => last == 0 || now.saturating_sub(last) >= secs,
        };
        if enabled && due {
            mark_check_started(window, store);
            let weak = window.as_weak();
            std::thread::spawn(move || {
                // 失败静默（沿用既有策略）：只影响横幅，不改时间戳。
                let Ok(Some(json)) = super::self_updater::fetch_channel_release(&channel) else {
                    return;
                };
                let tag = json["tag_name"].as_str().unwrap_or("").to_string();
                let newer = matches!(
                    (parse_version(&tag), parse_version(env!("CARGO_PKG_VERSION"))),
                    (Some(latest), Some(cur)) if latest > cur
                );
                if !newer {
                    return;
                }
                let badge = version_badge(&tag);
                let notes = crate::app::self_updater::release_notes(
                    json["body"].as_str().unwrap_or_default(),
                );
                let _ = weak.upgrade_in_event_loop(move |w| {
                    w.set_update_version(tag.into());
                    w.set_update_badge(badge.into());
                    w.set_update_notes(notes.into());
                    w.set_update_available(true);
                    // 自动检查也一样要弹「自动更新」对话框：用户此刻可能正开着设置面板，
                    // 而对话框挂在设置遮罩之后，压得住。
                    w.set_update_dialog_open(true);
                });
            });
        }
    }

    // ── In-app self-update (#self-update) ─────────────────────────────────
    // "Update now" downloads the matching release asset, extracts it and
    // replaces the running binary in place; the user then restarts. Any failure
    // falls back to the browser release page (the Download button stays put).
    {
        let weak = window.as_weak();
        let store_rc = store.clone();
        window.on_run_self_update(move || {
            let weak = weak.clone();
            let channel = store_rc.borrow().update_channel().to_string();
            std::thread::spawn(move || {
                let set = |state: i32, progress: f32, status: String| {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_update_state(state);
                            w.set_update_progress(progress);
                            w.set_update_status(status.into());
                            // 安装完成（3）与失败（4）必须让用户看到 ——
                            // 他可能下载到一半就把对话框关掉了。
                            if state == 3 || state == 4 {
                                w.set_update_dialog_open(true);
                            }
                        }
                    });
                };
                let current = crate::app::parse_version(env!("CARGO_PKG_VERSION"))
                    .unwrap_or((0, 0, 0, 0, 0));
                set(
                    1,
                    0.0,
                    crate::i18n::t("正在检查更新…", "Checking for updates…").to_string(),
                );
                let cand = match crate::app::self_updater::latest_update(current, &channel) {
                    Ok(Some(c)) => c,
                    Ok(None) => {
                        // 5 = "无需更新"，与 4（失败）分开：对话框标题跟着状态走，
                        // 借用失败档会显示成"更新失败"，可实际上什么都没坏。
                        set(
                            5,
                            0.0,
                            crate::i18n::t("已是最新版本", "Already up to date").to_string(),
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!("self-update lookup failed: {e:#}");
                        set(
                            4,
                            0.0,
                            crate::i18n::t(
                                "检查更新失败，请稍后重试",
                                "Check failed — try again later",
                            )
                            .to_string(),
                        );
                        return;
                    }
                };
                set(
                    1,
                    0.0,
                    crate::i18n::t("正在下载…", "Downloading…").to_string(),
                );
                let staged = {
                    let weak = weak.clone();
                    crate::app::self_updater::download_and_stage(&cand, move |p: f32| {
                        let weak = weak.clone();
                        let pct = (p * 100.0) as i32;
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_update_state(1);
                                w.set_update_progress(p);
                                w.set_update_status(
                                    format!(
                                        "{} {pct}%",
                                        crate::i18n::t("正在下载…", "Downloading…")
                                    )
                                    .into(),
                                );
                            }
                        });
                    })
                };
                let dir = match staged {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!("self-update download failed: {e:#}");
                        set(
                            4,
                            0.0,
                            crate::i18n::t(
                                "下载失败，请检查网络，或点下面的发布页手动下载",
                                "Download failed — check your network, or use the release page",
                            )
                            .to_string(),
                        );
                        return;
                    }
                };
                set(
                    2,
                    1.0,
                    crate::i18n::t("正在安装…", "Installing…").to_string(),
                );
                match crate::app::self_updater::install_staged(&dir) {
                    Ok(()) => {
                        tracing::info!("self-update installed {}", cand.version);
                        set(
                            3,
                            1.0,
                            crate::i18n::t(
                                "更新已安装，点击“重启”生效",
                                "Update installed — press Restart to apply",
                            )
                            .to_string(),
                        );
                    }
                    Err(e) => {
                        tracing::warn!("self-update install failed: {e:#}");
                        set(
                            4,
                            1.0,
                            crate::i18n::t(
                                "安装失败，请点下面的发布页手动下载安装包",
                                "Install failed — download the package from the release page",
                            )
                            .to_string(),
                        );
                    }
                }
            });
        });
    }
    // ── Settings → "Check now" (#self-update) ─────────────────────────────
    // Manual check, independent of the startup toggle. Result is surfaced
    // inline in the settings row; a new version additionally flips the banner.
    {
        let weak = window.as_weak();
        let store_rc = store.clone();
        window.on_check_update_now(move || {
            if let Some(w) = weak.upgrade() {
                mark_check_started(&w, &store_rc);
            }
            let weak = weak.clone();
            let check_channel = store_rc.borrow().update_channel().to_string();
            std::thread::spawn(move || {
                // 第三个参数是"发现更新"时的三件套：(版本号, 通道徽章, 发布说明)。
                let set = |checking: bool, status: String, found: Option<(String, String, String)>| {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_update_checking(checking);
                            w.set_update_check_status(status.into());
                            if let Some((v, badge, notes)) = found {
                                w.set_update_version(v.into());
                                w.set_update_badge(badge.into());
                                w.set_update_notes(notes.into());
                                w.set_update_available(true);
                                w.set_update_state(0);
                                // 手动检查发现新版本同样弹对话框（用户就是在设置里点的，
                                // 对话框显示在设置面板之上）。
                                w.set_update_dialog_open(true);
                            }
                        }
                    });
                };
                set(true, String::new(), None);
                let current =
                    crate::app::parse_version(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0, 0, 0));
                match crate::app::self_updater::latest_update(current, &check_channel) {
                    Ok(Some(c)) => {
                        let v = format!("v{}", c.version);
                        let status =
                            format!("{} {v}", crate::i18n::t("发现新版本", "New version found"));
                        set(false, status, Some((v, version_badge(&c.version), c.notes)));
                    }
                    Ok(None) => {
                        set(
                            false,
                            crate::i18n::t("已是最新版本", "Already up to date").to_string(),
                            None,
                        );
                    }
                    Err(e) => {
                        // 细节进日志（error.log 只收 warn 以上，用户报障时能拿到完整错误链）；
                        // 界面上只给一句本地化的话 —— 此前直接把 `{e:#}` 抛到那行说明位上，
                        // 于是中文界面里冒出一串 "query GitHub releases: …"。
                        tracing::warn!("manual update check failed: {e:#}");
                        set(
                            false,
                            crate::i18n::t("检查失败，请稍后重试", "Check failed — try again later")
                                .to_string(),
                            None,
                        );
                    }
                }
            });
        });
    }

    {
        window.on_restart_app(move || {
            if let Err(e) = crate::app::self_updater::restart_app() {
                tracing::warn!("restart after update failed: {e:#}");
            }
        });
    }

    // Transfer records (download/upload progress + history) shown in the popup.
    let transfers_model: Rc<VecModel<TransferInfo>> = Rc::new(VecModel::default());
    window.set_transfers(ModelRc::from(transfers_model.clone()));
    {
        let tm = transfers_model.clone();
        window.on_clear_transfers(move || tm.set_vec(Vec::<TransferInfo>::new()));
    }
    {
        // Cancel a transfer by id. The id is a UUID unique across sessions, so we
        // broadcast to every SFTP handle — only the owning one has it registered
        // and will act on it (#100).
        let sftp_handles = sftp_handles.clone();
        window.on_cancel_transfer(move |id: SharedString| {
            if let Ok(handles) = sftp_handles.lock() {
                for h in handles.values() {
                    h.cancel_transfer(id.to_string());
                }
            }
        });
    }

    // Open-source libraries with resolved versions, shown in the About popup.
    // Versions are baked in at compile time by build.rs → $OUT_DIR/deps.rs
    // (included as module-level `DEP_VERSIONS` above).
    {
        let get_ver = dep_version;

        let zh = crate::i18n::t;
        let libs: Vec<SharedString> = vec![
            SharedString::from(format!(
                "Slint v{} — {}",
                get_ver("slint"),
                zh("图形界面框架", "GUI framework")
            )),
            SharedString::from(format!(
                "russh v{} — {}",
                get_ver("russh"),
                zh("SSH 协议实现", "SSH protocol")
            )),
            SharedString::from(format!(
                "russh-sftp v{} — {}",
                get_ver("russh-sftp"),
                zh("SFTP 文件传输", "SFTP file transfer")
            )),
            SharedString::from(format!(
                "ssh-key v{} — {}",
                get_ver("ssh-key"),
                zh("SSH 密钥解析", "SSH key parsing")
            )),
            SharedString::from(format!(
                "tokio v{} — {}",
                get_ver("tokio"),
                zh("异步运行时", "async runtime")
            )),
            SharedString::from(format!(
                "alacritty_terminal v{} — {}",
                get_ver("alacritty_terminal"),
                zh("终端模拟与解析", "terminal emulator & parser")
            )),
            SharedString::from(format!(
                "sysinfo v{} — {}",
                get_ver("sysinfo"),
                zh("本机资源采集", "local resource sampling")
            )),
            SharedString::from(format!(
                "serde v{} — {}",
                get_ver("serde"),
                zh("配置序列化", "config serialization")
            )),
            SharedString::from(format!(
                "arboard v{} — {}",
                get_ver("arboard"),
                zh("系统剪贴板", "system clipboard")
            )),
            SharedString::from(format!(
                "rfd v{} — {}",
                get_ver("rfd"),
                zh("原生文件对话框", "native file dialogs")
            )),
            SharedString::from(format!(
                "directories v{} — {}",
                get_ver("directories"),
                zh("配置目录定位", "config dir lookup")
            )),
            SharedString::from(format!(
                "chrono v{} — {}",
                get_ver("chrono"),
                zh("日期时间处理", "date/time handling")
            )),
            SharedString::from(format!(
                "uuid v{} — {}",
                get_ver("uuid"),
                zh("唯一标识符", "unique identifiers")
            )),
            SharedString::from(format!(
                "anyhow v{} — {}",
                get_ver("anyhow"),
                zh("错误处理", "error handling")
            )),
            SharedString::from(format!(
                "tracing v{} — {}",
                get_ver("tracing"),
                zh("日志", "logging")
            )),
            SharedString::from(format!(
                "futures v{} — {}",
                get_ver("futures"),
                zh("异步辅助", "async helpers")
            )),
            SharedString::from(format!(
                "rand v{} — {}",
                get_ver("rand"),
                zh("随机数", "randomness")
            )),
            SharedString::from(format!(
                "winresource v{} — {}",
                get_ver("winresource"),
                zh("Windows 图标嵌入", "Windows icon embedding")
            )),
        ]
        .to_vec();
        window.set_about_libs(ModelRc::from(Rc::new(VecModel::from(libs))));
    }
}
/// 查编译期烘焙的依赖版本（`build.rs` -> `$OUT_DIR/deps.rs`，见文件顶部的 `DEP_VERSIONS`）。
///
/// 未命中返回 `"-"`：About 面板按名字查表，拼错会静默显示成 `-` ——
/// 用户贴 bug 报告时给出的库版本就是错的。
fn dep_version(name: &str) -> &str {
    DEP_VERSIONS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, v)| *v)
        .unwrap_or("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// About 面板真的会去查的名字必须命中（否则那一行显示成 `-`）。
    #[test]
    fn dep_version_finds_the_about_panel_entries() {
        for name in ["slint", "russh", "tokio"] {
            let v = dep_version(name);
            assert_ne!(v, "-", "{name} 未命中 DEP_VERSIONS");
            assert!(!v.is_empty(), "{name} 的版本号为空");
        }
    }

    #[test]
    fn dep_version_falls_back_to_dash() {
        assert_eq!(dep_version("no-such-crate"), "-");
        assert_eq!(dep_version(""), "-");
    }
}

#[cfg(test)]
mod interval_tests {
    use super::check_interval_secs;

    /// 「每次启动」不节流；其余各档必须**严格递增** —— 顺序写错会让"每周"比"每天"更频繁。
    #[test]
    fn intervals_are_ordered_and_startup_is_unthrottled() {
        assert_eq!(check_interval_secs("startup"), None);
        assert_eq!(check_interval_secs("nonsense"), None, "未知值按每次启动处理");
        let secs: Vec<i64> = ["daily", "weekly", "monthly", "semiannual", "yearly"]
            .iter()
            .map(|f| check_interval_secs(f).expect("必须有区间"))
            .collect();
        for pair in secs.windows(2) {
            assert!(pair[1] > pair[0], "区间必须递增：{secs:?}");
        }
    }
}
