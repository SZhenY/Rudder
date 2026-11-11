use super::*;

pub(crate) fn transfer_defs() -> Vec<SettingDef> {
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
