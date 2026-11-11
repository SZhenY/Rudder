use super::{history_view_pair, history_view_rows};

#[test]
fn lists_and_filters_commands_newest_last() {
    let history = vec![
        "git status".to_string(),
        "cargo check".to_string(),
        "git log".to_string(),
    ];

    let all: Vec<String> = history_view_rows(&history, "")
        .into_iter()
        .map(Into::into)
        .collect();
    assert_eq!(all, ["git status", "cargo check", "git log"]);

    let filtered: Vec<String> = history_view_rows(&history, "GIT")
        .into_iter()
        .map(Into::into)
        .collect();
    assert_eq!(filtered, ["git status", "git log"]);
}

#[test]
fn view_and_preview_columns_stay_aligned() {
    let history = vec![
        "git status".to_string(),
        "cat <<EOF\nfirst\nsecond\nEOF".to_string(),
        "GIT LOG".to_string(),
    ];
    let (rows, preview) = history_view_pair(&history, "git");
    assert_eq!(rows.len(), 2);
    assert_eq!(preview.len(), rows.len());
    for (row, shown) in rows.iter().zip(&preview) {
        assert!(!shown.contains('\n'), "preview must be single-line: {shown}");
        assert_eq!(shown.as_str(), crate::app::quick_commands::command_preview(row.as_str()));
    }
}
