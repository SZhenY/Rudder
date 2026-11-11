use super::*;

pub(crate) fn layout_defs() -> Vec<SettingDef> {
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
