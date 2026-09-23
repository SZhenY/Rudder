//! 外观页：UI 字体 / 壁纸与遮罩 / 渲染后端 / 动画 / 缩放 / 面板字体 / 隐藏特殊分区。
//!
//! 该页有两处容易被漏掉的落点，都在这里一并处理：
//! * `ui-font-index`（字体选择器索引）—— 只改 family 会让下拉框停在旧项；
//! * 壁纸切换要走 `apply_wallpaper`（完整的换肤与调色板派生），不能只 set 属性。

use slint::{Color, ComponentHandle, ModelRc, SharedString, VecModel};
use std::rc::Rc;

use super::{FontCatalog, Store, persist};
use crate::app::apply_wallpaper;
use crate::app::FontEntry;
use crate::app::fonts_ui::{auto_font_label, family_from_label, font_choices, resolve_ui_font_family};
use crate::app::resource_ui::sync_proc_theme;
use crate::app::terminal_ui::{
    apply_dark_mode, hex_from_rgb, parse_hex_color, theme_pref_is_dark,
};
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
        window.on_set_wallpaper(move |label: SharedString| {
            // 下拉里的一项 = 主题（跟随系统 / 深色 / 浅色）或一张上传的图片。
            let (theme, file) = wallpaper_choice_of_label(label.as_str());
            let Some(w) = weak.upgrade() else { return };
            // 用哪张背景图：主题项配它自己的内置图（跟随系统就按系统外观取），上传项就是文件。
            let wallpaper_id = if theme.is_empty() {
                file
            } else {
                let dark = match theme.as_str() {
                    "dark" => true,
                    "light" => false,
                    _ => theme_pref_is_dark(&store.borrow()),
                };
                apply_dark_mode(&w, &bufs_wp, dark);
                builtin_wallpaper_for(&theme, dark).to_string()
            };
            // `apply_builtin_theme = false`：深浅档刚刚已经由这一项定下来了，
            // 不需要壁纸再猜一次（它只知道图片自己的明暗）。
            apply_wallpaper(&w, &store.borrow(), &bufs_wp, &wallpaper_id, false);
            persist(&store, |s| {
                if !theme.is_empty() {
                    s.set_theme_pref(theme.clone());
                }
                s.set_wallpaper(wallpaper_id.clone());
            });
            // 深浅档可能刚变 → 主题色与光标色都要按新档位重新解析（两档下本来就是不同的值），
            // 下拉也要回到配置里那一项。
            let choice = store.borrow().accent().to_string();
            apply_accent(&w, &choice);
            let cursor = store.borrow().terminal_cursor_color().to_string();
            super::terminal::apply_cursor_color(&w, &cursor);
            w.set_accent_mode(store.borrow().theme_pref().into());
            publish_wallpaper_choices(&w, &store);
            if let Some(p) = proc_weak.upgrade() {
                sync_proc_theme(&w, &p);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_wp = bufs.clone();
        let proc_weak = proc_win.as_weak();
        window.on_pick_wallpaper_file(move || {
            let picked = rfd::FileDialog::new()
                .set_title(t("上传壁纸", "Upload wallpaper"))
                .add_filter("Images", &["png", "jpg", "jpeg", "webp", "bmp"])
                .pick_file();
            let Some(src) = picked else { return };
            // **复制**进 `config/wallpapers` —— 旧行为是记住原始路径，原文件一移走 / 删掉，
            // 壁纸就失效了；复制之后它跟字体一样是"应用自己的资源"。
            let Some(dst) = crate::wallpaper::import_wallpaper_file(&src) else {
                tracing::warn!("导入壁纸失败: {src:?}");
                return;
            };
            let id = dst.to_string_lossy().into_owned();
            if let Some(w) = weak.upgrade() {
                // 上传的图片只换图：深浅档保持用户当前的选择（与主题项不同）。
                apply_wallpaper(&w, &store.borrow(), &bufs_wp, &id, false);
                persist(&store, |s| {
                    s.set_wallpaper(id.clone());
                });
                // 刚落盘的文件要立刻出现在下拉里并被选中。
                publish_wallpaper_choices(&w, &store);
                if let Some(p) = proc_weak.upgrade() {
                    sync_proc_theme(&w, &p);
                }
            } else {
                persist(&store, |s| {
                    s.set_wallpaper(id.clone());
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
            // 「回声」判定：输入框里显示的本来就是**当前生效色**（预设与出厂色也填具体
            // 色号），那次程序化回显会走到这里 —— 若当成用户改动，预设就被固化成自定义色了。
            if is_accent_echo(&normalized, store.borrow().accent()) {
                return true;
            }
            persist(&store, |s| {
                s.set_accent(normalized.clone());
            });
            if let Some(w) = weak.upgrade() {
                apply_accent(&w, &normalized);
            }
            true
        });
    }

    {
        // 上传字体：**复制**进字体目录（`config/fonts`，与壁纸同一条规则）→ 重新扫描并注册
        // —— `load_external_fonts` 会把新文件交给 Slint 的字体集合，所以**不必重启** ——
        // 然后刷新两个字体列表、选中新家族并立即应用。
        let weak = window.as_weak();
        let store = store.clone();
        window.on_upload_ui_font(move || {
            let picked = rfd::FileDialog::new()
                .set_title(t("上传字体", "Upload font"))
                .add_filter("Fonts", &["ttf", "otf", "ttc", "otc"])
                .pick_file();
            let Some(src) = picked else { return };
            let Some(dst) = crate::fonts::import_font_file(&src) else {
                tracing::warn!("导入字体失败: {src:?}");
                return;
            };
            let external = crate::fonts::load_external_fonts(&crate::fonts::external_fonts_dirs());
            let family = crate::fonts::family_name_of(&dst).unwrap_or_default();

            let (term_labels, term_entries) = font_choices(&external, true);
            let (mut ui_labels, mut ui_entries) = font_choices(&external, false);
            // 界面字体列表最前面那项「跟随系统（自动）」（见 seed_settings 的同款注释）。
            ui_labels.insert(0, auto_font_label().into());
            ui_entries.insert(0, FontEntry::Auto);

            if let Some(w) = weak.upgrade() {
                w.set_term_fonts(ModelRc::from(Rc::new(VecModel::from(term_labels))));
                w.set_ui_fonts(ModelRc::from(Rc::new(VecModel::from(ui_labels))));
                // 列表插入新条目会让后面的下标整体后移 —— 两个选择器的下标都要按**当前
                // 存储值**重算，否则会停在错的那一项上。
                let term_family = store.borrow().font_family().to_string();
                w.set_term_font_index(
                    term_entries
                        .iter()
                        .position(|e| matches!(e, FontEntry::Family(f) if *f == term_family))
                        .unwrap_or(0) as i32,
                );
                if !family.is_empty() {
                    persist(&store, |s| {
                        s.set_ui_font_family(family.clone());
                    });
                    w.set_ui_font_index(
                        ui_entries
                            .iter()
                            .position(|e| matches!(e, FontEntry::Family(f) if *f == family))
                            .unwrap_or(0) as i32,
                    );
                    w.global::<Theme>().set_ui_font_family(resolve_ui_font_family());
                }
            }
        });
    }

    {
        // 调色盘提交（拖动松手时一次）。Slint 侧没有 hex 格式化能力，所以送过来的是
        // 三个通道值，在这里转成配置里那种 `#RRGGBB`。
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_accent_rgb(move |red: i32, green: i32, blue: i32| -> bool {
            let hex = hex_from_rgb(red, green, blue);
            let Some(normalized) = normalize_accent(&hex) else {
                return false;
            };
            // 与 hex 输入同一条「回声」判定：拖到与当前生效色相同的位置时不必变成自定义色。
            if is_accent_echo(&normalized, store.borrow().accent()) {
                return true;
            }
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
/// 不能分叉（改了这边忘了那边，用户看到的"默认蓝"就不是默认蓝了）。
pub(crate) const ACCENT_DEFAULT: (&str, &str) = ("#4a90e2", "#0071e3");

const ACCENT_PRESETS: &[(&str, &str, &str, &str, &str)] = &[
    // id,       深色档,     浅色档,     中文名,        英文名
    // 第一条是**默认蓝**（Rudder 一直以来的出厂色）。id 为空串 = 配置里"未选"，
    // 界面上的「默认蓝」色块就是它；`resolve_accent("")` 直接返回 None（不覆盖），
    // 真正的生效值来自 `theme.slint` 的 `accent-default`。
    ("",         ACCENT_DEFAULT.0, ACCENT_DEFAULT.1, "默认蓝",       "Default Blue"),
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
///
/// 现在壁纸功能已并进「壁纸」分区（内置 + 用户上传的选择器 + 遮罩），所以是 `true`。
/// 置 `false`（同时把 Slint 那侧一起改，测试会钉住）即可整体停用：界面藏起来 + 不再套用壁纸。
pub(crate) const WALLPAPER_UI_ENABLED: bool = true;

/// 把界面上的输入归一化成可存储的值：`""`（出厂默认）/ 预设 id / `#RRGGBB`。
///
/// 返回 `None` = 不合法（界面据此标红、既不应用也不持久化）。`#RGB` 简写会展开成
/// 6 位并转大写 —— 与预设 id 的大小写约定一致，比较时不必再 `eq_ignore_ascii_case`。
pub(crate) fn normalize_accent(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() {
        return Some(String::new());
    }
    // 老配置里的 "aurora"（当时的"极光蓝"，其实就是出厂色）统一落到「默认蓝」。
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
        // 自定义色**原样**返回：浅色档的压深交给 `theme.slint` 现算（`accent-custom` +
        // `.darker(0.25)`）。若在这里就压深，取色盘每次提交都会在"已经压深过的值"上再压
        // 一次 —— 连改几次就越改越暗。
        return parse_hex_color(s);
    }
    let preset = ACCENT_PRESETS.iter().find(|p| p.0 == s)?;
    parse_hex_color(if dark { preset.1 } else { preset.2 })
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

// ── 「壁纸」分区的选择器（内置 + 用户上传）────────────────────────────────
//
// 形状照 `FontCatalog`：界面拿到的是一串**显示名**，配置里存的是**稳定 id**，两边靠
// 下面这几个纯函数互查。用户上传的图片被复制进 `config/wallpapers`（见 wallpaper 模块），
// 所以列表的第三段就是那个目录里扫到的文件。

/// 「壁纸」下拉的一项。
///
/// 这个下拉**同时管主题与背景图**，因为「深色 / 浅色」就是原来的「简约·暗 / 简约·浅」：
/// 深浅档与那张配套的背景图是**同一个选择**，不再分两处设置。
/// * 主题项（跟随系统 / 深色 / 浅色）：`theme` 有值、`wallpaper` 为空 —— 用哪张内置图由
///   主题推出来（见 `builtin_wallpaper_for`）；
/// * 上传项：`wallpaper` 是文件路径、`theme` 为空 —— 只换图，深浅档保持用户当前的选择。
pub(crate) struct WallpaperChoice {
    pub(crate) label: String,
    pub(crate) theme: &'static str,
    pub(crate) wallpaper: String,
}

/// 主题 → 内置背景图（深/浅各一张）。跟随系统时按**系统外观**取，所以传进 `dark`。
pub(crate) fn builtin_wallpaper_for(theme: &str, dark: bool) -> &'static str {
    let want_dark = match theme {
        "dark" => true,
        "light" => false,
        _ => dark,
    };
    if want_dark {
        "builtin:dark"
    } else {
        "builtin:light"
    }
}

/// 可选的「壁纸」项：三个主题 + 用户上传的图片（按文件名排序）。
pub(crate) fn wallpaper_choices() -> Vec<WallpaperChoice> {
    let mut choices = vec![
        WallpaperChoice {
            label: t("跟随系统", "Follow system").to_string(),
            theme: "system",
            wallpaper: String::new(),
        },
        WallpaperChoice {
            label: t("深色", "Dark").to_string(),
            theme: "dark",
            wallpaper: String::new(),
        },
        WallpaperChoice {
            label: t("浅色", "Light").to_string(),
            theme: "light",
            wallpaper: String::new(),
        },
    ];
    for path in crate::wallpaper::scan_wallpaper_files(&crate::wallpaper::external_wallpapers_dir())
    {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        choices.push(WallpaperChoice {
            label: name,
            theme: "",
            wallpaper: path.to_string_lossy().into_owned(),
        });
    }
    choices
}

/// 送给 ComboBox 的标签模型。
pub(crate) fn wallpaper_labels_model(choices: &[WallpaperChoice]) -> ModelRc<SharedString> {
    let labels: Vec<SharedString> = choices.iter().map(|c| c.label.as_str().into()).collect();
    ModelRc::from(Rc::new(VecModel::from(labels)))
}

/// 当前组合在列表里的下标。
///
/// **先按壁纸匹配上传项**（它们的主题是"不动"，不能被主题项抢走），再按主题匹配三个主题项；
/// 都找不到就回到 0（跟随系统）。
pub(crate) fn wallpaper_index_of(choices: &[WallpaperChoice], theme: &str, wallpaper: &str) -> i32 {
    if let Some(i) = choices
        .iter()
        .position(|c| c.theme.is_empty() && c.wallpaper == wallpaper)
    {
        return i as i32;
    }
    choices
        .iter()
        .position(|c| c.theme == theme)
        .unwrap_or(0) as i32
}

/// 显示名 → (主题, 壁纸 id)（认不出来 → 跟随系统）。ComboBox 给的是**文本**。
fn wallpaper_choice_of_label(label: &str) -> (String, String) {
    wallpaper_choices()
        .into_iter()
        .find(|c| c.label == label)
        .map(|c| (c.theme.to_string(), c.wallpaper))
        .unwrap_or_else(|| ("system".to_string(), String::new()))
}

/// 把「壁纸」下拉的内容与选中项推给界面（内容 = 三个主题 + 上传的图片）。
pub(crate) fn publish_wallpaper_choices(w: &AppWindow, store: &Store) {
    let choices = wallpaper_choices();
    let (theme, wallpaper) = {
        let s = store.borrow();
        (s.theme_pref().to_string(), s.wallpaper().to_string())
    };
    w.set_wallpaper_labels(wallpaper_labels_model(&choices));
    w.set_wallpaper_index(wallpaper_index_of(&choices, &theme, &wallpaper));
}

/// 当前**生效**的主题色（`#RRGGBB`）：未选时就是出厂色（色表第一条 = `theme.slint` 的
/// `accent-default`）。界面上的"自定义颜色"输入框显示它 —— 用户因此始终看得到真实颜色。
pub(crate) fn effective_accent_hex(choice: &str, dark: bool) -> String {
    let fallback = if dark { ACCENT_DEFAULT.0 } else { ACCENT_DEFAULT.1 };
    match resolve_accent(choice, dark).or_else(|| parse_hex_color(fallback)) {
        Some(c) => hex_from_rgb(c.red() as i32, c.green() as i32, c.blue() as i32),
        None => String::new(),
    }
}

/// 是不是"回填造成的回声"。两档都认 —— 理由同 `terminal::is_follow_echo`：设置页是打开
/// 面板时才创建的，`changed` 可能**晚于**回填执行，期间深浅档可能已经翻过一轮。
fn is_accent_echo(v: &str, stored: &str) -> bool {
    v == effective_accent_hex(stored, true) || v == effective_accent_hex(stored, false)
}

/// 界面上「当前配色」显示的名字：预设名 / 自定义色原样 / 默认蓝。
fn accent_display_name(choice: &str) -> SharedString {
    if choice.is_empty() {
        return t("默认蓝", "Default Blue").into();
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
    // 自定义色（而不是预设的两档取值）：浅色档由 theme.slint 现算压深。
    w.global::<Theme>().set_accent_custom(choice.trim().starts_with('#'));
    w.set_accent_choice(choice.into());
    // 输入框回显**当前生效的颜色**（预设 / 出厂色也给具体色号）—— 用户一眼能看到实际值。
    // 这次回显会触发输入框的 `changed text` → `on_set_accent`，那里的"回声判定"会把它
    // 当成无操作，不会把预设变成自定义色。
    w.set_accent_hex(effective_accent_hex(choice, dark).into());
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
    // 壁纸下拉也要回到出厂默认那一项（内容 = 三个主题 + 上传的图片）。
    publish_wallpaper_choices(w, store);
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
    // 终端光标色同理：本页还原会把主题改回出厂默认（深浅档可能因此翻转），光标色在
    // "跟随主题"时要按**新的**档位重新解析 —— 否则设置页与终端里都还留着旧档位的颜色。
    let cursor = store.borrow().terminal_cursor_color().to_string();
    super::terminal::apply_cursor_color(w, &cursor);
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
        // 老配置里的 "aurora"（当时的"极光蓝"= 出厂色）统一落到「默认蓝」（空串）。
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

    /// 「壁纸」下拉：前三项是**主题**（跟随系统 / 深色 / 浅色），其余来自
    /// `config/wallpapers`。界面给显示名、配置里存"主题偏好 + 壁纸 id"，三者要能互查。
    #[test]
    fn wallpaper_choices_start_with_themes_and_map_back() {
        let choices = wallpaper_choices();
        assert!(choices.len() >= 3, "至少三个主题项");
        assert_eq!(choices[0].theme, "system");
        assert_eq!(choices[1].theme, "dark");
        assert_eq!(choices[2].theme, "light");
        // 主题项本身不带壁纸 id —— 图由主题推出来（深色 ↔ 原来那张"简约·暗"）。
        assert!(choices[0].wallpaper.is_empty());
        assert_eq!(builtin_wallpaper_for("dark", false), "builtin:dark");
        assert_eq!(builtin_wallpaper_for("light", true), "builtin:light");
        // 跟随系统：跟着系统外观走。
        assert_eq!(builtin_wallpaper_for("system", true), "builtin:dark");
        assert_eq!(builtin_wallpaper_for("system", false), "builtin:light");
        // 显示名 → 主题（界面回传的是名字）
        assert_eq!(wallpaper_choice_of_label(&choices[2].label).0, "light");
        // (主题, 壁纸) → 下标
        assert_eq!(wallpaper_index_of(&choices, "dark", "builtin:dark"), 1);
        assert_eq!(wallpaper_index_of(&choices, "system", "builtin:light"), 0);
        // 认不出来的名字 → 跟随系统
        assert_eq!(wallpaper_choice_of_label("不存在的项").0, "system");
    }

    /// 「回声」判定（主色）：预设两档的解析结果都不该被当成用户改动 —— 否则预设会被
    /// 固化成自定义色；用户真的选了别的颜色则照常写入。
    #[test]
    fn accent_echo_ignores_both_modes() {
        assert!(is_accent_echo("#22A2C9", "azure"));
        assert!(is_accent_echo("#0D7F9E", "azure"));
        assert!(is_accent_echo("#4A90E2", ""));
        assert!(!is_accent_echo("#FF0000", "azure"));
    }

    /// 「自定义颜色」输入框显示的是**当前生效色**（预设与出厂色也给具体色号）——
    /// 用户因此始终看得到真实颜色；回显时由「回声」判定保证不会把手上的选择改掉。
    #[test]
    fn effective_accent_hex_covers_presets_and_defaults() {
        assert_eq!(effective_accent_hex("", true), "#4A90E2");
        assert_eq!(effective_accent_hex("", false), "#0071E3");
        assert_eq!(effective_accent_hex("azure", true), "#22A2C9");
        assert_eq!(effective_accent_hex("azure", false), "#0D7F9E");
        // 自定义色原样（浅色档的压深在 Slint 侧，不影响这里显示的值）。
        assert_eq!(effective_accent_hex("#8899aa", true), "#8899AA");
        assert_eq!(effective_accent_hex("#8899aa", false), "#8899AA");
        // 认不出来的取值退回出厂色，而不是显示空白。
        assert_eq!(effective_accent_hex("nonsense", true), "#4A90E2");
    }

    /// 预设解析：深浅两档**各自**取色；未选（`""`）返回 `None` → 交给 Theme 的每档常量。
    #[test]
    fn accent_resolves_per_theme() {
        assert!(resolve_accent("", true).is_none());
        assert!(resolve_accent("", false).is_none());
        // 「默认蓝」就是出厂默认：不覆盖（`None`），交给 Theme 里每档的常量。
        assert!(resolve_accent("aurora", true).is_none()); // 老配置的 aurora 也归到默认蓝
        // 预设两档必须是两个颜色：同一个 hex 两档通用，必然有一档发灰 / 对比度不够。
        assert_ne!(
            resolve_accent("azure", true).unwrap(),
            resolve_accent("azure", false).unwrap()
        );
        // 自定义色**原样**返回（两档同一个值）：浅色档的压深由 `theme.slint` 用
        // `accent-custom` + `.darker(0.25)` 现算 —— 在这里压深的话，取色盘每提交一次就会
        // 在"已经压深过的值"上再压一次，连改几次就越改越暗。
        assert_eq!(
            resolve_accent("#8899AA", true).unwrap(),
            resolve_accent("#8899AA", false).unwrap()
        );
    }
}
