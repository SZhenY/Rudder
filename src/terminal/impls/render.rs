use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;

use unicode_width::UnicodeWidthChar;

use crate::terminal::{
    ATerm, CellAttr, HistSpan, Line as TermLine, OverlineRange, TermColor, attr_from_cell,
    cell_attrs, is_wide_continuation, row_wrapped,
};

/// Small window of retained bytes for split-ESC[3J detection (no longer
/// needed for resize reflow — alacritty handles that natively).
pub(crate) const RAW_CAP: usize = 256;

/// Tab stop interval in columns, matching alacritty's `INITIAL_TABSTOPS`.
const TAB_SPACES: i32 = 8;

/// alacritty 0.26's `put_tab` stores a literal `\t` in the grid cell and moves
/// the cursor to the next tab stop; the gap between the tab cell and the stop
/// is filled with ordinary blank cells.  Terminals render that as spaces up to
/// the stop, so we expand the tab into `(next_stop - column)` spaces and skip
/// the blank filler cells — otherwise the raw `\t` char reaches the Slint text
/// renderer and shows up as a tofu box / control glyph.
fn tab_expansion(column: i32) -> (usize, i32) {
    let next = (column / TAB_SPACES + 1) * TAB_SPACES;
    ((next - column) as usize, next)
}

/// Two cells share a visual style (hence can be merged into one run).
fn same_style(a: &CellAttr, b: &CellAttr) -> bool {
    a.fg == b.fg
        && a.bg == b.bg
        && a.bold == b.bold
        && a.dim == b.dim
        && a.italic == b.italic
        && a.underline == b.underline
        && a.hidden == b.hidden
        && a.strike == b.strike
        && a.inverse == b.inverse
}

/// Column ranges (absolute, merged) of this span that an SGR-53 overline
/// range covers.  Matching is by *absolute* grid position
/// (`history_size() + line`, invariant under scrolling), so ranges stay glued
/// to their content when the screen scrolls — including scrollback rows
/// rendered via `build_line`.
fn overline_segments(
    term: &ATerm,
    line: i32,
    col: i32,
    cells: i32,
    overlines: &[OverlineRange],
) -> Vec<(i32, i32)> {
    let abs = term.grid().history_size() as i64 + line as i64;
    let end = col + cells;
    let mut segs: Vec<(i32, i32)> = overlines
        .iter()
        .filter(|r| r.abs == abs)
        .map(|r| (r.col_start.max(col), r.col_end.min(end)))
        .filter(|(s, e)| s < e)
        .collect();
    segs.sort_unstable();
    let mut merged: Vec<(i32, i32)> = Vec::new();
    for (s, e) in segs {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// Split one run at the overline column boundaries so the decoration only
/// covers the exact SGR-53 range — a merged run can be far wider than the
/// overline (e.g. the trailing blanks of a line), and marking the whole run
/// made the line span the entire row.
///
/// Runs whose text isn't one-cell-per-char (wide CJK, combining marks) keep
/// a whole-run boolean instead of splitting; they're single glyphs anyway.
fn split_span_overlines(
    span: &HistSpan,
    term: &ATerm,
    line: i32,
    overlines: &[OverlineRange],
) -> Vec<HistSpan> {
    let segs = overline_segments(term, line, span.col, span.cells, overlines);
    if segs.is_empty() {
        return vec![span.clone()];
    }
    // Fully covered → one flagged run, no split needed.
    if segs.len() == 1 && segs[0].0 == span.col && segs[0].1 == span.col + span.cells {
        let mut s = span.clone();
        s.overline = true;
        return vec![s];
    }
    let chars: Vec<char> = span.text.chars().collect();
    let is_simple = chars.len() == span.cells as usize;
    if !is_simple {
        let mut s = span.clone();
        s.overline = true;
        return vec![s];
    }
    let mut out = Vec::with_capacity(segs.len() * 2 + 1);
    let mut pos = 0usize; // char index == column offset for simple runs
    let mut col = span.col;
    for (s, e) in segs {
        let cs = (s - span.col) as usize;
        let ce = (e - span.col) as usize;
        if cs > pos {
            out.push(span_slice(span, &chars, pos, cs, col, false));
            col += (cs - pos) as i32;
            pos = cs;
        }
        out.push(span_slice(span, &chars, pos, ce, col, true));
        col += (ce - pos) as i32;
        pos = ce;
    }
    if pos < chars.len() {
        out.push(span_slice(span, &chars, pos, chars.len(), col, false));
    }
    out
}

fn span_slice(
    span: &HistSpan,
    chars: &[char],
    cs: usize,
    ce: usize,
    col: i32,
    overline: bool,
) -> HistSpan {
    HistSpan {
        text: chars[cs..ce].iter().collect(),
        fg: span.fg,
        bg: span.bg,
        bold: span.bold,
        dim: span.dim,
        italic: span.italic,
        underline: span.underline,
        hidden: span.hidden,
        strike: span.strike,
        overline,
        inverse: span.inverse,
        col,
        cells: (ce - cs) as i32,
    }
}

/// Skip blank cells between a tab cell and its tab stop, returning the next
/// column to continue from.  Stops early if a non-blank cell is found (a
/// program overwrote part of the gap).
fn skip_tab_filler(term: &ATerm, row: u16, mut column: u16, stop: i32, columns: u16) -> u16 {
    while column < columns && (column as i32) < stop {
        let filler = cell_attrs(term, row, column);
        if filler.contents != " " {
            break;
        }
        column += 1;
    }
    column
}

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

/// Build one rendered line from the alacritty grid.
///
/// Wide (CJK) glyphs: the leading cell carries `WIDE_CHAR` and the spacer cell
/// `WIDE_CHAR_SPACER`; the spacer's empty content is skipped and the leading
/// cell emits a 2-cell run. Combining marks are already folded into the cell
/// contents by `attr_from_cell`.
pub(crate) fn build_row(
    term: &ATerm,
    row: u16,
    columns: u16,
    overlines: &[OverlineRange],
) -> TermLine {
    // Fast path: skip rows that alacritty's occ tracking knows are empty.
    // Most terminal screens are only partially filled, so this avoids
    // scanning all `columns` cells for blank rows.
    if term.grid()[Line(row as i32)].is_clear() {
        return (String::new(), Vec::new(), false);
    }

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

        let attr = cell_attrs(term, row, column);

        // Tab cells: expand to spaces up to the next 8-column tab stop and
        // skip the blank filler cells (see `tab_expansion`).
        if attr.contents == "\t" {
            let col_i = column as i32;
            let (spaces, next_col) = tab_expansion(col_i);
            let text = " ".repeat(spaces);
            plain.push_str(&text);
            let advance = skip_tab_filler(term, row, column + 1, next_col, columns);
            let cells = next_col - col_i;
            if !is_invisible_default_blank(&text, &attr) {
                runs.extend(make_span(
                    attr,
                    text,
                    column as i32,
                    cells,
                    term,
                    row as i32,
                    overlines,
                ));
            }
            column = advance;
            continue;
        }

        if attr.wide {
            let text = attr.contents.clone();
            plain.push_str(&text);
            runs.extend(make_span(
                attr,
                text,
                column as i32,
                2,
                term,
                row as i32,
                overlines,
            ));
            column += 2;
            continue;
        }

        let start_column = column;
        let mut text = attr.contents.clone();
        plain.push_str(&attr.contents);
        column += 1;
        while column < columns {
            let next = cell_attrs(term, row, column);
            // Tab cells must end the run so the outer loop can expand them;
            // otherwise the raw \t char would end up in the span text.
            if next.wide || next.contents == "\t" || !same_style(&attr, &next) {
                break;
            }
            plain.push_str(&next.contents);
            text.push_str(&next.contents);
            column += 1;
        }

        let cells = (column - start_column) as i32;
        if !is_invisible_default_blank(&text, &attr) {
            runs.extend(make_span(
                attr,
                text,
                start_column as i32,
                cells,
                term,
                row as i32,
                overlines,
            ));
        }
    }
    (plain, runs, row_wrapped(term, row))
}

/// A run of plain spaces on the default background is invisible (the grid
/// shows the terminal background) and must not be emitted as a span — it
/// would paint over the terminal's own background with an opaque fill.
fn is_invisible_default_blank(text: &str, attr: &CellAttr) -> bool {
    text.chars().all(|character| character == ' ')
        && matches!(attr.bg, TermColor::Default)
        && !attr.inverse
}

/// Convert cell attributes into one or more `HistSpan`s, applying SGR-53
/// overline ranges recorded by the ingest interceptor.  Overlined runs are
/// split at the range boundaries so the decoration never overflows the
/// covered columns.
fn make_span(
    attr: CellAttr,
    text: String,
    col: i32,
    cells: i32,
    term: &ATerm,
    line: i32,
    overlines: &[OverlineRange],
) -> Vec<HistSpan> {
    let base = HistSpan {
        text,
        fg: attr.fg,
        bg: attr.bg,
        bold: attr.bold,
        dim: attr.dim,
        italic: attr.italic,
        underline: attr.underline,
        hidden: attr.hidden,
        strike: attr.strike,
        overline: false,
        inverse: attr.inverse,
        col,
        cells,
    };
    split_span_overlines(&base, term, line, overlines)
}

/// Re-apply the current overline ranges to cached runs.  The render cache is
/// keyed on the visible plain text, but a row can be rewritten with identical
/// text while its overline range appears, moves, or is pruned — so cache hits
/// must re-run the split against the live range list.
pub(crate) fn refresh_overlines(
    runs: &[HistSpan],
    overlines: &[OverlineRange],
    term: &ATerm,
    line: i32,
) -> Vec<HistSpan> {
    if overlines.is_empty() {
        return runs.to_vec();
    }
    runs.iter()
        .flat_map(|hs| split_span_overlines(hs, term, line, overlines))
        .collect()
}

/// Read one grid line at the given `line` index (negative = scrollback
/// history, positive / zero = visible area).  Used to capture lines that
/// scrolled into alacritty's native scrollback without building the whole
/// screen.
pub(crate) fn build_line(
    term: &ATerm,
    line: Line,
    columns: u16,
    overlines: &[OverlineRange],
) -> TermLine {
    if term.grid()[line].is_clear() {
        return (String::new(), Vec::new(), false);
    }

    let mut plain = String::with_capacity(columns as usize);
    let mut runs = Vec::new();
    let mut column = 0u16;
    while column < columns {
        if column + 1 < columns {
            let spacer_pt = Point {
                line,
                column: Column(column as usize),
            };
            if term.grid()[spacer_pt]
                .flags
                .contains(Flags::WIDE_CHAR_SPACER)
            {
                column += 1;
                continue;
            }
        }

        let point = Point {
            line,
            column: Column(column as usize),
        };
        let attr = attr_from_cell(&term.grid()[point]);

        // Tab expansion — identical to `build_row`.
        if attr.contents == "\t" {
            let col_i = column as i32;
            let (spaces, next_col) = tab_expansion(col_i);
            let text = " ".repeat(spaces);
            plain.push_str(&text);
            let mut advance = column + 1;
            while advance < columns && (advance as i32) < next_col {
                let filler_pt = Point {
                    line,
                    column: Column(advance as usize),
                };
                if term.grid()[filler_pt].c != ' ' {
                    break;
                }
                advance += 1;
            }
            let cells = next_col - col_i;
            if !is_invisible_default_blank(&text, &attr) {
                runs.extend(make_span(
                    attr,
                    text,
                    column as i32,
                    cells,
                    term,
                    line.0,
                    overlines,
                ));
            }
            column = advance;
            continue;
        }

        if attr.wide {
            let text = attr.contents.clone();
            plain.push_str(&text);
            runs.extend(make_span(
                attr,
                text,
                column as i32,
                2,
                term,
                line.0,
                overlines,
            ));
            column += 2;
            continue;
        }

        let start_column = column;
        let mut text = attr.contents.clone();
        plain.push_str(&attr.contents);
        column += 1;
        while column < columns {
            let next_pt = Point {
                line,
                column: Column(column as usize),
            };
            let next = attr_from_cell(&term.grid()[next_pt]);
            // Tab cells end the run so the outer loop can expand them.
            if next.wide || next.contents == "\t" || !same_style(&attr, &next) {
                break;
            }
            plain.push_str(&next.contents);
            text.push_str(&next.contents);
            column += 1;
        }

        let cells = (column - start_column) as i32;
        if !is_invisible_default_blank(&text, &attr) {
            runs.extend(make_span(
                attr,
                text,
                start_column as i32,
                cells,
                term,
                line.0,
                overlines,
            ));
        }
    }

    // WRAPLINE for scrollback rows: check the last cell.
    let wrapped = if columns > 0 {
        let last = Point {
            line,
            column: Column(columns as usize - 1),
        };
        term.grid()[last].flags.contains(Flags::WRAPLINE)
    } else {
        false
    };
    (plain, runs, wrapped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{UnderlineStyle, new_term, process_bytes};

    #[test]
    fn tab_expands_to_next_stop() {
        let (mut term, mut proc) = new_term(5, 40, 100);
        // alacritty's put_tab stores a literal \t in cell 1 and skips to the
        // 8-column stop; cells 2-7 are blank fillers.
        process_bytes(&mut proc, &mut term, b"1\t2");
        let (plain, runs, _) = build_row(&term, 0, 40, &[]);
        assert_eq!(
            plain.trim_end(),
            "1       2",
            "tab must render as spaces up to the stop"
        );
        // The space filler is invisible (default bg), so only the two text
        // runs survive; the trailing blanks after '2' merge into its run.
        assert_eq!(runs.len(), 2, "runs: '1' @0 and '2' @8");
        assert_eq!(
            (runs[0].text.as_str(), runs[0].col, runs[0].cells),
            ("1", 0, 1)
        );
        assert_eq!((runs[1].col, runs[1].cells), (8, 32));
        assert!(runs[1].text.starts_with('2'));
        assert!(runs[1].text[1..].chars().all(|c| c == ' '));
    }

    #[test]
    fn leading_tab_pads_to_stop() {
        let (mut term, mut proc) = new_term(5, 40, 100);
        process_bytes(&mut proc, &mut term, b"\tX");
        let (plain, runs, _) = build_row(&term, 0, 40, &[]);
        assert_eq!(plain.trim_end(), "        X");
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].col, runs[0].cells), (8, 32));
    }

    #[test]
    fn styles_split_runs_and_merge_keeps_them() {
        let (mut term, mut proc) = new_term(5, 60, 100);
        // Two consecutive runs with the same fg/bg but different underline.
        process_bytes(&mut proc, &mut term, b"\x1b[4mAB\x1b[4:2mCD");
        let (plain, runs, _) = build_row(&term, 0, 60, &[]);
        assert_eq!(plain.trim_end(), "ABCD");
        assert_eq!(runs.len(), 2, "underline change must split the run");
        assert_eq!(runs[0].text, "AB");
        assert_eq!(runs[0].underline, UnderlineStyle::Single);
        assert_eq!(runs[1].text, "CD");
        assert_eq!(runs[1].underline, UnderlineStyle::Double);
    }

    #[test]
    fn overline_ranges_are_applied_to_spans() {
        // Fresh term: display_offset is 0, so abs == row.
        let range = OverlineRange {
            abs: 0,
            col_start: 2,
            col_end: 5,
        };
        let (mut term, mut proc) = new_term(5, 40, 100);
        process_bytes(&mut proc, &mut term, b"abcdef");
        let (_, runs, _) = build_row(&term, 0, 40, &[range]);
        // The merged run (cols 0..6) must be split at the range edges so the
        // decoration covers only cols 2..5: [ab] [cde] [f].
        assert_eq!(
            runs.len(),
            3,
            "overline must clip the run, not flag it whole"
        );
        assert_eq!((runs[0].text.as_str(), runs[0].overline), ("ab", false));
        assert_eq!((runs[1].text.as_str(), runs[1].overline), ("cde", true));
        // The tail run carries the trailing blanks merged into the row.
        assert!(runs[2].text.starts_with('f'));
        assert!(!runs[2].overline);
        // Columns stay contiguous across the split.
        assert_eq!((runs[0].col, runs[0].cells), (0, 2));
        assert_eq!((runs[1].col, runs[1].cells), (2, 3));
        assert_eq!(runs[2].col, 5);
    }

    #[test]
    fn overline_full_coverage_flags_whole_span() {
        // Range covering the whole merged run (text + trailing blanks) →
        // single flagged run, no split.
        let range = OverlineRange {
            abs: 0,
            col_start: 0,
            col_end: 40,
        };
        let (mut term, mut proc) = new_term(5, 40, 100);
        process_bytes(&mut proc, &mut term, b"abcdef");
        let (_, runs, _) = build_row(&term, 0, 40, &[range]);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].overline);
    }

    #[test]
    fn overline_range_outside_row_is_ignored() {
        let range = OverlineRange {
            abs: 1,
            col_start: 0,
            col_end: 6,
        };
        let (mut term, mut proc) = new_term(5, 40, 100);
        process_bytes(&mut proc, &mut term, b"abcdef");
        let (_, runs, _) = build_row(&term, 0, 40, &[range]);
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].overline);
    }

    #[test]
    fn dim_italic_hidden_strike_flow_through() {
        let (mut term, mut proc) = new_term(5, 60, 100);
        process_bytes(&mut proc, &mut term, b"\x1b[2;3;8;9mAB");
        let (_, runs, _) = build_row(&term, 0, 60, &[]);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].dim);
        assert!(runs[0].italic);
        assert!(runs[0].hidden);
        assert!(runs[0].strike);
        assert!(!runs[0].bold);
    }
}
