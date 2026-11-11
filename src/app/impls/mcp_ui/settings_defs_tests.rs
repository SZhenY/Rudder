use super::super::denied_reason;
use super::*;

fn group_defs() -> Vec<(&'static str, Vec<SettingDef>)> {
    vec![
        ("terminal.", terminal_defs()),
        ("appearance.", appearance_defs()),
        ("layout.", layout_defs()),
        ("transfer.", transfer_defs()),
        ("sync.", sync_defs()),
        ("update.", update_defs()),
        ("wsl.", wsl_defs()),
    ]
}

#[test]
fn table_keys_are_unique() {
    let mut seen = std::collections::BTreeSet::new();
    for def in settings_table() {
        assert!(seen.insert(def.key), "重复的设置键 {}", def.key);
    }
}

#[test]
fn table_keys_use_known_namespaces() {
    let namespaces: Vec<&str> = group_defs().iter().map(|(prefix, _)| *prefix).collect();
    for def in settings_table() {
        assert!(def.key.contains('.'), "键 {} 缺少命名空间", def.key);
        assert!(
            namespaces.iter().any(|ns| def.key.starts_with(ns)),
            "键 {} 不属于任何已登记分组",
            def.key
        );
    }
}

#[test]
fn each_group_keeps_its_own_prefix() {
    for (prefix, defs) in group_defs() {
        assert!(!defs.is_empty(), "{prefix} 分组为空");
        for def in &defs {
            assert!(
                def.key.starts_with(prefix),
                "{} 分组含越界键 {}",
                prefix,
                def.key
            );
        }
    }
}

#[test]
fn int_ranges_are_ordered() {
    for def in settings_table() {
        if let SettingKind::Int { min, max } = &def.kind {
            assert!(min <= max, "键 {} 的取值范围倒置", def.key);
        }
    }
}

#[test]
fn choice_candidates_are_non_empty_and_unique() {
    for def in settings_table() {
        if let SettingKind::Choice(choices) = &def.kind {
            let choices = *choices;
            assert!(!choices.is_empty(), "键 {} 的候选项为空", def.key);
            let unique: std::collections::BTreeSet<&str> = choices.iter().copied().collect();
            assert_eq!(unique.len(), choices.len(), "键 {} 的候选项重复", def.key);
        }
    }
}

#[test]
fn table_is_the_concat_of_all_groups() {
    let expected: Vec<&str> = group_defs()
        .iter()
        .flat_map(|(_, defs)| defs.iter().map(|def| def.key))
        .collect();
    let actual: Vec<&str> = settings_table().iter().map(|def| def.key).collect();
    assert_eq!(actual, expected);
}

#[test]
fn denied_keys_are_not_exposed_in_table() {
    for def in settings_table() {
        assert!(
            denied_reason(def.key).is_none(),
            "键 {} 被明确拒绝却仍在表中",
            def.key
        );
    }
}

#[test]
fn table_is_built_once() {
    assert!(std::ptr::eq(settings_table(), settings_table()));
}
