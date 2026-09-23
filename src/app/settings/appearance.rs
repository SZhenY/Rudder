//! 外观页：UI 字体 / 壁纸与遮罩 / 渲染后端 / 动画 / 缩放 / 面板字体 / 隐藏特殊分区。
//!
//! 该页有两处容易被漏掉的落点，都在这里一并处理：
//! * `ui-font-index`（字体选择器索引）—— 只改 family 会让下拉框停在旧项；
//! * 壁纸切换要走 `apply_wallpaper`（完整的换肤与调色板派生），不能只 set 属性。

use slint::{Color, ComponentHandle, ModelRc, SharedString, VecModel};
use std::rc::Rc;

use super::{FontCatalog, Store, persist};
use crate::app::apply_wallpaper;
use crate::app::fonts_ui::{family_from_label, resolve_ui_font_family};
use crate::app::resource_ui::sync_proc_theme;
use crate::app::terminal_ui::{apply_dark_mode, parse_hex_color, theme_pref_is_dark};
use crate::i18n::t;
use crate::terminal::TermBuffers;
use crate::ui::{ AccentPreset, AnimationSettings, AppWindow, ProcWindow, Theme };

/// 播种 + 注册持久化回调。
pub(crate) fn bind(window: &AppWindow, store: &Store, bufs: &TermBuffers, proc_win: &ProcWindow) {
    {
        let store = store.clone();
        window.on_set_animations_enabled(move |v| {
            persist(&store, |s| {
                s.set_animations_enabled(v);
            });
        });
    }

    {
        // Renderer selection is consumed before the first native window exists,
        // so persist it now and apply it on the next launch (#280).
        let store = store.clone();
        window.on_set_renderer_mode(move |mode: SharedString| {
            persist(&store, |s| {
                s.set_renderer_mode(mode.to_string());
            });
        });
    }

    {
        let store = store.clone();
        window.on_persist_wallpaper_overlay(move |v| {
            persist(&store, |s| {
                s.set_wallpaper_overlay(v);
            });
        });
    }

    {
        let store = store.clone();
        window.on_set_hide_special_partitions(move |v: bool| {
            persist(&store, |s| {
                s.set_hide_special_partitions(v);
            });
        });
    }

    {
        let store = store.clone();
        window.on_set_mount_filter(move |v: slint::SharedString| {
            persist(&store, |s| {
                s.set_mount_filter(v.to_string());
            });
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_ui_scale(move |percent: i32| {
            let clamped = clamp_ui_scale(percent);
            {
                persist(&store, |s| {
                    s.set_ui_scale(clamped);
                });
            }
            if let Some(w) = weak.upgrade() {
                w.global::<Theme>().set_ui_scale(clamped as f32 / 100.0);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_panel_font(move |percent: i32| {
            let clamped = clamp_panel_font(percent);
            {
                persist(&store, |s| {
                    s.set_panel_font(clamped);
                });
            }
            if let Some(w) = weak.upgrade() {
                w.global::<Theme>().set_panel_font(clamped as f32 / 100.0);
            }
        });
    }

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
                    selected_builtin_theme = Some(w.global::<Theme>().get_dark());
                }
                // Keep an already-open process window in sync with the change.
                if let Some(p) = proc_weak.upgrade() {
                    sync_proc_theme(&w, &p);
                }
            }
            persist(&store, |s| {
                s.set_wallpaper(id);
                // Choosing a built-in wallpaper applies its recommended palette once;
                // persist that result so it too survives the next launch. A later
                // manual theme toggle will overwrite this preference as expected.
                if let Some(dark) = selected_builtin_theme {
                    s.set_theme_pref(if dark { "dark" } else { "light" }.to_string());
                }
            });
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
                persist(&store, |s| {
                    s.set_wallpaper(id);
                });
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
                persist(&store, |s| {
                    s.set_ui_font_family(family.to_string());
                });
            }
            if let Some(w) = weak.upgrade() {
                // 写回**解析后**的值：显式选择的家族原样透传，选中「跟随系统（自动）」
                // 时存储的是空串、解析出来也是空串 → Slint 用它自己的平台默认字体。
                // ⚠️ 不能是逗号分隔的字体栈：Slint 的 `font-family` 是单个家族名，
                // 整串会被当成一个不存在的家族，界面看起来毫无变化。
                w.global::<Theme>().set_ui_font_family(resolve_ui_font_family());
            }
        });
    }

    {
        // 主题（深浅）：跟随系统 / 深色 / 浅色。切档要同时做三件事 —— 写偏好、换肤、
        // **按新档位重新解析主题色**（预设两档是两个颜色，自定义色在浅色档要压深）。
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_mode = bufs.clone();
        let proc_weak = proc_win.as_weak();
        window.on_set_appearance_mode(move |mode: SharedString| {
            let normalized = match mode.as_str() {
                "dark" | "light" => mode.to_string(),
                _ => "system".to_string(),
            };
            persist(&store, |s| {
                s.set_theme_pref(normalized.clone());
            });
            let Some(w) = weak.upgrade() else { return };
            apply_dark_mode(&w, &bufs_mode, theme_pref_is_dark(&store.borrow()));
            let choice = store.borrow().accent().to_string();
            apply_accent(&w, &choice);
            // 光标色在"跟随主题"时也要按新档位重新取。
            let cursor = store.borrow().terminal_cursor_color().to_string();
            super::terminal::apply_cursor_color(&w, &cursor);
            w.set_accent_mode(normalized.into());
            if let Some(p) = proc_weak.upgrade() {
                sync_proc_theme(&w, &p);
            }
        });
    }

    {
        // 主题色：预设 id / "#RRGGBB" / ""（出厂默认）。非法值返回 false —— 界面据此
        // 标红，并且**既不应用也不持久化**（与光标取色框同一套约定）。
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_accent(move |v: SharedString| -> bool {
            let Some(normalized) = normalize_accent(v.as_str()) else {
                return false;
            };
            persist(&store, |s| {
                s.set_accent(normalized.clone());
            });
            if let Some(w) = weak.upgrade() {
                apply_accent(&w, &normalized);
            }
            true
        });
    }
}

// ── 配色：主题色 ────────────────────────────────────────────────────────
//
// 预设色表是**唯一出处**：界面上的一排色块由它生成（颜色按当前深浅档解析好再送进
// Slint），配置里只存 `id`（或自定义色的 `#RRGGBB`）—— 所以"加一个预设"只改这一处。
//
// 选色取舍 —— 这是给**终端 / SSH 客户端**挑的，不是照抄别家的种子色：
// * 深浅两档各给一个值：浅底上要更深才够对比度，同一个 hex 两档通用必然有一档发灰；
// * 绕开红 / 橙 / 琥珀 —— 与状态色 `danger`(#e25c5c)、`warning`(#e2a84a) 撞车：主色一红，
//   按钮就和"删除 / 警告"分不清了；
// * 绿色只留深松绿：与 `success`（亮薄荷 #4ec9b0）拉开明度，不至于混淆；
// * 石墨是近中性的低饱和档：终端里花花绿绿的 ANSI 输出才是主角，主色不该抢戏。
/// 出厂默认主题色（深色档 / 浅色档）。**必须与 `ui/theme.slint` 的 `accent-default` 一致**
/// —— 由 `default_accent_matches_theme_slint` 测试钉住：色块上显示的颜色与实际生效的颜色
/// 不能分叉（改了这边忘了那边，用户看到的"原版"就不是原版了）。
pub(crate) const ACCENT_DEFAULT: (&str, &str) = ("#4a90e2", "#0071e3");

const ACCENT_PRESETS: &[(&str, &str, &str, &str, &str)] = &[
    // id,       深色档,     浅色档,     中文名,        英文名
    // 第一条是**原版**（Rudder 一直以来的默认蓝）。id 为空串 = 配置里"未选"，
    // 界面上的「原版」色块就是它；`resolve_accent("")` 直接返回 None（不覆盖），
    // 真正的生效值来自 `theme.slint` 的 `accent-default`。
    ("",         ACCENT_DEFAULT.0, ACCENT_DEFAULT.1, "原版（默认）", "Original"),
    ("azure",    "#22a2c9", "#0d7f9e", "天青",         "Azure"),
    ("pine",     "#2fb37e", "#14855a", "松绿",         "Pine"),
    ("indigo",   "#6c7ff0", "#4453d8", "靛蓝",         "Indigo"),
    ("violet",   "#9a6cf0", "#7a3fd6", "紫晶",         "Violet"),
    ("magenta",  "#d456b0", "#b52f8c", "品红",         "Magenta"),
    ("graphite", "#8b929e", "#5f6672", "石墨",         "Graphite"),
];

/// 壁纸相关分区是否开放（与 `ui/settings/pages/appearance.slint` 里的
/// `property <bool> wallpaper-enabled` **成对**，由 `wallpaper_switch_matches_ui` 测试钉住）。
///
/// 关闭时不只是"藏起界面"：`apply_wallpaper` 会整体按"没有壁纸"处理 —— 否则配置里默认的
/// `builtin:dark` 仍然生效，表现为"选了浅色主题，面板是浅的、窗口底色还是深的"（壁纸盖住
/// `window-base`，面板再磨砂叠在它上面），配色分区也就永远调不出亮底。
pub(crate) const WALLPAPER_UI_ENABLED: bool = false;

/// 把界面上的输入归一化成可存储的值：`""`（出厂默认）/ 预设 id / `#RRGGBB`。
///
/// 返回 `None` = 不合法（界面据此标红、既不应用也不持久化）。`#RGB` 简写会展开成
/// 6 位并转大写 —— 与预设 id 的大小写约定一致，比较时不必再 `eq_ignore_ascii_case`。
pub(crate) fn normalize_accent(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() {
        return Some(String::new());
    }
    // 老配置里的 "aurora"（当时的"极光蓝"，其实就是出厂色）统一落到「原版」。
    if s == "aurora" {
        return Some(String::new());
    }
    if let Some(digits) = s.strip_prefix('#') {
        let hex = match digits.len() {
            3 => digits.chars().flat_map(|c| [c, c]).collect::<String>(),
            6 => digits.to_string(),
            _ => return None,
        };
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        return Some(format!("#{}", hex.to_uppercase()));
    }
    ACCENT_PRESETS
        .iter()
        .find(|p| p.0 == s)
        .map(|p| p.0.to_string())
}

/// 当前深浅档下该用什么颜色；`None` = 出厂默认（交给 Theme 里每档的常量）。
fn resolve_accent(choice: &str, dark: bool) -> Option<Color> {
    let s = choice.trim();
    if s.is_empty() {
        return None;
    }
    if s.starts_with('#') {
        let c = parse_hex_color(s)?;
        // 自定义色：浅色档压深 25%，保证在浅面板上仍然读得清。
        return Some(if dark { c } else { scale_color(c, 0.75) });
    }
    let preset = ACCENT_PRESETS.iter().find(|p| p.0 == s)?;
    parse_hex_color(if dark { preset.1 } else { preset.2 })
}

/// 按比例压暗（Slint 语言里的 `.darker()` 在 Rust 侧没有对应 API，这里直接乘通道）。
fn scale_color(c: Color, k: f32) -> Color {
    Color::from_rgb_u8(
        (c.red() as f32 * k) as u8,
        (c.green() as f32 * k) as u8,
        (c.blue() as f32 * k) as u8,
    )
}

/// 送给界面的预设列表：颜色已按当前深浅档解析好，界面只管画（不存第二份色表）。
fn accent_presets_model(dark: bool) -> ModelRc<AccentPreset> {
    let rows: Vec<AccentPreset> = ACCENT_PRESETS
        .iter()
        .map(|(id, dark_hex, light_hex, zh, en)| AccentPreset {
            id: (*id).into(),
            name: t(zh, en).into(),
            color: parse_hex_color(if dark { dark_hex } else { light_hex })
                .unwrap_or(Color::from_rgb_u8(0x4a, 0x90, 0xe2)),
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

/// 界面上「当前配色」显示的名字：预设名 / 自定义色原样 / 原版。
fn accent_display_name(choice: &str) -> SharedString {
    if choice.is_empty() {
        return t("原版（默认）", "Original").into();
    }
    if choice.starts_with('#') {
        return choice.into();
    }
    ACCENT_PRESETS
        .iter()
        .find(|p| p.0 == choice)
        .map(|p| SharedString::from(t(p.3, p.4)))
        .unwrap_or_else(|| choice.into())
}

/// 把配置里的主题色套到界面上。
///
/// ⚠️ 换深浅档后**必须再调一次**：预设的两档本来就是两个颜色，自定义色在浅色档还要压深。
pub(crate) fn apply_accent(w: &AppWindow, choice: &str) {
    let dark = w.global::<Theme>().get_dark();
    let (overridden, seed) = match resolve_accent(choice, dark) {
        Some(c) => (true, c),
        // 未选（出厂默认）：不覆盖，交给 Theme 里每档的常量。
        None => (false, Color::from_rgb_u8(0x4a, 0x90, 0xe2)),
    };
    w.global::<Theme>().set_accent_overridden(overridden);
    w.global::<Theme>().set_accent_seed(seed);
    w.set_accent_choice(choice.into());
    // 自定义色时输入框回显它；切到预设 / 默认就清空输入框（免得显示一个没生效的值）。
    w.set_accent_hex(if choice.starts_with('#') {
        choice.into()
    } else {
        SharedString::new()
    });
    w.set_accent_presets(accent_presets_model(dark));
    w.set_accent_name(accent_display_name(choice));
}

/// 「还原本页默认」：替换外观域为出厂默认，再走与 `bind` 相同的落点。
///
/// 隐藏特殊分区。
///
/// 注意两点：其一，「隐藏特殊分区」的控件在 UI 上位于本页（此前误归到传输页的
/// 还原里）；其二，壁纸与遮罩透明度按规格也在还原范围内（此前被当作 B 类跳过）。
pub(crate) fn reset(
    w: &AppWindow,
    store: &Store,
    bufs: &TermBuffers,
    fonts: &FontCatalog,
    proc_win: &slint::Weak<ProcWindow>,
) {
    let d = crate::config::fresh_config();
    {
        persist(store, |s| {
            s.set_ui_font_family(d.appearance.ui_font_family.clone());
            s.set_ui_scale(d.appearance.ui_scale);
            s.set_panel_font(d.appearance.panel_font);
            s.set_renderer_mode(d.appearance.renderer_mode.clone());
            s.set_wallpaper(d.appearance.wallpaper.clone());
            s.set_wallpaper_overlay(d.appearance.wallpaper_overlay);
            s.set_accent(d.appearance.accent.clone());
            s.set_hide_special_partitions(d.appearance.hide_special_partitions);
        });
    }
    // UI 刷新走 getter（0 → 默认 / 平台默认）。
    let s = store.borrow();
    // 出厂默认是**空串 = auto**，还原后必须回到「跟随系统（自动）」条目。
    //
    // 索引按**存储值**算（空串 → Auto 条目）：拿解析后的值去算会落到某个具体家族
    // 条目上，而实际状态明明是 auto。
    let ui_stored = s.ui_font_family().to_string();
    w.global::<Theme>().set_ui_font_family(resolve_ui_font_family());
    w.set_ui_font_index(fonts.ui_index(&ui_stored));
    w.global::<Theme>().set_ui_scale(s.ui_scale() as f32 / 100.0);
    w.global::<Theme>().set_panel_font(s.panel_font() as f32 / 100.0);
    w.set_renderer_mode(s.renderer_mode().into());
    w.global::<Theme>().set_panel_alpha(s.wallpaper_overlay());
    w.set_hide_special_partitions(s.hide_special_partitions());
    drop(s);
    // 壁纸切换有完整的换肤 / 调色板派生流程，必须走 apply_wallpaper。
    //
    // ⚠️ 这里的 `apply_builtin_theme` 必须是 **true**。出厂默认壁纸是 `builtin:dark`，
    // 而用户此前可能停在"简约·浅"：只换图、不套用它推荐的深浅色，就会得到
    // "背景已经变暗、外层还罩着一层白"的错配 —— 而且重启也不会自愈，因为
    // `theme_pref` 仍是浅色。与"用户手选内置壁纸"完全同一套规则。
    apply_wallpaper(w, &store.borrow(), bufs, &d.appearance.wallpaper, true);
    if crate::wallpaper::is_builtin(&d.appearance.wallpaper) {
        // 把刚套用的深浅色持久化（同 on_set_wallpaper），否则下次启动又回到旧偏好。
        let dark = w.global::<Theme>().get_dark();
        persist(store, |s| {
            s.set_theme_pref(if dark { "dark" } else { "light" }.to_string());
        });
    }
    // 主题色：出厂默认是"未选（跟随每档常量）"。放在换肤**之后** —— 壁纸会决定深浅档，
    // 而主题色要按最终档位解析；下拉框也要跟着回到还原后的 theme_pref。
    apply_accent(w, store.borrow().accent());
    w.set_accent_mode(store.borrow().theme_pref().into());
    // 已打开的进程监视窗要跟着换肤（窗口可能没开，upgrade 失败就跳过）。
    if let Some(p) = proc_win.upgrade() {
        sync_proc_theme(w, &p);
    }
    // 动画开关没有后端持久化（Slint 全局，重启即回），还原即重新开启。
    w.global::<AnimationSettings>().set_enabled(true);
}
/// UI 缩放的百分比：先把负数夹到 0（否则 `as u32` 会回绕成 40 亿），再限制 80–200。
fn clamp_ui_scale(percent: i32) -> u32 {
    (percent.max(0) as u32).clamp(80, 200)
}

/// 面板字号的百分比：同上，范围 80–160。
fn clamp_panel_font(percent: i32) -> u32 {
    (percent.max(0) as u32).clamp(80, 160)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 边界 + **负数回绕**（`i32::MIN as u32` 是个巨大值，不能让它通过）。
    #[test]
    fn ui_scale_clamps_below_and_above() {
        assert_eq!(clamp_ui_scale(i32::MIN), 80);
        assert_eq!(clamp_ui_scale(-1), 80);
        assert_eq!(clamp_ui_scale(0), 80);
        assert_eq!(clamp_ui_scale(79), 80);
        assert_eq!(clamp_ui_scale(80), 80);
        assert_eq!(clamp_ui_scale(150), 150);
        assert_eq!(clamp_ui_scale(200), 200);
        assert_eq!(clamp_ui_scale(201), 200);
        assert_eq!(clamp_ui_scale(i32::MAX), 200);
    }

    #[test]
    fn panel_font_clamps_below_and_above() {
        assert_eq!(clamp_panel_font(i32::MIN), 80);
        assert_eq!(clamp_panel_font(0), 80);
        assert_eq!(clamp_panel_font(160), 160);
        assert_eq!(clamp_panel_font(161), 160);
        assert_eq!(clamp_panel_font(i32::MAX), 160);
    }

    /// 主题色输入的归一化：`""` / 预设 id / `#RRGGBB`；`#RGB` 展开并大写；其余非法。
    ///
    /// 非法返回 `None` —— 界面据此标红，且**既不应用也不持久化**。
    #[test]
    fn accent_normalizes_presets_and_hex() {
        assert_eq!(normalize_accent("").unwrap(), "");
        assert_eq!(normalize_accent("   ").unwrap(), "");
        assert_eq!(normalize_accent("graphite").unwrap(), "graphite");
        // 老配置里的 "aurora"（当时的"极光蓝"= 出厂色）统一落到「原版」（空串）。
        assert_eq!(normalize_accent("aurora").unwrap(), "");
        // `#RGB` 简写展开 + 大写：与预设 id 的大小写约定一致，比较时不必再忽略大小写。
        assert_eq!(normalize_accent("#abc").unwrap(), "#AABBCC");
        assert_eq!(normalize_accent(" #1f9fd0 ").unwrap(), "#1F9FD0");
        // 非法：不是预设 id、也不是合法十六进制。
        assert!(normalize_accent("AURORA").is_none());
        assert!(normalize_accent("azurex").is_none());
        assert!(normalize_accent("#12345").is_none());
        assert!(normalize_accent("#gggggg").is_none());
        assert!(normalize_accent("blue").is_none());
    }

    /// 预设解析：深浅两档**各自**取色；未选（`""`）返回 `None` → 交给 Theme 的每档常量。
    #[test]
    fn accent_resolves_per_theme() {
        assert!(resolve_accent("", true).is_none());
        assert!(resolve_accent("", false).is_none());
        // 「原版」就是出厂默认：不覆盖（`None`），交给 Theme 里每档的常量。
        assert!(resolve_accent("aurora", true).is_none()); // 老配置的 aurora 也归到原版
        // 预设两档必须是两个颜色：同一个 hex 两档通用，必然有一档发灰 / 对比度不够。
        assert_ne!(
            resolve_accent("azure", true).unwrap(),
            resolve_accent("azure", false).unwrap()
        );
        // 自定义色在浅色档压深（浅底上保对比度）。
        let on_dark = resolve_accent("#8899AA", true).unwrap();
        let on_light = resolve_accent("#8899AA", false).unwrap();
        assert!(on_light.red() < on_dark.red());
        assert!(on_light.green() < on_dark.green());
        assert!(on_light.blue() < on_dark.blue());
    }
}
