use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;

use unicode_width::UnicodeWidthChar;

use crate::terminal::{cell_attrs, is_wide_continuation, row_wrapped, ATerm, HistSpan, Line as TermLine, TermColor};

/// Small window of retained bytes for split-ESC[3J detection (no longer
/// needed for resize reflow — alacritty handles that natively).
pub(crate) const RAW_CAP: usize = 256;
/// Per-session rendered scrollback cap.
pub(crate) const MAX_HISTORY: usize = 100_000;

pub(crate) fn cell_prefix(chars: &[char]) -> Vec<usize> {
    let mut prefix = Vec::with_capacity(chars.len() + 1);
    let mut accumulated = 0usize;
    for &character in chars {
        prefix.push(accumulated);
        accumulated += character.width().unwrap_or(0);
    }
    prefix.push(accumulated);
    prefix
}

pub(crate) fn char_at_cell_start(prefix: &[usize], target: usize) -> usize {
    let char_count = prefix.len().saturating_sub(1);
    for index in 0..char_count {
        if prefix[index] <= target && target < prefix[index + 1] {
            return index;
        }
    }
    char_count
}

pub(crate) fn char_after_cell_end(prefix: &[usize], target: usize) -> usize {
    let char_count = prefix.len().saturating_sub(1);
    for index in 0..char_count {
        if prefix[index] > target {
            return index;
        }
    }
    char_count
}

/// Build one rendered line from the alacritty grid.
///
/// Wide (CJK) glyphs: the leading cell carries `WIDE_CHAR` and the spacer cell
/// `WIDE_CHAR_SPACER`; the spacer's empty content is skipped and the leading
/// cell emits a 2-cell run. Combining marks are already folded into the cell
/// contents by `cell_attrs`.
pub(crate) fn build_row(term: &ATerm, row: u16, columns: u16) -> TermLine {
    let mut plain = String::with_capacity(columns as usize);
    let mut runs = Vec::new();
    let mut column = 0u16;
    while column < columns {
        // Skip the spacer half of a wide char (its content lives in the
        // leading cell).
        if is_wide_continuation(term, row, column) {
            column += 1;
            continue;
        }

        let (contents, foreground, background, bold, wide, inverse) =
            cell_attrs(term, row, column);
        if wide {
            plain.push_str(&contents);
            runs.push(HistSpan {
                text: contents,
                fg: foreground,
                bg: background,
                bold,
                inverse,
                col: column as i32,
                cells: 2,
            });
            column += 2;
            continue;
        }

        let start_column = column;
        let mut text = contents.clone();
        plain.push_str(&contents);
        column += 1;
        while column < columns {
            let (next, next_fg, next_bg, next_bold, next_wide, next_inverse) =
                cell_attrs(term, row, column);
            if next_wide
                || next_fg != foreground
                || next_bg != background
                || next_bold != bold
                || next_inverse != inverse
            {
                break;
            }
            plain.push_str(&next);
            text.push_str(&next);
            column += 1;
        }

        let cells = (column - start_column) as i32;
        let invisible_default_blank = text.chars().all(|character| character == ' ')
            && matches!(background, TermColor::Default)
            && !inverse;
        if !invisible_default_blank {
            runs.push(HistSpan {
                text,
                fg: foreground,
                bg: background,
                bold,
                inverse,
                col: start_column as i32,
                cells,
            });
        }
    }
    (plain, runs, row_wrapped(term, row))
}

pub(crate) fn detect_scroll(previous: &[TermLine], current: &[TermLine]) -> usize {
    let mut best_shift = 0usize;
    let mut best_length = 0usize;
    for shift in 0..previous.len() {
        let mut length = 0usize;
        while shift + length < previous.len()
            && length < current.len()
            && previous[shift + length].0 == current[length].0
        {
            length += 1;
        }
        if length > best_length {
            best_length = length;
            best_shift = shift;
        }
    }
    best_shift
}

/// Read one grid line at the given `line` index (negative = scrollback
/// history, positive / zero = visible area).  Used to capture lines that
/// scrolled into alacritty's native scrollback without building the whole
/// screen.
pub(crate) fn build_line(term: &ATerm, line: Line, columns: u16) -> TermLine {
    let mut plain = String::with_capacity(columns as usize);
    let mut runs = Vec::new();
    let mut column = 0u16;
    while column < columns {
        if column + 1 < columns {
            let spacer_pt = Point {
                line,
                column: Column(column as usize),
            };
            if term.grid()[spacer_pt].flags.contains(Flags::WIDE_CHAR_SPACER) {
                column += 1;
                continue;
            }
        }

        let point = Point {
            line,
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

        if wide {
            plain.push_str(&contents);
            runs.push(HistSpan {
                text: contents,
                fg,
                bg,
                bold,
                inverse,
                col: column as i32,
                cells: 2,
            });
            column += 2;
            continue;
        }

        let start_column = column;
        let mut text = contents.clone();
        plain.push_str(&contents);
        column += 1;
        while column < columns {
            let next_pt = Point {
                line,
                column: Column(column as usize),
            };
            let next = &term.grid()[next_pt];
            let next_fg = TermColor::from(&next.fg);
            let next_bg = TermColor::from(&next.bg);
            let next_bold = next.flags.contains(Flags::BOLD);
            let next_wide = next.flags.contains(Flags::WIDE_CHAR);
            let next_inverse = next.flags.contains(Flags::INVERSE);
            if next_wide || next_fg != fg || next_bg != bg || next_bold != bold || next_inverse != inverse
            {
                break;
            }
            let mut next_text = next.c.to_string();
            if let Some(zw) = next.zerowidth() {
                for ch in zw { next_text.push(*ch); }
            }
            plain.push_str(&next_text);
            text.push_str(&next_text);
            column += 1;
        }

        let cells = (column - start_column) as i32;
        let invisible_default_blank = text.chars().all(|c| c == ' ')
            && matches!(bg, TermColor::Default) && !inverse;
        if !invisible_default_blank {
            runs.push(HistSpan {
                text,
                fg,
                bg,
                bold,
                inverse,
                col: start_column as i32,
                cells,
            });
        }
    }

    // WRAPLINE for scrollback rows: check the last cell.
    let wrapped = if columns > 0 {
        let last = Point { line, column: Column(columns as usize - 1) };
        term.grid()[last].flags.contains(Flags::WRAPLINE)
    } else {
        false
    };
    (plain, runs, wrapped)
}
