//! 设置项白名单表。按前缀分组，settings_table 按组拼接。
use super::*;
use std::sync::OnceLock;

#[path = "settings_defs_terminal.rs"] mod settings_defs_terminal;
pub(crate) use settings_defs_terminal::*;
#[path = "settings_defs_appearance.rs"] mod settings_defs_appearance;
pub(crate) use settings_defs_appearance::*;
#[path = "settings_defs_layout.rs"] mod settings_defs_layout;
pub(crate) use settings_defs_layout::*;
#[path = "settings_defs_transfer.rs"] mod settings_defs_transfer;
pub(crate) use settings_defs_transfer::*;
#[path = "settings_defs_sync.rs"] mod settings_defs_sync;
pub(crate) use settings_defs_sync::*;
#[path = "settings_defs_update.rs"] mod settings_defs_update;
pub(crate) use settings_defs_update::*;
#[path = "settings_defs_wsl.rs"] mod settings_defs_wsl;
pub(crate) use settings_defs_wsl::*;

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

#[cfg(test)]
#[path = "settings_defs_tests.rs"]
mod settings_defs_tests;
