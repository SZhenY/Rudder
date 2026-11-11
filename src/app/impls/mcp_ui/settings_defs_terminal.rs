use super::*;

pub(crate) fn terminal_defs() -> Vec<SettingDef> {
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
