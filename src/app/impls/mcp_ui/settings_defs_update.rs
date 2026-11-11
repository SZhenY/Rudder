use super::*;

pub(crate) fn update_defs() -> Vec<SettingDef> {
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
