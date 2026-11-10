use super::fonts_ui::family_from_label;
use super::{FontEntry, font_choices};

#[test]
fn label_strips_indent_and_rejects_headers() {
    // Family rows carry a two-space indent under their group header.
    assert_eq!(
        family_from_label("  JetBrains Mono"),
        Some("JetBrains Mono")
    );
    assert_eq!(
        family_from_label("  Maple Mono Normal NL NF CN"),
        Some("Maple Mono Normal NL NF CN")
    );
    assert_eq!(family_from_label("  Consolas"), Some("Consolas"));
    // Group headers are not selectable.
    assert_eq!(family_from_label("▍内嵌字体"), None);
    assert_eq!(family_from_label("▍系统字体"), None);
    // Unknown/unlabelled values pass through unchanged (backward compat).
    assert_eq!(family_from_label("JetBrains Mono"), Some("JetBrains Mono"));
}

#[test]
fn choices_list_built_embedded_external_system() {
    let external = vec!["外部字体 A".to_string(), "外部字体 B".to_string()];
    let (labels, entries) = font_choices(&external, true);
    // Grouped layout: header, then indented families under it.
    assert!(matches!(&entries[0], FontEntry::Header("内嵌字体")));
    assert!(labels[0].as_str().starts_with("▍"));
    assert!(matches!(&entries[1], FontEntry::Family(f) if f == "JetBrains Mono"));
    assert!(matches!(&entries[2], FontEntry::Header("外置字体")));
    assert!(matches!(&entries[3], FontEntry::Family(f) if f == "外部字体 A"));
    // The last header is the system group.
    let last_header = entries
        .iter()
        .rev()
        .find_map(|e| match e {
            FontEntry::Header(h) => Some(*h),
            _ => None,
        })
        .expect("system header must exist");
    assert_eq!(last_header, "系统字体");
    // Every family row strips back to its bare family name.
    for (label, entry) in labels.iter().zip(entries.iter()) {
        if let FontEntry::Family(f) = entry {
            assert_eq!(family_from_label(label), Some(f.as_str()));
        } else {
            assert!(
                family_from_label(label).is_none(),
                "headers are not selectable"
            );
        }
    }
}
