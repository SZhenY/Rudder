use super::*;

pub(crate) fn sync_defs() -> Vec<SettingDef> {
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
