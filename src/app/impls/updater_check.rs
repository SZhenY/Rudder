use super::*;

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
                // 失败静默（沿用既有策略）：只影响横幅，不改时间戳。
                let Ok(Some(json)) = crate::app::self_updater::fetch_channel_release(&channel)
                else {
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
                // 发布说明：优先该 tag 的 CHANGELOG 段落（中英对照），否则 release body。
                let notes = crate::app::self_updater::notes_for(
                    &tag,
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
                match crate::app::self_updater::latest_update(current, &check_channel) {
                    Ok(Some(c)) => {
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
