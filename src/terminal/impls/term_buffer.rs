use crate::terminal::{
    build_line, build_row, cursor_pos, highlight_plain_output, is_alt,
    process_bytes, render_term_span, resize_term, term_size, BuiltScreen, CsiState, HistSpan,
    Line, RenderedLine, TermBuffer, RAW_CAP,
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
        let hist_len = self.term.total_lines().saturating_sub(self.term.screen_lines());
        let combined_len = hist_len + rows;

        // Search scrollback first (newest → oldest), then live rows.
        let find_idx = (0..hist_len)
            .rev()
            .map(|i| build_line(&self.term, GridLine(-(i as i32 + 1)), cols).0)
            .chain((0..rows).map(|r| build_row(&self.term, r as u16, cols).0))
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
                                        format!("\x1b[?{};{}R", point.line.0 + 1, point.column.0 + 1)
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

        process_bytes(&mut self.processor, &mut self.term, bytes);
        let (rows, cols) = term_size(&self.term);
        let alt = is_alt(&self.term);
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
            TermDamage::Full => {
                (0..rows).map(|r| build_row(&self.term, r, cols)).collect()
            }
            TermDamage::Partial(damaged_lines) => {
                let damaged_rows: Vec<usize> = damaged_lines
                    .map(|b| b.line.saturating_sub(display_offset))
                    .filter(|&r| r < rows as usize)
                    .collect();
                let mut cur = self.prev.clone();
                cur.resize(rows as usize, (String::new(), Vec::new(), false));
                for r in damaged_rows {
                    cur[r] = build_row(&self.term, r as u16, cols);
                }
                cur
            }
        };
        self.term.reset_damage();
        self.prev = curr;
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
                let (plain, runs, _wrapped) = build_row(&self.term, r, cols);
                let display = plain.trim_end().to_string();

                // Reuse cached spans when the plain text is identical.
                // Reuse cached runs when plain text is identical, only
                // re-running render_term_span to produce the final TermSpan
                // slice (which contains non-Send slint::Image references).
                let line_spans: Vec<_> = if let Some(ref cached) = self.rendered[r as usize] {
                    if cached.plain_key == display {
                        cached
                            .runs
                            .iter()
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
            let rows_used = if alt {
                rows as i32
            } else {
                last_content + 1
            };
            return BuiltScreen {
                spans,
                cursor_row: cur_row as i32,
                cursor_col: cur_col as i32,
                rows_used,
                is_alt: alt,
                scroll_max: if alt { 0 } else { (self.term.total_lines().saturating_sub(self.term.screen_lines())) as i32 },
                scroll_offset: 0,
            };
        }

        // --- Scrolled view: read directly from alacritty's native scrollback
        //     grid via negative Line indices.  No separate rendered history —
        //     build_line() lazily converts raw Cells as needed, and the
        //     is_clear() fast path skips empty rows entirely.
        let hist_len = self.term.total_lines().saturating_sub(self.term.screen_lines());
        let win = rows as usize;
        let vo = self.view_offset;
        let mut spans = Vec::with_capacity(win * 6);
        let mut displayed = Vec::with_capacity(win);
        for d in 0..win {
            let grid_line = GridLine(d as i32 - vo as i32);
            let (plain, runs, _wrapped) = build_line(&self.term, grid_line, cols);
            let display = plain.trim_end().to_string();
            let hr = highlight_plain_output(runs, self.output_highlight, &self.custom_highlight_rules);
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
            highlight_plain_output(runs.to_vec(), self.output_highlight, &self.custom_highlight_rules)
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