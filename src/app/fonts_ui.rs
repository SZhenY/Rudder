//! UI font selection: enumeration, CJK coverage probing and the settings model.



use crate::i18n::t;

use super::HISTORY_STORE;

/// Does the terminal font family cover CJK glyphs?  A lightweight family-name
/// probe: CJK-capable builds conventionally tag their names with CN / SC /
/// TC / JP / KR / CJK / Han (e.g. "Maple Mono Normal NL NF CN", "Noto Sans
/// CJK SC").  When true, terminal spans keep the terminal font for Chinese
/// text so italic / thin variants apply to CJK glyphs too; when false they
/// fall back to the UI sans font (the embedded mono fonts have no CJK).
pub(crate) fn term_font_covers_cjk(family: &str) -> bool {
    let f = family.to_lowercase();
    ["cn", "sc", "tc", "jp", "kr", "cjk", "han"]
        .iter()
        .any(|tag| f.contains(tag))
}
/// 传给 Slint `font-family` 的界面字体。
///
/// ⚠️ **返回值必须是一个家族名，永远不能是"逗号分隔的字体栈"。** Slint 1.17 的
/// `font-family` 是**单个**家族名：文本布局把整串原样交给 parley
/// （`textlayout/sharedparley.rs` 里的 `FontFamilyName::named(整串)`），**不拆逗号**。
/// 一串名字会被当成一个不存在的家族 → 整体落回平台默认字体，用户看到的现象就是
/// "在设置里换了界面字体，界面没有任何变化"。缺字（中文、emoji）由 parley 按脚本
/// 自动回退，不需要、也没法用逗号链来编排。
pub(crate) fn resolve_ui_font_family() -> slint::SharedString {
    let saved = HISTORY_STORE.with(|s| {
        s.borrow()
            .as_ref()
            .map(|st| st.borrow().ui_font_family().to_owned())
            .unwrap_or_default()
    });
    if let Some(chosen) = explicit_ui_family(&saved) {
        tracing::debug!(font = %chosen, "ui-font: using saved preference");
        return chosen.into();
    }

    // Diagnostic / escape hatch (#129): force a specific UI font without a rebuild.
    // e.g. MEATSHELL_UI_FONT="Meatshell Mono" to test whether the embedded font
    // renders when system fonts don't. Empty value is ignored.
    if let Some(f) = std::env::var_os("MEATSHELL_UI_FONT") {
        let f = f.to_string_lossy().into_owned();
        if !f.trim().is_empty() {
            tracing::debug!(font = %f, "ui-font: overridden via MEATSHELL_UI_FONT");
            return f.into();
        }
    }

    // auto（空串）：返回空串 —— Slint 会用它自己的平台默认字体，这正是「跟随系统」
    // 的语义，渲染结果与平台上任何原生应用一致；中文/emoji 走 parley 的逐脚本回退。
    //
    // 唯一例外：系统字体库一个面都枚举不到（#129 的老场景）时，平台默认同样解析不出
    // 来，改用内嵌的 Meatshell Mono 兜底 —— 它一定能解析，至少不会整窗无字。
    if system_fonts_available() {
        "".into()
    } else {
        tracing::warn!("ui-font: no system fonts enumerated — using embedded 'Meatshell Mono'");
        "Meatshell Mono".into()
    }
}

/// 用户显式选过的家族 —— **原样**返回，不追加任何回退段（见上方 ⚠️）。
/// 空串 / 纯空白 = 没选过（auto），返回 `None` 交回平台默认。
///
/// 抽成纯函数是为了能测：它锁的正是"显式选择必须原样透传"这条不变量，
/// 而 `resolve_ui_font_family` 自身依赖 thread-local 的配置，测试里不便构造。
fn explicit_ui_family(saved: &str) -> Option<String> {
    let trimmed = saved.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 系统字体库能否枚举到字体 —— `false` 时连平台默认字体都不可靠（#129 的老场景）。
///
/// 注意这**不是**"有没有中文字体"：中文覆盖现在交给 parley 按脚本回退，不再需要
/// 我们挑一个 CJK 家族写进 `font-family`。这里只判断字体库本身是否可用。
///
/// macOS 历史（#129）：某些 macOS 26 机器上 PingFang/Hiragino 能枚举到、但光栅化
/// 出来是空白，Heiti/Songti 正常。那种情况本函数返回 `true`（字体库可用，坏的是
/// 个别家族），当时的绕法是"优先挑 Heiti"—— 随着回退策略交给 parley，这个候选表
/// 已不存在；若将来又出现整窗空白，先查这里，再看 `MEATSHELL_UI_FONT` 逃生口。
fn system_fonts_available() -> bool {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    db.faces().next().is_some()
}

/// Font picker list for Settings → Interface → Terminal font.
///
/// The ComboBox model is a flat string list with group headers:
///
/// ```text
/// ▍内嵌字体
///   JetBrains Mono
///   Meatshell Mono
/// ▍外置字体
///   Maple Mono Normal NL NF CN
/// ▍系统字体
///   Consolas
///   Cascadia Mono
/// ```
///
/// Header rows (▍) are not selectable; family rows strip their two-space
/// indent via [`family_from_label`] before the config is written, so the
/// stored value stays a bare family name. System monospace families are
/// listed last. Duplicates keep the highest-priority label (embedded >
/// external > system).
///
/// Returns `(labels, entries)` — parallel vectors, `entries` used to map a
/// saved family back to its list index.
/// When `monospace_filter` is false, all system fonts are included (for UI
/// font picker); when true, only monospace families (for terminal font picker).
pub(crate) fn font_choices(
    external: &[String],
    monospace_filter: bool,
) -> (Vec<slint::SharedString>, Vec<FontEntry>) {
    let mut labels: Vec<slint::SharedString> = Vec::new();
    let mut entries: Vec<FontEntry> = Vec::new();
    let mut known: std::collections::HashSet<String> = std::collections::HashSet::new();

    let push_family = |family: &str,
                       labels: &mut Vec<slint::SharedString>,
                       entries: &mut Vec<FontEntry>,
                       known: &mut std::collections::HashSet<String>| {
        if known.insert(family.to_string()) {
            labels.push(format!("  {family}").into());
            entries.push(FontEntry::Family(family.to_string()));
        }
    };
    let push_header = |header: &'static str,
                       labels: &mut Vec<slint::SharedString>,
                       entries: &mut Vec<FontEntry>| {
        labels.push(format!("▍{header}").into());
        entries.push(FontEntry::Header(header));
    };

    // Embedded first, external (registered from the fonts dir) next,
    // system monospace families last — highest priority wins on duplicates.
    push_header(t("内嵌字体", "Embedded fonts"), &mut labels, &mut entries);
    for family in ["JetBrains Mono", "Meatshell Mono"] {
        push_family(family, &mut labels, &mut entries, &mut known);
    }
    push_header(t("外置字体", "External fonts"), &mut labels, &mut entries);
    for family in external {
        push_family(family, &mut labels, &mut entries, &mut known);
    }
    push_header(t("系统字体", "System fonts"), &mut labels, &mut entries);
    let sys = if monospace_filter {
        crate::fonts::system_monospace_families()
    } else {
        crate::fonts::system_families()
    };
    for family in sys {
        push_family(&family, &mut labels, &mut entries, &mut known);
    }
    (labels, entries)
}
/// Resolve a picker label to a bare family name.
///
/// Group headers (`▍…`) return `None` — selecting them must be a no-op.
/// Family rows (two-space indented) return the name without the indent.
pub(crate) fn family_from_label(label: &str) -> Option<&str> {
    if label.starts_with('▍') {
        return None;
    }
    // 「跟随系统（自动）」不对应任何具体家族：存空串 = auto，由
    // `resolve_ui_font_family()` 交回 Slint 的平台默认字体。
    if label == auto_font_label() {
        return Some("");
    }
    Some(label.strip_prefix("  ").unwrap_or(label))
}

/// 「跟随系统（自动）」条目的标签。只出现在**界面字体**列表里（终端字体必须指定
/// 具体家族，没有"自动"一说）。
pub(crate) fn auto_font_label() -> String {
    format!("  {}", t("跟随系统（自动）", "System default (auto)"))
}
/// One entry of the font picker list.
#[allow(dead_code)] // Header payload read by tests only
pub(crate) enum FontEntry {
    /// A non-selectable group header, shown as `▍内嵌字体` etc.
    Header(&'static str),
    /// 「跟随系统（自动）」：存空串，交给 Slint 的平台默认字体。
    /// 有了它，出厂默认在选择器里显示的就是「跟随系统」，而不是某个被猜出来的家族名。
    Auto,
    /// A selectable family, rendered indented under its header.
    Family(String),
}

#[cfg(test)]
mod font_stack_tests {
    use super::*;

    /// 显式选过的界面字体必须**原样**透传，不得追加任何回退段。
    ///
    /// 锁的是一次真实回归：这里曾返回"所选家族 + CJK 段 + 内嵌兜底"的逗号分隔字体栈，
    /// 而 Slint 的 `font-family` 是单个家族名（`FontFamilyName::named(整串)`，不拆逗号）
    /// —— 整串永远匹配不上，于是"在设置里换界面字体，界面毫无变化"。
    #[test]
    fn an_explicit_choice_is_passed_through_verbatim() {
        assert_eq!(
            explicit_ui_family("Maple Mono Normal NL NF CN").as_deref(),
            Some("Maple Mono Normal NL NF CN")
        );
        assert!(
            !explicit_ui_family("Helvetica Neue").unwrap().contains(','),
            "界面字体只能是单个家族名 —— Slint 不拆逗号，整串会被当成不存在的家族"
        );
        // 空 / 纯空白 = auto（交回平台默认）
        assert_eq!(explicit_ui_family(""), None);
        assert_eq!(explicit_ui_family("   "), None);
    }

    /// auto（空串）解析出来的也不能是字体栈。
    ///
    /// 测试环境没有配置 store，走的正是 auto 分支：结果是空串（让 Slint 用平台默认
    /// 字体）或系统字体库不可用时的内嵌兜底 —— 两者都是**单个**名字。
    #[test]
    fn the_auto_path_never_produces_a_font_stack() {
        let resolved = resolve_ui_font_family();
        assert!(
            !resolved.contains(','),
            "auto 不能解析出逗号分隔的字体栈：{resolved:?}"
        );
    }

    /// 「跟随系统（自动）」必须映射回空串（= auto）；
    /// 否则它会被当成一个普通家族名写进配置，重启后指向一个不存在的字体。
    #[test]
    fn the_auto_entry_maps_back_to_an_empty_family() {
        assert_eq!(family_from_label(&auto_font_label()), Some(""));
        // 分组标题仍然不可选
        assert_eq!(family_from_label("▍内嵌字体"), None);
        // 普通家族照旧（标签带两空格缩进）
        assert_eq!(family_from_label("  JetBrains Mono"), Some("JetBrains Mono"));
    }
}
