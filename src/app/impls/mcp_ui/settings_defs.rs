//! 设置项白名单表。按前缀分组，settings_table 按组拼接。
use super::*;
use std::sync::OnceLock;

fn terminal_defs() -> Vec<SettingDef> {
    vec![
        SettingDef {
            key: "terminal.font_size",
            kind: SettingKind::Int { min: 8, max: 32 },
            get: |ui| json!(ui.ctx.store.borrow().font_size()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_font_size(value.as_u64().unwrap_or(13) as u32);
                Ok(())
            },
            apply: |ui, _| {
                ui.window
                    .global::<Theme>()
                    .set_term_font_size(ui.ctx.store.borrow().font_size() as f32);
            },
        },
        SettingDef {
            key: "terminal.bold",
            kind: SettingKind::Bool,
            get: |ui| json!(ui.ctx.store.borrow().terminal_bold()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_terminal_bold(value.as_bool().unwrap_or(false));
                Ok(())
            },
            apply: |ui, _| {
                ui.window
                    .global::<Theme>()
                    .set_term_font_bold(ui.ctx.store.borrow().terminal_bold());
            },
        },
        SettingDef {
            key: "terminal.cursor_style",
            kind: SettingKind::Choice(&["block", "bar", "underline"]),
            get: |ui| json!(ui.ctx.store.borrow().terminal_cursor_style()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_terminal_cursor_style(value.as_str().unwrap_or("bar").to_string());
                Ok(())
            },
            apply: |ui, _| {
                ui.window
                    .set_term_cursor_style(ui.ctx.store.borrow().terminal_cursor_style().into());
            },
        },
        SettingDef {
            key: "terminal.cursor_color",
            kind: SettingKind::HexColor,
            get: |ui| json!(ui.ctx.store.borrow().terminal_cursor_color()),
            set: |ui, value| {
                let color = value.as_str().unwrap_or_default().to_string();
                if !ui.ctx.store.borrow_mut().set_terminal_cursor_color(&color) {
                    return Err(McpError::Invalid("invalid hex color".to_string()));
                }
                Ok(())
            },
            apply: |ui, _| {
                terminal_settings::apply_cursor_color(
                    &ui.window,
                    ui.ctx.store.borrow().terminal_cursor_color(),
                );
            },
        },
        SettingDef {
            key: "terminal.convert_eol",
            kind: SettingKind::Bool,
            get: |ui| json!(ui.ctx.store.borrow().convert_eol()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_convert_eol(value.as_bool().unwrap_or(true));
                Ok(())
            },
            apply: |ui, _| {
                ui.window
                    .set_convert_eol(ui.ctx.store.borrow().convert_eol());
            },
        },
        SettingDef {
            key: "terminal.font_family",
            kind: SettingKind::Text,
            get: |ui| json!(ui.ctx.store.borrow().font_family()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_font_family(value.as_str().unwrap_or_default().trim().to_string());
                Ok(())
            },
            apply: |ui, _| {
                let family = ui.ctx.store.borrow().font_family().to_string();
                ui.window
                    .global::<Theme>()
                    .set_term_font_family(family.clone().into());
                ui.window
                    .global::<Theme>()
                    .set_term_font_cjk(crate::app::fonts_ui::term_font_covers_cjk(&family));
                // 选中项跟着走：设置窗 1 Hz 会被主窗口的索引覆盖，不同步的话
                // 界面显示与实际字体不一致（看着像没改成功）。
                let catalog = font_catalog();
                ui.window.set_term_font_index(catalog.term_index(&family));
            },
        },
        SettingDef {
            key: "terminal.scrollback_lines",
            kind: SettingKind::Int {
                min: 100,
                max: 1_000_000,
            },
            get: |ui| json!(ui.ctx.store.borrow().scrollback_lines()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_scrollback_lines(value.as_u64().unwrap_or(5_000) as usize);
                Ok(())
            },
            apply: |ui, _| {
                let lines = ui.ctx.store.borrow().scrollback_lines();
                ui.window.set_scrollback_lines(lines.to_string().into());
            },
        },
        SettingDef {
            key: "terminal.osc52",
            kind: SettingKind::Bool,
            get: |ui| json!(ui.ctx.store.borrow().osc52_clipboard()),
            set: |ui, value| {
                let enabled = value.as_bool().unwrap_or(false);
                ui.ctx.store.borrow_mut().set_osc52_clipboard(enabled);
                // 剪贴板那侧读的是原子量，不只读配置。
                crate::terminal::vt_adapter::OSC52_ENABLED.store(enabled, Ordering::Relaxed);
                Ok(())
            },
            apply: |ui, _| {
                ui.window
                    .set_osc52_clipboard(ui.ctx.store.borrow().osc52_clipboard());
            },
        },
        SettingDef {
            key: "terminal.json_format",
            kind: SettingKind::Bool,
            get: |ui| json!(ui.ctx.store.borrow().json_format_output()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_json_format_output(value.as_bool().unwrap_or(true));
                Ok(())
            },
            apply: |ui, _| {
                let enabled = ui.ctx.store.borrow().json_format_output();
                // 已在跑的标签页也要立刻换行为，否则要重连才生效。
                for buffer in ui
                    .ctx
                    .bufs
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .values()
                {
                    buffer
                        .lock()
                        .unwrap_or_else(|err| err.into_inner())
                        .json_format_output = enabled;
                }
                ui.window.set_json_format_output(enabled);
            },
        },
        SettingDef {
            key: "terminal.output_highlight",
            kind: SettingKind::Structured,
            get: |ui| {
                let store = ui.ctx.store.borrow();
                json!({
                    "enabled": store.output_highlight_enabled(),
                    "preset": store.output_highlight_preset(),
                    "rules": store.output_highlight_rules(),
                })
            },
            set: |ui, value| {
                let object = value.as_object().ok_or_else(|| {
                    McpError::Invalid("`terminal.output_highlight` must be an object".into())
                })?;
                let enabled = object
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let preset = object
                    .get("preset")
                    .and_then(Value::as_str)
                    .unwrap_or("builtin")
                    .to_string();
                let rules = match object.get("rules") {
                    Some(rules) => Some(
                        serde_json::from_value::<Vec<crate::config::OutputHighlightRule>>(
                            rules.clone(),
                        )
                        .map_err(|err| {
                            McpError::Invalid(format!("invalid output highlight rules: {err}"))
                        })?,
                    ),
                    None => None,
                };
                let mut store = ui.ctx.store.borrow_mut();
                store.set_output_highlight_enabled(enabled);
                store.set_output_highlight_preset(preset);
                if let Some(rules) = rules {
                    store.set_output_highlight_rules(rules);
                }
                Ok(())
            },
            apply: |ui, _| {
                let (enabled, preset, rules) = {
                    let store = ui.ctx.store.borrow();
                    (
                        store.output_highlight_enabled(),
                        store.output_highlight_preset().to_string(),
                        store.output_highlight_rules().to_vec(),
                    )
                };
                crate::app::terminal_ui::apply_output_highlight(
                    &ui.window,
                    &ui.ctx.bufs,
                    enabled,
                    &preset,
                );
                crate::app::terminal_ui::apply_custom_output_rules(
                    &ui.window,
                    &ui.ctx.bufs,
                    &rules,
                );
            },
        },
    ]
}

fn appearance_defs() -> Vec<SettingDef> {
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

fn layout_defs() -> Vec<SettingDef> {
    vec![
        SettingDef {
            key: "layout.collapse_sidebar_default",
            kind: SettingKind::Bool,
            get: |ui| json!(ui.ctx.store.borrow().collapse_sidebar_default()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_collapse_sidebar_default(value.as_bool().unwrap_or(false));
                Ok(())
            },
            apply: |ui, _| {
                crate::app::settings::layout::apply_layout_prefs(&ui.window, &ui.ctx.store)
            },
        },
        SettingDef {
            key: "layout.collapse_sftp_default",
            kind: SettingKind::Bool,
            get: |ui| json!(ui.ctx.store.borrow().collapse_sftp_default()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_collapse_sftp_default(value.as_bool().unwrap_or(false));
                Ok(())
            },
            apply: |ui, _| {
                crate::app::settings::layout::apply_layout_prefs(&ui.window, &ui.ctx.store)
            },
        },
        SettingDef {
            key: "layout.hide_cmd_bar",
            kind: SettingKind::Bool,
            get: |ui| json!(ui.ctx.store.borrow().cmd_bar_hidden()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_cmd_bar_hidden(value.as_bool().unwrap_or(false));
                Ok(())
            },
            apply: |ui, _| {
                ui.window
                    .set_show_cmd_bar(!ui.ctx.store.borrow().cmd_bar_hidden());
            },
        },
        SettingDef {
            key: "layout.quick_commands_sidebar",
            kind: SettingKind::Bool,
            get: |ui| json!(ui.ctx.store.borrow().quick_commands_as_sidebar()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_quick_commands_as_sidebar(value.as_bool().unwrap_or(false));
                Ok(())
            },
            apply: |ui, _| {
                // 复用界面那条：它还要把快捷面板的开关一起同步过去。
                let enabled = ui.ctx.store.borrow().quick_commands_as_sidebar();
                crate::app::settings::layout::apply_quick_commands_as_sidebar(
                    &ui.window,
                    &ui.ctx.store,
                    enabled,
                );
            },
        },
    ]
}

fn transfer_defs() -> Vec<SettingDef> {
    vec![
        SettingDef {
            key: "transfer.sftp_follow_cd",
            kind: SettingKind::Bool,
            get: |ui| json!(ui.ctx.store.borrow().sftp_follow_cd()),
            set: |ui, value| {
                let follow = value.as_bool().unwrap_or(true);
                ui.ctx.store.borrow_mut().set_sftp_follow_cd(follow);
                ui.ctx.sftp_follow_cd.store(follow, Ordering::Relaxed);
                Ok(())
            },
            apply: |ui, _| {
                ui.window
                    .set_sftp_follow_cd(ui.ctx.store.borrow().sftp_follow_cd());
            },
        },
        SettingDef {
            key: "transfer.download_always_ask",
            kind: SettingKind::Bool,
            get: |ui| json!(ui.ctx.store.borrow().download_always_ask()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_download_always_ask(value.as_bool().unwrap_or(false));
                Ok(())
            },
            apply: |ui, _| {
                ui.window
                    .set_download_always_ask(ui.ctx.store.borrow().download_always_ask());
            },
        },
    ]
}

fn sync_defs() -> Vec<SettingDef> {
    vec![
        SettingDef {
            key: "sync.upload_enabled",
            kind: SettingKind::Bool,
            get: |ui| json!(ui.ctx.store.borrow().sync_upload()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_sync_upload(value.as_bool().unwrap_or(false));
                Ok(())
            },
            apply: |ui, _| {
                ui.window
                    .set_sync_upload_enabled(ui.ctx.store.borrow().sync_upload());
            },
        },
        SettingDef {
            key: "sync.webdav_url",
            kind: SettingKind::Text,
            get: |ui| json!(ui.ctx.store.borrow().webdav_url()),
            set: |ui, value| {
                webdav_update(
                    ui,
                    Some(value.as_str().unwrap_or_default().to_string()),
                    None,
                    None,
                )
            },
            apply: |ui, _| {
                ui.window
                    .set_webdav_url(ui.ctx.store.borrow().webdav_url().into());
            },
        },
        SettingDef {
            key: "sync.webdav_username",
            kind: SettingKind::Text,
            get: |ui| json!(ui.ctx.store.borrow().webdav_username()),
            set: |ui, value| {
                webdav_update(
                    ui,
                    None,
                    Some(value.as_str().unwrap_or_default().to_string()),
                    None,
                )
            },
            apply: |ui, _| {
                ui.window
                    .set_webdav_username(ui.ctx.store.borrow().webdav_username().into());
            },
        },
        SettingDef {
            key: "sync.webdav_remote_path",
            kind: SettingKind::Text,
            get: |ui| json!(ui.ctx.store.borrow().webdav_remote_path()),
            set: |ui, value| {
                webdav_update(
                    ui,
                    None,
                    None,
                    Some(value.as_str().unwrap_or_default().to_string()),
                )
            },
            apply: |ui, _| {
                ui.window
                    .set_webdav_remote_path(ui.ctx.store.borrow().webdav_remote_path().into());
            },
        },
    ]
}

fn update_defs() -> Vec<SettingDef> {
    vec![
        SettingDef {
            key: "update.check_enabled",
            kind: SettingKind::Bool,
            get: |ui| json!(ui.ctx.store.borrow().update_check_enabled()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_update_check_enabled(value.as_bool().unwrap_or(true));
                Ok(())
            },
            apply: |ui, _| {
                ui.window
                    .set_update_check_enabled(ui.ctx.store.borrow().update_check_enabled());
            },
        },
        SettingDef {
            key: "update.channel",
            kind: SettingKind::Choice(&["stable", "beta", "all"]),
            get: |ui| json!(ui.ctx.store.borrow().update_channel()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_update_channel(value.as_str().unwrap_or("stable").to_string());
                Ok(())
            },
            apply: |ui, _| {
                ui.window
                    .set_update_channel(ui.ctx.store.borrow().update_channel().into());
            },
        },
        SettingDef {
            key: "update.frequency",
            kind: SettingKind::Choice(&crate::config::UPDATE_FREQUENCIES),
            get: |ui| json!(ui.ctx.store.borrow().update_frequency()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_update_frequency(value.as_str().unwrap_or("daily").to_string());
                Ok(())
            },
            apply: |ui, _| {
                let current = ui.ctx.store.borrow().update_frequency().to_string();
                let index = crate::config::UPDATE_FREQUENCIES
                    .iter()
                    .position(|value| *value == current)
                    .unwrap_or(0) as i32;
                ui.window.set_update_freq_index(index);
            },
        },
        SettingDef {
            key: "update.proxy_enabled",
            kind: SettingKind::Bool,
            get: |ui| json!(ui.ctx.store.borrow().update_proxy_enabled()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_update_proxy_enabled(value.as_bool().unwrap_or(false));
                Ok(())
            },
            apply: |ui, _| {
                // 更新器读的是全局快照，改完配置得推一次。
                crate::app::settings::update::apply_net(&ui.window, &ui.ctx.store, None);
            },
        },
        SettingDef {
            key: "update.proxy",
            kind: SettingKind::Text,
            get: |ui| json!(ui.ctx.store.borrow().update_proxy()),
            set: |ui, value| {
                ui.ctx
                    .store
                    .borrow_mut()
                    .set_update_proxy(value.as_str().unwrap_or_default().trim().to_string());
                Ok(())
            },
            apply: |ui, _| {
                let proxy = ui.ctx.store.borrow().update_proxy().to_string();
                crate::app::settings::update::apply_net(&ui.window, &ui.ctx.store, Some(&proxy));
            },
        },
    ]
}

fn wsl_defs() -> Vec<SettingDef> {
    vec![SettingDef {
        key: "wsl.profiles",
        kind: SettingKind::Structured,
        get: |ui| json!(ui.ctx.store.borrow().wsl_profiles()),
        set: |ui, value| {
            let items = value
                .as_array()
                .ok_or_else(|| McpError::Invalid("`wsl.profiles` must be an array".into()))?;
            let existing = ui.ctx.store.borrow().wsl_profiles().to_vec();
            let mut profiles = Vec::with_capacity(items.len());
            for item in items {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if name.is_empty() {
                    return Err(McpError::Invalid(
                        "wsl profile `name` is required".to_string(),
                    ));
                }
                // 带 id 就沿用（更新），否则按同名匹配旧条目，最后才新建。
                let id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .or_else(|| {
                        existing
                            .iter()
                            .find(|profile| profile.name == name)
                            .map(|profile| profile.id.clone())
                    })
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                profiles.push(crate::config::WslProfile {
                    id,
                    name,
                    distribution: item
                        .get("distribution")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                    directory: item
                        .get("directory")
                        .and_then(Value::as_str)
                        .unwrap_or("~")
                        .to_string(),
                });
            }
            ui.ctx.store.borrow_mut().set_wsl_profiles(profiles);
            Ok(())
        },
        apply: |ui, _| {
            let model = crate::app::session_models::wsl_profile_model(&ui.ctx.store.borrow());
            ui.window.set_wsl_profiles(model);
            // 设置窗口拿到的是独立副本，不同步就会显示旧列表。
            crate::app::settings_window::resync_if_open(&ui.window);
        },
    }]
}

pub(super) fn settings_table() -> &'static [SettingDef] {
    static TABLE: OnceLock<Vec<SettingDef>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = Vec::new();
        table.extend(terminal_defs());
        table.extend(appearance_defs());
        table.extend(layout_defs());
        table.extend(transfer_defs());
        table.extend(sync_defs());
        table.extend(update_defs());
        table.extend(wsl_defs());
        table
    })
}
