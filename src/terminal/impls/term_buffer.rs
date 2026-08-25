use crate::terminal::{
    BuiltScreen, CsiState, HistSpan, Line, OverlineRange, RAW_CAP, RenderedLine, TermBuffer,
    build_line, build_row, cursor_pos, highlight_plain_output, is_alt, process_bytes,
    refresh_overlines, render_term_span, resize_term, term_size,
};
use crate::ui::TermMatch;
use crate::ui::TermSpan;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalQuery {
    Status,
    CursorPosition { private: bool },
    PrimaryDeviceAttributes,
}

fn terminal_query(sequence: &[u8]) -> Option<TerminalQuery> {
    match sequence {
        b"\x1b[5n" => Some(TerminalQuery::Status),
        b"\x1b[6n" => Some(TerminalQuery::CursorPosition { private: false }),
        b"\x1b[?6n" => Some(TerminalQuery::CursorPosition { private: true }),
        b"\x1b[c" | b"\x1b[0c" => Some(TerminalQuery::PrimaryDeviceAttributes),
        _ => None,
    }
}

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line as GridLine;
use alacritty_terminal::term::TermDamage;
// Selection type used only in selection_rects_visible via term.selection

impl TermBuffer {
    /// Selection highlight rectangles for the current visible window.
    pub(crate) fn selection_rects_visible(&self, cols: u16) -> Vec<TermMatch> {
        let sel = match self.term.selection {
            Some(ref s) => s,
            None => return Vec::new(),
        };
        let range = match sel.to_range(&self.term) {
            Some(r) => r,
            None => return Vec::new(),
        };

        let rows = term_size(&self.term).0 as i32;
        let vo = self.view_offset as i32;
        let lo = range.start.line.0 + vo;
        let hi = range.end.line.0 + vo;
        if hi < 0 || lo >= rows {
            return Vec::new();
        }
        let lo_r = lo.max(0);
        let hi_r = hi.min(rows - 1);

        let mut out = Vec::new();
        for vis in lo_r..=hi_r {
            let gl = GridLine(vis - vo);
            let (c0, c1) = if gl == range.start.line && gl == range.end.line {
                (
                    range.start.column.0.min(range.end.column.0),
                    range.end.column.0.max(range.start.column.0),
                )
            } else if gl == range.start.line {
                (range.start.column.0, cols.saturating_sub(1) as usize)
            } else if gl == range.end.line {
                (0, range.end.column.0)
            } else {
                (0, cols.saturating_sub(1) as usize)
            };
            out.push(TermMatch {
                row: vis,
                col: c0 as i32,
                len: (c1.saturating_sub(c0) + 1) as i32,
            });
        }
        out
    }

    /// Jump to the first matching row when the find query points outside the
    /// visible window (#233).  Searches alacritty's native scrollback grid
    /// (negative Line indices) and the live visible rows.
    pub(crate) fn scroll_to_first_find_match(&mut self, query: &str) -> bool {
        if query.is_empty() || is_alt(&self.term) {
            return false;
        }
        let q = query.to_lowercase();
        let (rows, cols) = term_size(&self.term);
        let rows = rows as usize;
        let hist_len = self
            .term
            .total_lines()
            .saturating_sub(self.term.screen_lines());
        let combined_len = hist_len + rows;

        // Search scrollback first (newest → oldest), then live rows.
        let find_idx = (0..hist_len)
            .rev()
            .map(|i| build_line(&self.term, GridLine(-(i as i32 + 1)), cols, &[]).0)
            .chain((0..rows).map(|r| build_row(&self.term, r as u16, cols, &[]).0))
            .position(|line| line.to_lowercase().contains(&q));

        let Some(match_idx) = find_idx else {
            return false;
        };
        let top = match_idx.min(combined_len.saturating_sub(rows));
        let new_offset = combined_len.saturating_sub(rows + top);
        if self.view_offset == new_offset {
            return false;
        }
        self.view_offset = new_offset;
        true
    }

    /// Feed bytes to alacritty.  Scrollback is read on demand from the
    /// alacritty Grid via negative `Line` indices in `render()`, so we no
    /// longer need to capture scrolled-off lines into a separate history.
    /// The returned bytes are terminal-query replies (DSR/CPR/DA1) that must
    /// be written back to the PTY immediately (#328).
    pub(crate) fn ingest(&mut self, input: &[u8]) -> Vec<u8> {
        // Pretty-print + colour complete JSON lines before any other handling
        // so the grid, raw replay stream, and query scanner all see the same
        // bytes (#338).
        let formatted = self
            .json_format_output
            .then(|| crate::terminal::format_json_output(input));
        let input = formatted.as_deref().unwrap_or(input);
        let replies = self.detect_terminal_queries(input);
        // Rewrite HVP (`ESC [ … f`) → CUP (`ESC [ … H`) so vt100 (which only
        // implements `H`) honours btop/htop's absolute cursor positioning.
        let bytes = self.rewrite_hvp(input);
        // Retain the (post-rewrite) stream, capped, so a resize can replay it at
        // the new width and reflow already-printed output (#169).
        self.raw.extend(bytes.iter().copied());
        // CSI 3 J means "erase saved lines". The vt100 crate clears its own
        // scrollback, but Rudder maintains a separate rendered history and a
        // raw replay stream for resize reflow. Drop both sides of that history,
        // including when the CSI sequence was split across SSH reads (#319).
        let erase_saved_through = {
            let raw = self.raw.make_contiguous();
            raw.windows(4)
                .rposition(|window| window == b"\x1b[3J")
                .map(|position| position + 4)
        };
        if let Some(end) = erase_saved_through {
            self.raw.drain(..end);
            self.prev.clear();
            self.rendered.clear();
            self.view_offset = 0;
            self.term.selection = None;
            self.clear_overlines();
        }
        self.cap_raw();
        self.ingest_chunk(&bytes);
        replies
    }

    /// Scan input for DSR/CPR/DA1 terminal queries and build replies.  The
    /// CSI scanner survives split reads thanks to `csi_pending`.
    fn detect_terminal_queries(&mut self, input: &[u8]) -> Vec<u8> {
        let mut replies = Vec::new();
        for &byte in input {
            match self.csi_state {
                CsiState::Normal => {
                    if byte == 0x1b {
                        self.csi_pending.clear();
                        self.csi_pending.push(byte);
                        self.csi_state = CsiState::Esc;
                    }
                }
                CsiState::Esc => {
                    if byte == b'[' {
                        self.csi_pending.push(byte);
                        self.csi_state = CsiState::Csi;
                    } else {
                        self.csi_pending.clear();
                        if byte == 0x1b {
                            self.csi_pending.push(byte);
                        } else {
                            self.csi_state = CsiState::Normal;
                        }
                    }
                }
                CsiState::Csi => {
                    self.csi_pending.push(byte);
                    if (0x40..=0x7e).contains(&byte) {
                        if let Some(kind) = terminal_query(&self.csi_pending) {
                            match kind {
                                TerminalQuery::Status => replies.extend_from_slice(b"\x1b[0n"),
                                TerminalQuery::CursorPosition { private } => {
                                    let point = self.term.grid().cursor.point;
                                    let response = if private {
                                        format!(
                                            "\x1b[?{};{}R",
                                            point.line.0 + 1,
                                            point.column.0 + 1
                                        )
                                    } else {
                                        format!("\x1b[{};{}R", point.line.0 + 1, point.column.0 + 1)
                                    };
                                    replies.extend_from_slice(response.as_bytes());
                                }
                                TerminalQuery::PrimaryDeviceAttributes => {
                                    replies.extend_from_slice(b"\x1b[?1;2c")
                                }
                            }
                        }
                        self.csi_pending.clear();
                        self.csi_state = CsiState::Normal;
                    } else if self.csi_pending.len() > 64 {
                        self.csi_pending.clear();
                        self.csi_state = CsiState::Normal;
                    }
                }
            }
        }
        replies
    }

    /// Feed bytes to alacritty.  Scrollback is read directly from
    /// alacritty's native Grid on demand (via `build_line` with negative
    /// Line indices), so we no longer need to capture scrolled-off lines
    /// into a separate history.
    fn ingest_chunk(&mut self, bytes: &[u8]) {
        let has_cursor_home = bytes.windows(3).any(|w| w == b"\x1b[H");
        let has_erase_display =
            bytes.windows(4).any(|w| w == b"\x1b[2J") || bytes.windows(3).any(|w| w == b"\x1b[J");
        let is_fullscreen_refresh = has_cursor_home && has_erase_display;

        // Segment the stream at SGR sequences so the overline/double-underline
        // interceptor can read the exact cursor position where each SGR takes
        // effect (the grid is up to date for every segment we feed).
        self.ingest_segments(bytes);
        let (rows, cols) = term_size(&self.term);
        let alt = is_alt(&self.term);
        if alt || is_fullscreen_refresh {
            // Alt-screen switch and full-screen redraws rewrite the whole
            // grid — stale overline ranges no longer describe the content.
            self.clear_overlines();
        }
        if alt {
            self.view_offset = 0;
            self.prev.clear();
            self.rendered.clear();
            return;
        }
        if is_fullscreen_refresh {
            self.view_offset = 0;
            self.prev.clear();
            self.rendered.clear();
            return;
        }

        // Build the current visible grid.  Use alacritty's built-in damage
        // tracking to only rebuild lines that actually changed (a full-screen
        // rebuild is still done when damage indicates a full redraw).
        //
        // display_offset is read *before* damage() to avoid conflicting
        // with the mutable borrow held by the damage iterator.
        let display_offset = self.term.grid().display_offset();
        let curr: Vec<Line> = match self.term.damage() {
            TermDamage::Full => (0..rows)
                .map(|r| build_row(&self.term, r, cols, &self.overline_ranges))
                .collect(),
            TermDamage::Partial(damaged_lines) => {
                let damaged_rows: Vec<usize> = damaged_lines
                    .map(|b| b.line.saturating_sub(display_offset))
                    .filter(|&r| r < rows as usize)
                    .collect();
                let mut cur = self.prev.clone();
                cur.resize(rows as usize, (String::new(), Vec::new(), false));
                for r in damaged_rows {
                    cur[r] = build_row(&self.term, r as u16, cols, &self.overline_ranges);
                }
                cur
            }
        };
        self.term.reset_damage();
        self.prev = curr;
    }

    /// Feed a byte slice to the terminal, pausing at every complete SGR
    /// sequence so the interceptor can (a) record the cursor position where
    /// SGR 53 turns overline on/off, and (b) rewrite SGR 21 (which vte 0.15
    /// mis-parses as CancelBold) into the `4:2` double-underline form.
    ///
    /// A CSI sequence split across chunks (SSH / pipe reads are arbitrary)
    /// is buffered in `sgr_buf` and completed by the next ingest; the vte
    /// parser itself is chunk-agnostic, but the interceptor must see the
    /// whole sequence to act on SGR 53 / 21.
    fn ingest_segments(&mut self, bytes: &[u8]) {
        if self.sgr_buf.is_empty() {
            self.ingest_segments_inner(bytes);
        } else {
            let mut combined = std::mem::take(&mut self.sgr_buf);
            combined.extend_from_slice(bytes);
            self.ingest_segments_inner(&combined);
        }
    }

    fn ingest_segments_inner(&mut self, bytes: &[u8]) {
        let (seqs, tail) = scan_csi_sequences(bytes);
        let mut feed_from = 0usize;
        for (start, end) in seqs {
            if start > feed_from {
                process_bytes(
                    &mut self.processor,
                    &mut self.term,
                    &bytes[feed_from..start],
                );
            }
            let (row, col) = cursor_pos(&self.term);
            self.apply_sgr(&bytes[start..end], row as i32, col as i32);
            feed_from = end;
        }
        match tail {
            Some(t) => {
                // Unterminated CSI at the chunk tail: feed everything before
                // it, buffer the rest for the next ingest.
                if t > feed_from {
                    process_bytes(&mut self.processor, &mut self.term, &bytes[feed_from..t]);
                }
                self.sgr_buf = bytes[t..].to_vec();
            }
            None => {
                if feed_from < bytes.len() {
                    process_bytes(&mut self.processor, &mut self.term, &bytes[feed_from..]);
                }
            }
        }
    }

    /// Handle one SGR sequence: rewrite unsupported parameters, keep the
    /// overline state machine in sync, and feed the (possibly rewritten)
    /// sequence to the parser.
    fn apply_sgr(&mut self, seq: &[u8], row: i32, col: i32) {
        // seq = ESC [ params m
        let params = if seq.len() >= 3 {
            &seq[2..seq.len() - 1]
        } else {
            &seq[..0]
        };
        let mut has_53 = false;
        let mut has_reset = false;
        // Drop the overline parameter and rewrite 21 → 4:2 (vte parses 21 as
        // CancelBold; the xterm convention — and our char test suite — want
        // double underline).  Rebuilding the parameter list instead of
        // splicing in place guarantees a dropped tail parameter leaves no
        // trailing `;` behind (an empty SGR param would read as a reset).
        let mut parts: Vec<&[u8]> = Vec::with_capacity(4);
        let split: Vec<&[u8]> = params.split(|&b| b == b';').collect();
        for (i, part) in split.iter().enumerate() {
            // 38;5;N / 48;5;N — the trailing N is the 256-colour *index*, not
            // an independent SGR parameter. Treating it as SGR 21/53/reset
            // would corrupt e.g. `48;5;53m` (dark magenta background) into a
            // dropped/rewritten parameter → alacritty ignores it → the cell
            // keeps its previous background (transparent) and the swatch
            // renders as the theme background (#cube53-regression).
            let is_color_index =
                i >= 2 && split[i - 1] == b"5" && (split[i - 2] == b"38" || split[i - 2] == b"48");
            let is_21 = !is_color_index && *part == b"21";
            let is_53 = !is_color_index && *part == b"53";
            let is_reset = !is_color_index && matches!(*part, b"0" | b"22" | b"24" | b"29");
            has_53 |= is_53;
            has_reset |= is_reset;
            if is_53 {
                continue;
            }
            parts.push(if is_21 { b"4:2" } else { part });
        }
        let rewritten: Vec<u8> = parts.join(&b';');
        // Overline state machine: 53 turns it on, a reset (0/22/24/29) or any
        // SGR without 53 turns it off again.
        if has_53 && !self.overline_active {
            self.overline_active = true;
            self.overline_start = Some((row, col));
        } else if !has_53 && self.overline_active {
            self.close_overline(row, col);
        }
        if has_reset && has_53 {
            // 0m…53m-style oddities: a reset wins over the 53 in the same
            // sequence; treat the range as closed at this point.
            self.close_overline(row, col);
        }

        // A sequence made up entirely of dropped parameters (e.g. `53m`)
        // must not reach the parser as an empty SGR — that would read as a
        // full reset (SGR 0) and wipe unrelated attributes.
        if rewritten.is_empty() {
            return;
        }

        let mut out = Vec::with_capacity(rewritten.len() + 3);
        out.extend_from_slice(b"\x1b[");
        out.extend_from_slice(&rewritten);
        out.push(b'm');
        process_bytes(&mut self.processor, &mut self.term, &out);
    }

    /// Close the pending overline range (cursor at `row`/`col`, exclusive
    /// end) and split it into per-row column ranges.  Ranges are anchored to
    /// the grid's *absolute* position (`history_size() + line`), which is
    /// invariant under scrolling: a row that scrolls from `line` to
    /// `line - k` gains exactly `k` history rows, so the anchor follows its
    /// content.
    fn close_overline(&mut self, row: i32, col: i32) {
        if let Some((r0, c0)) = self.overline_start.take() {
            let (_rows, cols) = term_size(&self.term);
            let cols = cols as i32;
            let base = self.term.grid().history_size() as i64;
            if row == r0 {
                if col > c0 {
                    self.overline_ranges.push(OverlineRange {
                        abs: base + r0 as i64,
                        col_start: c0,
                        col_end: col,
                    });
                }
            } else if row > r0 {
                for r in r0..row {
                    let col_start = if r == r0 { c0 } else { 0 };
                    self.overline_ranges.push(OverlineRange {
                        abs: base + r as i64,
                        col_start,
                        col_end: cols,
                    });
                }
                if col > 0 {
                    self.overline_ranges.push(OverlineRange {
                        abs: base + row as i64,
                        col_start: 0,
                        col_end: col,
                    });
                }
            }
            // Hard cap: stale ranges from scrolling can accumulate.
            const MAX_RANGES: usize = 512;
            if self.overline_ranges.len() > MAX_RANGES {
                self.overline_ranges
                    .drain(..self.overline_ranges.len() - MAX_RANGES);
            }
        }
        self.overline_active = false;
    }

    /// Drop interceptor state (alt-screen switch, clear-screen, reflow).
    fn clear_overlines(&mut self) {
        self.overline_active = false;
        self.overline_start = None;
        self.overline_ranges.clear();
    }

    fn cap_raw(&mut self) {
        if self.raw.len() <= RAW_CAP {
            return;
        }
        let overflow = self.raw.len() - RAW_CAP;
        self.raw.drain(0..overflow);
        while let Some(&b) = self.raw.front() {
            self.raw.pop_front();
            if b == b'\n' {
                break;
            }
        }
    }

    pub(crate) fn reflow(&mut self, new_rows: u16, new_cols: u16) {
        resize_term(&mut self.term, new_rows, new_cols);
        self.prev.clear();
        self.rendered.clear();
        self.view_offset = 0;
        self.term.selection = None;
        self.clear_overlines();
    }

    fn rewrite_hvp(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            match self.csi_state {
                CsiState::Normal => {
                    if b == 0x1b {
                        self.csi_state = CsiState::Esc;
                    }
                    out.push(b);
                }
                CsiState::Esc => {
                    if b == b'[' {
                        self.csi_state = CsiState::Csi;
                    } else {
                        self.csi_state = if b == 0x1b {
                            CsiState::Esc
                        } else {
                            CsiState::Normal
                        };
                    }
                    out.push(b);
                }
                CsiState::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        out.push(if b == b'f' { b'H' } else { b });
                        self.csi_state = CsiState::Normal;
                    } else {
                        out.push(b);
                    }
                }
            }
        }
        out
    }

    /// Scrollback is read directly from alacritty's grid on demand.
    fn ensure_history(&mut self) {
        // no-op: alacritty maintains scrollback natively
    }

    /// Render the terminal grid for the current scrollback `view_offset`
    /// (0 = live).  Row-level caching avoids rebuilding spans for unchanged
    /// lines — huge win for tail / idle screens.
    pub(crate) fn render(&mut self) -> BuiltScreen {
        self.ensure_history();
        let (rows, cols) = term_size(&self.term);
        let (cur_row, cur_col) = cursor_pos(&self.term);
        let alt = is_alt(&self.term);

        // Ensure the cache matches the current grid size.
        self.rendered.resize(rows as usize, None);

        // --- Live view (also alt-screen): render the current grid -----------
        if alt || self.view_offset == 0 {
            let mut spans = Vec::with_capacity(rows as usize * 6);
            let mut displayed = Vec::with_capacity(rows as usize);
            let mut last_content = 0i32;
            for r in 0..rows {
                let (plain, runs, _wrapped) = build_row(&self.term, r, cols, &self.overline_ranges);
                let display = plain.trim_end().to_string();

                // Reuse cached spans when the plain text is identical.
                // Reuse cached runs when plain text is identical, only
                // re-running render_term_span to produce the final TermSpan
                // slice (which contains non-Send slint::Image references).
                // The cached runs are re-checked against the *current*
                // overline ranges: a row can be rewritten with identical
                // visible text but a new SGR-53 range (or a range pruned by
                // the cap), and stale flags must not stick.
                let line_spans: Vec<_> = if let Some(ref cached) = self.rendered[r as usize] {
                    if cached.plain_key == display {
                        let runs = refresh_overlines(
                            &cached.runs,
                            &self.overline_ranges,
                            &self.term,
                            r as i32,
                        );
                        runs.iter()
                            .flat_map(|hs| render_term_span(hs, r as i32, self.is_dark))
                            .collect()
                    } else {
                        self.build_spans(r as i32, &display, &runs, alt)
                    }
                } else {
                    self.build_spans(r as i32, &display, &runs, alt)
                };

                if !line_spans.is_empty() {
                    last_content = r as i32;
                }
                spans.extend(line_spans);
                displayed.push(display);
            }
            self.displayed_text = displayed;
            let rows_used = if alt { rows as i32 } else { last_content + 1 };
            return BuiltScreen {
                spans,
                cursor_row: cur_row as i32,
                cursor_col: cur_col as i32,
                rows_used,
                is_alt: alt,
                scroll_max: if alt {
                    0
                } else {
                    (self
                        .term
                        .total_lines()
                        .saturating_sub(self.term.screen_lines())) as i32
                },
                scroll_offset: 0,
            };
        }

        // --- Scrolled view: read directly from alacritty's native scrollback
        //     grid via negative Line indices.  No separate rendered history —
        //     build_line() lazily converts raw Cells as needed, and the
        //     is_clear() fast path skips empty rows entirely.
        let hist_len = self
            .term
            .total_lines()
            .saturating_sub(self.term.screen_lines());
        let win = rows as usize;
        let vo = self.view_offset;
        let mut spans = Vec::with_capacity(win * 6);
        let mut displayed = Vec::with_capacity(win);
        for d in 0..win {
            let grid_line = GridLine(d as i32 - vo as i32);
            let (plain, runs, _wrapped) =
                build_line(&self.term, grid_line, cols, &self.overline_ranges);
            let display = plain.trim_end().to_string();
            let hr =
                highlight_plain_output(runs, self.output_highlight, &self.custom_highlight_rules);
            for hs in &hr {
                spans.extend(render_term_span(hs, d as i32, self.is_dark));
            }
            displayed.push(display);
        }
        while displayed.len() < win {
            displayed.push(String::new());
        }
        self.displayed_text = displayed;
        BuiltScreen {
            spans,
            cursor_row: -1,
            cursor_col: 0,
            rows_used: win as i32,
            is_alt: false,
            scroll_max: hist_len as i32,
            scroll_offset: vo as i32,
        }
    }

    /// Build spans for one live row, update the render cache, and return them.
    fn build_spans(
        &mut self,
        row: i32,
        plain_key: &str,
        runs: &[HistSpan],
        alt: bool,
    ) -> Vec<TermSpan> {
        let runs = if alt {
            runs.to_vec()
        } else {
            highlight_plain_output(
                runs.to_vec(),
                self.output_highlight,
                &self.custom_highlight_rules,
            )
        };
        let spans: Vec<_> = runs
            .iter()
            .flat_map(|hs| render_term_span(hs, row, self.is_dark))
            .collect();
        self.rendered[row as usize] = Some(RenderedLine {
            plain_key: plain_key.to_string(),
            runs,
        });
        spans
    }
}

/// Scan `bytes` for every complete `ESC [ … final` CSI sequence, returning
/// their half-open ranges.  Only SGR (`m`) sequences are returned; other CSI
/// sequences are skipped over so the caller still feeds their bytes to the
/// parser in order.
///
/// Returns `(sequences, tail)` where `tail` is the offset of an *incomplete*
/// CSI sequence at the end of the slice (ESC seen, final byte not yet) — the
/// caller buffers `bytes[tail..]` and prepends it to the next chunk so
/// split reads (SSH/pipe) don't lose SGR 53 / 21.
fn scan_csi_sequences(bytes: &[u8]) -> (Vec<(usize, usize)>, Option<usize>) {
    let mut seqs = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        if bytes.get(i + 1) != Some(&b'[') {
            // ESC + non-'[': two-byte escape (ESC 7 / ESC c …) or an OSC
            // introducer (ESC ] …) — skip past the next byte and continue.
            i += 2;
            continue;
        }
        let mut j = i + 2;
        while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
            j += 1;
        }
        if j >= bytes.len() {
            return (seqs, Some(i)); // unterminated CSI at the tail
        }
        if bytes[j] == b'm' {
            seqs.push((i, j + 1));
        }
        i = j + 1; // skip the completed (SGR or not) sequence
    }
    (seqs, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{
        CsiState, OutputHighlightPreset, TermColor, UnderlineStyle, build_line, build_row,
        cell_attrs, new_term,
    };

    fn make_buffer() -> TermBuffer {
        let (term, processor) = new_term(10, 40, 100);
        TermBuffer {
            term,
            processor,
            find_query: String::new(),
            is_dark: true,
            output_highlight: OutputHighlightPreset::Off,
            custom_highlight_rules: Vec::new(),
            prev: Vec::new(),
            view_offset: 0,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
            csi_pending: Vec::new(),
            raw: std::collections::VecDeque::new(),
            rendered: Vec::new(),
            overline_active: false,
            overline_start: None,
            overline_ranges: Vec::new(),
            sgr_buf: Vec::new(),
            interactive_echo_until: std::time::Instant::now(),
            json_format_output: false,
        }
    }

    #[test]
    fn sgr53_records_overline_range() {
        let mut buf = make_buffer();
        buf.ingest(b"\x1b[53mOVERLINE\x1b[0m");
        assert_eq!(buf.overline_ranges.len(), 1);
        let r = buf.overline_ranges[0];
        assert_eq!((r.abs, r.col_start, r.col_end), (0, 0, 8));
        let (_plain, runs, _) = build_row(&buf.term, 0, 40, &buf.overline_ranges);
        // The row merges OVERLINE + trailing blanks; the overline clips to
        // the recorded range (cols 0..8) and the blanks stay clean.
        assert_eq!(runs.len(), 2);
        assert!(runs[0].overline);
        assert_eq!(runs[0].cells, 8);
        assert!(!runs[1].overline);
        // Plain text must survive: the empty SGR rewrite must NOT reset styles.
        let attr = cell_attrs(&buf.term, 0, 0);
        assert_eq!(attr.contents, "O");
    }

    #[test]
    fn sgr53_with_other_params_keeps_them() {
        let mut buf = make_buffer();
        // 31;53 → red + overline; the 31 must reach the parser.
        buf.ingest(b"\x1b[31;53mX");
        let attr = cell_attrs(&buf.term, 0, 0);
        assert_eq!(
            attr.fg,
            TermColor::Idx(1),
            "SGR 31 must survive the 53 rewrite"
        );
        assert_eq!(buf.overline_ranges.len(), 0, "range still open until reset");
        assert!(buf.overline_active);
    }

    #[test]
    fn sgr53_closed_by_sgr_without_53() {
        let mut buf = make_buffer();
        buf.ingest(b"\x1b[53mAA\x1b[31mBB");
        assert_eq!(buf.overline_ranges.len(), 1);
        assert_eq!(
            (
                buf.overline_ranges[0].col_start,
                buf.overline_ranges[0].col_end
            ),
            (0, 2)
        );
        let (_plain, runs, _) = build_row(&buf.term, 0, 40, &buf.overline_ranges);
        assert!(runs[0].overline, "AA is overlined");
        assert!(!runs[1].overline, "BB is not");
    }

    #[test]
    fn sgr53_split_across_chunks_survives() {
        // SSH / pipe reads split arbitrarily: `ESC [ 5` lands at the end of
        // one chunk and `3 m … ESC [ 0 m` in the next. The interceptor must
        // buffer the tail and act on the reassembled sequence.
        let mut buf = make_buffer();
        buf.ingest(b"\x1b[5");
        assert!(!buf.overline_active, "incomplete sequence must not act yet");
        assert!(
            !buf.sgr_buf.is_empty(),
            "unterminated CSI tail must be buffered"
        );
        buf.ingest(b"3mOVER\x1b[0m");
        assert_eq!(buf.overline_ranges.len(), 1);
        let r = buf.overline_ranges[0];
        assert_eq!((r.abs, r.col_start, r.col_end), (0, 0, 4));
        let (_plain, runs, _) = build_row(&buf.term, 0, 40, &buf.overline_ranges);
        assert!(
            runs[0].overline,
            "overline must survive a split SGR sequence"
        );
    }

    #[test]
    fn cache_hit_refreshes_overline_ranges() {
        // The render cache is keyed on plain text; rewriting a row with the
        // *same* visible text but a new SGR-53 range must refresh the flags
        // on a cache hit, otherwise the overline stays invisible.
        let mut buf = make_buffer();
        buf.ingest(b"abcdef");
        let first = buf.render();
        assert!(first.spans.iter().all(|s| !s.overline));

        buf.ingest(b"\r\x1b[0mabc\x1b[53mde\x1b[0m");
        let second = buf.render();
        let overlined: Vec<(i32, i32)> = second
            .spans
            .iter()
            .filter(|s| s.overline)
            .map(|s| (s.col, s.cells))
            .collect();
        assert_eq!(
            overlined,
            vec![(3, 2)],
            "cache hit must pick up the new range"
        );
    }

    #[test]
    fn overline_clips_merged_run_in_render_path() {
        // The [4]-style row: prefix text, then SGR 53 around the sample.
        // The merged run extends to the end of the line (trailing blanks);
        // the overline must clip to the range, not span the whole row.
        let mut buf = make_buffer();
        buf.ingest(b"abcdefghij\x1b[53mXX\x1b[0m");
        let (_plain, runs, _) = build_row(&buf.term, 0, 40, &buf.overline_ranges);
        let overlined: Vec<(&str, i32)> = runs
            .iter()
            .filter(|r| r.overline)
            .map(|r| (r.text.as_str(), r.cells))
            .collect();
        assert_eq!(
            overlined,
            vec![("XX", 2)],
            "only the SGR-53 range is overlined"
        );
        let total_cells: i32 = runs.iter().map(|r| r.cells).sum();
        assert_eq!(total_cells, 40, "split must keep the row's column coverage");
    }

    #[test]
    fn sgr21_rewrites_to_double_underline() {
        let mut buf = make_buffer();
        // vte 0.15 parses 21 as CancelBold; the interceptor rewrites it to
        // the 4:2 double-underline form alacritty understands.
        buf.ingest(b"\x1b[21mDUB");
        let attr = cell_attrs(&buf.term, 0, 0);
        assert_eq!(attr.underline, UnderlineStyle::Double);
    }

    #[test]
    fn clear_screen_drops_overline_state() {
        let mut buf = make_buffer();
        buf.ingest(b"\x1b[53mX\x1b[0m\x1b[2J\x1b[H");
        assert!(buf.overline_ranges.is_empty());
        assert!(!buf.overline_active);
    }

    #[test]
    fn multi_row_overline_splits_into_ranges() {
        let mut buf = make_buffer();
        // Overline across a wrap: 46 chars → row 0 (cols 0..40) + row 1 (cols 0..6).
        buf.ingest(b"\x1b[53mABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghij\x1b[0m");
        let rows: Vec<(i64, i32, i32)> = buf
            .overline_ranges
            .iter()
            .map(|r| (r.abs, r.col_start, r.col_end))
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], (0, 0, 40));
        assert_eq!(rows[1], (1, 0, 6));
    }

    #[test]
    fn end_to_end_render_keeps_overline_live_and_scrolled() {
        // Mimic catting the char-test file: 60 filler lines scroll the
        // screen, then an overline line lands near the bottom (live) and can
        // later be reached in scrollback.  Renders through the full
        // `render()` path (not just build_row/build_line).
        let (term, processor) = new_term(40, 100, 1000);
        let mut buf = TermBuffer {
            term,
            processor,
            find_query: String::new(),
            is_dark: true,
            output_highlight: OutputHighlightPreset::Off,
            custom_highlight_rules: Vec::new(),
            prev: Vec::new(),
            view_offset: 0,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
            csi_pending: Vec::new(),
            raw: std::collections::VecDeque::new(),
            rendered: Vec::new(),
            overline_active: false,
            overline_start: None,
            overline_ranges: Vec::new(),
            sgr_buf: Vec::new(),
            interactive_echo_until: std::time::Instant::now(),
            json_format_output: false,
        };
        let mut input = Vec::new();
        for i in 0..60 {
            input.extend_from_slice(format!("fill line {i:02}\r\n").as_bytes());
        }
        // The [4] Overline row: long prefix, then SGR 53 around the sample.
        input.extend_from_slice(
            "上划线 Overline                           -> \x1b[53m示例文本 Sample Text\x1b[0m\r\n"
                .as_bytes(),
        );
        buf.ingest(&input);

        // Live view: the overline row is the 61st content line on a 40-row
        // screen (row 38); wide CJK chars each form their own span, so match
        // on the first hanzi of the sample.
        let screen = buf.render();
        let live = screen
            .spans
            .iter()
            .find(|s| s.row == 38 && s.text.as_str().contains("示"))
            .expect("live screen must contain the overline row");
        assert!(live.overline, "overline must render in the live view");

        // Scrolled view (any non-zero offset renders via build_line): the
        // overline row still sits at display row 39 (grid line 38).
        buf.view_offset = 1;
        let screen = buf.render();
        let scrolled = screen
            .spans
            .iter()
            .find(|s| s.row == 39 && s.text.as_str().contains("示"))
            .expect("scrolled view must contain the overline row");
        assert!(
            scrolled.overline,
            "overline must render in the scrolled view"
        );
    }

    #[test]
    fn overline_survives_scrolling_into_scrollback() {
        // The real-world failure mode: catting the char-test file scrolls the
        // [4] section off-screen; when the user scrolls back up, the overline
        // must still render on the scrollback line.
        let mut buf = make_buffer(); // 10 rows × 40 cols
        // 20 lines × 40 cols on a 10-row screen → 10 rows scrolled out.
        // Line 6 (line-05) carries a 2-char overlined suffix.
        let mut input = Vec::new();
        for i in 0..20 {
            let body = format!("line-{i:02}{}", " ".repeat(31)); // 7 + 31 = 38 cols
            input.extend_from_slice(body.as_bytes());
            if i == 5 {
                input.extend_from_slice(b"\x1b[53mOV\x1b[0m"); // 2 cols → 40 total
            }
            input.extend_from_slice(b"\r\n");
        }
        buf.ingest(&input);
        // 20 lines + the trailing \r\n's empty line = 21 rows on a 10-row
        // screen → 11 rows scrolled into history.
        let history = buf.term.grid().history_size();
        assert_eq!(history, 11, "content should have scrolled by 11 rows");
        // line-05 now lives in scrollback: screen shows line-11..19 + blank,
        // so the 6th content line is at grid line -6.
        let (plain, runs, _) = build_line(&buf.term, GridLine(-6), 40, &buf.overline_ranges);
        assert_eq!(plain.trim_end(), "line-05                               OV");
        assert!(
            runs.iter().any(|r| r.overline),
            "overline must survive scrolling"
        );
        // And the same line rendered at its live position pre-scroll matches
        // the absolute anchor: a fresh identical line without overline must
        // NOT be flagged at the same relative row.
        let (_, runs2, _) = build_line(&buf.term, GridLine(-5), 40, &buf.overline_ranges);
        assert!(
            !runs2.iter().any(|r| r.overline),
            "neighbouring line must not be overlined"
        );
    }
}
#[cfg(test)]
mod real_file_overline_verify {
    use super::*;
    use crate::terminal::{CsiState, OutputHighlightPreset, build_line, new_term};
    use std::collections::VecDeque;

    #[test]
    fn real_chars_file_overline_renders_when_scrolled_back() {
        // The user's actual scenario: cat the 206-line char test file on a
        // 30x100 terminal, then scroll back to the [4] overline row.
        // Skip gracefully when the file isn't present (fresh checkouts).
        let Ok(data) = std::fs::read("../terminal_chars_test.txt") else {
            eprintln!("skipping: terminal_chars_test.txt not found");
            return;
        };
        let (term, processor) = new_term(30, 100, 5000);
        let mut buf = TermBuffer {
            term,
            processor,
            find_query: String::new(),
            is_dark: true,
            output_highlight: OutputHighlightPreset::Off,
            custom_highlight_rules: Vec::new(),
            prev: Vec::new(),
            view_offset: 0,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
            csi_pending: Vec::new(),
            raw: VecDeque::new(),
            rendered: Vec::new(),
            overline_active: false,
            overline_start: None,
            overline_ranges: Vec::new(),
            sgr_buf: Vec::new(),
            interactive_echo_until: std::time::Instant::now(),
            json_format_output: false,
        };
        // Feed in realistic 4 KiB chunks (cat / SSH behaviour).
        for chunk in data.chunks(4096) {
            buf.ingest(chunk);
        }
        eprintln!(
            "history={} ranges={:?}",
            buf.term.grid().history_size(),
            buf.overline_ranges
        );
        // Find the overline row: scan scrollback for the [4] Overline line.
        let hist = buf.term.grid().history_size();
        let mut found = None;
        for k in 0..hist {
            let line = GridLine(-(k as i32 + 1));
            let (plain, runs, _) = build_line(&buf.term, line, 100, &buf.overline_ranges);
            if plain.contains("示例文本") {
                found = Some((line.0, plain.clone(), runs.iter().any(|r| r.overline)));
                break;
            }
        }
        let (line_no, plain, overlined) = found.expect("overline row must be in scrollback");
        eprintln!("overline row at grid line {line_no}: {plain:?} overlined={overlined}");
        assert!(
            overlined,
            "overline must survive in the real-file scrollback scenario"
        );
    }
}

#[cfg(test)]
mod render_path_cube_tests {
    use super::*;
    use crate::terminal::{OutputHighlightPreset, TermColor, UnderlineStyle, build_row, new_term};

    fn make_buffer() -> TermBuffer {
        let (term, processor) = new_term(10, 40, 100);
        TermBuffer {
            term,
            processor,
            find_query: String::new(),
            is_dark: true,
            output_highlight: OutputHighlightPreset::Off,
            custom_highlight_rules: Vec::new(),
            prev: Vec::new(),
            view_offset: 0,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
            csi_pending: Vec::new(),
            raw: std::collections::VecDeque::new(),
            rendered: Vec::new(),
            overline_active: false,
            overline_start: None,
            overline_ranges: Vec::new(),
            sgr_buf: Vec::new(),
            interactive_echo_until: std::time::Instant::now(),
            json_format_output: false,
        }
    }

    /// The user's exact path: ingest through TermBuffer, render(), and check
    /// the produced TermSpan for cube index 53 — it must be magenta, not the
    /// theme background (transparent) and not black.
    #[test]
    fn render_path_cube_53_is_magenta() {
        let mut buf = make_buffer(); // 10 rows x 40 cols
        // Fill two rows so the cube row lands on the live screen (row 2).
        buf.ingest(b"line1\r\nline2\r\n");
        let mut input = Vec::new();
        for i in 52u8..=87 {
            input.extend_from_slice(format!("\x1b[48;5;{i}m \x1b[0m").as_bytes());
        }
        input.push(b'\r');
        buf.ingest(&input);

        let screen = buf.render();
        // Find the span at col 1 (second cube cell = index 53) with a space.
        let span53 = screen
            .spans
            .iter()
            .find(|s| s.col == 1 && s.text.as_str() == " ")
            .expect("53 swatch span must exist");
        eprintln!(
            "span53: bg=({},{},{}) alpha={}",
            span53.bg.red(),
            span53.bg.green(),
            span53.bg.blue(),
            span53.bg.alpha()
        );
        // Exact xterm value: 53 = (95,0,95) dark magenta, opaque.
        assert_eq!(
            (span53.bg.red(), span53.bg.green(), span53.bg.blue()),
            (95, 0, 95),
            "53 must be exact dark magenta via render()"
        );
        assert!(span53.bg.alpha() > 0, "53 must be opaque");
    }

    /// The root cause of the "cube 53 renders black" regression: the SGR
    /// interceptor used to treat the trailing `53` of `48;5;53m` as the
    /// overline parameter (SGR 53), dropping it and leaving the cell with a
    /// transparent background. 256-colour indices must never be intercepted.
    #[test]
    fn sgr_interceptor_leaves_256_colour_indices_alone() {
        // Background 48;5;53 must survive untouched.
        let mut buf = make_buffer();
        buf.ingest(b"\x1b[48;5;53mX\x1b[0m");
        let (_plain, runs, _) = build_row(&buf.term, 0, 40, &[]);
        assert!(
            matches!(runs[0].bg, TermColor::Idx(53)),
            "48;5;53 bg must be Idx(53)"
        );
        // Foreground 38;5;53 likewise.
        let mut buf = make_buffer();
        buf.ingest(b"\x1b[38;5;53mX\x1b[0m");
        let (_plain, runs, _) = build_row(&buf.term, 0, 40, &[]);
        assert!(
            matches!(runs[0].fg, TermColor::Idx(53)),
            "38;5;53 fg must be Idx(53)"
        );
        // 48;5;21 must NOT be rewritten into double underline (4:2).
        let mut buf = make_buffer();
        buf.ingest(b"\x1b[48;5;21mX\x1b[0m");
        let (_plain, runs, _) = build_row(&buf.term, 0, 40, &[]);
        assert!(
            matches!(runs[0].bg, TermColor::Idx(21)),
            "48;5;21 bg must be Idx(21)"
        );
        assert_eq!(
            runs[0].underline,
            UnderlineStyle::None,
            "no double underline from 256-colour 21"
        );
        // A *standalone* SGR 53 still opens the overline range.
        let mut buf = make_buffer();
        buf.ingest(b"\x1b[53mOVERLINE\x1b[0m");
        assert_eq!(buf.overline_ranges.len(), 1);
        assert_eq!(
            (
                buf.overline_ranges[0].col_start,
                buf.overline_ranges[0].col_end
            ),
            (0, 8)
        );
        // And the existing overline cells still render overlined.
        let (_plain, runs, _) = build_row(&buf.term, 0, 40, &buf.overline_ranges);
        assert!(
            runs[0].overline,
            "standalone 53m must still produce overline"
        );
    }
}
