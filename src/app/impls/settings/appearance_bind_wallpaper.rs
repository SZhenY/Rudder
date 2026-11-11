use super::*;

pub(super) fn wire_wallpaper(window: &AppWindow, store: &Store, bufs: &TermBuffers) {
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_mount_filter(move |v: slint::SharedString| {
            persist(&store, |s| {
                s.set_mount_filter(v.to_string());
            });
            if let Some(w) = weak.upgrade() {
                w.set_mount_filter(v.clone());
            }
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
            // 换壁纸 = 重新定主题色：清掉之前的预设/自定义，回到「跟随」→ 新壁纸的互补色
            //（修复"有自定义颜色时互补色不生效"）；深浅档若刚变也一并按新档解析。
            reset_accent_to_follow(&w, &store);
            // 换壁纸 = 光标回到「跟随」→ 新壁纸的互补色。
            reset_cursor_to_follow(&w, &store);
            w.set_accent_mode(store.borrow().theme_pref().into());
            publish_wallpaper_choices(&w, &store);
            if let Some(p) = crate::app::sub_windows::proc_weak().upgrade() {
                sync_proc_theme(&w, &p);
            }
            // 设置窗也要同步；不能放进上面的 if let。
            crate::app::settings_window::resync_if_open(&w);
            schedule_post_fade_sync(&w, &store, &crate::app::sub_windows::proc_weak());
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_wp = bufs.clone();
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
                // 换壁纸 = 主题色回到「跟随」→ 新壁纸的互补色。
                reset_accent_to_follow(&w, &store);
                // 换壁纸 = 光标回到「跟随」→ 新壁纸的互补色。
                reset_cursor_to_follow(&w, &store);
                persist(&store, |s| {
                    s.set_wallpaper(id.clone());
                });
                // 刚落盘的文件要立刻出现在下拉里并被选中。
                publish_wallpaper_choices(&w, &store);
                if let Some(p) = crate::app::sub_windows::proc_weak().upgrade() {
                    sync_proc_theme(&w, &p);
                }
                // 设置窗也要同步；不能放进上面的 if let。
                crate::app::settings_window::resync_if_open(&w);
                schedule_post_fade_sync(&w, &store, &crate::app::sub_windows::proc_weak());
            } else {
                persist(&store, |s| {
                    s.set_wallpaper(id.clone());
                });
            }
        });
    }
}
