use super::*;

/// 自动检查只提示"升级"：降级 / 切换只在手动检查里出现（原逻辑是 `latest > cur`）。
fn auto_notice(candidate: &crate::app::self_updater::UpdateCandidate) -> Option<(String, String)> {
    if candidate.is_switch {
        tracing::info!(
            "auto update: {} is a downgrade/switch, not prompting",
            candidate.version
        );
        return None;
    }
    // UI 显示的是 `v0.8.2` 这个形状（`ui/app.slint` 的 `update-version`）。
    let tag = format!("v{}", candidate.version);
    let badge = version_badge(&tag);
    Some((tag, badge))
}

pub(super) fn wire_update_checks(window: &AppWindow, ctx: &AppContext) {
    let store = &ctx.store;

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
                let current =
                    crate::app::parse_version(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0, 0, 0));
                // 复用与「手动下载」同一条 lookup：它自带平台资产匹配。三种非候选
                // 结局都静默，只留日志 —— 发版瞬间资产没传完不该弹窗。
                use crate::app::self_updater::UpdateLookup;
                let candidate =
                    match crate::app::self_updater::latest_update(current, &channel) {
                        Ok(UpdateLookup::Found(c)) => c,
                        Ok(UpdateLookup::UpToDate) => {
                            tracing::info!(channel = %channel, "auto update: up to date");
                            return;
                        }
                        Ok(UpdateLookup::NoAssetForPlatform { version }) => {
                            tracing::info!(
                                channel = %channel,
                                %version,
                                "auto update: no asset for this platform"
                            );
                            return;
                        }
                        Ok(UpdateLookup::NoMsiForPlatform { version }) => {
                            tracing::info!(
                                channel = %channel,
                                %version,
                                "auto update: no MSI for this platform"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::info!("auto update check failed: {e:#}");
                            return;
                        }
                    };
                let Some((tag, badge)) = auto_notice(&candidate) else {
                    return;
                };
                let notes = candidate.notes;
                let _ = weak.upgrade_in_event_loop(move |w| {
                    w.set_update_version(tag.into());
                    w.set_update_badge(badge.into());
                    w.set_update_notes(notes.into());
                    // 与手动检查对齐：上一次失败 / 降级态不能污染这一次的对话框标题。
                    w.set_update_downgrade(false);
                    w.set_update_state(0);
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
                use crate::app::self_updater::UpdateLookup;
                let cand = match crate::app::self_updater::latest_update(current, &channel) {
                    Ok(UpdateLookup::Found(c)) => c,
                    Ok(UpdateLookup::UpToDate) => {
                        // 5 = "无需更新"，与 4（失败）分开：对话框标题跟着状态走，
                        // 借用失败档会显示成"更新失败"，可实际上什么都没坏。
                        set(
                            5,
                            0.0,
                            crate::i18n::t("已是最新版本", "Already up to date").to_string(),
                        );
                        return;
                    }
                    Ok(UpdateLookup::NoAssetForPlatform { version }) => {
                        set(
                            6,
                            0.0,
                            format!(
                                "v{version} {}",
                                crate::i18n::t(
                                    "暂未提供本平台的安装包（可能仍在构建），请稍后再试或到发布页手动下载",
                                    "has no package for this platform yet (it may still be building) — try again later or use the release page"
                                )
                            ),
                        );
                        return;
                    }
                    Ok(UpdateLookup::NoMsiForPlatform { version }) => {
                        set(
                            6,
                            0.0,
                            format!(
                                "v{version} {}",
                                crate::i18n::t(
                                    "未提供 MSI 安装包（预发布版不产 MSI），请改用便携版或等待正式版",
                                    "ships no MSI (pre-releases do not) — use the portable build or wait for the stable release"
                                )
                            ),
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
                let staged = match staged {
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
                match crate::app::self_updater::install_staged(&staged) {
                    Ok(crate::app::self_updater::InstallOutcome::Replaced) => {
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
                    // MSI：安装程序已经在跑，它要覆盖的正是本进程占着的 exe。
                    // 退出让出文件锁；安装进度由 msiexec 自己的窗口显示。
                    Ok(crate::app::self_updater::InstallOutcome::InstallerLaunched) => {
                        tracing::info!("self-update handed to msiexec: {}", cand.version);
                        set(
                            3,
                            1.0,
                            crate::i18n::t(
                                "安装程序已启动，应用将退出；安装完成后请重新打开 Rudder",
                                "Installer started — Rudder will exit; reopen it when setup finishes",
                            )
                            .to_string(),
                        );
                        // 上面这行进的是事件队列，而 Slint 文档明确写着退出时
                        // "已排队的事件可能不会被处理"。延后 0.8s 再退，让提示真的
                        // 画出来；此刻 UAC 还没点，msiexec 也还在等，不占文件锁。
                        std::thread::spawn(|| {
                            std::thread::sleep(std::time::Duration::from_millis(800));
                            let _ = slint::quit_event_loop();
                        });
                    }
                    Err(e) => {
                        tracing::warn!("self-update install failed: {e:#}");
                        // MSI 的失败原因（UAC 被拒 / ShellExecute 返回码 / 安装包路径）
                        // 必须给到用户：这些信息在"去发布页重新下载"里一条也拿不到。
                        let text = if cand.is_msi {
                            format!("{}：{e}", crate::i18n::t("安装失败", "Install failed"))
                        } else {
                            crate::i18n::t(
                                "安装失败，请点下面的发布页手动下载安装包",
                                "Install failed — download the package from the release page",
                            )
                            .to_string()
                        };
                        set(4, 1.0, text);
                    }
                }
            });
        });
    }
    // ── Settings → "Check now" + 切换更新通道 (#self-update) ───────────────
    // Manual check, independent of the startup toggle. Result is surfaced
    // inline in the settings row; a new version additionally opens the dialog.
    //
    // 「切换更新通道」也走这条检查（`settings::update` 里持久化之后 invoke 一次）——
    // 本机是 `0.7.9-beta1` 而切到「正式版」、正式版最新是 `0.7.9` 时，这时要提示
    // "可切换到旧版本"，而不是"已是最新版本"（用户 2026-09-21 的要求）。
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
                // 第三个参数是"发现更新"时的四件套：(版本号, 通道徽章, 发布说明, 是否切换)。
                let set =
                    |checking: bool,
                     status: String,
                     found: Option<(String, String, String, bool)>| {
                        let weak = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_update_checking(checking);
                                w.set_update_check_status(status.into());
                                // 每次更新状态都先清掉上一次的失败原因。
                                w.set_update_check_error("".into());
                                if let Some((v, badge, notes, is_switch)) = found {
                                    w.set_update_version(v.into());
                                    w.set_update_badge(badge.into());
                                    w.set_update_notes(notes.into());
                                    w.set_update_downgrade(is_switch);
                                    w.set_update_available(true);
                                    w.set_update_state(0);
                                    // 手动检查发现新版本同样弹对话框（用户就是在设置里点的，
                                    // 对话框显示在设置面板之上）。
                                    w.set_update_dialog_open(true);
                                }
                                // 设置窗口是独立窗口，异步结果不会自己流过去：状态行、失败原因、
                                // 上次检查时间在这里一并向设置窗口回灌。
                                crate::app::settings_window::resync_if_open(&w);
                            }
                        });
                    };
                set(
                    true,
                    crate::i18n::t("正在检查更新…", "Checking for updates…").to_string(),
                    None,
                );
                let current =
                    crate::app::parse_version(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0, 0, 0));
                use crate::app::self_updater::UpdateLookup;
                match crate::app::self_updater::latest_update(current, &check_channel) {
                    Ok(UpdateLookup::Found(c)) => {
                        let v = format!("v{}", c.version);
                        // 切回旧版时说"发现新版本"会误导，措辞分开。
                        let status = if c.is_switch {
                            format!(
                                "{} {v}",
                                crate::i18n::t("可切换到旧版本", "Older version available")
                            )
                        } else {
                            format!("{} {v}", crate::i18n::t("发现新版本", "New version found"))
                        };
                        set(
                            false,
                            status,
                            Some((v, version_badge(&c.version), c.notes, c.is_switch)),
                        );
                    }
                    Ok(UpdateLookup::UpToDate) => {
                        set(
                            false,
                            crate::i18n::t("已是最新版本", "Already up to date").to_string(),
                            None,
                        );
                    }
                    Ok(UpdateLookup::NoAssetForPlatform { version }) => {
                        set(
                            false,
                            format!(
                                "v{version} {}",
                                crate::i18n::t("暂无本平台安装包", "no package for this platform")
                            ),
                            None,
                        );
                    }
                    Ok(UpdateLookup::NoMsiForPlatform { version }) => {
                        set(
                            false,
                            format!(
                                "v{version} {}",
                                crate::i18n::t("未提供 MSI 安装包", "ships no MSI")
                            ),
                            None,
                        );
                    }
                    Err(e) => {
                        // 细节进日志（error.log 只收 warn 以上，用户报障时能拿到完整错误链）；
                        // 界面上只给一句本地化的话 —— 此前直接把 `{e:#}` 抛到那行说明位上，
                        // 于是中文界面里冒出一串 "query GitHub releases: …"。
                        tracing::warn!("manual update check failed: {e:#}");
                        let detail = format!("{e:#}");
                        set(
                            false,
                            crate::i18n::t(
                                "检查失败，请稍后重试",
                                "Check failed — try again later",
                            )
                            .to_string(),
                            None,
                        );
                        // 错误链单独送到设置页那行小字：普通用户只看上面那句，
                        // 要报障时能直接看到到底卡在哪（代理 / 证书 / 限流）。
                        let weak = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_update_check_error(detail.into());
                                crate::app::settings_window::resync_if_open(&w);
                            }
                        });
                    }
                }
            });
        });
    }

    {
        let weak = window.as_weak();
        window.on_restart_app(move || {
            if let Err(e) = crate::app::self_updater::restart_app() {
                tracing::warn!("restart after update failed: {e:#}");
                // exec 失败时当前进程还在跑，不给提示的话用户只看到"点了没反应"
                // （0.8.0-beta3 的 macOS 自更新就是栽在这里：新二进制没有执行位）。
                if let Some(w) = weak.upgrade() {
                    w.set_update_state(4);
                    w.set_update_status(
                        crate::i18n::t(
                            "重启失败：请手动退出再打开。若仍打不开，请从发布页重新下载安装",
                            "Restart failed — quit and reopen manually. If it still won't open, reinstall from the release page",
                        )
                        .into(),
                    );
                    w.set_update_dialog_open(true);
                }
            }
        });
    }
}

#[cfg(test)]
mod auto_notice_tests {
    use super::auto_notice;
    use crate::app::self_updater::UpdateCandidate;

    fn candidate(version: &str, is_switch: bool) -> UpdateCandidate {
        UpdateCandidate {
            version: version.to_string(),
            asset_name: String::new(),
            download_url: String::new(),
            notes: String::new(),
            is_switch,
            is_msi: false,
        }
    }

    #[test]
    fn auto_check_stays_upgrade_only() {
        assert!(auto_notice(&candidate("0.8.2", true)).is_none());
    }

    #[test]
    fn auto_notice_restores_the_v_prefix_and_badge() {
        let (tag, badge) = auto_notice(&candidate("0.8.3", false)).expect("notice");
        assert_eq!(tag, "v0.8.3");
        assert_eq!(badge, crate::i18n::t("正式版", "Stable").to_string());

        let (tag, badge) = auto_notice(&candidate("0.8.3-beta1", false)).expect("notice");
        assert_eq!(tag, "v0.8.3-beta1");
        assert_eq!(badge, crate::i18n::t("测试版", "Beta").to_string());
    }
}
