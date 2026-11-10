use super::{MAX_NOTES_CHARS, release_notes};

/// Markdown 标记要清掉：普通 `Text` 不渲染它，留下来的只是符号噪声。
#[test]
fn strips_markdown_markup() {
    let body = "## Improvements\n\n- **HTTP transfer recovery** — better retries\n- `foo` and __bar__\n";
    assert_eq!(
        release_notes(body),
        "Improvements\n\n• HTTP transfer recovery — better retries\n• foo and bar"
    );
}

/// HTML 注释（含跨行）整段丢掉 —— 发布模板常留着它们。
#[test]
fn drops_html_comments_even_multiline() {
    let body = "<!-- release template\nnotes for maintainers -->\nReal notes\n<!-- inline -->tail";
    assert_eq!(release_notes(body), "Real notes\ntail");
}

/// 空行压缩 + 首尾清理：对话框里那块框不许开头结尾是一片空白。
#[test]
fn collapses_blank_runs_and_trims_ends() {
    assert_eq!(release_notes("\n\nA\n\n\n\nB\n\n"), "A\n\nB");
    assert_eq!(release_notes("   \n  \n"), "");
}

/// 超长截断，并按**字符**算（中文说明不会被截成半个字）。
#[test]
fn truncates_long_bodies_by_chars() {
    let long = "更".repeat(MAX_NOTES_CHARS + 200);
    let out = release_notes(&long);
    assert_eq!(out.chars().count(), MAX_NOTES_CHARS + 1, "截断后带一个省略号");
    assert!(out.ends_with('…'));

    let short = "短说明";
    assert_eq!(release_notes(short), "短说明", "没超长就不该动它");
}

/// 没有 body（很常见）→ 空串，对话框据此整块不显示。
#[test]
fn empty_body_is_empty() {
    assert_eq!(release_notes(""), "");
}
