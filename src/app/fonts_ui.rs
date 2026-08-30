//! UI font selection: enumeration, CJK coverage probing and the settings model.



use crate::i18n::t;

use super::HISTORY_STORE;

/// Enumerate installed monospace font families for the Interface font picker.
/// Terminals want fixed-width fonts, so non-monospace families are filtered out.
/// Choose a UI font family that fontdb can actually resolve, falling back to the
/// embedded "Meatshell Mono" when the system font database is empty/unreadable.
///
/// macOS 26 (Tahoe) shipped a system where fontdb couldn't register the named
/// CJK font ("PingFang SC"), so hard-coding that name made the whole UI render
/// blank (#129). This probes the loaded faces and picks the first CJK-capable
/// family that exists; if none do, it returns the embedded font so the window is
/// still visible (Latin text shows; CJK may tofu — far better than a blank UI).
///
/// Emits a one-line WARN summary (faces loaded + chosen font) so the choice lands
/// in `error.log` for diagnostics without needing RUST_LOG.
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
pub(crate) fn resolve_ui_font_family() -> slint::SharedString {
    use fontdb::{Database, Family, Query, Stretch, Style, Weight};

    // User has picked a UI font in settings → use it unconditionally.
    let saved = HISTORY_STORE.with(|s| {
        s.borrow()
            .as_ref()
            .map(|st| st.borrow().ui_font_family().to_owned())
            .unwrap_or_default()
    });
    if !saved.is_empty() {
        tracing::debug!(font = %saved, "ui-font: using saved preference");
        return saved.into();
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

    let mut db = Database::new();
    db.load_system_fonts();
    let face_count = db.faces().count();

    // CJK-capable system families, most-preferred first, per platform. The UI
    // default font must cover CJK because TextInput doesn't glyph-fallback (#54).
    //
    // macOS note (#129): the modern system CJK fonts (PingFang SC, Hiragino) fail
    // to rasterize under femtovg on some macOS 26 machines — fontdb finds them but
    // every glyph comes out blank. The older Heiti/Songti faces render fine and
    // ship on every macOS, so we prefer them and keep PingFang only as a late
    // fallback. (Verified on an M2/macOS 26: Heiti SC/STHeiti/Songti SC render,
    // PingFang/Hiragino don't.) Power users can still force one via
    // MEATSHELL_UI_FONT. Heiti SC is a clean sans-serif (better for UI than the
    // serif Songti), so it leads.
    #[cfg(target_os = "macos")]
    let candidates: &[&str] = &[
        "Heiti SC",
        "STHeiti",
        "Songti SC",
        "PingFang SC",
        "Hiragino Sans GB",
    ];
    #[cfg(target_os = "windows")]
    let candidates: &[&str] = &[
        // DengXian (等线) leads on Windows: it is the only built-in CJK family
        // with an Italic face, and Slint's femtovg renderer does *not* synthesize
        // oblique — font-italic matches real italic font files only. YaHei/SimHei/
        // SimSun have no italic variants, so CJK spans (rendered with this UI
        // font) would never appear slanted (#italic-cjk).
        "DengXian",
        "Microsoft YaHei UI",
        "Microsoft YaHei",
        "SimHei",
        "SimSun",
    ];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let candidates: &[&str] = &[
        "Noto Sans CJK SC",
        "Noto Sans CJK",
        "Source Han Sans SC",
        "WenQuanYi Micro Hei",
        "Droid Sans Fallback",
    ];

    for name in candidates {
        let q = Query {
            families: &[Family::Name(name)],
            weight: Weight::NORMAL,
            stretch: Stretch::Normal,
            style: Style::Normal,
        };
        if db.query(&q).is_some() {
            tracing::debug!(
                faces = face_count,
                font = name,
                "ui-font: using system CJK font"
            );
            return (*name).into();
        }
    }

    // No preferred family resolved. List what *is* available (if anything) so the
    // log shows whether enumeration is empty or just missing our candidates (#129).
    if face_count > 0 {
        let mut fams: Vec<String> = db
            .faces()
            .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
            .collect();
        fams.sort();
        fams.dedup();
        let sample: Vec<String> = fams.into_iter().take(40).collect();
        tracing::warn!(faces = face_count, available = ?sample,
            "ui-font: no preferred CJK font resolved; listing available families");
    }
    tracing::warn!(
        faces = face_count,
        "ui-font: falling back to embedded 'Meatshell Mono' (system fonts unusable, #129)"
    );
    "Meatshell Mono".into()
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
    Some(label.strip_prefix("  ").unwrap_or(label))
}
/// One entry of the font picker list.
#[allow(dead_code)] // Header payload read by tests only
pub(crate) enum FontEntry {
    /// A non-selectable group header, shown as `▍内嵌字体` etc.
    Header(&'static str),
    /// A selectable family, rendered indented under its header.
    Family(String),
}
