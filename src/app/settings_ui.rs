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

/// Shared config-store handle (`Rc<RefCell<ConfigStore>>`).
type Store = Rc<RefCell<ConfigStore>>;

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
}

impl SettingsPage {
    pub(crate) fn parse(id: &str) -> Option<Self> {
        match id {
            "terminal" => Some(Self::Terminal),
            "appearance" => Some(Self::Appearance),
            "layout" => Some(Self::Layout),
            "transfer" => Some(Self::Transfer),
            _ => None,
        }
    }
}

/// Dispatch one `reset-page` request from the UI to the page that owns it.
/// Re-apply the persisted layout preferences to the live window: panel docking
/// sides and sizes, collapse states, and — critically — the conflict resolution
/// between panels that would otherwise share the same dock edge. Two expanded
/// panels docked on one side overlap, which is exactly the "broken layout"
/// reported after resetting the layout page.
///
/// Shared by startup seeding and by the layout page's "restore defaults".
fn apply_layout_prefs(w: &AppWindow, store: &Store) {
    let s = store.borrow();
    let collapse_sidebar = s.collapse_sidebar_default();
    let collapse_sftp = s.collapse_sftp_default();
    let sidebar_dock = s.sidebar_dock();
    let welcome_as_sidebar = s.welcome_as_sidebar();
    let quick_commands_as_sidebar = s.quick_commands_as_sidebar();
    let quick_panel_open = quick_commands_as_sidebar && s.quick_panel_open();
    let quick_panel_collapsed = s.quick_panel_collapsed();
    let quick_panel_dock = s.quick_panel_dock();
    let welcome_sidebar_dock = s.welcome_sidebar_dock();
    let mut sidebar_collapsed = s.sidebar_collapsed().unwrap_or(collapse_sidebar);
    let mut welcome_collapsed = s.welcome_collapsed().unwrap_or(false);
    if welcome_as_sidebar
        && sidebar_dock == welcome_sidebar_dock
        && !sidebar_collapsed
        && !welcome_collapsed
    {
        sidebar_collapsed = true;
    }
    if quick_panel_open && !quick_panel_collapsed {
        if sidebar_dock == quick_panel_dock {
            sidebar_collapsed = true;
        }
        if welcome_as_sidebar && welcome_sidebar_dock == quick_panel_dock {
            welcome_collapsed = true;
        }
    }
    w.set_collapse_sidebar_default(collapse_sidebar);
    w.set_collapse_sftp_default(collapse_sftp);
    // Restore the persisted panel docking layout (#dock).
    w.set_sidebar_width(s.sidebar_width());
    w.set_sidebar_height(s.sidebar_height());
    w.set_sidebar_dock(sidebar_dock.into());
    w.set_sftp_panel_width(s.sftp_panel_width());
    w.set_sftp_panel_height(s.sftp_panel_height());
    w.set_sftp_dock(s.sftp_dock().into());
    w.set_quick_commands_as_sidebar(quick_commands_as_sidebar);
    w.set_quick_panel_open(quick_panel_open);
    w.set_quick_panel_collapsed(quick_panel_collapsed);
    w.set_quick_panel_width(s.quick_panel_width());
    w.set_quick_panel_height(s.quick_panel_height());
    w.set_quick_panel_dock(quick_panel_dock.into());
    w.set_welcome_as_sidebar(welcome_as_sidebar);
    w.set_welcome_sidebar_width(s.welcome_sidebar_width());
    w.set_welcome_sidebar_dock(welcome_sidebar_dock.into());
    w.set_welcome_collapsed(welcome_collapsed);
    w.set_sidebar_collapsed(sidebar_collapsed);
    w.set_wallpaper_overlay(s.wallpaper_overlay());
    w.set_update_check_enabled(s.update_check_enabled()); // #184
    // 动画开关此前是 Slint-only 全局、从不持久化；现在与其它偏好一样由 config 驱动。
    w.set_animations_enabled(s.animations_enabled());
    if collapse_sftp {
        w.set_sftp_collapsed(true);
        w.set_sftp_saved_height(s.sftp_panel_height());
    }
}

/// 布局迁移需要的句柄（全是 `Rc`，克隆廉价）。
#[derive(Clone)]
struct LayoutRefs {
    layout: Rc<RefCell<crate::layout::Layout>>,
    content_size: Rc<std::cell::Cell<(f32, f32)>>,
    tabs_model: Rc<VecModel<TabInfo>>,
    panes_model: Rc<VecModel<PaneInfo>>,
    splitters_model: Rc<VecModel<SplitterInfo>>,
}

fn reset_page(w: &AppWindow, store: &Store, bufs: &TermBuffers, refs: &LayoutRefs, page: &str) {
    let Some(page) = SettingsPage::parse(page) else {
        tracing::warn!("reset-page: unknown settings page {page:?}");
        return;
    };
    match page {
        SettingsPage::Terminal => reset_terminal_page(w, store, bufs),
        SettingsPage::Appearance => reset_appearance_page(w, store, bufs),
        SettingsPage::Layout => reset_layout_page(w, store, refs),
        SettingsPage::Transfer => reset_transfer_page(w, store),
    }
}

/// 终端页：字体 / 光标 / 回滚 / 高亮 / 粘贴行尾 / OSC52 / JSON 格式化
fn reset_terminal_page(w: &AppWindow, store: &Store, bufs: &TermBuffers) {
    let d = crate::config::fresh_config();
    {
        let mut s = store.borrow_mut();
        s.set_font_family(d.font_family.clone());
        s.set_font_size(d.font_size);
        s.set_terminal_bold(d.terminal_bold);
        s.set_terminal_cursor_style(d.terminal_cursor_style.clone());
        s.set_terminal_cursor_color(&d.terminal_cursor_color);
        s.set_scrollback_lines(d.scrollback_lines);
        s.set_output_highlight_enabled(!d.output_highlight_disabled);
        s.set_output_highlight_preset(d.output_highlight_preset.clone());
        // 终端页其余 A 类项：粘贴行尾 / OSC52 / JSON 格式化
        s.set_convert_eol(d.convert_eol);
        s.set_osc52_clipboard(d.osc52_clipboard);
        s.set_json_format_output(!d.json_format_disabled);
        // B 类：自定义规则数据保留，仅取消使用（enabled=false）
        for index in 0..s.output_highlight_rules().len() {
            s.set_output_highlight_rule_enabled(index, false);
        }
        if let Err(error) = s.save() {
            tracing::warn!("failed to save config: {error:#}");
        }
    }
    // UI 刷新走 getter（带 0 → 默认 的哨兵映射），保证显示的就是真实生效值，
    // 而不是把派生 Default 的 0 / "" 原样写进控件。
    let rules;
    {
        let s = store.borrow();
        w.set_term_font_family(s.font_family().into());
        w.set_term_font_size(s.font_size() as f32);
        w.set_term_font_bold(s.terminal_bold());
        w.set_term_cursor_style(s.terminal_cursor_style().into());
        w.set_term_cursor_color_hex(s.terminal_cursor_color().into());
        if let Some(color) = parse_hex_color(s.terminal_cursor_color()) {
            w.set_term_cursor_color(color);
        }
        w.set_scrollback_lines(s.scrollback_lines().to_string().into());
        w.set_output_highlight_enabled(s.output_highlight_enabled());
        w.set_output_highlight_preset(s.output_highlight_preset().into());
        w.set_output_highlight_rules(output_highlight_rule_model(&s));
        w.set_convert_eol(s.convert_eol());
        w.set_osc52_clipboard(s.osc52_clipboard());
        w.set_json_format_output(s.json_format_output());
        rules = s.output_highlight_rules().to_vec();
    }
    // 回滚行数变更 → 终端缓冲 reset；高亮按新 preset / 规则重编译。
    for_each_buffer(w, bufs, |b| b.reset(d.scrollback_lines));
    apply_output_highlight(w, bufs, !d.output_highlight_disabled, &d.output_highlight_preset);
    apply_custom_output_rules(w, bufs, &rules);
}

/// 外观页：UI 字体 / 壁纸 / 遮罩透明度 / 渲染后端 / 动画 / 缩放 / 面板字体 /
/// 隐藏特殊分区。
///
/// 注意两点：其一，「隐藏特殊分区」的控件在 UI 上位于本页（此前误归到传输页的
/// 还原里）；其二，壁纸与遮罩透明度按规格也在还原范围内（此前被当作 B 类跳过）。
fn reset_appearance_page(w: &AppWindow, store: &Store, bufs: &TermBuffers) {
    let d = crate::config::fresh_config();
    {
        let mut s = store.borrow_mut();
        s.set_ui_font_family(d.ui_font_family.clone());
        s.set_ui_scale(d.ui_scale);
        s.set_panel_font(d.panel_font);
        s.set_renderer_mode(d.renderer_mode.clone());
        s.set_wallpaper(d.wallpaper.clone());
        s.set_wallpaper_overlay(d.wallpaper_overlay);
        s.set_hide_special_partitions(d.hide_special_partitions);
        if let Err(error) = s.save() {
            tracing::warn!("failed to save config: {error:#}");
        }
    }
    // UI 刷新走 getter（0 → 默认 / 平台默认）。
    let s = store.borrow();
    w.set_ui_font_family(resolve_ui_font_family());
    w.set_ui_scale(s.ui_scale() as f32 / 100.0);
    w.set_panel_font(s.panel_font() as f32 / 100.0);
    w.set_renderer_mode(s.renderer_mode().into());
    w.set_wallpaper_overlay(s.wallpaper_overlay());
    w.set_hide_special_partitions(s.hide_special_partitions());
    drop(s);
    // 壁纸切换有完整的换肤 / 调色板派生流程，必须走 apply_wallpaper。
    apply_wallpaper(w, &store.borrow(), bufs, &d.wallpaper, false);
    // 动画开关没有后端持久化（Slint 全局，重启即回），还原即重新开启。
    w.set_animations_enabled(true);
}

/// 布局页：侧栏开关 / 默认折叠 / 停靠边。
/// 注：侧栏宽度是拖拽产生的交互状态（设置页无对应控件），不纳入还原。
fn reset_layout_page(w: &AppWindow, store: &Store, refs: &LayoutRefs) {
    let d = crate::config::ConfigFile::default();
    {
        let mut s = store.borrow_mut();
        s.set_welcome_as_sidebar(d.welcome_as_sidebar);
        s.set_quick_commands_as_sidebar(d.quick_commands_as_sidebar);
        s.set_collapse_sidebar_default(d.collapse_sidebar_default);
        s.set_collapse_sftp_default(d.collapse_sftp_default);
        s.set_sidebar_dock(d.sidebar_dock.clone());
        if let Err(error) = s.save() {
            tracing::warn!("failed to save config: {error:#}");
        }
    }
    // 布局是派生状态：sidebar_dock / welcome_as_sidebar 变了之后，dock 冲突消解、
    // 各面板几何量、窗格树都必须重算 —— 只 set 属性会留下陈旧的窗格模型，表现为
    // 面板相互重叠（用户报告的布局错乱）。而 welcome_as_sidebar 又是双向绑定
    // 属性，在回调里直接改会递归销毁 Welcome 子树（#323），所以整个视觉迁移
    // 延迟一帧执行 —— 与运行时开关 on_set_welcome_as_sidebar 同一套路。
    let weak = w.as_weak();
    let r_store = (*store).clone();
    let r = refs.clone();
    slint::Timer::single_shot(std::time::Duration::ZERO, move || {
        let Some(w) = weak.upgrade() else { return };
        apply_layout_prefs(&w, &r_store);
        // welcome 页在侧栏与窗格树之间迁移，必须与属性保持一致。
        let welcome = r_store.borrow().welcome_as_sidebar();
        {
            let mut lay = r.layout.borrow_mut();
            if welcome {
                lay.remove_tab("welcome");
            } else if lay.leaf_of_tab("welcome").is_none() {
                lay.add_tab("welcome".into());
            }
        }
        refresh_panes(
            &w,
            &r.layout.borrow(),
            r.content_size.get(),
            &r.tabs_model,
            &r.panes_model,
            &r.splitters_model,
        );
    });
}

/// 传输页：A 类 = 跟随 cd / 总是询问保存位置。B 类保留：挂载点过滤、下载目录。
/// 注意：「隐藏特殊分区」的控件在 UI 上位于外观页，其还原也归外观页。
fn reset_transfer_page(w: &AppWindow, store: &Store) {
    let d = crate::config::fresh_config();
    // sftp_follow_cd 的存储字段是 sftp_no_follow_cd（语义反转）
    let follow_cd = !d.sftp_no_follow_cd;
    {
        let mut s = store.borrow_mut();
        s.set_sftp_follow_cd(follow_cd);
        s.set_download_always_ask(d.download_always_ask);
        if let Err(error) = s.save() {
            tracing::warn!("failed to save config: {error:#}");
        }
    }
    w.set_sftp_follow_cd(follow_cd);
    w.set_download_always_ask(d.download_always_ask);
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

    // Apply the saved (or system-detected) theme.
    // "dark" / "light" → use that directly; "system" or unset → ask the OS;
    // OS unknown → fall back to dark.
    {
        let is_dark = theme_pref_is_dark(&store.borrow());
        window.set_dark_mode(is_dark);
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
            window.set_term_font_family(fam.into());
        }
        window.set_term_font_cjk(cjk);
        window.set_term_font_size(s.font_size() as f32);
        window.set_term_font_bold(s.terminal_bold());
        window.set_scrollback_lines(s.scrollback_lines().to_string().into());
        window.set_term_cursor_style(s.terminal_cursor_style().into());
        if let Some(color) = parse_hex_color(s.terminal_cursor_color()) {
            window.set_term_cursor_color_hex(s.terminal_cursor_color().into());
            window.set_term_cursor_color(color);
        }
        window.set_output_highlight_enabled(s.output_highlight_enabled());
        window.set_output_highlight_preset(s.output_highlight_preset().into());
        window.set_output_highlight_rules(output_highlight_rule_model(&s));
        window.set_json_format_output(s.json_format_output());
        window.set_ui_scale(s.ui_scale() as f32 / 100.0); // global UI zoom (#100)
        window.set_panel_font(s.panel_font() as f32 / 100.0); // settings-panel font scale
        window.set_renderer_mode(s.renderer_mode().into());
    }

    // Apply the saved immersive wallpaper (overrides dark/light when set; a
    // missing custom file falls back to the plain theme).
    {
        let id = store.borrow().wallpaper().to_string();
        // Restoring a saved wallpaper must not override the user's persisted
        // light/dark preference. Built-in wallpapers only suggest their paired
        // theme when the user actively selects them (#theme-persistence).
        apply_wallpaper(window, &store.borrow(), bufs, &id, false);
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
    window.set_ui_font_family(resolve_ui_font_family());
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
    // Populate the Interface font picker: embedded first, external next,
    // system monospace families last, each labelled with its source.
    let (font_labels, font_entries) = font_choices(&external_fonts, true);
    window.set_term_fonts(ModelRc::from(Rc::new(VecModel::from(font_labels))));
    // Restore the saved family: find its index in the picker list (fall back
    // to the first selectable family when it isn't listed).
    let saved_family = store.borrow().font_family().to_string();
    let font_index = font_entries
        .iter()
        .position(|e| matches!(e, FontEntry::Family(f) if *f == saved_family))
        .or_else(|| {
            font_entries
                .iter()
                .position(|e| matches!(e, FontEntry::Family(_)))
        })
        .unwrap_or(0);
    window.set_term_font_index(font_index as i32);

    // UI font picker: embedded + external + system (proportional fonts allowed).
    let (ui_labels, ui_entries) = font_choices(&external_fonts, false);
    window.set_ui_fonts(ModelRc::from(Rc::new(VecModel::from(ui_labels))));
    // Restore saved UI font index (empty string = auto-detect).
    let ui_saved = store.borrow().ui_font_family().to_string();
    let ui_index = if ui_saved.is_empty() {
        0 // "auto" position — first header
    } else {
        ui_entries
            .iter()
            .position(|e| matches!(e, FontEntry::Family(f) if *f == ui_saved))
            .unwrap_or(0)
    };
    window.set_ui_font_index(ui_index as i32);

    // Command bar (#55): seed quick commands + history from the config. Groups
    // start collapsed by default (#55).
    window.set_quick_commands(quick_cmd_model(
        &store.borrow(),
        &all_quick_group_names(&store.borrow()),
    ));
    window.set_command_history(history_model(&store.borrow()));
    window.set_history_view(history_view_model(&store.borrow(), "")); // #101

    // Interface setting: SFTP follows the terminal's cd (the flag itself lives
    // in `AppContext`; see the note in the pump).
    window.set_sftp_follow_cd(store.borrow().sftp_follow_cd());
    {
        let store = store.clone();
        let flag = sftp_follow_cd.clone();
        window.on_set_sftp_follow_cd(move |follow| {
            flag.store(follow, std::sync::atomic::Ordering::Relaxed);
            let mut s = store.borrow_mut();
            s.set_sftp_follow_cd(follow);
            let _ = s.save();
        });
    }

    // Interface setting: always ask where to save on download (#87). Read live
    // by the download handler from the window property, so just set + persist.
    window.set_download_always_ask(store.borrow().download_always_ask());
    {
        let store = store.clone();
        window.on_set_download_always_ask(move |ask| {
            let mut s = store.borrow_mut();
            s.set_download_always_ask(ask);
            let _ = s.save();
        });
    }

    // Toolbar toggle: hide/show the quick-command bar (persisted globally).
    window.set_cmd_bar_hidden(store.borrow().cmd_bar_hidden());
    {
        let store = store.clone();
        window.on_set_cmd_bar_hidden(move |hidden| {
            let mut s = store.borrow_mut();
            s.set_cmd_bar_hidden(hidden);
            if let Err(error) = s.save() {
                tracing::warn!("failed to save config: {error:#}");
            }
        });
    }

    // Zen (focus) mode: sidebar + tab strip hidden, persisted across launches.
    window.set_zen_mode(store.borrow().zen_mode());
    {
        let store = store.clone();
        window.on_set_zen_mode(move |enabled| {
            let mut s = store.borrow_mut();
            s.set_zen_mode(enabled);
            if let Err(error) = s.save() {
                tracing::warn!("failed to save config: {error:#}");
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_toggle_zen_key(move || {
            if let Some(w) = weak.upgrade() {
                let next = !w.get_zen_mode();
                let mut s = store.borrow_mut();
                s.set_zen_mode(next);
                if let Err(error) = s.save() {
                    tracing::warn!("failed to save config: {error:#}");
                }
                w.set_zen_mode(next);
            }
        });
    }

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
    {
        let store = store.clone();
        window.on_set_collapse_sidebar_default(move |v| {
            let mut s = store.borrow_mut();
            s.set_collapse_sidebar_default(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_animations_enabled(move |v| {
            let mut s = store.borrow_mut();
            s.set_animations_enabled(v);
            if let Err(error) = s.save() {
                tracing::warn!("failed to save config: {error:#}");
            }
        });
    }
    {
        let store = store.clone();
        window.on_set_quick_commands_as_sidebar(move |v| {
            let mut s = store.borrow_mut();
            s.set_quick_commands_as_sidebar(v);
            let _ = s.save();
        });
    }
    {
        // Toggle the startup new-version check (#184). Takes effect next launch
        // for the check itself; the banner just won't appear once it's off.
        let store = store.clone();
        window.on_set_update_check_enabled(move |v| {
            let mut s = store.borrow_mut();
            s.set_update_check_enabled(v);
            let _ = s.save();
        });
    }
    {
        // Renderer selection is consumed before the first native window exists,
        // so persist it now and apply it on the next launch (#280).
        let store = store.clone();
        window.on_set_renderer_mode(move |mode: SharedString| {
            let mut s = store.borrow_mut();
            s.set_renderer_mode(mode.to_string());
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_persist_sidebar_width(move |w| {
            let mut s = store.borrow_mut();
            s.set_sidebar_width(w);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        let handles = handles.clone();
        window.on_set_sidebar_collapsed(move |v| {
            let mut s = store.borrow_mut();
            s.set_sidebar_collapsed(v);
            let _ = s.save();
            // Pause resource monitoring for every live session while the
            // sidebar is hidden; resume when it comes back (upstream b17da25).
            for handle in handles.borrow().values() {
                handle.set_resource_monitoring(!v);
            }
        });
    }
    {
        let store = store.clone();
        window.on_persist_welcome_sidebar_width(move |w| {
            let mut s = store.borrow_mut();
            s.set_welcome_sidebar_width(w);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_persist_welcome_sidebar_dock(move |dock| {
            let mut s = store.borrow_mut();
            s.set_welcome_sidebar_dock(dock.to_string());
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_welcome_collapsed(move |v| {
            let mut s = store.borrow_mut();
            s.set_welcome_collapsed(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_persist_wallpaper_overlay(move |v| {
            let mut s = store.borrow_mut();
            s.set_wallpaper_overlay(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_collapse_sftp_default(move |v| {
            let mut s = store.borrow_mut();
            s.set_collapse_sftp_default(v);
            let _ = s.save();
        });
    }

    // Session-sync upload setting (#sync). Persisted; only has effect while the
    // session-sync toggle is on. Read live from the window in the upload handler.
    window.set_sync_upload_enabled(store.borrow().sync_upload());
    {
        let store = store.clone();
        window.on_set_sync_upload_enabled(move |v| {
            let mut s = store.borrow_mut();
            s.set_sync_upload(v);
            let _ = s.save();
        });
    }

    // WebDAV config sync (#185): manual upload/download of the portable session
    // export JSON. It is intentionally not automatic on startup.
    {
        let s = store.borrow();
        window.set_webdav_enabled(s.webdav_enabled());
        window.set_webdav_url(s.webdav_url().into());
        window.set_webdav_username(s.webdav_username().into());
        window.set_webdav_password(s.webdav_password().into());
        window.set_webdav_remote_path(s.webdav_remote_path().into());
        window.set_webdav_accept_invalid_certs(s.webdav_accept_invalid_certs());
        window.set_webdav_status(String::new().into());
    }
    {
        let store = store.clone();
        window.on_save_webdav_settings(
            move |enabled: bool,
                  url: SharedString,
                  username: SharedString,
                  password: SharedString,
                  remote_path: SharedString,
                  accept_invalid_certs: bool| {
                let mut s = store.borrow_mut();
                s.set_webdav_settings(
                    enabled,
                    url.to_string(),
                    username.to_string(),
                    password.to_string(),
                    remote_path.to_string(),
                    accept_invalid_certs,
                );
                let _ = s.save();
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
            let password = w.get_webdav_password().to_string();
            let remote_path = w.get_webdav_remote_path().to_string();
            let accept_invalid_certs = w.get_webdav_accept_invalid_certs();
            {
                let mut s = store.borrow_mut();
                s.set_webdav_settings(
                    enabled,
                    url.clone(),
                    username.clone(),
                    password.clone(),
                    remote_path.clone(),
                    accept_invalid_certs,
                );
                let _ = s.save();
            }
            if !enabled {
                w.set_webdav_status(t("请先启用 WebDAV 同步", "enable WebDAV sync first").into());
                return;
            }
            let res = store.borrow().export_json().and_then(|(json, count)| {
                webdav_put_json(
                    &url,
                    &remote_path,
                    &username,
                    &password,
                    accept_invalid_certs,
                    json,
                )
                .map(|_| count)
            });
            let msg = match res {
                Ok(n) => format!("{} {}", t("已上传连接", "uploaded connections"), n),
                Err(e) => format!("{}: {}", t("上传失败", "upload failed"), e),
            };
            w.set_webdav_status(msg.into());
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_cursor_color(move |value: SharedString| {
            let Some(color) = parse_hex_color(value.as_str()) else {
                return false;
            };
            {
                let mut s = store.borrow_mut();
                if !s.set_terminal_cursor_color(value.as_str()) {
                    return false;
                }
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_cursor_color(color);
            }
            true
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_add_output_highlight_rule(
            move |pattern: SharedString,
                  is_regex,
                  case_sensitive,
                  whole_line,
                  color: SharedString| {
                let pattern = pattern.trim().to_string();
                let validation = validate_output_highlight_rule(&pattern, is_regex, case_sensitive);
                let Some(w) = weak.upgrade() else {
                    return false;
                };
                if let Err(message) = validation {
                    w.set_output_highlight_rule_status(message.into());
                    return false;
                }
                if store.borrow().output_highlight_rules().len() >= 128 {
                    w.set_output_highlight_rule_status(
                        t("自定义规则最多 128 条", "Custom rules are limited to 128").into(),
                    );
                    return false;
                }
                {
                    let mut s = store.borrow_mut();
                    s.add_output_highlight_rule(OutputHighlightRule {
                        pattern,
                        regex: is_regex,
                        case_sensitive,
                        whole_line,
                        color: color.to_string(),
                        enabled: true,
                    });
                    let _ = s.save();
                    w.set_output_highlight_rules(output_highlight_rule_model(&s));
                    apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
                }
                w.set_output_highlight_rule_status("".into());
                true
            },
        );
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_remove_output_highlight_rule(move |index| {
            let Some(w) = weak.upgrade() else { return };
            let mut s = store.borrow_mut();
            s.remove_output_highlight_rule(index.max(0) as usize);
            let _ = s.save();
            w.set_output_highlight_rules(output_highlight_rule_model(&s));
            apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
            w.set_output_highlight_rule_status("".into());
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_output_highlight_rule_enabled(move |index, enabled| {
            let Some(w) = weak.upgrade() else { return };
            let mut s = store.borrow_mut();
            s.set_output_highlight_rule_enabled(index.max(0) as usize, enabled);
            let _ = s.save();
            w.set_output_highlight_rules(output_highlight_rule_model(&s));
            apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
        });
    }
    // Interface settings: apply + persist the terminal font family / size.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font(move |label: SharedString| {
            // The picker labels entries with their source; store only the
            // bare family name so the config stays portable. Group headers
            // (▍…) are not selectable — ignore them.
            let Some(family) = family_from_label(&label) else {
                return;
            };
            {
                let mut s = store.borrow_mut();
                s.set_font_family(family.to_string());
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_font_family(family.into());
                w.set_term_font_cjk(term_font_covers_cjk(family));
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_ui_font(move |label: SharedString| {
            let Some(family) = family_from_label(&label) else {
                return;
            };
            {
                let mut s = store.borrow_mut();
                s.set_ui_font_family(family.to_string());
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_ui_font_family(family.into());
            }
        });
    }
    // Output highlighting: persist the switch/preset and immediately rebuild
    // every open terminal, including scrollback captured before the change.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_output_highlight(move |enabled, preset: SharedString| {
            let preset = preset.to_string();
            {
                let mut s = store.borrow_mut();
                s.set_output_highlight_enabled(enabled);
                s.set_output_highlight_preset(preset.clone());
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                apply_output_highlight(&w, &bufs, enabled, &preset);
            }
        });
    }
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
        let r_refs = LayoutRefs {
            layout: layout.clone(),
            content_size: content_size.clone(),
            tabs_model: tabs_model.clone(),
            panes_model: panes_model.clone(),
            splitters_model: splitters_model.clone(),
        };
        window.on_reset_page(move |page: slint::SharedString| {
            let Some(w) = weak.upgrade() else { return };
            reset_page(&w, &r_store, &r_bufs, &r_refs, page.as_str());
        });
    }
    {
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_json_format_output(move |enabled| {
            {
                let mut s = store.borrow_mut();
                s.set_json_format_output(enabled);
                let _ = s.save();
            }
            // Flip live buffers so the change applies without reconnecting.
            for buffer in bufs.lock().unwrap_or_else(|e| e.into_inner()).values() {
                buffer.lock().unwrap_or_else(|e| e.into_inner()).json_format_output = enabled;
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font_size(move |size: i32| {
            {
                let mut s = store.borrow_mut();
                s.set_font_size(size as u32);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_font_size(size as f32);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font_bold(move |bold: bool| {
            {
                let mut s = store.borrow_mut();
                s.set_terminal_bold(bold);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_font_bold(bold);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_scrollback_lines(move |lines: slint::SharedString| -> bool {
            // Validate: 100..=1_000_000. Malformed input is rejected (UI shows
            // the invalid state) and nothing is persisted.
            let digits: String = lines.chars().filter(|c| c.is_ascii_digit()).collect();
            match digits.parse::<usize>() {
                Ok(n) if (100..=1_000_000).contains(&n) => {
                    let mut s = store.borrow_mut();
                    s.set_scrollback_lines(n);
                    let _ = s.save();
                    // Write the canonical value back to the UI so the settings
                    // panel (conditionally rendered) shows the new value when
                    // reopened — without this it reverts to the stale one.
                    if let Some(w) = weak.upgrade() {
                        w.set_scrollback_lines(digits.into());
                    }
                    true
                }
                _ => false,
            }
        });
    }
    {
        let store = store.clone();
        window.on_set_convert_eol(move |v: bool| {
            let mut s = store.borrow_mut();
            s.set_convert_eol(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_osc52_clipboard(move |v: bool| {
            let mut s = store.borrow_mut();
            s.set_osc52_clipboard(v);
            let _ = s.save();
            crate::terminal::vt_adapter::OSC52_ENABLED
                .store(v, std::sync::atomic::Ordering::Relaxed);
        });
    }
    {
        let store = store.clone();
        window.on_set_hide_special_partitions(move |v: bool| {
            let mut s = store.borrow_mut();
            s.set_hide_special_partitions(v);
            let _ = s.save();
        });
    }
    {
        let store = store.clone();
        window.on_set_mount_filter(move |v: slint::SharedString| {
            let mut s = store.borrow_mut();
            s.set_mount_filter(v.to_string());
            let _ = s.save();
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_cursor_style(move |style: SharedString| {
            let normalized = {
                let mut s = store.borrow_mut();
                s.set_terminal_cursor_style(style.to_string());
                let normalized = s.terminal_cursor_style().to_string();
                let _ = s.save();
                normalized
            };
            if let Some(w) = weak.upgrade() {
                w.set_term_cursor_style(normalized.into());
            }
        });
    }
    // Global UI scale (#100): persist the percent and apply it live.
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_ui_scale(move |percent: i32| {
            let clamped = (percent.max(0) as u32).clamp(80, 200);
            {
                let mut s = store.borrow_mut();
                s.set_ui_scale(clamped);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_ui_scale(clamped as f32 / 100.0);
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_panel_font(move |percent: i32| {
            let clamped = (percent.max(0) as u32).clamp(80, 160);
            {
                let mut s = store.borrow_mut();
                s.set_panel_font(clamped);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_panel_font(clamped as f32 / 100.0);
            }
        });
    }

    // Wallpaper: pick a built-in / none, or open the file dialog for a custom one.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_wp = bufs.clone();
        let proc_weak = proc_win.as_weak();
        window.on_set_wallpaper(move |id: SharedString| {
            let id = id.to_string();
            let mut selected_builtin_theme = None;
            if let Some(w) = weak.upgrade() {
                apply_wallpaper(&w, &store.borrow(), &bufs_wp, &id, true);
                if crate::wallpaper::is_builtin(&id) {
                    selected_builtin_theme = Some(w.get_dark_mode());
                }
                // Keep an already-open process window in sync with the change.
                if let Some(p) = proc_weak.upgrade() {
                    sync_proc_theme(&w, &p);
                }
            }
            let mut s = store.borrow_mut();
            s.set_wallpaper(id);
            // Choosing a built-in wallpaper applies its recommended palette once;
            // persist that result so it too survives the next launch. A later
            // manual theme toggle will overwrite this preference as expected.
            if let Some(dark) = selected_builtin_theme {
                s.set_theme_pref(if dark { "dark" } else { "light" }.to_string());
            }
            let _ = s.save();
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_wp = bufs.clone();
        let proc_weak = proc_win.as_weak();
        window.on_pick_wallpaper_file(move || {
            let picked = rfd::FileDialog::new()
                .set_title(t("选择壁纸", "Choose wallpaper"))
                .add_filter("Images", &["png", "jpg", "jpeg", "webp", "bmp"])
                .pick_file();
            if let Some(path) = picked {
                let id = path.to_string_lossy().to_string();
                if let Some(w) = weak.upgrade() {
                    apply_wallpaper(&w, &store.borrow(), &bufs_wp, &id, false);
                    if let Some(p) = proc_weak.upgrade() {
                        sync_proc_theme(&w, &p);
                    }
                }
                let mut s = store.borrow_mut();
                s.set_wallpaper(id);
                let _ = s.save();
            }
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
            let _ = s.save();
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
            let _ = s.save();
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
        window.on_webdav_download(move || {
            let Some(w) = weak.upgrade() else { return };
            let enabled = w.get_webdav_enabled();
            let url = w.get_webdav_url().to_string();
            let username = w.get_webdav_username().to_string();
            let password = w.get_webdav_password().to_string();
            let remote_path = w.get_webdav_remote_path().to_string();
            let accept_invalid_certs = w.get_webdav_accept_invalid_certs();
            {
                let mut s = store.borrow_mut();
                s.set_webdav_settings(
                    enabled,
                    url.clone(),
                    username.clone(),
                    password.clone(),
                    remote_path.clone(),
                    accept_invalid_certs,
                );
                let _ = s.save();
            }
            if !enabled {
                w.set_webdav_status(t("请先启用 WebDAV 同步", "enable WebDAV sync first").into());
                return;
            }
            let res = webdav_get_json(
                &url,
                &remote_path,
                &username,
                &password,
                accept_invalid_certs,
            )
            .and_then(|json| store.borrow_mut().import_json(&json));
            let msg = match res {
                Ok((added, skipped)) => {
                    sync_sessions_for_window(&weak, &store.borrow(), &sessions_model);
                    format!(
                        "{} {}, {} {}",
                        t("已导入", "imported"),
                        added,
                        t("跳过", "skipped"),
                        skipped
                    )
                }
                Err(e) => format!("{}: {}", t("下载失败", "download failed"), e),
            };
            w.set_webdav_status(msg.into());
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
                    &layout.borrow(),
                    content_size.get(),
                    &tabs_model,
                    &panes_model,
                    &splitters_model,
                );
            }
        });
    }
    // Toggle welcome-as-sidebar at runtime: persist, then move the welcome tab in
    // or out of the split-tree (sidebar mode = no welcome tab) and re-flatten.
    {
        let weak = window.as_weak();
        let store = store.clone();
        let layout = layout.clone();
        let content_size = content_size.clone();
        let tabs_model = tabs_model.clone();
        let panes_model = panes_model.clone();
        let splitters_model = splitters_model.clone();
        window.on_set_welcome_as_sidebar(move |v| {
            // Persist first: saving config never touches the Slint tree, and
            // doing it synchronously means an immediate window close cannot
            // lose the preference. Only the property/layout transition has to
            // wait — it is two-way-bound through InterfacePanel and changing it
            // destroys/recreates the Welcome subtree that owns the Switch, so
            // defer the *entire* transition until this callback has returned;
            // deferring only refresh_panes still destroys the component tree
            // recursively on Windows (#323).
            {
                let mut s = store.borrow_mut();
                s.set_welcome_as_sidebar(v);
                if let Err(error) = s.save() {
                    tracing::warn!("failed to save config: {error:#}");
                }
            }
            let weak = weak.clone();
            let layout = layout.clone();
            let content_size = content_size.clone();
            let tabs_model = tabs_model.clone();
            let panes_model = panes_model.clone();
            let splitters_model = splitters_model.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                if let Some(w) = weak.upgrade() {
                    w.set_welcome_as_sidebar(v);
                    {
                        let mut lay = layout.borrow_mut();
                        if v {
                            lay.remove_tab("welcome");
                        } else if lay.leaf_of_tab("welcome").is_none() {
                            lay.add_tab("welcome".into());
                        }
                    }
                    refresh_panes(
                        &w,
                        &layout.borrow(),
                        content_size.get(),
                        &tabs_model,
                        &panes_model,
                        &splitters_model,
                    );
                }
            });
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
        let terminals_model = terminals_model.clone();
        window.on_set_pane_sftp_saved_height(move |tab_id: SharedString, v: f32| {
            update_terminal_row(&terminals_model, &tab_id, |r| r.sftp_saved_height = v);
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

    /// Collect every `root.reset-page("…")` id the UI actually emits.
    fn reset_page_ids_from_ui() -> std::collections::BTreeSet<String> {
        const UI: &str = include_str!("../../ui/interface_panel.slint");
        let mut ids = std::collections::BTreeSet::new();
        for line in UI.lines() {
            let Some((_, rest)) = line.split_once("root.reset-page(") else {
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
}
