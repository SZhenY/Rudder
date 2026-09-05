//! In-app update check (#48).

use std::cell::RefCell;
use std::rc::Rc;

use crate::config::ConfigStore;
use crate::sftp::SftpHandles;
use slint::{ComponentHandle as _, ModelRc, SharedString, VecModel};

use crate::ui::{AppWindow, TransferInfo};

use super::{parse_version, DEP_VERSIONS};

/// Wire the in-app update-check banner (#48): download opens the releases page.
pub(crate) fn wire_update_check(
    window: &AppWindow,
    store: &Rc<RefCell<ConfigStore>>,
    sftp_handles: &SftpHandles,
) {
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
    // Skipped entirely when the user turned the check off (#184).
    if store.borrow().update_check_enabled() {
        let weak = window.as_weak();
        std::thread::spawn(move || {
            let body = match ureq::get("https://api.github.com/repos/SZhenY/Rudder/releases/latest")
                .set("User-Agent", "rudder-update-check")
                .timeout(std::time::Duration::from_secs(8))
                .call()
            {
                Ok(resp) => resp.into_string().unwrap_or_default(),
                Err(_) => return,
            };
            let json: serde_json::Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(_) => return,
            };
            let tag = json["tag_name"].as_str().unwrap_or("").to_string();
            let newer = matches!(
                (parse_version(&tag), parse_version(env!("CARGO_PKG_VERSION"))),
                (Some(latest), Some(cur)) if latest > cur
            );
            if !newer {
                return;
            }
            let _ = weak.upgrade_in_event_loop(move |w| {
                w.set_update_version(tag.into());
                w.set_update_available(true);
            });
        });
    }

    // ── In-app self-update (#self-update) ─────────────────────────────────
    // "Update now" downloads the matching release asset, extracts it and
    // replaces the running binary in place; the user then restarts. Any failure
    // falls back to the browser release page (the Download button stays put).
    {
        let weak = window.as_weak();
        window.on_run_self_update(move || {
            let weak = weak.clone();
            std::thread::spawn(move || {
                let set = |state: i32, progress: f32, status: String| {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_update_state(state);
                            w.set_update_progress(progress);
                            w.set_update_status(status.into());
                        }
                    });
                };
                let current = crate::app::parse_version(env!("CARGO_PKG_VERSION"))
                    .unwrap_or((0, 0, 0));
                set(
                    1,
                    0.0,
                    crate::i18n::t("正在检查更新…", "Checking for updates…").to_string(),
                );
                let cand = match crate::app::self_updater::latest_update(current) {
                    Ok(Some(c)) => c,
                    Ok(None) => {
                        set(
                            4,
                            0.0,
                            crate::i18n::t("已是最新版本", "Already up to date").to_string(),
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!("self-update lookup failed: {e:#}");
                        set(4, 0.0, format!("{e:#}"));
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
                        set(4, 0.0, format!("{e:#}"));
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
                        set(4, 1.0, format!("{e:#}"));
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
        window.on_check_update_now(move || {
            let weak = weak.clone();
            std::thread::spawn(move || {
                let set = |checking: bool, status: String, found: Option<String>| {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_update_checking(checking);
                            w.set_update_check_status(status.into());
                            if let Some(v) = found {
                                w.set_update_version(v.into());
                                w.set_update_available(true);
                                w.set_update_state(0);
                            }
                        }
                    });
                };
                set(true, String::new(), None);
                let current =
                    crate::app::parse_version(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0));
                match crate::app::self_updater::latest_update(current) {
                    Ok(Some(c)) => {
                        let v = format!("v{}", c.version);
                        set(
                            false,
                            crate::i18n::t("发现新版本", "New version found").to_string(),
                            Some(v),
                        );
                    }
                    Ok(None) => {
                        set(
                            false,
                            crate::i18n::t("已是最新版本", "Already up to date").to_string(),
                            None,
                        );
                    }
                    Err(e) => {
                        tracing::warn!("manual update check failed: {e:#}");
                        set(false, format!("{e:#}"), None);
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
        let get_ver = |name: &str| -> &str {
            DEP_VERSIONS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| *v)
                .unwrap_or("-")
        };

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
