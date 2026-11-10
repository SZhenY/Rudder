use super::history_view_rows;

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
