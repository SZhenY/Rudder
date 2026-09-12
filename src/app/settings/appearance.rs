//! 外观页：UI 字体 / 壁纸与遮罩 / 渲染后端 / 动画 / 缩放 / 面板字体 / 隐藏特殊分区。
//!
//! 该页有两处容易被漏掉的落点，都在这里一并处理：
//! * `ui-font-index`（字体选择器索引）—— 只改 family 会让下拉框停在旧项；
//! * 壁纸切换要走 `apply_wallpaper`（完整的换肤与调色板派生），不能只 set 属性。

use slint::{ComponentHandle, SharedString};

use super::{Store, persist};
use crate::app::apply_wallpaper;
use crate::app::fonts_ui::{family_from_label, resolve_ui_font_family};
use crate::app::resource_ui::sync_proc_theme;
use crate::i18n::t;
use crate::terminal::TermBuffers;
use crate::ui::{AppWindow, ProcWindow};

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
            let clamped = (percent.max(0) as u32).clamp(80, 200);
            {
                persist(&store, |s| {
                    s.set_ui_scale(clamped);
                });
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
                persist(&store, |s| {
                    s.set_panel_font(clamped);
                });
            }
            if let Some(w) = weak.upgrade() {
                w.set_panel_font(clamped as f32 / 100.0);
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
                    selected_builtin_theme = Some(w.get_dark_mode());
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
                w.set_ui_font_family(family.into());
            }
        });
    }
}

/// 「还原本页默认」：替换外观域为出厂默认，再走与 `bind` 相同的落点。
///
/// 隐藏特殊分区。
///
/// 注意两点：其一，「隐藏特殊分区」的控件在 UI 上位于本页（此前误归到传输页的
/// 还原里）；其二，壁纸与遮罩透明度按规格也在还原范围内（此前被当作 B 类跳过）。
pub(crate) fn reset(w: &AppWindow, store: &Store, bufs: &TermBuffers) {
    let d = crate::config::fresh_config();
    {
        persist(store, |s| {
            s.set_ui_font_family(d.appearance.ui_font_family.clone());
            s.set_ui_scale(d.appearance.ui_scale);
            s.set_panel_font(d.appearance.panel_font);
            s.set_renderer_mode(d.appearance.renderer_mode.clone());
            s.set_wallpaper(d.appearance.wallpaper.clone());
            s.set_wallpaper_overlay(d.appearance.wallpaper_overlay);
            s.set_hide_special_partitions(d.appearance.hide_special_partitions);
        });
    }
    // UI 刷新走 getter（0 → 默认 / 平台默认）。
    let s = store.borrow();
    w.set_ui_font_family(resolve_ui_font_family());
    // 同样要把选择器索引同步过去（空串 = auto，落在列表第 0 项）。
    w.set_ui_font_index(0); // 空串 = auto，恒为字体列表第 0 项
    w.set_ui_scale(s.ui_scale() as f32 / 100.0);
    w.set_panel_font(s.panel_font() as f32 / 100.0);
    w.set_renderer_mode(s.renderer_mode().into());
    w.set_wallpaper_overlay(s.wallpaper_overlay());
    w.set_hide_special_partitions(s.hide_special_partitions());
    drop(s);
    // 壁纸切换有完整的换肤 / 调色板派生流程，必须走 apply_wallpaper。
    apply_wallpaper(w, &store.borrow(), bufs, &d.appearance.wallpaper, false);
    // 动画开关没有后端持久化（Slint 全局，重启即回），还原即重新开启。
    w.set_animations_enabled(true);
}
