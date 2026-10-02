//! Settings seeding: push the saved configuration into the Slint properties and
//! register the `on_set_*` callbacks that persist later edits back to the store.
//!
//! Extracted from `app.rs` (refactor plan stage A4). Slint-thread-only: every
//! callback here runs on the UI thread and only touches `ConfigStore` + the
//! window's properties.
//!
//! This is a straight move of the section that used to sit between "process
//! monitor window" and "Wire callbacks" in `run()`.

// Everything this section needs was already in scope in `app.rs`; reuse that
// namespace rather than re-deriving a long import list by hand.
use super::*;
use crate::ui::{ Theme };

/// Shared config-store handle (`Rc<RefCell<ConfigStore>>`).
// 配置存储句柄：与 `super::settings` 共用同一个别名定义。
use super::settings::{FontCatalog, Store};

/// Settings pages that expose a "restore this page's defaults" button.
///
/// Adding a variant here makes every `match` below fail to compile until the new
/// page is handled — that is the point: it is what stops a page from being
/// forgotten the way four of them were (see the note at the `on_reset_page`
/// registration).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsPage {
    Terminal,
    Appearance,
    Transfer,
}

impl SettingsPage {
    pub(crate) fn parse(id: &str) -> Option<Self> {
        match id {
            "terminal" => Some(Self::Terminal),
            "appearance" => Some(Self::Appearance),
            "transfer" => Some(Self::Transfer),
            _ => None,
        }
    }
}


/// 「还原本页默认」需要的全部句柄（`Rc` / `Arc`，克隆廉价）。
///
/// 存在的意义是把「一项设置的全部落点」集中起来：除了配置字段与控件属性，
/// 还有派生 UI 状态（字体选择器索引）、运行时镜像（SFTP 跟随标志）等。
/// 注：不派生 `Clone` —— 它持有 `ProcWindow`（Slint 组件句柄不实现 Clone），
/// 且全项目只构造一次、按引用传给 `reset_page`。
struct ResetRefs {
    fonts: FontCatalog,
    /// 泵线程读取的「SFTP 跟随 cd」实时标志。还原必须同时更新它，否则配置与
    /// 界面都变了、**已打开会话的行为却没变** —— 表现为"还原不生效"。
    sftp_follow_cd: Arc<std::sync::atomic::AtomicBool>,
    /// 进程监视窗：外观页还原可能改变深浅色，已打开的窗口要跟着换肤。
    /// 存 `Weak` 而非强引用 —— 组件句柄不实现 `Clone`，且不该由它延长窗口寿命。
    proc_win: slint::Weak<crate::ui::ProcWindow>,
}

fn reset_page(w: &AppWindow, store: &Store, bufs: &TermBuffers, refs: &ResetRefs, page: &str) {
    let Some(page) = SettingsPage::parse(page) else {
        tracing::warn!("reset-page: unknown settings page {page:?}");
        return;
    };
    match page {
        SettingsPage::Terminal => settings::terminal::reset(w, store, bufs, &refs.fonts),
        SettingsPage::Appearance => {
            settings::appearance::reset(w, store, bufs, &refs.fonts, &refs.proc_win)
        }
        SettingsPage::Transfer => settings::transfer::reset(w, store, &refs.sftp_follow_cd),
    }
}







pub(super) fn seed_settings(window: &AppWindow, proc_win: &ProcWindow, ctx: &AppContext) {
    let AppContext {
        store,
        bufs,
        tab_statuses,
        layout,
        panes_model,
        splitters_model,
        terminals_model,
        tabs_model,
        sessions_model,
        content_size,
        handles,
        sftp_follow_cd,
        runtime,
        pending_window_size_restore,
        ..
    } = ctx;

    // Apply the saved UI language.  The Rust-side flag drives `i18n::t(...)`;
    // `apply_to_slint` selects the bundled `.po` for the static `@tr(...)` text
    // (must run after the first component exists, which it now does).
    crate::i18n::set_language(store.borrow().language());
    crate::i18n::apply_to_slint();
    window.set_lang_en(crate::i18n::is_en());

    // 应用已保存的深浅档（档位只由壁纸决定，系统联动已取消）。
    {
        let is_dark = store.borrow().dark();
        window.global::<Theme>().set_dark(is_dark);
    }
    // On macOS, app shortcuts use Cmd (⌘) so physical Ctrl stays free for the
    // shell (#158); on Windows/Linux they stay Ctrl-based.
    window.set_is_mac(cfg!(target_os = "macos"));
    window.set_is_windows(cfg!(windows));

    // Apply the saved terminal font (Interface settings). An empty family keeps
    // the built-in default; the size always applies (defaults to 13).
    {
        let s = store.borrow();
        let fam = s.font_family().to_string();
        // Does the active terminal font cover CJK? Terminal spans then keep
        // it for Chinese text (italic/thin variants apply) instead of falling
        // back to the UI sans font (#54). Family-name tag probe: CN/SC/TC/
        // JP/KR/CJK/Han. An empty family means the embedded JetBrains Mono
        // default, which has no CJK glyphs → Chinese falls back to the UI
        // font (external CJK fonts like Maple Mono CN self-identify via "CN").
        let cjk = term_font_covers_cjk(if fam.is_empty() {
            "JetBrains Mono"
        } else {
            &fam
        });
        if !fam.is_empty() {
            window.global::<Theme>().set_term_font_family(fam.into());
        }
        window.global::<Theme>().set_term_font_cjk(cjk);
        window.global::<Theme>().set_term_font_size(s.font_size() as f32);
        window.global::<Theme>().set_term_font_bold(s.terminal_bold());
        window.set_scrollback_lines(s.scrollback_lines().to_string().into());
        window.set_large_scrollback(s.large_scrollback());
        window.set_term_cursor_style(s.terminal_cursor_style().into());
        // 光标色：空串 = 跟随主题 → 按当前（已确定的）深浅档解析。
        window.set_output_highlight_enabled(s.output_highlight_enabled());
        window.set_output_highlight_preset(s.output_highlight_preset().into());
        window.set_output_highlight_rules(output_highlight_rule_model(&s));
        window.set_json_format_output(s.json_format_output());
        window.global::<Theme>().set_ui_scale(s.ui_scale() as f32 / 100.0); // global UI zoom (#100)
        // 同上：三档语义值，否则胶囊匹配不上、没有高亮
        window.set_renderer_mode(s.renderer_mode_choice().into());
        // v0.8.0 设置窗迁移项：新设置页需要回显当前值。
        window.set_collapse_sftp_default(s.collapse_sftp_default());
    }

    // 设置页导航搜索：按关键词过滤分类（中/英文标签 + 副标题都参与匹配）。
    // contains 无法在 Slint 里表达，过滤在 Rust 做完回写六个可见性。
    {
        let hay = [
            (0i32, "基础 general language 语言 主题 更新 功能入口 theme accent wallpaper 壁纸 缩放 scale 动画 animation 渲染器 renderer 更新渠道 channel"),
            (1, "终端 terminal fonts 字体 cursor 光标 scrollback 滚回 osc52 高亮 highlight 规则 rule preset 预设"),
            (2, "文件 files sftp 分区 partitions 传输 transfer eol json 下载 download 挂载 mount filter 过滤 折叠 collapse"),
            (4, "快捷键 shortcuts keys 全局 global 终端快捷键"),
            (6, "云同步 cloud webdav 同步 sync 上传 upload 下载 download"),
            (5, "关于 about 版本 version 日志 log 反馈 feedback libs"),
        ];
        let weak = window.as_weak();
        window.on_nav_search_changed(move |q: slint::SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let q = q.trim().to_lowercase();
            for (id, text) in hay {
                let hit = q.is_empty() || text.to_lowercase().contains(&q);
                match id {
                    0 => w.set_cat_visible_0(hit),
                    1 => w.set_cat_visible_1(hit),
                    2 => w.set_cat_visible_2(hit),
                    4 => w.set_cat_visible_4(hit),
                    6 => w.set_cat_visible_6(hit),
                    _ => w.set_cat_visible_5(hit),
                }
            }
        });
    }

    // Apply the saved immersive wallpaper (overrides dark/light when set; a
    // missing custom file falls back to the plain theme).
    {
        let id = {
            let s = store.borrow();
            let w = s.wallpaper().to_string();
            // 老配置没有壁纸 → 补成内置深色图（深浅档与磨砂从此都有依据）。
            if w.is_empty() {
                drop(s);
                settings::persist(store, |s| {
                    s.set_wallpaper("builtin:dark".to_string());
                });
                "builtin:dark".to_string()
            } else {
                w
            }
        };
        // Restoring a saved wallpaper must not override the user's persisted
        // light/dark preference. Built-in wallpapers only suggest their paired
        // theme when the user actively selects them (#theme-persistence).
        apply_wallpaper(window, &store.borrow(), bufs, &id, false);
        // 「壁纸」下拉（跟随系统 / 深色 / 浅色 + `config/wallpapers` 里的图片）：标签与
        // 选中下标都由 Rust 算，界面只负责画 —— 与界面字体的选择器同一套形状。
        settings::appearance::publish_wallpaper_choices(window, store);
    }

    // 主题色 + 主题（深浅）：放在换肤**之后** —— 壁纸会决定深浅档，而主题色要按最终
    // 档位解析（预设两档是两个颜色，自定义色在浅色档要压深）。
    {
        let choice = store.borrow().accent().to_string();
        settings::appearance::apply_accent(window, &choice);
        // 光标色"伪显式"值迁移（两类历史回填泄漏，见 migrate_cursor_follow 文档）——
        // 必须在 apply_accent **之后**：要拿已解析的主题色生效色做比对。
        settings::terminal::migrate_cursor_follow(store, window);
        // 光标色在主题色**之后**应用：跟随主题时拿到的是壁纸对比色，不是出厂蓝。
        settings::terminal::apply_cursor_color(window, store.borrow().terminal_cursor_color());
    }

    // Editable inputs (e.g. the SFTP path bar) need a CJK-capable font: the
    // embedded mono font has no Chinese glyphs and native TextInput doesn't
    // glyph-fallback like Text does, so typed Chinese would render as tofu (#54).
    //
    // We must NOT hard-code one system font name: on macOS 26 (Tahoe) fontdb
    // failed to register "PingFang SC", so the UI default font resolved to nothing
    // and *all* text vanished (#129) — icons survived only because they use an
    // embedded font. Instead probe what fontdb actually loaded and pick the first
    // resolvable CJK family, falling back to the embedded "Meatshell Mono" so the
    // window is never fully blank even when the system font DB is unreadable.
    window.global::<Theme>().set_ui_font_family(resolve_ui_font_family());
    // Runtime font loading: fonts dropped into the fonts dir (Windows:
    // <exe_dir>/config/fonts; macOS/Linux: per-user config dir) are registered
    // with Slint's shared collection and become selectable below — large CJK
    // families like Maple Mono no longer need to be embedded at build time.
    let fonts_dirs = crate::fonts::external_fonts_dirs();
    tracing::info!(
        "external fonts dirs: {}",
        fonts_dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")
    );
    let external_fonts = crate::fonts::load_external_fonts(&fonts_dirs);
    // Populate the font pickers: embedded first, external next, system families
    // last, each labelled with its source.
    let (font_labels, font_entries) = font_choices(&external_fonts, true);
    window.set_term_fonts(ModelRc::from(Rc::new(VecModel::from(font_labels))));
    let (mut ui_labels, mut ui_entries) = font_choices(&external_fonts, false);
    // 界面字体列表最前面补一项「跟随系统（自动）」= 出厂默认（空串）的显示项。
    // 没有它时，选择器只能显示"字体栈里第一个可枚举的家族"—— macOS 上那是
    // Helvetica Neue，而实际首选是 SF Pro Text，显示与事实不符。
    ui_labels.insert(0, crate::app::fonts_ui::auto_font_label().into());
    ui_entries.insert(0, crate::app::FontEntry::Auto);
    window.set_ui_fonts(ModelRc::from(Rc::new(VecModel::from(ui_labels))));
    // 索引换算统一走 FontCatalog，启动播种与「还原本页默认」共用同一套规则
    // —— 否则还原时极易漏掉索引，导致选择器停在旧项、显示与实际字体不符。
    let fonts = FontCatalog {
        term: Rc::new(font_entries),
        ui: Rc::new(ui_entries),
    };
    let saved_family = store.borrow().font_family().to_string();
    window.set_term_font_index(fonts.term_index(&saved_family));
    // 索引必须与「实际生效的字体」对应：未设置时 `ui_font_family()` 返回空串，
    // 而真正生效的是平台默认（一个字体栈）。用解析后的值换算，才能与「还原本页
    // 默认」走到同一个结果 —— 否则启动显示与还原显示会不一致。
    // 索引按**存储值**算：空串 = auto → 「跟随系统（自动）」条目。
    // （用解析后的字体栈去算索引会落到"栈里第一个可枚举的家族" —— macOS 上是
    // Helvetica Neue，而实际状态是 auto。显示文本仍用解析后的字体栈。）
    let ui_stored = store.borrow().ui_font_family().to_string();
    window.set_ui_font_index(fonts.ui_index(&ui_stored));

    // Command bar (#55): seed quick commands + history from the config. Groups
    // start collapsed by default (#55).
    window.set_quick_commands(quick_cmd_model(
        &store.borrow(),
        &all_quick_group_names(&store.borrow()),
    ));
    window.set_command_history(history_model(&store.borrow()));
    window.set_history_view(history_view_model(&store.borrow(), "")); // #101
    window.set_history_preview(history_preview_model(&store.borrow(), ""));

    settings::transfer::bind(window, store, sftp_follow_cd);
    settings::sync::bind(window, store, sessions_model);
    settings::appearance::bind(window, store, bufs, proc_win);
    settings::terminal::bind(window, store, bufs);
    settings::layout::bind(window, store);



    // Zen (focus) mode: sidebar + tab strip hidden, persisted across launches.
    window.set_zen_mode(store.borrow().zen_mode());



    // Terminal: EOL conversion + OSC 52 clipboard.  Read-once seed + persist.
    {
        let s = store.borrow();
        window.set_convert_eol(s.convert_eol());
        window.set_osc52_clipboard(s.osc52_clipboard());
        crate::terminal::vt_adapter::OSC52_ENABLED
            .store(s.osc52_clipboard(), std::sync::atomic::Ordering::Relaxed);
        crate::webdav::set_webdav_cert_pin(s.webdav_cert_pin());
    }
    {
        let s = store.borrow();
        window.set_hide_special_partitions(s.hide_special_partitions());
        window.set_mount_filter(s.mount_filter().into());
    }

    // Capture the user's preferred size. The first native Resized event
    // drives restoration below; this is deterministic and avoids guessing
    // how long Slint/window-manager initialization takes (#278).
    {
        let s = store.borrow();
        let (ww, wh) = s.window_size();
        let preferred = (ww > 0.0 && wh > 0.0).then_some((ww, wh));
        pending_window_size_restore.set(preferred);
    }



    settings::update::bind(window, store);









    // Session-sync upload setting (#sync). Persisted; only has effect while the
    // session-sync toggle is on. Read live from the window in the upload handler.


    // WebDAV config sync (#185): manual upload/download of the portable session
    // export JSON. It is intentionally not automatic on startup.










    // ── Settings: per-page "restore defaults" ─────────────────────────────
    //
    // A 类字段写回 `ConfigFile::default()`；B 类（自定义规则数据）保留，仅把每条
    // `enabled` 置 false（"取消使用"而非删除）。
    //
    // ⚠ 修复说明：此前五个页面各自调用一次 `window.on_reset_page(...)`，靠
    // `if page != "xxx" { return }` 分流。但 Slint 的 `on_*` 是 **setter** ——
    // 后一次注册会替换前一次，因此只有最后一个页面（当时是 "update"）的 handler
    // 真正生效，其余四页的按钮点了毫无反应且完全静默。现在改为注册一次、
    // 由 `reset_page` 内部分派。
    {
        let weak = window.as_weak();
        let r_store = store.clone();
        let r_bufs = bufs.clone();
        let r_refs = ResetRefs {
            fonts: fonts.clone(),
            sftp_follow_cd: sftp_follow_cd.clone(),
            // 外观页还原可能改变深浅色（默认壁纸是暗色的），已打开的进程监视窗
            // 要跟着换肤 —— 与用户手动选壁纸走同一条同步路径。
            proc_win: proc_win.as_weak(),
        };
        window.on_reset_page(move |page: slint::SharedString| {
            let Some(w) = weak.upgrade() else { return };
            reset_page(&w, &r_store, &r_bufs, &r_refs, page.as_str());
        });
    }















    window.set_wsl_profiles(wsl_profile_model(&store.borrow()));
    {
        let weak = window.as_weak();
        window.on_pick_wsl_directory(move || {
            if let Some(folder) = rfd::FileDialog::new().pick_folder()
                && let Some(w) = weak.upgrade()
            {
                w.set_wsl_new_directory(folder.to_string_lossy().to_string().into());
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_add_wsl_profile(move |name, distribution, directory| {
            let mut s = store.borrow_mut();
            s.add_wsl_profile(
                name.to_string(),
                distribution.to_string(),
                directory.to_string(),
            );
            s.save_logging();
            if let Some(w) = weak.upgrade() {
                w.set_wsl_profiles(wsl_profile_model(&s));
                sync_sessions_to_model(&s, &sessions_model);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let sessions_model = sessions_model.clone();
        window.on_remove_wsl_profile(move |id| {
            let mut s = store.borrow_mut();
            s.remove_wsl_profile(id.as_str());
            s.save_logging();
            if let Some(w) = weak.upgrade() {
                w.set_wsl_profiles(wsl_profile_model(&s));
                sync_sessions_to_model(&s, &sessions_model);
            }
        });
    }


    {
        let weak = window.as_weak();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_content_resized(move |w: f32, h: f32| {
            content_size.set((w, h));
            if let Some(win) = weak.upgrade() {
                refresh_panes(
                    &win,
                    &layout,
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }
        });
    }

    // Per-session SFTP state: collapse + sizes live in each tab's TerminalState so
    // split panes / other tabs each keep their own (resizing/collapsing one no
    // longer bleeds onto the rest) (#v0.5).
    {
        let terminals_model = terminals_model.clone();
        window.on_set_pane_sftp_collapsed(move |tab_id: SharedString, v: bool| {
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_collapsed = v);
        });
    }
    {
        let terminals_model = terminals_model.clone();
        let weak = window.as_weak();
        window.on_set_pane_sftp_height(move |tab_id: SharedString, v: f32| {
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_panel_height = v);
            // Mirror to the global default so it persists (saved on close) and
            // seeds new sessions; other open tabs use their own field, unaffected.
            if let Some(w) = weak.upgrade() {
                w.set_sftp_panel_height(v);
            }
        });
    }
    {
        let terminals_model = terminals_model.clone();
        let weak = window.as_weak();
        window.on_set_pane_sftp_width(move |tab_id: SharedString, v: f32| {
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_panel_width = v);
            if let Some(w) = weak.upgrade() {
                w.set_sftp_panel_width(v);
            }
        });
    }

    {
        let proc_weak = proc_win.as_weak();
        let handles = handles.clone();
        let statuses = tab_statuses.clone();
        let runtime = runtime.clone();
        proc_win.on_terminate_process(
            move |tab_id: SharedString, pid: SharedString, password: SharedString| {
                let tab_id = tab_id.to_string();
                let Ok(pid) = pid.parse::<u32>() else {
                    set_process_action_error(&proc_weak, t("无效的 PID", "Invalid PID"));
                    return;
                };

                // Re-check the source tab, PID, and owner against the latest sample;
                // the main window may have switched tabs since the menu was opened.
                let ownership = {
                    let states = statuses.lock().unwrap_or_else(|e| e.into_inner());
                    states.get(&tab_id).map_or_else(
                        || Err(t("当前会话不可用", "The current session is unavailable")),
                        |status| {
                            status
                                .procs
                                .iter()
                                .find(|p| p.pid == pid)
                                .map(|process| process_needs_root(&status.user, &process.user))
                                .ok_or_else(|| t("进程已退出", "The process has already exited"))
                        },
                    )
                };
                let needs_root = match ownership {
                    Ok(value) => value,
                    Err(message) => {
                        set_process_action_error(&proc_weak, message);
                        return;
                    }
                };
                if needs_root && password.is_empty() {
                    set_process_action_error(
                        &proc_weak,
                        t(
                            "请输入管理员（sudo）密码",
                            "Enter the administrator (sudo) password",
                        ),
                    );
                    return;
                }

                let root_password =
                    needs_root.then(|| crate::config::Secret::new(password.to_string()));
                let response = handles
                    .borrow()
                    .get(&tab_id)
                    .map(|handle| handle.kill_process(pid, root_password));
                let Some(response) = response else {
                    set_process_action_error(
                        &proc_weak,
                        t("SSH 会话不可用", "The SSH session is unavailable"),
                    );
                    return;
                };

                let done_weak = proc_weak.clone();
                runtime.spawn(async move {
                    let result = response
                        .await
                        .unwrap_or_else(|_| crate::ssh::ProcessKillResult {
                            success: false,
                            message: t("SSH 会话已关闭", "The SSH session has closed").to_string(),
                        });
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(pw) = done_weak.upgrade() {
                            pw.set_action_busy(false);
                            pw.set_action_error(!result.success);
                            pw.set_action_status(result.message.into());
                        }
                    });
                });
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::FontEntry;

    /// Collect every reset-page id the UI actually wires up.
    ///
    /// 设置页合并为单页后，还原按钮以
    /// `ResetButton { page: "x"; reset(p) => { root.reset-page(p); } }` 声明，
    /// 因此解析 `page:` 引号里的 id。写错（或按钮被删）会让集合变化，
    /// 下面的测试就会失败 —— 不会静默放过。
    fn reset_page_ids_from_ui() -> std::collections::BTreeSet<String> {
        const PAGE: &str = include_str!("../../ui/shell/page_settings.slint");
        let mut ids = std::collections::BTreeSet::new();
        for line in PAGE.lines() {
            let Some((_, rest)) = line.split_once("page:") else {
                continue;
            };
            let Some((_, after_quote)) = rest.split_once('"') else {
                continue;
            };
            let Some((id, _)) = after_quote.split_once('"') else {
                continue;
            };
            ids.insert(id.to_string());
        }
        ids
    }

    /// 每一页的「还原本页默认」按钮都必须在 `reset_page` 里有对应分支。
    ///
    /// 这条测试锁的是一次真实发生过的回归：五页各自注册一次 `window.on_reset_page`，
    /// 而 Slint 的 `on_*` 是 setter —— 后注册覆盖先注册，导致四页按钮静默失效。
    /// 若将来新增了按钮却忘了加分支（或改名导致对不上），这里会失败。
    #[test]
    fn every_reset_page_button_has_a_handler() {
        let ids = reset_page_ids_from_ui();
        assert!(
            !ids.is_empty(),
            "没有从 page_settings.slint 解析到任何还原按钮 —— 测试本身已失效，请修正解析逻辑"
        );
        for id in &ids {
            assert!(
                SettingsPage::parse(id).is_some(),
                "UI 有「还原本页默认」按钮，但后端 reset_page 没有处理 page={id:?}"
            );
        }
    }

    /// 反向保障：已知的页面 id 不能写错（否则 UI 传来的字符串永远匹配不上）。
    ///
    /// 注意规格：WSL / 同步 / 新版本提示三页没有「还原本页默认」按钮 ——
    /// "update" 因此必须**不可**解析（曾经存在，2026-09 按规格移除）。
    #[test]
    fn known_page_ids_round_trip() {
        for id in ["terminal", "appearance", "transfer"] {
            assert!(SettingsPage::parse(id).is_some(), "{id} 应当可解析");
        }
        for id in ["", "wsl", "sync", "update", "Terminal", "terminals"] {
            assert!(SettingsPage::parse(id).is_none(), "{id} 不应可解析");
        }
    }

    /// 与真实字体列表同构的目录：下标 0 是分组标题，其后才是可选家族。
    fn test_catalog() -> FontCatalog {
        FontCatalog {
            term: Rc::new(vec![
                FontEntry::Header("内嵌字体"),
                FontEntry::Family("JetBrains Mono".into()),
                FontEntry::Family("Meatshell Mono".into()),
            ]),
            ui: Rc::new(vec![
                FontEntry::Header("内嵌字体"),
                FontEntry::Family("JetBrains Mono".into()),
                FontEntry::Family("Heiti SC".into()),
                FontEntry::Family("Helvetica Neue".into()),
            ]),
        }
    }

    /// 界面字体索引**永远不能落在分组标题上**。
    ///
    /// 锁的是一次真实回归：auto（空串）被硬性映射到下标 0，而 0 是 `▍内嵌字体`
    /// 这条不可选的分组标题，于是「界面字体」一栏显示成了"内嵌字体"。
    #[test]
    fn ui_index_never_lands_on_a_group_header() {
        let fonts = test_catalog();
        // 空串 = auto；未安装 / 解析不到的家族也会走同一条回退路径。
        for input in ["", "不存在的字体", "Noto Sans Xyz"] {
            let i = fonts.ui_index(input);
            assert!(
                matches!(fonts.ui[i as usize], FontEntry::Family(_)),
                "ui_index({input:?}) = {i} 落在分组标题上 —— 选择器会显示标题而不是字体名"
            );
        }
    }

    /// 出厂默认（空串 = auto）指向「跟随系统（自动）」条目 —— 既不是分组标题，
    /// 也不是某个被猜出来的家族名（那样选择器显示的字体会与实际渲染的不符）。
    #[test]
    fn ui_index_points_the_default_at_the_auto_entry() {
        // 真实列表：auto 在最前，其后才是分组标题与各家族。
        let fonts = FontCatalog {
            term: Rc::new(vec![FontEntry::Family("JetBrains Mono".into())]),
            ui: Rc::new(vec![
                FontEntry::Auto,
                FontEntry::Header("内嵌字体"),
                FontEntry::Family("JetBrains Mono".into()),
                FontEntry::Family("Heiti SC".into()),
                FontEntry::Family("Helvetica Neue".into()),
            ]),
        };
        assert_eq!(fonts.ui_index(""), 0, "空串应命中 auto 条目");
        assert_eq!(fonts.ui_index("Heiti SC"), 3, "显式家族按名字命中（含 auto 偏移）");

        // 没有 auto 条目时（终端列表那种形态）仍回退到第一个可选家族，
        // 且不会落到分组标题上。
        let plain = test_catalog();
        let i = plain.ui_index("");
        assert!(matches!(plain.ui[i as usize], FontEntry::Family(_)));
    }
}

#[cfg(test)]
mod wiring_tests {


    /// UI 上位于本页、但**有意不由还原处理**的属性。每一条都要写清原因。
    const UI_ONLY: &[&str] = &[
        // 平台 / 语言 / 交互态
        "lang-en", "is-mac", "is-windows", "ifd-page", "reset-armed", "current-index",
        // 子组件内部属性（SettingRow / Switch / Stepper / 颜色按钮 …）
        "value", "text", "icon", "label", "active", "desc", "on", "selected", "hex",
        "swatch-color", "preview-color", "cursor-kind", "minimum", "maximum", "step", "unit",
        // 由输入内容派生的 Slint 内部状态
        "scrollback-valid",
        // 分类页淡入容器（CatFade）的**布局转发属性**：只是把 spacing / padding-top
        // 传给内层布局（Rectangle 已有同名属性故改名），不是设置项、无需还原。
        "cat-spacing", "cat-padding-top",
        // 分类页淡出/换页状态机的内部状态（纯 UI，非设置项）
        "cat-shown", "cat-swapping", "cat-out", "cat-opacity",
        // 弹层淡入驱动属性（changed 不可用：is-open 引用触发编译器 panic，见 page_settings 注释）
        "wp-appear", "font-appear", "tfont-appear",
        // 渲染档位的**显示名**（不是配置字段）：真正参与还原的是 `renderer-mode`，
        // 这几个只是「值 ↔ 界面文字」映射用的常量，随语言/平台变化。
        "lbl-auto", "lbl-soft", "lbl-gpu",
        // 主题（深浅）档位的**显示名**：真正参与还原的是配置里的 `dark`，
        // 这两个只是"值 ↔ 界面文字"映射用的常量，随语言变化。
        "lbl-dark", "lbl-light",
        // 壁纸分区的临时开关：编译期常量（置回 true 即恢复），不是设置项。
        "wallpaper-enabled",
        // 一次性瞬态
        "renderer-restart-required",
        // 选择器数据源（列表本身不随还原变化）
        "term-fonts", "ui-fonts",
        // 自定义高亮规则的编辑草稿（非持久化字段）
        "new-rule-pattern", "new-rule-regex", "new-rule-case",
        "new-rule-whole", "new-rule-color",
        // 分类导航与页面结构态
        "current-cat", "cat-visible-0", "cat-visible-1", "cat-visible-2",
        "cat-visible-4", "cat-visible-5", "cat-visible-6", "nav-search",
        // 还原按钮（两段式确认）自身状态与文案
        "armed", "cancel-label", "label-armed", "page",
        // 磨砂滑杆手势仲裁 / 接力层瞬态
        "frost-dragging", "frost-pin-y", "frost-idle", "frost-overlay",
        "frost-overlay-init", "frost-overlay-idle", "frost-overlay-last-x",
        "frost-drag-value", "frost-travel",
        // 调色盘 HSV 草稿（非持久化）
        "hue", "sat",
        // 挂载过滤输入草稿（mount-filter 是 B 类，草稿随动不还原）
        "mount-filter-text",
        // 滚回上限（随大容量开关派生，非独立设置）
        "scrollback-max",
        // 关于页展示数据
        "about-libs", "app-version",
        // 快捷键分区展示数据（无还原语义）
        "keys",
        // 页面布局态 / 组件内部
        "top-inset", "width", "val", "valid",
        // 云同步状态展示（运行数据，非设置）
        "webdav-status",
        // 组件内部属性 / 文案
        "glyph", "sub", "title", "opts", "presets",
    ];

    /// 本页显示、但**有意不还原**的项 —— B 类用户数据。
    const NOT_RESET_BY_DESIGN: &[(&str, &str)] = &[
        // 更新检查的**实时状态**：由 `updater.rs` 在后台线程里写（检查中 / 已是最新 /
        // 发现新版本），不是用户可编辑的偏好，还原它没有意义（点「立即检查」就会刷新）。
        (
            "update-check-status",
            "更新检查的实时状态由 updater 写入，不是可还原的偏好",
        ),
        (
            "update-checking",
            "同上：检查进行中的标志，由 updater 写入",
        ),
        (
            "mount-filter",
            "挂载点过滤是用户自定义数据（B 类），与自定义高亮规则一样只保留不还原",
        ),
        (
            "sync-upload-enabled",
            "云同步开关是用户数据（B 类），同步页没有还原按钮",
        ),
        // 云同步的连接信息全部是用户数据（B 类），同步页没有还原按钮
        (
            "webdav-enabled",
            "云同步开关是用户数据（B 类），同步页没有还原按钮",
        ),
        ("webdav-url", "同上：用户连接数据，不参与还原"),
        ("webdav-username", "同上：用户连接数据，不参与还原"),
        ("webdav-password", "同上：用户连接数据，不参与还原"),
        ("webdav-remote-path", "同上：用户连接数据，不参与还原"),
        ("webdav-certs", "同上：用户连接数据，不参与还原"),
        // 更新分区按规格没有还原按钮（检查频率 / 渠道 / 开关都是用户偏好）
        ("update-check", "更新偏好是用户数据（B 类），更新分区没有还原按钮"),
        ("update-channel", "同上：更新偏好不参与还原"),
        ("update-freq-index", "检查频率的界面索引（B 类，不参与还原）"),
        ("update-freq-labels", "检查频率的界面文案映射（随语言变化）"),
        ("update-last-check", "上次检查时间是运行数据，不是设置"),
    ];

    /// 由还原函数**调用到的辅助函数**间接落地的属性。
    const COVERED_BY_HELPER: &[(&str, &str)] = &[
        ("current-wallpaper", "apply_wallpaper 内写入"),
        ("custom-wallpaper-name", "apply_wallpaper 内写入"),
        ("wp-is-custom", "apply_wallpaper 内写入"),
        ("accent-choice", "apply_accent 内写入"),
        ("accent-name", "apply_accent 内写入"),
        ("wallpaper-labels", "publish_wallpaper_choices 内写入"),
        ("wallpaper-index", "publish_wallpaper_choices 内写入"),
        ("term-cursor-color", "apply_cursor_color 内写入"),
        ("term-cursor-choice", "apply_cursor_color 内写入"),
        ("term-cursor-color-hex", "apply_cursor_color 内写入"),
        ("accent-hex", "apply_accent 内写入"),
        ("accent-presets", "apply_accent 内写入"),
        // 页面级别名：新设置页的属性名 ≠ shell 属性名，还原写 shell 属性后经
        // 绑定传导到页面 —— 测试的按名比对认不出这层别名，在此登记。
        ("flag-osc", "页面别名 = osc52-clipboard（terminal reset 内写入）"),
        ("hide-partitions", "页面别名 = hide-special-partitions（appearance reset 内写入）"),
        ("collapse-sftp", "页面别名 = collapse-sftp-default（transfer reset 内写入）"),
    ];

    fn snake(kebab: &str) -> String {
        kebab.replace('-', "_")
    }

    /// 一段 Slint 源码里 **root.<prop>** 形式的属性引用（排除回调调用）。
    ///
    /// 设置页合并为单页后，「这一页绑定了哪些设置」的权威来源就是
    /// **页面文件本身**（`ui/shell/page_settings.slint`）里的 `root.<prop>` 引用。
    fn props_in(src: &str) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        let mut from = 0;
        while let Some(i) = src[from..].find("root.") {
            let s = from + i + "root.".len();
            let name: String = src[s..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            // 后接 '(' 的是回调调用，不是属性引用。
            let is_call = src[s + name.len()..].starts_with('(');
            let advance = s + name.len().max(1);
            if !name.is_empty() && !is_call {
                out.insert(name);
            }
            from = advance;
        }
        out
    }

    fn fn_body(src: &str, name: &str) -> String {
        let sig = format!("fn {name}(");
        let start = src.find(&sig).unwrap_or_else(|| panic!("找不到函数 {name}"));
        let rest = &src[start..];
        let end = rest.find("\n}\n").map(|i| i + 3).unwrap_or(rest.len());
        rest[..end].to_string()
    }

    /// **每页还原必须覆盖该页绑定的每一个设置属性。**
    ///
    /// 锁的是一类真实 bug：还原只写了配置字段与控件属性，漏掉派生状态 ——
    /// 字体选择器索引（G1/G2）、SFTP 跟随的原子标志（G3）、规则状态文案（G4）。
    /// 它们的共同特征是"UI 显示已还原、实际没生效"，人工 review 极难发现。
    #[test]
    fn every_page_property_is_covered_by_its_reset() {
        // 设置页合并为单页后，三页的还原函数**共同**覆盖页面绑定的全部可还原
        // 属性（单页文件同时包含多页分区，无法再按文件拆分断言）。
        let page_src = include_str!("../../ui/shell/page_settings.slint");
        let combined_reset_body: String = [
            (include_str!("settings/terminal.rs"), "reset"),
            (include_str!("settings/appearance.rs"), "reset"),
            (include_str!("settings/transfer.rs"), "reset"),
        ]
        .into_iter()
        .map(|(src, f)| fn_body(src, f))
        .collect();

        let mut uncovered = Vec::new();
        for prop in props_in(page_src) {
            if UI_ONLY.contains(&prop.as_str())
                || NOT_RESET_BY_DESIGN.iter().any(|(p, _)| *p == prop)
                || COVERED_BY_HELPER.iter().any(|(p, _)| *p == prop)
            {
                continue;
            }
            if !combined_reset_body.contains(&snake(&prop)) {
                uncovered.push(prop);
            }
        }
        assert!(
            uncovered.is_empty(),
            "以下设置项在页面绑定，但「还原本页默认」没有处理：\n  {}",
            uncovered.join("\n  ")
        );
    }

    /// 防漏：页面文件必须真的解析出属性（解析逻辑失效时要立刻发现）。
    #[test]
    fn allowlists_only_mention_pages_that_exist() {
        let page_src = include_str!("../../ui/shell/page_settings.slint");
        assert!(
            !props_in(page_src).is_empty(),
            "设置页解析不到任何属性 —— 测试的解析逻辑已失效"
        );
    }

    /// 「默认蓝」那条色表行的两个 hex 必须与 `theme.slint` 的 `accent-default` 一致。
    ///
    /// 两处都在描述"出厂默认蓝"：色块显示的是色表里的值，真正生效的是 Theme 里的值 ——
    /// 一处分叉就会出现"色块是蓝的、界面不是"这种没人会去查的差异。
    #[test]
    fn default_accent_matches_theme_slint() {
        let theme = include_str!("../../ui/theme.slint");
        let (dark, light) = crate::app::settings::appearance::ACCENT_DEFAULT;
        let expected = format!("accent-default: dark ? {dark} : {light}");
        assert!(
            theme.contains(&expected),
            "theme.slint 里找不到 `{expected}` —— 出厂默认蓝与色表已经分叉"
        );
    }

}
