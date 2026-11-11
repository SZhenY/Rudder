use super::*;

pub(crate) fn appearance_defs() -> Vec<SettingDef> {
    vec![
        SettingDef {
            key: "appearance.theme",
            kind: SettingKind::Choice(&["system", "dark", "light"]),
            get: |ui| json!(ui.ctx.store.borrow().theme_pref()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_theme_pref(value.as_str().unwrap_or("system").to_string());
                Ok(())
            },
            apply: |ui, _| {
                let dark = terminal_ui::theme_pref_is_dark(&ui.ctx.store.borrow());
                terminal_ui::apply_dark_mode(&ui.window, &ui.ctx.bufs, dark);
                let choice = ui.ctx.store.borrow().accent().to_string();
                let follow = appearance::apply_accent(&ui.window, &choice);
                let cursor = ui.ctx.store.borrow().terminal_cursor_color().to_string();
                terminal_settings::apply_cursor_color_with(&ui.window, &cursor, &follow);
                let mode = ui.ctx.store.borrow().theme_pref().to_string();
                ui.window.set_accent_mode(mode.into());
                settings_window::resync_if_open(&ui.window);
            },
        },
        SettingDef {
            key: "appearance.accent",
            kind: SettingKind::HexColor,
            get: |ui| json!(ui.ctx.store.borrow().accent()),
            set: |ui, value| {
                let choice = value.as_str().unwrap_or_default().to_string();
                if !choice.is_empty() && appearance::normalize_accent(&choice).is_none() {
                    return Err(McpError::Invalid(
                        "accent must be \"\", a preset id, or #RRGGBB".to_string(),
                    ));
                }
                ui.ctx.store.borrow_mut().set_accent(choice);
                Ok(())
            },
            apply: |ui, _| {
                let choice = ui.ctx.store.borrow().accent().to_string();
                let follow = appearance::apply_accent(&ui.window, &choice);
                let cursor = ui.ctx.store.borrow().terminal_cursor_color().to_string();
                terminal_settings::apply_cursor_color_with(&ui.window, &cursor, &follow);
                settings_window::resync_if_open(&ui.window);
            },
        },
        SettingDef {
            key: "appearance.ui_scale",
            kind: SettingKind::Int { min: 80, max: 200 },
            get: |ui| json!(ui.ctx.store.borrow().ui_scale()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_ui_scale(value.as_u64().unwrap_or(100) as u32);
                Ok(())
            },
            apply: |ui, _| {
                ui.window
                    .global::<Theme>()
                    .set_ui_scale(ui.ctx.store.borrow().ui_scale() as f32 / 100.0);
            },
        },
        SettingDef {
            key: "appearance.panel_font",
            kind: SettingKind::Int { min: 80, max: 160 },
            get: |ui| json!(ui.ctx.store.borrow().panel_font()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_panel_font(value.as_u64().unwrap_or(100) as u32);
                Ok(())
            },
            apply: |ui, _| {
                ui.window
                    .global::<Theme>()
                    .set_panel_font(ui.ctx.store.borrow().panel_font() as f32 / 100.0);
            },
        },
        SettingDef {
            key: "appearance.wallpaper",
            kind: SettingKind::Text,
            get: |ui| {
                let store = ui.ctx.store.borrow();
                match store.wallpaper().strip_prefix("builtin:") {
                    // 三档内置背景图直接报档位；上传的图片报当前深浅档。
                    Some(mode) => json!(mode),
                    None => json!(store.theme_pref()),
                }
            },
            // 写入交给 apply：换壁纸是"动作"，要同时改深浅档、换肤、重算主题色
            // 与光标色（见 on_set_wallpaper），这里重复一遍必然分叉。
            set: |_, _| Ok(()),
            apply: |ui, value| {
                let mode = value.as_str().unwrap_or("system");
                let label = crate::app::settings::appearance::wallpaper_choices()
                    .into_iter()
                    .find(|choice| {
                        if matches!(mode, "system" | "dark" | "light") {
                            choice.theme == mode && choice.wallpaper.is_empty()
                        } else {
                            choice.theme.is_empty() && choice.wallpaper == mode
                        }
                    })
                    .map(|choice| choice.label);
                if let Some(label) = label {
                    ui.window.invoke_set_wallpaper(label.into());
                }
            },
        },
        SettingDef {
            key: "appearance.ui_font",
            kind: SettingKind::Text,
            get: |ui| json!(ui.ctx.store.borrow().ui_font_family()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_ui_font_family(value.as_str().unwrap_or_default().trim().to_string());
                Ok(())
            },
            apply: |ui, _| {
                ui.window
                    .global::<Theme>()
                    .set_ui_font_family(crate::app::fonts_ui::resolve_ui_font_family());
                let stored = ui.ctx.store.borrow().ui_font_family().to_string();
                let catalog = font_catalog();
                ui.window.set_ui_font_index(catalog.ui_index(&stored));
            },
        },
    ]
}
