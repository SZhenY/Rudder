use super::*;

pub(crate) fn wsl_defs() -> Vec<SettingDef> {
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
