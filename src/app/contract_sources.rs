//! 契约测试读取的源码文本，集中登记。拆分文件后只需在这里补片段。

use std::collections::BTreeSet;

/// 设置页 Slint 源码片段，按页分组。
pub(crate) const PAGE_SLINT: &[(&str, &[&str])] = &[
    (
        "terminal",
        &[include_str!("../../ui/settings/pages/terminal.slint")],
    ),
    (
        "appearance",
        &[include_str!("../../ui/settings/pages/appearance.slint")],
    ),
    (
        "layout",
        &[include_str!("../../ui/settings/pages/layout.slint")],
    ),
    (
        "transfer",
        &[include_str!("../../ui/settings/pages/transfer.slint")],
    ),
];

/// 设置页 Rust 源码片段，按页分组。
pub(crate) const PAGE_RUST: &[(&str, &[&str])] = &[
    ("terminal", &[include_str!("settings/terminal.rs")]),
    ("appearance", &[include_str!("settings/appearance.rs")]),
    ("layout", &[include_str!("settings/layout.rs")]),
    ("transfer", &[include_str!("settings/transfer.rs")]),
];

pub(crate) const THEME_SLINT: &[&str] = &[include_str!("../../ui/theme.slint")];
pub(crate) const WINDOW_SHELL_SLINT: &[&str] =
    &[include_str!("../../ui/components/window_shell.slint")];
pub(crate) const SETTINGS_WINDOW_RS: &[&str] = &[include_str!("settings_window.rs")];

pub(crate) fn sources_for(
    groups: &'static [(&'static str, &'static [&'static str])],
    page: &str,
) -> &'static [&'static str] {
    groups
        .iter()
        .find(|(name, _)| *name == page)
        .map(|(_, sources)| *sources)
        .unwrap_or_else(|| panic!("源码清单里没有页面 {page}"))
}

/// 逐片段扫描 `root.<prop>`，合并成属性名集合。
pub(crate) fn props_in_all(sources: &[&str]) -> BTreeSet<String> {
    sources.iter().flat_map(|src| props_in(src)).collect()
}

fn props_in(src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut from = 0;
    while let Some(i) = src[from..].find("root.") {
        let s = from + i + "root.".len();
        let name: String = src[s..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let is_call = src[s + name.len()..].starts_with('(');
        let advance = s + name.len().max(1);
        if !name.is_empty() && !is_call {
            out.insert(name);
        }
        from = advance;
    }
    out
}

/// 在含 `fn <name>(` 的那一片段里截取函数体，不跨片段查找边界。
pub(crate) fn fn_body_in(sources: &[&str], name: &str) -> String {
    let sig = format!("fn {name}(");
    let src = sources
        .iter()
        .find(|src| src.contains(&sig))
        .unwrap_or_else(|| panic!("找不到函数 {name}"));
    let start = src.find(&sig).unwrap();
    let rest = &src[start..];
    let end = rest.find("\n}\n").map(|i| i + 3).unwrap_or(rest.len());
    rest[..end].to_string()
}

/// 收集页面文件里 `page-id:` 声明的还原页 id。
pub(crate) fn reset_page_ids() -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for line in PAGE_SLINT
        .iter()
        .flat_map(|(_, sources)| sources.iter())
        .flat_map(|src| src.lines())
    {
        let Some((_, rest)) = line.split_once("page-id:") else {
            continue;
        };
        let Some((_, after_quote)) = rest.split_once('"') else {
            continue;
        };
        let Some((id, _)) = after_quote.split_once('"') else {
            continue;
        };
        ids.insert(id.to_string());
    }
    ids
}

/// 取 `export global Theme { .. }` 里的 `in-out property` 名。
pub(crate) fn mutable_theme_props() -> Vec<String> {
    THEME_SLINT
        .iter()
        .flat_map(|src| {
            let Some(start) = src.find("export global Theme {") else {
                return Vec::new();
            };
            let end = start + src[start..].find("\n}").expect("Theme 全局未闭合");
            src[start..end]
                .lines()
                .filter_map(|line| {
                    let rest = line.trim().strip_prefix("in-out property <")?;
                    let name = rest.split('>').nth(1)?.split([':', ';']).next()?.trim();
                    (!name.is_empty()).then(|| name.to_string())
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

pub(crate) fn sources_contain(sources: &[&str], needle: &str) -> bool {
    sources.iter().any(|src| src.contains(needle))
}

pub(crate) fn none_contains(sources: &[&str], needle: &str) -> bool {
    sources.iter().all(|src| !src.contains(needle))
}

/// 清单本身不能失效：页面齐全、片段非空、关键标记恰好出现一次。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_lists_stay_usable() {
        for (page, sources) in PAGE_SLINT {
            assert!(!sources.is_empty(), "{page} 页没有登记源码片段");
            assert!(
                sources.iter().all(|src| !src.is_empty()),
                "{page} 页有空片段"
            );
        }
        for (page, _) in PAGE_SLINT {
            assert!(
                PAGE_RUST.iter().any(|(p, _)| p == page),
                "{page} 缺 Rust 片段"
            );
        }
        assert_eq!(
            THEME_SLINT
                .iter()
                .filter(|src| src.contains("export global Theme {"))
                .count(),
            1,
            "Theme 全局必须整块留在单一片段"
        );
        assert_eq!(
            SETTINGS_WINDOW_RS
                .iter()
                .filter(|src| src.contains("fn sync_settings_theme"))
                .count(),
            1,
            "sync_settings_theme 必须完整留在单一片段"
        );
    }
}
