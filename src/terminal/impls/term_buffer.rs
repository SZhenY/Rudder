use crate::terminal::{
    build_row, cursor_pos, detect_scroll, highlight_plain_output, is_alt,
    process_bytes, render_term_span, resize_term, term_size, BuiltScreen, CsiState, HistSpan,
    Line, MAX_HISTORY, RenderedLine, TermBuffer, RAW_CAP,
};
use crate::ui::TermMatch;
use crate::ui::TermSpan;

use alacritty_terminal::index::{Column, Line as GridLine, Point};
// Selection type used only in selection_rects_visible via term.selection

impl TermBuffer {
    /// Visible → grid Point (selection / mouse callbacks).
    fn vis_point(&self, vis_row: i32, vis_col: i32) -> Point {
        Point {
            line: GridLine(vis_row - self.view_offset as i32),
            column: Column(vis_col.max(0) as usize),
        }
    }

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

    /// Live screen rows for find / scrollback-aware operations.
    fn live_rows(&self) -> (Vec<Line>, usize) {
        let (rows, cols) = term_size(&self.term);
        let live: Vec<Line> = (0..rows)
            .map(|r| build_row(&self.term, r, cols))
            .collect();
        let used = live
            .iter()
            .rposition(|(_, runs, _)| !runs.is_empty())
            .map(|i| i + 1)
            .unwrap_or(0);
        (live, used)
    }

    /// Jump to the first matching row when the find query points outside the
    /// visible window (#233).
    pub(crate) fn scroll_to_first_find_match(&mut self, query: &str) -> bool {
        if query.is_empty() || is_alt(&self.term) {
            return false;
        }
        let q = query.to_lowercase();
        let (live, _) = self.live_rows();
        let rows = term_size(&self.term).0 as usize;
        let hist_len = self.history.len();
        let combined_len = hist_len + live.len();
        let Some(match_idx) = self
            .history
            .iter()
            .map(|line| &line.0)
            .chain(live.iter().map(|line| &line.0))
            .position(|line| line.to_lowercase().contains(&q))
        else {
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

    /// Feed bytes and capture scrolled-off lines into history.
    ///
    /// We detect scroll by diffing the screen before/after a `process`, which
    /// can only recover up to one screen of shift per call.  A single large
    /// burst can scroll many screens at once, so we split the input at newline
    /// boundaries into batches of at most ~half a screen of lines and capture
    /// after each — that way no batch ever scrolls more than the diff can see,
    /// and nothing is lost.  (Splitting only on `\n` is safe: VT escape
    /// sequences never contain a newline.)
    pub(crate) fn ingest(&mut self, input: &[u8]) {
        // Rewrite HVP (`ESC [ … f`) → CUP (`ESC [ … H`) so vt100 (which only
        // implements `H`) honours btop/htop's absolute cursor positioning.
        let bytes = self.rewrite_hvp(input);
        // Retain the (post-rewrite) stream, capped, so a resize can replay it at
        // the new width and reflow already-printed output (#169).
        self.raw.extend(bytes.iter().copied());
        // CSI 3 J means "erase saved lines". The vt100 crate clears its own
        // scrollback, but MeatShell maintains a separate rendered history and a
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
            self.history.clear();
            self.prev.clear();
            self.rendered.clear();
            self.view_offset = 0;
            self.term.selection = None;
        }
        self.cap_raw();
        self.ingest_chunk(&bytes);
    }

    /// Feed bytes to alacritty and detect scrolling by comparing visible
    /// lines before/after each chunk — the original reliable approach.
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
            self.history.clear();
            self.prev.clear();
            self.rendered.clear();
            return;
        }
        if is_fullscreen_refresh {
            self.view_offset = 0;
            self.history.clear();
            self.prev.clear();
            self.rendered.clear();
            return;
        }

        let curr: Vec<Line> = (0..rows).map(|r| build_row(&self.term, r, cols)).collect();
        if !self.prev.is_empty() {
            let k = detect_scroll(&self.prev, &curr);
            for line in self.prev.iter().take(k) {
                self.history.push_back(line.clone());
            }
            while self.history.len() > MAX_HISTORY {
                self.history.pop_front();
            }
        }
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
        self.history.clear();
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

    /// History is now populated eagerly by `ingest_chunk` via `detect_scroll` —
    /// the same reliable approach used in the original vt100-based code.
    fn ensure_history(&mut self) {
        // no-op: ingest_chunk already fills self.history
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
            let mut spans = Vec::new();
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
                scroll_max: if alt { 0 } else { self.history.len() as i32 },
                scroll_offset: 0,
            };
        }

        // --- Scrolled view: window into history ++ live content -------------
        let live: Vec<Line> = (0..rows).map(|r| build_row(&self.term, r, cols)).collect();
        let hist_len = self.history.len();
        let combined_len = hist_len + live.len();
        let win = rows as usize;
        let start = combined_len.saturating_sub(win + self.view_offset);
        let end = (start + win).min(combined_len);

        let mut spans = Vec::new();
        let mut displayed = Vec::with_capacity(win);
        for (d, idx) in (start..end).enumerate() {
            let line: &Line = if idx < hist_len {
                &self.history[idx]
            } else {
                &live[idx - hist_len]
            };
            let runs = highlight_plain_output(
                line.1.clone(),
                self.output_highlight,
                &self.custom_highlight_rules,
            );
            for hs in &runs {
                spans.extend(render_term_span(hs, d as i32, self.is_dark));
            }
            displayed.push(line.0.trim_end().to_string());
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
            scroll_max: self.history.len() as i32,
            scroll_offset: self.view_offset as i32,
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

    /// Invalidate all cached render rows (call after reset / reflow / ESC[3J).
    fn invalidate_render_cache(&mut self) {
        self.rendered.clear();
    }
}