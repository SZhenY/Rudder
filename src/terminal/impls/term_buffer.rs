use crate::terminal::{
    BuiltScreen, CsiState, FrameStats, HistSpan, MouseReport, OverlineRange, RAW_CAP, RenderedLine,
    ScrollLine, TermBuffer, build_line, build_row, cursor_pos, highlight_plain_output, is_alt,
    merge_runs, process_bytes, refresh_overlines, render_term_span, resize_term, term_size,
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

/// Upper bound on cached scrollback lines. Roughly one screenful per entry, so
/// this keeps the cache in the low hundreds of KiB; exceeding it clears rather
/// than evicts, because the entries are immutable and cheap to rebuild.
const SCROLL_CACHE_MAX: usize = 4096;
/// 回到实时视图后，连续多少帧没有回滚就清空 `scroll_cache`（B1.3）。
/// 取 8 帧：终端只在有变化时重绘，实际约合 1–4 秒；上下滚动回看历史时会不断重置，
/// 因此不影响"回滚浏览"这个缓存本来的用途。
const SCROLL_LIVE_GRACE: u16 = 8;
// Selection type used only in selection_rects_visible via term.selection

impl TermBuffer {
    /// 新建一个终端缓冲区（给定尺寸与回滚上限，其余字段取运行时初值）。
    ///
    /// 生产侧（`session_callbacks` 开新标签页）与单测**共用这一个构造点**：原来那是一处
    /// 22 字段的结构体字面量，加字段必漏，测试也没法随手造 buffer（M3 补测试的前置）。
    /// 主题 / 高亮预设 / 自定义规则 / JSON 美化这几项由调用方在返回后覆盖。
    pub(crate) fn new(rows: u16, cols: u16, scrollback_lines: usize) -> Self {
        let (term, processor) = crate::terminal::new_term(rows, cols, scrollback_lines);
        Self {
            term,
            processor,
            find_query: String::new(),
            is_dark: false,
            output_highlight: crate::terminal::OutputHighlightPreset::from_settings(false, ""),
            custom_highlight_rules: Vec::new(),
            view_offset: 0,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
            csi_pending: Vec::new(),
            mouse_tracked: false,
            raw: std::collections::VecDeque::new(),
            frame_stats: Default::default(),
            rendered: Vec::new(),
            scroll_cache: std::collections::HashMap::new(),
            scroll_live_frames: 0,
            render_gen: 0,
            overline_active: false,
            overline_start: None,
            overline_ranges: Vec::new(),
            sgr_buf: Vec::new(),
            interactive_echo_until: std::time::Instant::now(),
            json_format_output: false,
        }
    }

    /// Reset to a fresh screen: new parser + cleared caches/selection/raw.
    pub(crate) fn reset(&mut self, scrollback_lines: usize) {
        let (rows, cols) = term_size(&self.term);
        (self.term, self.processor) = crate::terminal::new_term(rows, cols, scrollback_lines);
        // ⚠️ `scroll_cache` 里存的是**旧 term** 的行快照：行号相同、plain 文本相同的行
        // 会命中缓存，于是旧着色被贴到新内容上（`reflow()` 里为此专门清它，这里漏了）。
        // `sgr_buf` 里还可能是半截 SGR 序列，跨 term 复用同样会错。bump 一代让旧条目
        // 全部失效 —— 它同时会清 `rendered`（原来的那行删除）。
        self.bump_render_gen();
        self.drop_scroll_cache();
        self.sgr_buf.clear();
        self.displayed_text.clear();
        self.view_offset = 0;
        self.term.selection = None;
        self.raw.clear();
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
            .position(|line| Self::contains_ci(&q, &line));

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

    /// 大小写不敏感的子串查找。
    ///
    /// 纯 ASCII 时**完全不分配** —— `to_lowercase()` 在 50 万行的会话上就是 50 万次堆分配，
    /// 而查找导航本身已经够贵了。
    fn contains_ci(needle_lower: &str, haystack: &str) -> bool {
        if needle_lower.is_empty() {
            return true;
        }
        if haystack.is_ascii() && needle_lower.is_ascii() {
            let (h, n) = (haystack.as_bytes(), needle_lower.as_bytes());
            return h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n));
        }
        haystack.to_lowercase().contains(needle_lower)
    }

    /// Jump to the next (`forward`) or previous match of `query` measured
    /// from the current view top, wrapping around the whole scrollback+live
    /// range. Returns false when nothing matches or the view cannot move.
    /// Line granularity, like `scroll_to_first_find_match` (#find-nav).
    pub(crate) fn scroll_to_find_match(&mut self, query: &str, forward: bool) -> bool {
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
        if combined_len == 0 {
            return false;
        }
        // combined index i: i < hist_len → scrollback (oldest first);
        // i >= hist_len → live rows.
        let line_contains = |term: &crate::terminal::ATerm, i: usize| -> bool {
            let line = if i < hist_len {
                build_line(term, GridLine(i as i32 - hist_len as i32), cols, &[]).0
            } else {
                build_row(term, (i - hist_len) as u16, cols, &[]).0
            };
            Self::contains_ci(&q, &line)
        };
        // **朝搜索方向扫，命中即停**：以前是"把全部命中收集完再挑"，20 万~50 万行时那是一次
        // O(总行数 × 列数) 的卡顿。语义与旧实现等价（含 wrap 分支：前向回绕到最旧、后向回绕到最新）。
        let cur_top = hist_len.saturating_sub(self.view_offset);
        let last = combined_len - 1;
        let cur_top = cur_top.min(last);
        let target = if forward {
            (cur_top + 1..combined_len)
                .find(|&i| line_contains(&self.term, i))
                .or_else(|| (0..=cur_top).find(|&i| line_contains(&self.term, i)))
        } else {
            (0..cur_top)
                .rev()
                .find(|&i| line_contains(&self.term, i))
                .or_else(|| (cur_top..combined_len).rev().find(|&i| line_contains(&self.term, i)))
        };
        let Some(target) = target else {
            return false;
        };
        let clamped = target.min(combined_len.saturating_sub(rows));
        let new_offset = combined_len.saturating_sub(rows + clamped);
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
    /// Refresh the cached `mouse_tracked` flag from the parser's current mouse
    /// protocol state (the remote app enables it with e.g. `\x1b[?1000h` /
    /// `\x1b[?1002h`). Kept as a cached bool so the UI hot path doesn't need to
    /// re-derive it per frame (upstream d8eff40; alacritty variant).
    fn sync_mouse_tracked(&mut self) {
        self.mouse_tracked =
            super::vt_adapter::mouse_report(&self.term) != MouseReport::None;
    }

    pub(crate) fn ingest(&mut self, input: &[u8]) -> Vec<u8> {
        // Pretty-print + colour complete JSON lines before any other handling
        // so the grid, raw replay stream, and query scanner all see the same
        // bytes (#338).
        let formatted = self
            .json_format_output
            .then(|| crate::terminal::format_json_output(input));
        let input = formatted.as_deref().unwrap_or(input);
        let replies = self.detect_terminal_queries(input);
        // vte treats HVP (`ESC [ … f`) identically to CUP (`ESC [ … H`), so no
        // rewrite is needed — pass the stream through as-is.
        // 没有 ESC 的块占终端流量的绝大多数（普通日志、`seq` 输出…）：它既不可能含
        // `CSI 3 J`，也不可能续上上一块的半截序列 —— 所以"留存进 raw + 反向扫描"整段可以跳过。
        // （`raw` 只在检测 `CSI 3 J` 时被读；`!self.raw.is_empty()` 覆盖"上一块留了半截"的情况。）
        let has_esc = memchr::memchr(b'\x1b', input).is_some();
        if has_esc || !self.raw.is_empty() {
            // Retain the stream, capped, so a split `CSI 3 J` is still caught (#319).
            self.raw.extend(input.iter().copied());
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
            // ⚠️ 这里必须连 `scroll_cache` 一起失效（B1.1 的同类漏项）：缓存按**回滚行号**
            // 索引，而 `CSI 3 J` 把回滚整个清空、后面的行号全部前移 —— 旧条目会以"行号相同、
            // plain 文本相同"命中，把上一批内容的着色贴到新内容上。`bump_render_gen()`
            // 同时清 `rendered`（原本那行删除）。
            self.bump_render_gen();
            self.drop_scroll_cache();
                self.view_offset = 0;
                self.term.selection = None;
                self.clear_overlines();
            }
            self.cap_raw();
        }
        // 以前这里先 `input.to_vec()` 再解析 —— 一次没有任何作用的整块拷贝（流的重写不在这里做）。
        self.ingest_chunk(input);
        self.sync_mouse_tracked();
        replies
    }

    /// Scan input for DSR/CPR/DA1 terminal queries and build replies.  The
    /// CSI scanner survives split reads thanks to `csi_pending`.
    fn detect_terminal_queries(&mut self, input: &[u8]) -> Vec<u8> {
        // 没有 ESC、且上一块没留下未完成的 CSI ⇒ 不可能含查询序列。这是每块的第 2 遍全扫，
        // 用 memchr 分流比逐字节状态机快一个量级。
        if matches!(self.csi_state, CsiState::Normal) && memchr::memchr(b'\x1b', input).is_none() {
            return Vec::new();
        }
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
        // 这三处 `windows(..)` 全扫只为判断"整屏重绘"这个启发式 —— 没有 ESC 的块不可能命中。
        let (has_cursor_home, has_erase_display) = if memchr::memchr(b'\x1b', bytes).is_some() {
            (
                bytes.windows(3).any(|w| w == b"\x1b[H"),
                bytes.windows(4).any(|w| w == b"\x1b[2J")
                    || bytes.windows(3).any(|w| w == b"\x1b[J"),
            )
        } else {
            (false, false)
        };
        let is_fullscreen_refresh = has_cursor_home && has_erase_display;

        // Segment the stream at SGR sequences so the overline/double-underline
        // interceptor can read the exact cursor position where each SGR takes
        // effect (the grid is up to date for every segment we feed).
        self.ingest_segments(bytes);
        let alt = is_alt(&self.term);
        if alt || is_fullscreen_refresh {
            // Alt-screen switch and full-screen redraws rewrite the whole
            // grid — stale overline ranges no longer describe the content.
            self.clear_overlines();
        }
        if alt {
            self.view_offset = 0;
            self.rendered.clear();
            return;
        }
        if is_fullscreen_refresh {
            self.view_offset = 0;
            self.rendered.clear();
            return;
        }

        // Consume and reset alacritty's damage tracking. The actual line
        // rebuild happens lazily in render() (damage informs nothing there —
        // the per-line plain-text cache decides what to recompute), so
        // building rows here was pure waste: every rebuilt line was dropped
        // without ever being read (#prev-deadwork).
        drop(self.term.damage());
        self.term.reset_damage();
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

        // ── 快路径：参数里既没有 53（我们拦截的 overline）也没有 21（要改写成 4:2）──
        //
        // 此时下面那套"重建参数表"的结果与原文**逐字节相同**（只丢 53 / 只改 21），
        // 所以可以直接把原序列喂给解析器。彩色输出里每块（64 KiB）有两万多个 SGR，
        // 走慢路径要为每个序列付 4 次堆分配 + 一次整串重拷 —— 这是 ingest 在彩色
        // 语料上比无色慢 9 倍的主因。
        //
        // 唯一还必须做的是 overline 状态机：**任何不带 53 的 SGR** 在 overline 打开时
        // 都要把它闭合（与下面那段 `!has_53 && self.overline_active` 同义）。
        //
        // 注意：这里用朴素的"参数里有没有 21/53"判断 —— 极端情况下 `38;2;53;100;200m`
        // 这类**颜色分量**恰好是 21/53 会被判成"需要慢路径"，那是安全的（走下面已有的、
        // 处理过 extended-colour 前缀的正确逻辑），只是少见而已。
        let needs_rewrite = params.split(|&b| b == b';').any(|part| part == b"53" || part == b"21");
        if !needs_rewrite {
            if self.overline_active {
                self.close_overline(row, col);
            }
            process_bytes(&mut self.processor, &mut self.term, seq);
            return;
        }

        let mut has_53 = false;
        let mut has_reset = false;
        // Drop the overline parameter and rewrite 21 → 4:2 (vte parses 21 as
        // CancelBold; the xterm convention — and our char test suite — want
        // double underline).  Rebuilding the parameter list instead of
        // splicing in place guarantees a dropped tail parameter leaves no
        // trailing `;` behind (an empty SGR param would read as a reset).
        let mut parts: Vec<&[u8]> = Vec::with_capacity(4);
        let split: Vec<&[u8]> = params.split(|&b| b == b';').collect();
        // Parameters that belong to an extended-colour prefix (38/48/58) and
        // must NOT be interpreted as standalone SGR 21/53/reset codes. The
        // previous lookup only recognised `38;5;N`/`48;5;N` (#cube53); a
        // truecolour `38;2;R;G;B` with a component of exactly 21 or 53 (e.g.
        // `38;2;53;100;200m`) slipped through and was rewritten/dropped, and
        // `58;5;N` (underline colour) hit the same trap. A forward skip
        // counter handles every prefix form (`5` = 1 colour arg, `2` = 3).
        let mut skip = 0usize;
        for (i, part) in split.iter().enumerate() {
            if skip > 0 {
                skip -= 1;
                parts.push(part);
                continue;
            }
            if matches!(&**part, b"38" | b"48" | b"58") {
                match (split.get(i + 1).map(|v| &**v), split.get(i + 2).map(|v| &**v)) {
                    (Some(b"5"), _) => skip = 2, // 38;5;N
                    (Some(b"2"), _) => skip = 4, // 38;2;R;G;B
                    _ => {}
                }
            }
            let is_color_index = skip > 0;
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
        let was_scrolled = self.view_offset > 0;
        let old_offset = self.view_offset;
        resize_term(&mut self.term, new_rows, new_cols);
        self.rendered.clear();
        // A reflow rewraps every line: cached scrollback spans were built
        // against the old width and are meaningless now.
        self.drop_scroll_cache();
        self.render_gen = self.render_gen.wrapping_add(1);
        // Rows rewrap on resize, so the exact offset cannot be preserved —
        // but snapping to the bottom while the user was reading history was
        // jarring. Clamp the old offset into the new history so they stay
        // roughly where they were (#reflow-keep-offset).
        self.view_offset = if was_scrolled {
            old_offset.min(
                self.term
                    .grid()
                    .history_size()
                    .saturating_sub(new_rows as usize),
            )
        } else {
            0
        };
        self.term.selection = None;
        self.clear_overlines();
    }

    /// Invalidate everything derived from the current render settings.
    ///
    /// Called when something *outside* the terminal grid changes how a line
    /// looks (theme, highlight preset, custom rules). Without this the
    /// scrollback cache would keep serving spans coloured by the old settings.
    pub(crate) fn bump_render_gen(&mut self) {
        self.render_gen = self.render_gen.wrapping_add(1);
        self.rendered.clear();
    }

    /// 丢弃回滚渲染缓存，并把哈希表的**容量**一并交还。
    ///
    /// `clear()` 只 drop 条目（条目里的 `plain_key` / `runs` 堆随之释放 ✓），
    /// 但**桶数组按峰值常驻** ✗：4096 条目的表约 **0.5 MB / 标签页**（8192 桶 × 64 B），
    /// 且此后永不回收（全项目原先 0 处 `shrink_to_fit`）。
    /// 所以"确实要废弃缓存"的时机都走这里 —— 代价只是下次滚动回看时的一次摊还增长。
    ///
    /// ⚠️ `render()` 里 `SCROLL_CACHE_MAX` 那条路径**不能**用它：那里紧接着就要重新
    /// 填满，shrink 只会换来一次多余的重分配。
    pub(crate) fn drop_scroll_cache(&mut self) {
        self.scroll_cache.clear();
        self.scroll_cache.shrink_to_fit();
    }

    /// Render the terminal grid for the current scrollback `view_offset`
    /// (0 = live).  Row-level caching avoids rebuilding spans for unchanged
    /// lines — huge win for tail / idle screens.
    pub(crate) fn render(&mut self) -> BuiltScreen {
        let (rows, cols) = term_size(&self.term);
        let (cur_row, cur_col) = cursor_pos(&self.term);
        let alt = is_alt(&self.term);

        // Ensure the cache matches the current grid size.
        self.rendered.resize(rows as usize, None);

        // --- Live view (also alt-screen): render the current grid -----------
        if alt || self.view_offset == 0 {
            // 每帧重置；本分支结束时写回 self.frame_stats（见字段文档）。
            let mut stats = FrameStats::default();
            let mut spans = Vec::with_capacity(rows as usize * 6);
            let mut displayed = Vec::with_capacity(rows as usize);
            let mut last_content = 0i32;
            for r in 0..rows {
                let (plain, runs, _wrapped) = build_row(&self.term, r, cols, &self.overline_ranges);
                // Compare the cache against a borrowed slice first: allocating a
                // `String` for every row on every frame is pure waste when the
                // row is unchanged (24–50 rows × 10–30 fps). The owned copy is
                // only built where it is actually stored (`displayed_text`).
                let display_key = plain.trim_end();

                // Reuse cached spans when the plain text is identical.
                // Reuse cached runs when plain text is identical, only
                // re-running render_term_span to produce the final TermSpan
                // slice (which contains non-Send slint::Image references).
                // The cached runs are re-checked against the *current*
                // overline ranges: a row can be rewritten with identical
                // visible text but a new SGR-53 range (or a range pruned by
                // the cap), and stale flags must not stick.
                let line_spans: Vec<_> = if let Some(ref cached) = self.rendered[r as usize] {
                    // The plain-text key alone is not enough: a program can
                    // rewrite the row with identical text but different SGR
                    // attributes (highlight bars, colour changes) — comparing
                    // the freshly built runs too keeps such restyles visible
                    // (#cache-style-key).
                    if cached.plain_key == display_key
                        && cached.raw_runs == runs
                        && cached.highlighted != alt
                    {
                        stats.reused += 1;
                        let runs = merge_runs(&refresh_overlines(
                            &cached.runs,
                            &self.overline_ranges,
                            &self.term,
                            r as i32,
                        ));
                        runs.iter()
                            .flat_map(|hs| render_term_span(hs, r as i32, self.is_dark))
                            .collect()
                    } else {
                        stats.rebuilt += 1;
                        self.build_spans(r as i32, display_key, &runs, alt)
                    }
                } else {
                    stats.rebuilt += 1;
                    self.build_spans(r as i32, display_key, &runs, alt)
                };

                if !line_spans.is_empty() {
                    last_content = r as i32;
                }
                spans.extend(line_spans);
                displayed.push(display_key.to_string());
            }
            self.displayed_text = displayed;
            // B1.3：回到实时视图后行缓存不再被读取，连续几帧没有回滚就释放它。
            if self.scroll_cache.is_empty() {
                self.scroll_live_frames = 0;
            } else if self.scroll_live_frames < SCROLL_LIVE_GRACE {
                self.scroll_live_frames += 1;
            } else {
                // 这是**最常走**的一条废弃路径（每次滚动回看结束都命中一次），
                // 也是 0.5 MB 桶数组真正回到分配器的地方。
                self.drop_scroll_cache();
                self.render_gen = self.render_gen.wrapping_add(1);
                self.scroll_live_frames = 0;
            }
            let rows_used = if alt { rows as i32 } else { last_content + 1 };
            self.frame_stats = stats;
            return BuiltScreen {
                mouse_tracked: self.mouse_tracked,
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
        // Scrolling back does not change history content, so what a screen row
        // renders depends only on the absolute grid line it shows. Cache by
        // that line and the per-frame whole-viewport rebuild (22 highlight
        // regexes per row) collapses into a borrow: consecutive frames while
        // the user scrolls are otherwise byte-identical work.
        let generation = self.render_gen;
        // 回滚视图：重新计时（用户可能随时回到实时视图）
        self.scroll_live_frames = 0;
        for d in 0..win {
            let line_no = d as i32 - vo as i32;
            let grid_line = GridLine(line_no);
            let (plain, runs, _wrapped) =
                build_line(&self.term, grid_line, cols, &self.overline_ranges);
            let display = plain.trim_end();

            // `hit` resolves the immutable borrow before the mutable
            // `scroll_cache.insert` below, so the two can coexist here.
            let hit = self
                .scroll_cache
                .get(&line_no)
                .is_some_and(|c| c.generation == generation && c.plain_key == display);
            if hit {
                let cached = &self.scroll_cache[&line_no];
                // 文本相同还不够：相对行号可能落到"文本一样、样式不同"的另一行上，必须比对
                // 高亮前的 runs；overlines 也要按**当前**区间重新切（同 live 路径的理由）。
                if cached.raw_runs == runs {
                    let refreshed = refresh_overlines(
                        &cached.runs,
                        &self.overline_ranges,
                        &self.term,
                        line_no,
                    );
                    for hs in merge_runs(&refreshed) {
                        spans.extend(render_term_span(&hs, d as i32, self.is_dark));
                    }
                    displayed.push(display.to_string());
                    continue;
                }
            }

            let hr = highlight_plain_output(
                runs.clone(),
                self.output_highlight,
                &self.custom_highlight_rules,
            );
            for hs in merge_runs(&hr) {
                spans.extend(render_term_span(&hs, d as i32, self.is_dark));
            }
            // Bounded: an long scroll-back session would otherwise accumulate
            // one entry per history line ever shown. Clearing is cheap — the
            // lines are immutable and re-highlight within a single frame.
            // ⚠️ 这里**故意**不走 `drop_scroll_cache()`：紧接着的 insert 立刻要把表重新
            // 填满，shrink_to_fit 只会换来一次多余的重分配（清空 ≠ 废弃）。
            if self.scroll_cache.len() >= SCROLL_CACHE_MAX {
                self.scroll_cache.clear();
            }
            self.scroll_cache.insert(
                line_no,
                ScrollLine {
                    generation,
                    plain_key: display.to_string(),
                    raw_runs: runs,
                    runs: hr,
                },
            );
            displayed.push(display.to_string());
        }
        while displayed.len() < win {
            displayed.push(String::new());
        }
        self.displayed_text = displayed;
        BuiltScreen {
            mouse_tracked: self.mouse_tracked,
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
        // 高亮**前**那份留着做缓存键（见 `RenderedLine::raw_runs`）。
        let raw_runs = runs.to_vec();
        let runs = if alt {
            raw_runs.clone()
        } else {
            highlight_plain_output(
                raw_runs.clone(),
                self.output_highlight,
                &self.custom_highlight_rules,
            )
        };
        let spans: Vec<_> = merge_runs(&runs)
            .iter()
            .flat_map(|hs| render_term_span(hs, row, self.is_dark))
            .collect();
        self.rendered[row as usize] = Some(RenderedLine {
            plain_key: plain_key.to_string(),
            raw_runs,
            runs,
            highlighted: !alt,
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
    // 普通输出没有 ESC：一次 memchr 就够，省掉逐字节循环（`Vec::new()` 不分配）。
    if memchr::memchr(b'\x1b', bytes).is_none() {
        return (Vec::new(), None);
    }
    let mut seqs = Vec::new();
    let mut i = 0;
    // 用 memchr 直接跳到下一个 ESC。原来是一条条 `i += 1` 走完全块 —— 彩色输出里
    // 非 ESC 字节仍是绝大多数，那等于每个块都白扫一遍 64 KiB。语义与逐字节版一致。
    while let Some(offset) = memchr::memchr(b'\x1b', &bytes[i..]) {
        i += offset;
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
    use alacritty_terminal::index::{Column, Line, Point};
    use crate::terminal::{
        CsiState, OutputHighlightPreset, TermColor, UnderlineStyle, attr_from_cell,
        build_line, build_row, new_term,
    };

    fn cell_attr(term: &crate::terminal::ATerm, row: u16, col: u16) -> crate::terminal::CellAttr {
        attr_from_cell(&term.grid()[Point::new(Line(row as i32), Column(col as usize))])
    }

    fn make_buffer() -> TermBuffer {
        let (term, processor) = new_term(10, 40, 100);
        TermBuffer {
            term,
            processor,
            find_query: String::new(),
            is_dark: true,
            output_highlight: OutputHighlightPreset::Off,
            custom_highlight_rules: Vec::new(),
            view_offset: 0,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
            csi_pending: Vec::new(),
            raw: std::collections::VecDeque::new(),
            frame_stats: Default::default(),
            rendered: Vec::new(),
            scroll_cache: std::collections::HashMap::new(),
            scroll_live_frames: 0,
            render_gen: 0,
            overline_active: false,
            overline_start: None,
            overline_ranges: Vec::new(),
            sgr_buf: Vec::new(),
            interactive_echo_until: std::time::Instant::now(),
            json_format_output: false,
            mouse_tracked: false,
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
        let attr = cell_attr(&buf.term, 0, 0);
        assert_eq!(attr.contents, "O");
    }

    #[test]
    fn sgr53_with_other_params_keeps_them() {
        let mut buf = make_buffer();
        // 31;53 → red + overline; the 31 must reach the parser.
        buf.ingest(b"\x1b[31;53mX");
        let attr = cell_attr(&buf.term, 0, 0);
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
        let attr = cell_attr(&buf.term, 0, 0);
        assert_eq!(attr.underline, UnderlineStyle::Double);
    }

    /// `CSI 3 J`（清除回滚，例如 `clear -x` / `reset`）必须让**按回滚行号索引**的渲染
    /// 缓存一起失效：清空后行号整体前移，旧条目会以"行号相同 + plain 文本相同"命中，
    /// 把上一批内容的着色贴到新内容上（与 B1.1 同一类漏项，这里锁定它）。
    /// **离线压测（默认忽略）**：把"刷屏"这条热路径按真实参数跑出来，分别报告
    /// **ingest（每 64 KiB 块）** 与 **render（每块之后一帧）** 的时间分布 —— 用来定位
    /// "忽快忽慢"到底是解析侧的长尾，还是渲染侧的。
    ///
    /// 真实参数：SSH 侧把相邻输出合并到 64 KiB 再交给 `ingest`（`app.rs::OUTPUT_MERGE_BYTE_CAP`），
    /// 每块之后 UI 侧渲染一帧。
    ///
    ///     cargo test --release -- --ignored --nocapture flood_profile
    ///
    /// 火焰图（直接对着测试二进制跑最省事）：
    ///
    ///     cargo test --release --no-run
    ///     samply record ./target/release/deps/rudder-<hash> --ignored --nocapture flood_profile
    #[test]
    #[ignore = "压测：用 --ignored 显式运行"]
    fn flood_profile_reports_ingest_and_render_timings() {
        use std::time::Instant;

        const CHUNK_LINES: usize = 1_400; // ≈ 64 KiB（每行 46 字节）
        // 采样/调参用：默认 ≈30 万行，环境变量可放大（例如采样时拉长到几秒）。
        let chunks: usize = std::env::var("RUDDER_FLOOD_CHUNKS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(220);
        // 语料两档：默认无色（每行 1 个 run），`RUDDER_FLOOD_COLOR=1` 时给每行塞 8 段
        // 不同颜色 —— 后者才代表 `ls --color`、彩色日志、`git diff` 这类"**每行多 run、
        // 一屏上千个 span**"的真实负载；模型写入那一段的代价只有在它上面才看得出。
        let colorful = std::env::var("RUDDER_FLOOD_COLOR").is_ok();
        let plain_line = b"line 123 abcdefghijklmnopqrstuvwxyz 0123456789\r\n";
        let color_line =
            b"\x1b[31mred\x1b[0m \x1b[32mgreen\x1b[0m \x1b[33myellow\x1b[0m \x1b[34mblue\x1b[0m \
              \x1b[35mmagenta\x1b[0m \x1b[36mcyan\x1b[0m \x1b[1;37mbold\x1b[0m \x1b[4munder\x1b[0m\r\n";
        let line: &[u8] = if colorful { color_line } else { plain_line };

        // 回滚环大小可调：用来判断"长尾是不是回滚环淘汰造成的"（0 = 不留历史）。
        let scrollback: usize = std::env::var("RUDDER_FLOOD_SCROLLBACK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5_000);
        let mut buf = TermBuffer::new(24, 80, scrollback);
        let mut chunk = Vec::with_capacity(line.len() * CHUNK_LINES);
        for _ in 0..CHUNK_LINES {
            chunk.extend_from_slice(line);
        }

        let mut ingest_us = Vec::with_capacity(chunks);
        let mut render_us = Vec::with_capacity(chunks);
        // 真实 UI 每帧还要把这份 spans **增量写进 Slint 模型**（`apply_rows_slice` 逐项
        // `PartialEq` + `set_row_data`）—— 这段以前没被量过，而"一屏上千个 span"的固定开销
        // 就落在这里，也是 alacritty 用 GPU 实例缓冲换掉的那一块。
        let mut model_us = Vec::with_capacity(chunks);
        let model = slint::VecModel::<crate::ui::TermSpan>::default();
        let start = Instant::now();
        for _ in 0..chunks {
            let t = Instant::now();
            buf.ingest(&chunk);
            ingest_us.push(t.elapsed().as_micros());

            let t = Instant::now();
            let screen = buf.render();
            render_us.push(t.elapsed().as_micros());
            std::hint::black_box(&screen);

            let t = Instant::now();
            std::hint::black_box(crate::app::resource_ui::apply_rows_slice(&model, &screen.spans));
            model_us.push(t.elapsed().as_micros());
        }
        let total = start.elapsed();

        // ── 回滚视图（`scroll_cache` 那条 path）：进入历史后连渲若干帧 ──────────────
        // 与实时视图完全不同的分支：每帧 `for d in 0..win` 重走视口，靠 `scroll_cache`
        // （按绝对行号 + generation，上限 4096）兜。**第一帧必然全部未命中**，后续帧才是
        // 稳态 —— 两个数分开记，因为它们对应"滚一下"和"一直滚"两种体验。
        const SCROLL_FRAMES: usize = 60;
        buf.view_offset = 200;
        let mut scroll_us = Vec::with_capacity(SCROLL_FRAMES);
        let scroll_start = Instant::now();
        for _ in 0..SCROLL_FRAMES {
            let t = Instant::now();
            let s = buf.render();
            scroll_us.push(t.elapsed().as_micros());
            std::hint::black_box(&s);
        }
        let scroll_total = scroll_start.elapsed();
        let scroll_first_us = scroll_us.first().copied().unwrap_or(0);

        // 最慢的几次发生在第几块 / 第几帧 —— 周期性长尾（例如每 N 次一次）能一眼看出来。
        let slowest = |v: &[u128], n: usize| {
            let mut idx: Vec<usize> = (0..v.len()).collect();
            idx.sort_by_key(|&i| std::cmp::Reverse(v[i]));
            idx.into_iter()
                .take(n)
                .map(|i| format!("#{i}={}us", v[i]))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let stats = |v: &mut Vec<u128>| {
            v.sort_unstable();
            let at = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
            (at(0.5), at(0.95), at(0.99), v[v.len() - 1])
        };
        // 先算"最慢的几次"（需要原始顺序），再排序求分位。
        let i_slow = slowest(&ingest_us, 5);
        let r_slow = slowest(&render_us, 5);
        let m_slow = slowest(&model_us, 5);
        let (i50, i95, i99, imax) = stats(&mut ingest_us);
        let (r50, r95, r99, rmax) = stats(&mut render_us);
        let (s50, s95, s99, smax) = stats(&mut scroll_us);
        let (m50, m95, m99, mmax) = stats(&mut model_us);
        println!("最慢 ingest 块: {i_slow}");
        println!("最慢 render 帧: {r_slow}");
        println!("最慢 model 帧: {m_slow}");
        println!("== flood profile ==");
        println!(
            "总耗时 {:?}（{chunks} 块 × {CHUNK_LINES} 行 ≈ {} 万行）",
            total,
            chunks * CHUNK_LINES / 10_000
        );
        println!("ingest / 块: p50={i50}us p95={i95}us p99={i99}us max={imax}us");
        println!("render / 帧: p50={r50}us p95={r95}us p99={r99}us max={rmax}us");
        println!("model / 帧: p50={m50}us p95={m95}us p99={m99}us max={mmax}us");
        println!(
            "回滚 / 帧: 首帧={scroll_first_us}us（未命中） p50={s50}us p95={s95}us p99={s99}us max={smax}us \
             （{SCROLL_FRAMES} 帧共 {scroll_total:?}）"
        );
    }

    /// 查找导航：**只朝搜索方向扫、命中即停**（以前会把全部命中收集完再挑 —— 20 万~50 万行
    /// 时那是一次 O(总行数 × 列数) 的卡顿）。这里把方向、回绕与"找不到不动"钉住。
    #[test]
    fn find_navigation_moves_in_the_requested_direction_and_wraps() {
        let mut buf = make_buffer();
        for i in 0..40 {
            buf.ingest(format!("line {i} marker-{i}\r\n").as_bytes());
        }

        // 向后（更旧）找 → 离开实时底部，且越找越旧
        assert!(buf.scroll_to_find_match("marker-20", false), "向后应能找到");
        let first = buf.view_offset;
        assert!(first > 0, "应当离开实时底部");
        assert!(buf.scroll_to_find_match("marker-10", false), "继续向后应能找到更旧的");
        assert!(buf.view_offset > first, "视口应朝更旧的方向移动");

        // 向前（更新）找一个更下面的标记 → 视口朝更新方向回来
        assert!(buf.scroll_to_find_match("marker-30", true), "向前应能找到更新的");
        assert!(buf.view_offset < first, "视口应朝更新的方向移动");

        // 回绕：向前找最旧的一行（在视口上方）→ 必须绕到最旧处，而不是原地返回 false
        assert!(buf.scroll_to_find_match("marker-0", true), "向前找不到时应回绕到最旧的行");

        // 找不到 → false，且视口不动
        let untouched = buf.view_offset;
        assert!(!buf.scroll_to_find_match("no-such-marker", true));
        assert_eq!(buf.view_offset, untouched, "找不到时视口不应移动");
    }

    /// 首个命中：能跳进历史，找不到时不动。
    #[test]
    fn first_find_match_jumps_into_history() {
        let mut buf = make_buffer();
        for i in 0..40 {
            buf.ingest(format!("line {i} marker-{i}\r\n").as_bytes());
        }
        assert!(buf.scroll_to_first_find_match("marker-5"));
        assert!(buf.view_offset > 0, "应跳到历史里的匹配行");
        assert!(!buf.scroll_to_first_find_match("no-such-marker"));
    }

    /// `contains_ci`：大小写不敏感；ASCII 走不分配的路径，非 ASCII 回落到 `to_lowercase`。
    #[test]
    fn contains_ci_matches_case_insensitively() {
        assert!(TermBuffer::contains_ci("marker", "LINE 3 MARKER-3"));
        assert!(TermBuffer::contains_ci("marker", "line 3 marker-3"));
        assert!(!TermBuffer::contains_ci("marker", "line 3"));
        assert!(TermBuffer::contains_ci("中文", "含中文的行"));
        assert!(TermBuffer::contains_ci("", "任意"));
    }

    #[test]
    fn erase_saved_lines_invalidates_the_scroll_cache() {
        let mut buf = make_buffer();
        for i in 0..40 {
            buf.ingest(format!("line {i}\r\n").as_bytes());
        }
        buf.view_offset = 10; // 进入回滚视图
        let _ = buf.render(); // 填充 scroll_cache
        assert!(!buf.scroll_cache.is_empty(), "回滚渲染应填充缓存");
        let gen_before = buf.render_gen;

        buf.ingest(b"\x1b[3J"); // erase saved lines

        assert!(buf.scroll_cache.is_empty(), "CSI 3J 后回滚缓存必须清空");
        // B1.9（原 N1）：不只是条目 —— 哈希表的**容量**也要交还。`clear()` 会保留桶数组
        // （4096 条目的表约 0.5 MB / 标签页），`shrink_to_fit` 才是真正释放的那一半。
        assert_eq!(
            buf.scroll_cache.capacity(),
            0,
            "清空回滚缓存后，桶数组容量必须一并归还"
        );
        assert_ne!(buf.render_gen, gen_before, "渲染代号必须自增");
        assert_eq!(buf.view_offset, 0, "回到实时视图");
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
            view_offset: 0,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
            csi_pending: Vec::new(),
            raw: std::collections::VecDeque::new(),
            frame_stats: Default::default(),
            rendered: Vec::new(),
            scroll_cache: std::collections::HashMap::new(),
            scroll_live_frames: 0,
            render_gen: 0,
            overline_active: false,
            overline_start: None,
            overline_ranges: Vec::new(),
            sgr_buf: Vec::new(),
            interactive_echo_until: std::time::Instant::now(),
            json_format_output: false,
            mouse_tracked: false,
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
        //
        // The fixture is embedded at compile time so the test cannot silently
        // skip. It used to be `fs::read("../terminal_chars_test.txt")`, which
        // resolves against the *process* CWD — cargo runs tests with the CWD set
        // to the package root, i.e. one level above the repo's `tests/` dir — so
        // the read never succeeded and the test returned without asserting
        // anything. `ppk.rs` embeds its fixtures the same way.
        const DATA: &[u8] = include_bytes!("../../../tests/terminal_chars_test.txt");
        let (term, processor) = new_term(30, 100, 5000);
        let mut buf = TermBuffer {
            term,
            processor,
            find_query: String::new(),
            is_dark: true,
            output_highlight: OutputHighlightPreset::Off,
            custom_highlight_rules: Vec::new(),
            view_offset: 0,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
            csi_pending: Vec::new(),
            raw: VecDeque::new(),
            frame_stats: Default::default(),
            rendered: Vec::new(),
            scroll_cache: std::collections::HashMap::new(),
            scroll_live_frames: 0,
            render_gen: 0,
            overline_active: false,
            overline_start: None,
            overline_ranges: Vec::new(),
            sgr_buf: Vec::new(),
            interactive_echo_until: std::time::Instant::now(),
            json_format_output: false,
            mouse_tracked: false,
        };
        // Feed in realistic 4 KiB chunks (cat / SSH behaviour).
        for chunk in DATA.chunks(4096) {
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
            view_offset: 0,
            displayed_text: Vec::new(),
            csi_state: CsiState::Normal,
            csi_pending: Vec::new(),
            raw: std::collections::VecDeque::new(),
            frame_stats: Default::default(),
            rendered: Vec::new(),
            scroll_cache: std::collections::HashMap::new(),
            scroll_live_frames: 0,
            render_gen: 0,
            overline_active: false,
            overline_start: None,
            overline_ranges: Vec::new(),
            sgr_buf: Vec::new(),
            interactive_echo_until: std::time::Instant::now(),
            json_format_output: false,
            mouse_tracked: false,
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
