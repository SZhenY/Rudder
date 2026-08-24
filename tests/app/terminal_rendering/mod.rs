use super::*;

fn hist_line(s: &str) -> Line {
    (s.to_string(), Vec::new(), false)
}

fn wrapped_hist_line(s: &str) -> Line {
    (s.to_string(), Vec::new(), true)
}

fn make_buf(
    rows: u16,
    cols: u16,
    history: &[&str],
    live_lines: &[&str],
    view_offset: usize,
) -> TermBuffer {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(live_lines.join("\r\n").as_bytes());
    TermBuffer {
        parser,
        find_query: String::new(),
        is_dark: false,
        output_highlight: OutputHighlightPreset::Log,
        custom_highlight_rules: Vec::new(),
        json_format_output: false,
        interactive_echo_until: std::time::Instant::now(),
        sel_anchor: None,
        sel_focus: None,
        sel_ranges: Vec::new(),
        history: history.iter().map(|s| hist_line(s)).collect(),
        prev: Vec::new(),
        view_offset,
        scroll_accum: 0.0,
        displayed_text: Vec::new(),
        csi_state: CsiState::Normal,
        csi_pending: Vec::new(),
        raw: std::collections::VecDeque::new(),
    }
}

#[test]
fn settings_modal_yields_macos_wheel_to_its_own_scroll_view() {
    assert!(macos_terminal_wheel_can_target_terminal(false));
    assert!(!macos_terminal_wheel_can_target_terminal(true));
}

mod colors;
mod protocol;
mod selection;
mod sftp_sorting;
