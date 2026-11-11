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
use super::settings::layout::apply_layout_prefs;

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
    Layout,
    Transfer,
    Ai,
}

impl SettingsPage {
    pub(crate) fn parse(id: &str) -> Option<Self> {
        match id {
            "terminal" => Some(Self::Terminal),
            "appearance" => Some(Self::Appearance),
            "layout" => Some(Self::Layout),
            "transfer" => Some(Self::Transfer),
            "ai" => Some(Self::Ai),
            _ => None,
        }
    }
}


/// 「还原本页默认」需要的全部句柄（`Rc` / `Arc`，克隆廉价）。
///
/// 存在的意义是把「一项设置的全部落点」集中起来：除了配置字段与控件属性，
/// 还有派生 UI 状态（字体选择器索引）、运行时镜像（SFTP 跟随标志）等。
/// 注：不派生 `Clone`，全项目只构造一次、按引用传给 `reset_page`。
struct ResetRefs {
    /// 泵线程读取的「SFTP 跟随 cd」实时标志。还原必须同时更新它，否则配置与
    /// 界面都变了、**已打开会话的行为却没变** —— 表现为"还原不生效"。
    sftp_follow_cd: Arc<std::sync::atomic::AtomicBool>,
}

fn reset_page(w: &AppWindow, store: &Store, bufs: &TermBuffers, refs: &ResetRefs, page: &str) {
    let Some(page) = SettingsPage::parse(page) else {
        tracing::warn!("reset-page: unknown settings page {page:?}");
        return;
    };
    // 还原按钮只在设置页里，此时字体列表必然已经构建过；这里是廉价 Rc 拷贝。
    let fonts = ensure_font_pickers(w);
    match page {
        SettingsPage::Terminal => settings::terminal::reset(w, store, bufs, &fonts),
        SettingsPage::Appearance => {
            settings::appearance::reset(w, store, bufs, &fonts, &crate::app::sub_windows::proc_weak())
        }
        SettingsPage::Layout => settings::layout::reset(w, store),
        SettingsPage::Transfer => settings::transfer::reset(w, store, &refs.sftp_follow_cd),
        SettingsPage::Ai => settings::ai::reset(w, store),
    }
}






/// 「跟随系统」的**实时**跟随：启动时只探测一次，之后系统外观改了界面不会自己变。
///
/// 用一个低频定时器补上这一步：**只有** preference 是 `system` 时才真的去问系统
/// （其它偏好下只是读一次配置就返回）。`dark_light::detect()` 实测 ≈ 4.8 ms/次
/// （20 次 95 ms），5 s 一次约 0.1% 单核 —— 换来"改系统外观 → 界面几秒内跟着变"。
///
/// 定时器**故意不 stop**：它随窗口活到进程结束（与系统采样器同一套做法）。
fn start_system_theme_watcher(window: &AppWindow, store: &Store, bufs: &TermBuffers) {
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let store = store.clone();
    let bufs_watch = bufs.clone();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(5),
        move || {
            let Some(w) = weak.upgrade() else { return };
            if store.borrow().theme_pref() != "system" {
                return;
            }
            let dark = theme_pref_is_dark(&store.borrow());
            if dark == w.global::<Theme>().get_dark() {
                return;
            }
            apply_dark_mode(&w, &bufs_watch, dark);
            // 深浅档变了 → 主题色要按新档位重新解析（预设两档是两个颜色，自定义色在
            // 浅色档要压深）；光标色在"跟随主题"时同样按新档位取（深→亮 / 浅→暗）。
            let choice = store.borrow().accent().to_string();
            settings::appearance::apply_accent(&w, &choice);
            let cursor = store.borrow().terminal_cursor_color().to_string();
            settings::terminal::apply_cursor_color(&w, &cursor);
            // 背景图也跟着系统走：「跟随系统」用的内置图是按深浅档取的，不重取就会出现
            // "浅色界面配深色底图"。上传的图片与系统外观无关，跳过。
            let wallpaper_now = store.borrow().wallpaper().to_string();
            if crate::wallpaper::is_builtin(&wallpaper_now) {
                let id = settings::appearance::builtin_wallpaper_for("system", dark);
                if id != wallpaper_now {
                    apply_wallpaper(&w, &store.borrow(), &bufs_watch, id, false);
                    settings::persist(&store, |s| {
                        s.set_wallpaper(id.to_string());
                    });
                    settings::appearance::publish_wallpaper_choices(&w, &store);
                }
            }
            // 系统外观变了 → 设置窗口（独立窗口，`Theme` / `Palette` 各一份副本）也要刷，
            // 否则"跟随系统"自动切换时它还是旧样子。
            crate::app::settings_window::resync_if_open(&w);
        },
    );
    // 存入 thread_local 保活。
    SYSTEM_THEME_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
}

thread_local! {
    /// 外置字体目录注册后得到的家族名（启动只注册，不建列表）。
    static EXTERNAL_FONTS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// 字体选择器条目表；首次打开设置页才构建（`fontdb` 全量扫描）。
    static FONT_CATALOG: std::cell::RefCell<Option<FontCatalog>> =
        const { std::cell::RefCell::new(None) };
    /// 外观轮询定时器，显式持有以保活。
    static SYSTEM_THEME_TIMER: std::cell::RefCell<Option<slint::Timer>> =
        const { std::cell::RefCell::new(None) };
}

/// 构建（或复用）字体选择器列表。首次调用扫描系统字体，之后只拷贝 `Rc`。
pub(super) fn ensure_font_pickers(window: &AppWindow) -> FontCatalog {
    if let Some(catalog) = FONT_CATALOG.with(|slot| slot.borrow().clone()) {
        return catalog;
    }
    let external = EXTERNAL_FONTS.with(|slot| slot.borrow().clone());
    let (font_labels, font_entries) = font_choices(&external, true);
    window.set_term_fonts(ModelRc::from(Rc::new(VecModel::from(font_labels))));
    // 界面字体列表最前面补一项「跟随系统（自动）」= 出厂默认（空串）的显示项。
    let (mut ui_labels, mut ui_entries) = font_choices(&external, false);
    ui_labels.insert(0, crate::app::fonts_ui::auto_font_label().into());
    ui_entries.insert(0, crate::app::FontEntry::Auto);
    window.set_ui_fonts(ModelRc::from(Rc::new(VecModel::from(ui_labels))));
    let catalog = FontCatalog {
        term: Rc::new(font_entries),
        ui: Rc::new(ui_entries),
    };
    let (saved_family, ui_stored) = HISTORY_STORE.with(|store| {
        store
            .borrow()
            .as_ref()
            .map(|store| {
                let store = store.borrow();
                (
                    store.font_family().to_string(),
                    store.ui_font_family().to_string(),
                )
            })
            .unwrap_or_default()
    });
    window.set_term_font_index(catalog.term_index(&saved_family));
    window.set_ui_font_index(catalog.ui_index(&ui_stored));
    FONT_CATALOG.with(|slot| *slot.borrow_mut() = Some(catalog.clone()));
    catalog
}

#[path = "settings_ui_seed_callbacks.rs"] mod settings_ui_seed_callbacks;

pub(super) fn seed_settings(window: &AppWindow, ctx: &AppContext) {
    let AppContext {
        store,
        bufs,
        sessions_model,
        sftp_follow_cd,
        pending_window_size_restore,
        ..
    } = ctx;

    // Apply the saved UI language.  The Rust-side flag drives `i18n::t(...)`;
    // `apply_to_slint` selects the bundled `.po` for the static `@tr(...)` text
    // (must run after the first component exists, which it now does).
    crate::i18n::set_language(store.borrow().language());
    crate::i18n::apply_to_slint();
    window.set_lang_en(crate::i18n::is_en());

    // Apply the saved (or system-detected) theme.
    // "dark" / "light" → use that directly; "system" or unset → ask the OS;
    // OS unknown → fall back to dark.
    {
        let is_dark = theme_pref_is_dark(&store.borrow());
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
        window.set_output_highlight_enabled(s.output_highlight_enabled());
        window.set_output_highlight_preset(s.output_highlight_preset().into());
        window.set_output_highlight_rules(output_highlight_rule_model(&s));
        window.set_json_format_output(s.json_format_output());
        window.global::<Theme>().set_ui_scale(s.ui_scale() as f32 / 100.0); // global UI zoom (#100)
        window.global::<Theme>().set_panel_font(s.panel_font() as f32 / 100.0); // settings-panel font scale
        window.set_renderer_mode(s.renderer_mode_choice().into());
    }

    // Apply the saved immersive wallpaper (overrides dark/light when set; a
    // missing custom file falls back to the plain theme).
    {
        let id = store.borrow().wallpaper().to_string();
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
        let (choice, mode) = {
            let s = store.borrow();
            (s.accent().to_string(), s.theme_pref().to_string())
        };
        let follow_hex = settings::appearance::apply_accent(window, &choice);
        window.set_accent_mode(mode.into());
        // 光标色必须等主题色落定之后才解析：空串 = 跟随主题，取的是 `Theme.accent-solid`，
        // 而它要等壁纸（wp-accent）与主题色都写进去才成立。早一步算，拿到的是出厂蓝。
        let cursor = store.borrow().terminal_cursor_color().to_string();
        // 用 apply_accent 的返回值而不是 Theme.accent-solid：后者此刻还在动画起始帧上。
        settings::terminal::apply_cursor_color_with(window, &cursor, &follow_hex);
    }

    start_system_theme_watcher(window, store, bufs);
    // Editable inputs need a CJK-capable font: native TextInput doesn't
    // glyph-fallback like Text does (#54). An empty ui-font-family lets Slint
    // use the platform default; resolve_ui_font_family falls back to embedded
    // JetBrains Mono when no system font registers, so text never vanishes (#129).
    window.global::<Theme>().set_ui_font_family(resolve_ui_font_family());
    // Runtime font loading: fonts dropped into the fonts dir (Windows:
    // <exe_dir>/config/fonts; macOS/Linux: per-user config dir) are registered
    // with Slint's shared collection so a saved external family renders from the
    // first frame. The picker lists themselves are built on first settings open:
    // `font_choices` scans every system font with fontdb, which startup doesn't
    // need (see `ensure_font_pickers`).
    let fonts_dirs = crate::fonts::external_fonts_dirs();
    tracing::info!(
        "external fonts dirs: {}",
        fonts_dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")
    );
    let external_fonts = crate::fonts::load_external_fonts(&fonts_dirs);
    EXTERNAL_FONTS.with(|slot| *slot.borrow_mut() = external_fonts);

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
    settings::appearance::bind(window, store, bufs);
    settings::terminal::bind(window, store, bufs);
    settings::layout::bind(window, store);


    // Toolbar toggle: hide/show the quick-command bar (persisted globally).
    // 配置存的是 hide_cmd_bar（反向），UI 属性是正向的 show-cmd-bar。
    window.set_show_cmd_bar(!store.borrow().cmd_bar_hidden());


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

    // Interface setting: collapse the sidebars by default (#78). Seed the
    // checkboxes, apply the collapsed state once at startup, and persist toggles.
    apply_layout_prefs(window, store);
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
    // AI 页：MCP 开关 / 监听面 / 令牌 / 权限位。
    settings::ai::bind(window, store);









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
    settings_ui_seed_callbacks::wire_seed_callbacks(window, ctx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::FontEntry;

    /// Collect every reset-page id the UI actually wires up.
    ///
    /// 阶段 A 起按钮不再内联 `root.reset-page("x")`，而是
    /// `ResetBar { page-id: "x"; reset-page(p) => { root.reset-page(p); } }`，
    /// 因此解析目标随之改为 `page-id:`。写错（或按钮被删）会让集合变化，
    /// 下面的测试就会失败 —— 不会静默放过。
    fn reset_page_ids_from_ui() -> std::collections::BTreeSet<String> {
        crate::app::contract_sources::reset_page_ids()
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
            "没有从 interface_panel.slint 解析到任何 reset-page 调用 —— 测试本身已失效，请修正解析逻辑"
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
        for id in ["terminal", "appearance", "layout", "transfer"] {
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
    use crate::app::contract_sources::{
        PAGE_RUST, PAGE_SLINT, THEME_SLINT, fn_body_in, props_in_all, sources_contain,
        sources_for,
    };

    /// UI 上位于本页、但**有意不由还原处理**的属性。每一条都要写清原因。
    const UI_ONLY: &[&str] = &[
        // 平台 / 语言 / 交互态
        "lang-en", "is-mac", "is-windows", "ifd-page", "reset-armed", "current-index",
        // 子组件内部属性（SettingRow / Switch / Stepper / 颜色按钮 …）
        "value", "text", "icon", "label", "active", "desc", "on", "selected", "hex",
        "swatch-color", "preview-color", "cursor-kind", "minimum", "maximum", "step", "unit",
        // 由输入内容派生的 Slint 内部状态
        "scrollback-valid",
        // 渲染档位的**显示名**（不是配置字段）：真正参与还原的是 `renderer-mode`，
        // 这几个只是「值 ↔ 界面文字」映射用的常量，随语言/平台变化。
        "lbl-auto", "lbl-soft", "lbl-gpu",
        // 主题（深浅）档位的**显示名**：真正参与还原的是配置里的 `theme_pref`，
        // 这三个只是"值 ↔ 界面文字"映射用的常量，随语言变化。
        "lbl-system", "lbl-dark", "lbl-light",
        // 壁纸分区的临时开关：编译期常量（置回 true 即恢复），不是设置项。
        "wallpaper-enabled",
        // 一次性瞬态
        "renderer-restart-required",
        // 选择器数据源（列表本身不随还原变化）
        "term-fonts", "ui-fonts",
        // 自定义高亮规则的编辑草稿（非持久化字段）
        "new-rule-pattern", "new-rule-regex", "new-rule-case-sensitive",
        "new-rule-whole-line", "new-rule-color",
    ];

    /// 本页显示、但**有意不还原**的项 —— B 类用户数据。
    const NOT_RESET_BY_DESIGN: &[(&str, &str)] = &[(
        "mount-filter",
        "挂载点过滤是用户自定义数据（B 类），与自定义高亮规则一样只保留不还原",
    )];

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
    ];

    fn snake(kebab: &str) -> String {
        kebab.replace('-', "_")
    }

    /// **每页还原必须覆盖该页绑定的每一个设置属性。**
    ///
    /// 锁的是一类真实 bug：还原只写了配置字段与控件属性，漏掉派生状态 ——
    /// 字体选择器索引（G1/G2）、SFTP 跟随的原子标志（G3）、规则状态文案（G4）。
    /// 它们的共同特征是"UI 显示已还原、实际没生效"，人工 review 极难发现。
    #[test]
    fn every_page_property_is_covered_by_its_reset() {
        // 每页的还原逻辑与页面文件都由 contract_sources 统一登记。
        let mut uncovered = Vec::new();
        for (page, rust_sources) in PAGE_RUST {
            let body = fn_body_in(rust_sources, "reset");
            for prop in props_in_all(sources_for(PAGE_SLINT, page)) {
                if UI_ONLY.contains(&prop.as_str())
                    || NOT_RESET_BY_DESIGN.iter().any(|(p, _)| *p == prop)
                    || COVERED_BY_HELPER.iter().any(|(p, _)| *p == prop)
                {
                    continue;
                }
                if !body.contains(&snake(&prop)) {
                    uncovered.push(format!("{page}: {prop}"));
                }
            }
        }
        assert!(
            uncovered.is_empty(),
            "以下设置项在本页绑定，但「还原本页默认」没有处理：\n  {}",
            uncovered.join("\n  ")
        );
    }

    /// 防漏：页面文件必须真的解析出属性（解析逻辑失效时要立刻发现）。
    #[test]
    fn allowlists_only_mention_pages_that_exist() {
        for (page, sources) in PAGE_SLINT {
            assert!(
                !props_in_all(sources).is_empty(),
                "{page} 页解析不到任何属性 —— 测试的解析逻辑已失效"
            );
        }
    }

    /// 「默认蓝」那条色表行的两个 hex 必须与 `theme.slint` 的 `accent-default` 一致。
    ///
    /// 两处都在描述"出厂默认蓝"：色块显示的是色表里的值，真正生效的是 Theme 里的值 ——
    /// 一处分叉就会出现"色块是蓝的、界面不是"这种没人会去查的差异。
    #[test]
    fn default_accent_matches_theme_slint() {
        let (dark, light) = crate::app::settings::appearance::ACCENT_DEFAULT;
        let expected = format!("accent-default: dark ? {dark} : {light}");
        assert!(
            sources_contain(THEME_SLINT, &expected),
            "theme.slint 里找不到 `{expected}` —— 出厂默认蓝与色表已经分叉"
        );
    }

    /// 壁纸开关：Rust 常量与 Slint 的 `wallpaper-enabled` 必须一致。
    ///
    /// 二者不一致就有过真 bug：界面藏起来了、`apply_wallpaper` 仍在给窗口压壁纸，
    /// 表现是"选了浅色主题，面板是浅的、窗口底色还是深的"。
    #[test]
    fn wallpaper_switch_matches_ui() {
        let page = sources_for(PAGE_SLINT, "appearance");
        let expected = format!(
            "wallpaper-enabled: {}",
            crate::app::settings::appearance::WALLPAPER_UI_ENABLED
        );
        assert!(
            sources_contain(page, &expected),
            "appearance.slint 里的 `{expected}` 与 Rust 的 WALLPAPER_UI_ENABLED 不一致"
        );
    }
}
