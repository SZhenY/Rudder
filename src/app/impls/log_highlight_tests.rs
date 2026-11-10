use super::*;
use crate::terminal::TermColor;

fn plain_run(text: &str, col: i32) -> HistSpan {
    HistSpan {
        text: text.to_string(),
        fg: TermColor::Default,
        bg: TermColor::Default,
        bold: false,
        dim: false,
        italic: false,
        underline: Default::default(),
        hidden: false,
        strike: false,
        overline: false,
        inverse: false,
        col,
        cells: text.chars().count() as i32,
    }
}

fn custom_rule(
    pattern: &str,
    regex: bool,
    case_sensitive: bool,
    whole_line: bool,
    color: &str,
) -> CompiledOutputRule {
    compile_output_rules(&[OutputHighlightRule {
        pattern: pattern.to_string(),
        regex,
        case_sensitive,
        whole_line,
        color: color.to_string(),
        enabled: true,
    }])
    .pop()
    .expect("test rule should compile")
}

#[test]
fn highlights_uppercase_level_and_preserves_columns() {
    let runs = highlight_plain_output(
        vec![plain_run("2026-07-14T10:20:30Z ERROR request failed", 0)],
        OutputHighlightPreset::Log,
        &[],
    );
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[1].text, "ERROR");
    assert_eq!(runs[1].col, 21);
    assert_eq!(runs[1].cells, 5);
    assert!(runs[1].bold);
    assert!(matches!(runs[1].fg, TermColor::Idx(9)));
    assert_eq!(runs[2].col, 26);
}

#[test]
fn highlights_structured_lowercase_level_only() {
    let json = r#"{"level":"warn","message":"disk nearly full"}"#;
    let runs =
        highlight_plain_output(vec![plain_run(json, 4)], OutputHighlightPreset::Log, &[]);
    let level = runs
        .iter()
        .find(|run| run.text == "warn")
        .expect("structured level should be highlighted");
    assert!(matches!(level.fg, TermColor::Idx(11)));

    assert!(log_level_marker("an error occurred", 96).is_none());
    assert!(log_level_marker("ERROR_CODE=5", 96).is_none());
}

#[test]
fn preserves_existing_ansi_styles() {
    let mut coloured = plain_run("ERROR", 0);
    coloured.fg = TermColor::Idx(2);
    let runs = highlight_plain_output(vec![coloured], OutputHighlightPreset::Log, &[]);
    assert_eq!(runs.len(), 1);
    assert!(matches!(runs[0].fg, TermColor::Idx(2)));
    assert!(!runs[0].bold);
}

#[test]
fn alternate_screen_does_not_add_log_colours() {
    let (mut term, mut processor) = crate::terminal::new_term(3, 30, 0);
    process_bytes(&mut processor, &mut term, b"\x1b[?1049hERROR");
    assert!(crate::terminal::is_alt(&term));
    let (_plain, runs, _wrapped) = build_row(&term, 0, 30, &[]);
    let level = runs
        .iter()
        .find(|run| run.text.contains("ERROR"))
        .expect("alternate-screen text should still render");
    assert!(matches!(level.fg, TermColor::Default));
    assert!(!level.bold);
}

#[test]
fn off_preset_leaves_plain_levels_untouched() {
    let runs = highlight_plain_output(
        vec![plain_run("ERROR request failed", 0)],
        OutputHighlightPreset::Off,
        &[],
    );
    assert_eq!(runs.len(), 1);
    assert!(matches!(runs[0].fg, TermColor::Default));
    assert!(!runs[0].bold);
}

#[test]
fn devops_preset_adds_deployment_and_structured_states() {
    let success = highlight_plain_output(
        vec![plain_run("deploy SUCCESS", 0)],
        OutputHighlightPreset::DevOps,
        &[],
    );
    let token = success
        .iter()
        .find(|run| run.text == "SUCCESS")
        .expect("DevOps success should be highlighted");
    assert!(matches!(token.fg, TermColor::Idx(10)));

    let json = highlight_plain_output(
        vec![plain_run(r#"{"status":"failed"}"#, 0)],
        OutputHighlightPreset::DevOps,
        &[],
    );
    let token = json
        .iter()
        .find(|run| run.text == "failed")
        .expect("structured DevOps state should be highlighted");
    assert!(matches!(token.fg, TermColor::Idx(9)));

    let conservative = highlight_plain_output(
        vec![plain_run("deploy SUCCESS", 0)],
        OutputHighlightPreset::Log,
        &[],
    );
    assert_eq!(conservative.len(), 1);
}

#[test]
fn custom_literal_is_case_insensitive_and_overrides_builtin_colour() {
    let rule = custom_rule("error", false, false, false, "green");
    let runs = highlight_plain_output(
        vec![plain_run("ERROR then error", 0)],
        OutputHighlightPreset::Log,
        &[rule],
    );
    let hits: Vec<_> = runs
        .iter()
        .filter(|run| matches!(run.fg, TermColor::Idx(10)))
        .collect();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].text, "ERROR");
    assert_eq!(hits[1].text, "error");
    assert!(!runs.iter().any(|run| matches!(run.fg, TermColor::Idx(9))));
}

#[test]
fn custom_regex_can_highlight_whole_line_without_overwriting_ansi() {
    let rule = custom_rule(r"timeout|denied", true, false, true, "magenta");
    let mut ansi = plain_run(" ANSI", 18);
    ansi.fg = TermColor::Idx(2);
    let runs = highlight_plain_output(
        vec![plain_run("request timeout   ", 0), ansi],
        OutputHighlightPreset::Log,
        &[rule],
    );
    assert!(matches!(runs[0].fg, TermColor::Idx(13)));
    assert!(runs[0].bold);
    assert!(matches!(runs[1].fg, TermColor::Idx(2)));
}

#[test]
fn custom_unicode_match_preserves_terminal_grid_columns() {
    let rule = custom_rule("错误", false, true, false, "red");
    let text = "前缀错误 done";
    let mut run = plain_run(text, 0);
    run.cells = text_cell_width(text);
    let runs = highlight_plain_output(vec![run], OutputHighlightPreset::Log, &[rule]);
    let hit = runs
        .iter()
        .find(|run| run.text == "错误")
        .expect("CJK keyword should be highlighted");
    assert_eq!(hit.col, 4);
    assert_eq!(hit.cells, 4);
}

#[test]
fn invalid_regex_is_rejected_before_persistence() {
    assert!(validate_output_highlight_rule("([", true, false).is_err());
    assert!(validate_output_highlight_rule("literal", false, false).is_ok());
}

#[test]
fn builtin_preset_highlights_numbers_urls_ips_uuids_and_keywords() {
    // Bare number → left unstyled (no catch-all number rule, no grey)
    let num = highlight_plain_output(
        vec![plain_run("processed 42 records", 0)],
        OutputHighlightPreset::Builtin,
        &[],
    );
    assert!(
        num.iter()
            .any(|r| r.text.contains("42") && matches!(r.fg, TermColor::Default)),
        "bare number should stay default-coloured"
    );

    // URL → blue (12), including the port and path
    let url = highlight_plain_output(
        vec![plain_run("GET http://api.dev:8080/v1", 0)],
        OutputHighlightPreset::Builtin,
        &[],
    );
    assert!(
        url.iter()
            .any(|r| r.text.contains("http") && matches!(r.fg, TermColor::Idx(12)))
    );

    // Bare domain without scheme → blue (12)
    let domain = highlight_plain_output(
        vec![plain_run("ping www.baidu.com", 0)],
        OutputHighlightPreset::Builtin,
        &[],
    );
    assert!(
        domain
            .iter()
            .any(|r| r.text == "www.baidu.com" && matches!(r.fg, TermColor::Idx(12))),
        "bare domain www.baidu.com should be styled blue"
    );

    // A single-dot filename must NOT be mistaken for a domain.
    let fname = highlight_plain_output(
        vec![plain_run("-rw-r--r-- 1 root root 8388608 querylog.json", 0)],
        OutputHighlightPreset::Builtin,
        &[],
    );
    assert!(
        !fname
            .iter()
            .any(|r| r.text.contains("querylog.json") && matches!(r.fg, TermColor::Idx(12))),
        "single-dot filename should not be styled as a domain"
    );

    // IPv4 → blue (12)
    let ip = highlight_plain_output(
        vec![plain_run("client 192.168.1.100", 0)],
        OutputHighlightPreset::Builtin,
        &[],
    );
    assert!(
        ip.iter()
            .any(|r| r.text == "192.168.1.100" && matches!(r.fg, TermColor::Idx(12)))
    );

    // IPv6 with `::` compression → one blue span (not split, no grey hex)
    let ipv6 = highlight_plain_output(
        vec![plain_run("addr fe80::f0da:145:b458:4e3e", 0)],
        OutputHighlightPreset::Builtin,
        &[],
    );
    assert!(ipv6.iter().any(
        |r| r.text == "fe80::f0da:145:b458:4e3e" && matches!(r.fg, TermColor::Idx(12))
    ),
        "IPv6 with :: must be one blue span");

    // UUID → magenta (13)
    let uuid = highlight_plain_output(
        vec![plain_run("id=550e8400-e29b-41d4-a716-446655440000", 0)],
        OutputHighlightPreset::Builtin,
        &[],
    );
    assert!(
        uuid.iter()
            .any(|r| r.text.contains("550e8400") && matches!(r.fg, TermColor::Idx(13)))
    );

    // Severity keyword → red (9), case-insensitive
    let kw = highlight_plain_output(
        vec![plain_run("error failed", 0)],
        OutputHighlightPreset::Builtin,
        &[],
    );
    assert!(
        kw.iter()
            .any(|r| r.text == "error" && matches!(r.fg, TermColor::Idx(9)))
    );
}

#[test]
fn builtin_preset_highlights_dates_and_times_without_grey_fragments() {
    // `date` output: month-day, time and year are yellow; no grey digits.
    let date = highlight_plain_output(
        vec![plain_run("Sat Aug 15 10:44:22 AM CST 2026", 0)],
        OutputHighlightPreset::Builtin,
        &[],
    );
    assert!(
        date.iter()
            .any(|r| r.text.contains("Aug 15") && matches!(r.fg, TermColor::Idx(11))),
        "month-day should be yellow"
    );
    assert!(
        date.iter()
            .any(|r| r.text.contains("10:44:22") && matches!(r.fg, TermColor::Idx(11))),
        "clock time should be yellow"
    );
    assert!(
        date.iter()
            .any(|r| r.text == "2026" && matches!(r.fg, TermColor::Idx(11))),
        "year should be yellow"
    );
    assert!(
        date.iter().all(|r| !matches!(r.fg, TermColor::Idx(8))),
        "no grey fragment should appear in date output"
    );
}

#[test]
fn builtin_preset_handles_sized_numbers_root_mount_and_prompt_path() {
    // 1.9G must be highlighted as a whole, not just the leading `1`
    // (regression: `\b\d+...\b` backtracks at `9G` because both are word
    // chars, leaving `.9` unstyled).
    // Sized number must be styled as one green span (e.g. `391M`), not a
    // bare `391` followed by an unstyled `M`.
    let sized = highlight_plain_output(
        vec![plain_run("tmpfs  391M  40M  351M  11% /run", 0)],
        OutputHighlightPreset::Builtin,
        &[],
    );
    assert!(
        sized
            .iter()
            .any(|r| r.text == "391M" && matches!(r.fg, TermColor::Idx(10))),
        "sized number 391M should be one green span"
    );
    assert!(
        sized.iter().all(|r| r.text != "391"),
        "391 must not be split off from its M unit"
    );
    // The full `df`-style line: the bare root `/` mount point is styled.
    let df = highlight_plain_output(
        vec![plain_run("/dev/mmcblk0p2  6.7G  3.2G  3.4G  48% /", 0)],
        OutputHighlightPreset::Builtin,
        &[],
    );
    assert!(
        df.iter()
            .any(|r| r.text == "6.7G" && matches!(r.fg, TermColor::Idx(10)))
    );
    assert!(
        df.iter()
            .any(|r| r.text.ends_with('/') && matches!(r.fg, TermColor::Idx(14))),
        "bare root mount point `/` should be styled as a path"
    );

    // Prompt path after a colon: `ne@fnnas:/vol1/1000/...` — the path
    // following the `:` must be styled magenta, distinct from mount points.
    let prompt = highlight_plain_output(
        vec![plain_run("ne@fnnas:/vol1/1000/Adguardhome/work/data$", 0)],
        OutputHighlightPreset::Builtin,
        &[],
    );
    assert!(
        prompt
            .iter()
            .any(|r| r.text.contains("/vol1/1000") && matches!(r.fg, TermColor::Idx(13))),
        "prompt path after colon should be styled magenta"
    );
}
