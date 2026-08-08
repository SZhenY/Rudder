//! Thin adapter over `alacritty_terminal` so the rest of Rudder talks to a
//! stable, parser-agnostic surface (mirrors the old `vt100::Screen` API shape).

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::terminal::types::ATerm;
use crate::terminal::TermColor;

/// Global OSC 52 toggle — set by app.rs on startup / config change.
pub static OSC52_ENABLED: AtomicBool = AtomicBool::new(true);

// ── Custom Dimensions impl — alacritty 0.26 does not export a standalone
//    TermSize type (it's behind #[cfg(test)]), so we bring our own.
#[derive(Clone, Copy, Debug)]
struct TermSize {
    columns: usize,
    screen_lines: usize,
}

impl TermSize {
    fn new(columns: usize, screen_lines: usize) -> Self {
        Self {
            columns,
            screen_lines,
        }
    }
}

impl Dimensions for TermSize {
    fn columns(&self) -> usize {
        self.columns
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
}

/// Build a terminal + processor (replaces `vt100::Parser::new(rows, cols, history)`).
pub(crate) fn new_term(rows: u16, cols: u16, history: usize) -> (ATerm, Processor) {
    let config = TermConfig {
        scrolling_history: history,
        ..TermConfig::default()
    };
    let size = TermSize::new(cols as usize, rows as usize);
    let term = Term::new(config.clone(), &size, VoidListener);
    (term, Processor::new())
}

/// Screen size `(rows, cols)` — replaces `screen.size()`.
pub(crate) fn term_size(term: &ATerm) -> (u16, u16) {
    let grid = term.grid();
    (grid.screen_lines() as u16, grid.columns() as u16)
}

/// Cursor position `(row, col)` — replaces `screen.cursor_position()`.
pub(crate) fn cursor_pos(term: &ATerm) -> (u16, u16) {
    let point = term.grid().cursor.point;
    (point.line.0 as u16, point.column.0 as u16)
}

/// Alternate-screen active — replaces `screen.alternate_screen()`.
pub(crate) fn is_alt(term: &ATerm) -> bool {
    term.mode().contains(TermMode::ALT_SCREEN)
}

/// Bracketed paste enabled — replaces `screen.bracketed_paste()`.
pub(crate) fn bracketed_paste(term: &ATerm) -> bool {
    term.mode().contains(TermMode::BRACKETED_PASTE)
}

/// Application cursor keys mode — replaces `screen.application_cursor()`.
pub(crate) fn app_cursor(term: &ATerm) -> bool {
    term.mode().contains(TermMode::APP_CURSOR)
}

/// Mouse reporting enabled + encoding — replaces
/// `screen.mouse_protocol_mode()/mouse_protocol_encoding()`.
///
/// Mouse protocol mode: use the composite `MOUSE_MODE` flag to check whether
/// *any* mouse reporting is active, then inspect individual encoding bits.
/// Note: URXVT mouse encoding was removed from alacritty 0.26; only SGR and
/// X10 (legacy) remain.
pub(crate) fn mouse_report(term: &ATerm) -> MouseReport {
    let mode = term.mode();
    if !mode.intersects(TermMode::MOUSE_MODE) {
        MouseReport::None
    } else if mode.contains(TermMode::SGR_MOUSE) {
        MouseReport::Sgr
    } else {
        // X10 encoding (the fallback when MOUSE_MODE is set but neither
        // SGR nor URXVT encoding is).
        MouseReport::X10
    }
}

/// Mouse protocol state (mirrors the old `vt100::MouseProtocolMode` usage).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MouseReport {
    None,
    X10,
    Sgr,
}

/// Read one cell's attributes — replaces `screen.cell(row, col)` +
/// `cell.contents()/fgcolor()/bgcolor()/bold()/is_wide()/inverse()`.
///
/// Wide chars: the leading cell carries `WIDE_CHAR`; the spacer cell carries
/// `WIDE_CHAR_SPACER` (its `c` is a space). Combining marks live in
/// `cell.zerowidth()` and are appended to the contents.
#[allow(clippy::type_complexity)]
pub(crate) fn cell_attrs(
    term: &ATerm,
    row: u16,
    column: u16,
) -> (String, TermColor, TermColor, bool, bool, bool) {
    let point = Point {
        line: Line(row as i32),
        column: Column(column as usize),
    };
    let cell = &term.grid()[point];

    let mut contents = cell.c.to_string();
    if let Some(zw) = cell.zerowidth() {
        for ch in zw {
            contents.push(*ch);
        }
    }

    let fg = TermColor::from(&cell.fg);
    let bg = TermColor::from(&cell.bg);
    let bold = cell.flags.contains(Flags::BOLD);
    let wide = cell.flags.contains(Flags::WIDE_CHAR);
    let inverse = cell.flags.contains(Flags::INVERSE);

    (contents, fg, bg, bold, wide, inverse)
}

/// Is this cell the spacer half of a wide (CJK) char? — replaces
/// `cell.is_wide_continuation()`.
pub(crate) fn is_wide_continuation(term: &ATerm, row: u16, column: u16) -> bool {
    let point = Point {
        line: Line(row as i32),
        column: Column(column as usize),
    };
    term.grid()[point].flags.contains(Flags::WIDE_CHAR_SPACER)
}

/// Is this row continued onto the next by automatic wrapping? — replaces
/// `screen.row_wrapped(row)`.
///
/// Alacritty stores the WRAPLINE flag on the last cell of the row (the line
/// was auto-wrapped and its content continues on the next line).
pub(crate) fn row_wrapped(term: &ATerm, row: u16) -> bool {
    let cols = term.grid().columns();
    if cols == 0 {
        return false;
    }
    let point = Point {
        line: Line(row as i32),
        column: Column(cols - 1),
    };
    term.grid()[point].flags.contains(Flags::WRAPLINE)
}

/// Feed bytes into the terminal (replaces `parser.process`).
/// vte 0.15 `Processor::advance<H: Handler>(&mut self, handler: &mut H, bytes: &[u8])`
/// accepts a whole slice, so we can just pass it through.
/// Before feeding, intercept OSC 52 clipboard writes for the `osc52_clipboard` feature.
pub(crate) fn process_bytes(processor: &mut Processor, term: &mut ATerm, bytes: &[u8]) {
    // OSC 52 clipboard interception — only when enabled by the user.
    if OSC52_ENABLED.load(Ordering::Relaxed) {
        if let Some(data) = osc52_extract(bytes) {
            std::thread::spawn(move || {
                let _ = arboard::Clipboard::new().and_then(|mut c| c.set_text(data));
            });
        }
    }
    processor.advance(term, bytes);
}

/// Scan `bytes` for a single OSC 52 clipboard-write sequence and return the
/// decoded payload if found.  Format: ESC ] 5 2 ; Pc ; base64 ST  where ST is
/// BEL (\\x07) or ESC \\ (\\x1b\\x5c).
fn osc52_extract(bytes: &[u8]) -> Option<String> {
    // Fast path: no ESC byte at all → nothing to do.
    let esc_pos = bytes.iter().position(|&b| b == 0x1b)?;
    let rest = &bytes[esc_pos..];

    if !rest.starts_with(b"\x1b]52;") {
        return None;
    }
    // Skip OSC introducer "ESC]52;"
    let after_prefix = &rest[5..];
    // Find the second semicolon (after Pc).  Pc is a single optional clipboard
    // selector char; skip it and locate the payload start.
    let payload_start = {
        let semicolon = after_prefix.iter().position(|&b| b == b';')?;
        after_prefix.get(semicolon + 1..)?
    };
    // Find the terminator: BEL (\\x07) or ST (ESC \\x5c).
    let payload_end = payload_start.iter().position(|&b| b == 0x07 || b == 0x1b)?;
    let b64 = &payload_start[..payload_end];

    if b64.is_empty() {
        return None;
    }
    // Decode base64 — ignore errors (malformed OSC 52 = no-op).
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    String::from_utf8(decoded).ok()
}

/// Resize the grid (native reflow) — replaces `parser.set_size` and the
/// rebuild+replay used for reflow on the normal screen.
pub(crate) fn resize_term(term: &mut ATerm, rows: u16, cols: u16) {
    let size = TermSize::new(cols as usize, rows as usize);
    term.resize(size);
}

/// Build a plain-text line from the grid (test helper / snapshot equivalent of
/// the old `screen.contents()`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn grid_to_lines(term: &ATerm) -> Vec<String> {
    let (rows, cols) = term_size(term);
    let mut out = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let mut s = String::new();
        let mut c = 0u16;
        while c < cols {
            let (text, ..) = cell_attrs(term, r, c);
            s.push_str(&text);
            c += 1;
        }
        // Trim only trailing empty / space-fill cells (after the last
        // non-space content), matching vt100's `screen.contents()` behaviour.
        let trim_end = s.len() - s.chars().rev().take_while(|c| *c == ' ').count();
        out.push(s[..trim_end].to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_and_read_back() {
        let (mut term, mut proc) = new_term(5, 30, 100);
        process_bytes(&mut proc, &mut term, b"hello");
        assert_eq!(term_size(&term), (5, 30));
        let (text, ..) = cell_attrs(&term, 0, 0);
        assert_eq!(text, "h");
        let (row, col) = cursor_pos(&term);
        assert_eq!((row, col), (0, 5));
    }

    #[test]
    fn wide_char_marking() {
        let (mut term, mut proc) = new_term(3, 30, 100);
        // "你" is a 2-cell wide char.
        process_bytes(&mut proc, &mut term, "你".as_bytes());
        let (_, _, _, _, wide, _) = cell_attrs(&term, 0, 0);
        assert!(wide, "leading cell should carry WIDE_CHAR");
        assert!(
            is_wide_continuation(&term, 0, 1),
            "spacer cell should carry WIDE_CHAR_SPACER"
        );
        let (text, ..) = cell_attrs(&term, 0, 0);
        assert_eq!(text, "你");
    }

    #[test]
    fn alt_screen_detection() {
        let (mut term, mut proc) = new_term(5, 30, 100);
        process_bytes(&mut proc, &mut term, b"\x1b[?1049h");
        assert!(is_alt(&term));
        process_bytes(&mut proc, &mut term, b"\x1b[?1049l");
        assert!(!is_alt(&term));
    }

    #[test]
    fn bracketed_paste_detection() {
        let (mut term, mut proc) = new_term(5, 30, 100);
        process_bytes(&mut proc, &mut term, b"\x1b[?2004h");
        assert!(bracketed_paste(&term));
        process_bytes(&mut proc, &mut term, b"\x1b[?2004l");
        assert!(!bracketed_paste(&term));
    }

    #[test]
    fn sgr_mouse_detection() {
        let (mut term, mut proc) = new_term(5, 30, 100);
        process_bytes(&mut proc, &mut term, b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(mouse_report(&term), MouseReport::Sgr);
        process_bytes(&mut proc, &mut term, b"\x1b[?1006l");
        assert_eq!(mouse_report(&term), MouseReport::X10);
    }

    #[test]
    fn resize_reflows_width() {
        let (mut term, mut proc) = new_term(3, 10, 100);
        process_bytes(&mut proc, &mut term, b"abcdefghijklmnop");
        let wide_lines = grid_to_lines(&term);
        resize_term(&mut term, 3, 20);
        let narrow_lines = grid_to_lines(&term);
        // The same text, re-wrapped to the new width.
        assert_eq!(narrow_lines.join(""), wide_lines.join(""));
    }

    #[test]
    fn combining_marks_are_kept() {
        let (mut term, mut proc) = new_term(3, 30, 100);
        // "e" + U+0301 (combining acute)
        process_bytes(&mut proc, &mut term, "e\u{301}".as_bytes());
        let (text, ..) = cell_attrs(&term, 0, 0);
        assert_eq!(text, "e\u{301}");
    }
}
