use super::*;

pub(super) fn wire_theme(window: &AppWindow, store: &Store, bufs: &TermBuffers) {
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
                w.global::<Theme>()
                    .set_ui_font_family(resolve_ui_font_family());
                // 与终端字体同理：不回写选中项，AI 页 1 Hz 轮询的 `resync_if_open`
                // 会把设置窗的选择弹回旧项。
                let index = w
                    .get_ui_fonts()
                    .iter()
                    .position(|item| item == label)
                    .unwrap_or(0) as i32;
                w.set_ui_font_index(index);
            }
        });
    }

    {
        // 主题（深浅）：跟随系统 / 深色 / 浅色。切档要同时做三件事 —— 写偏好、换肤、
        // **按新档位重新解析主题色**（预设两档是两个颜色，自定义色在浅色档要压深）。
        let weak = window.as_weak();
        let store = store.clone();
        let bufs_mode = bufs.clone();
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
            crate::app::settings::terminal::apply_cursor_color(&w, &cursor);
            w.set_accent_mode(normalized.into());
            if let Some(p) = crate::app::sub_windows::proc_weak().upgrade() {
                sync_proc_theme(&w, &p);
            }
            // 设置窗也要同步；不能放进上面的 if let。
            crate::app::settings_window::resync_if_open(&w);
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
            let Some(w) = weak.upgrade() else {
                return false;
            };
            if is_accent_echo(
                &normalized,
                store.borrow().accent(),
                wallpaper_accent_hex(&w).as_deref(),
            ) {
                return true;
            }
            persist(&store, |s| {
                s.set_accent(normalized.clone());
            });
            apply_accent(&w, &normalized);
            // 主题色变了 → 光标若在「跟随」应同步到新主题色（显式选过的保持不变）。
            let cursor = store.borrow().terminal_cursor_color().to_string();
            crate::app::settings::terminal::apply_cursor_color(&w, &cursor);
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
                    w.global::<Theme>()
                        .set_ui_font_family(resolve_ui_font_family());
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
            let Some(w) = weak.upgrade() else {
                return false;
            };
            if is_accent_echo(
                &normalized,
                store.borrow().accent(),
                wallpaper_accent_hex(&w).as_deref(),
            ) {
                return true;
            }
            persist(&store, |s| {
                s.set_accent(normalized.clone());
            });
            apply_accent(&w, &normalized);
            // 主题色变了 → 光标若在「跟随」应同步到新主题色（显式选过的保持不变）。
            let cursor = store.borrow().terminal_cursor_color().to_string();
            crate::app::settings::terminal::apply_cursor_color(&w, &cursor);
            true
        });
    }
}
