use crate::terminal::{
    BuiltScreen, CsiState, FrameStats, HistSpan, MouseReport, OverlineRange, RenderedLine,
    ScrollLine, ScrollbackKey, TermBuffer, TextSnapshot, build_line, build_row, cursor_pos,
    highlight_plain_output, is_alt, merge_runs, process_bytes, refresh_overlines, render_term_span,
    resize_term, term_size,
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
    /// 输出序号（累计写入的字节数）。
    ///
    /// 等待类调用（`wait_for_output` / PTY 执行）先用它判断"有没有新内容"，
    /// 没变就不必生成整屏文本 —— 后者要逐行走 `build_line`，是这条路径的主要开销。
    pub(crate) fn output_seq(&self) -> u64 {
        self.output_seq
    }

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
            raw_tail: Vec::new(),
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
            output_seq: 0,
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
        self.raw_tail.clear();
    }

    /// Handle a local scrollback-navigation key (Home / End / PageUp / PageDown).
    ///
    /// 只有**已经从实时底部滚开**的普通屏才拥有这些键：返回 `true` 表示已本地消费，
    /// `false` 表示调用方应把原键照旧透传给 PTY —— 备用屏程序（less / vim / tmux…）
    /// 与实时终端仍然拥有它们（上游 35158dd）。一屏 = 当前屏幕行数。
    pub(crate) fn scrollback_navigate(&mut self, key: ScrollbackKey) -> bool {
        if self.view_offset == 0 || is_alt(&self.term) {
            return false;
        }

        let max_offset = self
            .term
            .total_lines()
            .saturating_sub(self.term.screen_lines());
        let page_rows = self.term.screen_lines().max(1);
        self.view_offset = match key {
            ScrollbackKey::Home => max_offset,
            ScrollbackKey::End => 0,
            ScrollbackKey::PageUp => self.view_offset.saturating_add(page_rows).min(max_offset),
            ScrollbackKey::PageDown => self.view_offset.saturating_sub(page_rows),
        };
        true
    }

    /// 可见区域（或含回滚）的纯文本快照。
    ///
    /// **只读**：既不调用 `render()`、也不改 `view_offset` / 行缓存 —— 因此可以
    /// 从任意线程在 `Mutex` 后面安全调用（MCP 的 `read_terminal` 就是这么用的）。
    /// 行文本来自 `build_line`，与滚动视图走同一条渲染路径，CJK 宽字符与组合符
    /// 的口径天然一致。
    pub(crate) fn snapshot_text(&self, max_rows: usize, include_scrollback: bool) -> TextSnapshot {
        let (rows, cols) = term_size(&self.term);
        let (cursor_row, cursor_col) = cursor_pos(&self.term);
        let total = self.term.total_lines();
        let screen = self.term.screen_lines();
        let limit = max_rows.clamp(1, 400);

        // 视口从 `Line(0)` 起（`topmost_line()` = -history）；回滚模式取最近 limit 行。
        let (first, count, mut truncated) = if include_scrollback {
            let take = limit.min(total);
            (self.term.topmost_line().0 + (total - take) as i32, take, total > take)
        } else {
            (0, screen, false)
        };

        let mut lines: Vec<(String, bool)> = (0..count)
            .map(|offset| {
                let (plain, _runs, wrapped) =
                    build_line(&self.term, GridLine(first + offset as i32), cols, &[]);
                (plain, wrapped)
            })
            .collect();
        // 视口下方通常是空行；回滚模式的末尾也可能是一片空白。两种模式都裁掉，
        // 否则 `max_rows` 会被这些空行占满（MCP 的 wait_for_output 会拿到整屏空格）。
        while lines
            .last()
            .is_some_and(|(line, _)| line.trim_end().is_empty())
        {
            lines.pop();
        }
        if lines.len() > limit {
            lines.drain(0..lines.len() - limit);
            truncated = true;
        }

        let mut text = String::with_capacity(lines.len() * (cols as usize + 1));
        for (line, wrapped) in lines {
            if wrapped {
                // 软换行是终端宽度造成的，不是真换行：续行直接拼回去。
                // 断开的话，AI 按整词搜索（`aarch64 GNU/Linux`）永远匹配不上。
                text.push_str(&line);
            } else {
                text.push_str(line.trim_end());
                text.push('\n');
            }
        }
        TextSnapshot {
            rows,
            cols,
            cursor_row: cursor_row as i32,
            cursor_col: cursor_col as i32,
            text,
            truncated,
            output_seq: self.output_seq,
        }
    }

    /// 用户**主动关标签页**时释放这块缓冲的内存大头。
    ///
    /// 回滚历史是每个标签页的内存大头（默认 5 000 行 ≈ 14 MB；开了大回滚可达 GB 级）。
    /// ⚠️ **不要**在意外断线（网络 / 远端挂掉）时调用它 —— 那时用户正要看断线前发生了什么；
    /// 释放只发生在主动关标签页（`tab_callbacks.rs` 的 pane-tab-closed）。
    ///
    /// `Grid::clear_history()` 恰好就是"丢历史、留屏幕"：alacritty 自己把历史行从存储里
    /// 摘掉并把 `display_offset` 归零。我们再清掉自己的行级缓存并把 `view_offset` 同步归零，
    /// 否则渲染还会按旧的滚回偏移去读已经不存在的历史。
    ///
    /// ⚠️ `scroll_cache` 存的是历史行快照：不失效会把旧着色贴到新内容上 —— `reset()` 的
    /// 注释里记过同样的坑。
    pub(crate) fn release_history_keep_screen(&mut self) {
        self.term.grid_mut().clear_history();
        self.bump_render_gen();
        self.drop_scroll_cache();
        self.displayed_text.clear();
        self.overline_ranges.clear();
        self.sgr_buf.clear();
        self.csi_pending.clear();
        self.view_offset = 0;
        self.term.selection = None;
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
        // 外部观察者（MCP 的 wait_for_output）用这个计数判断"之后有没有新输出"。
        self.output_seq = self.output_seq.saturating_add(input.len() as u64);
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
        // `CSI 3 J`，也不可能续上上一块的半截序列 —— 所以整段探测可以跳过。
        // `CSI 3 J` (erase saved lines) can be split across SSH reads (#319).
        // Only the last three bytes of the previous chunk can begin such a
        // sequence, so keep that tail instead of buffering the whole stream.
        let has_esc = memchr::memchr(b'\x1b', input).is_some();
        if has_esc || !self.raw_tail.is_empty() {
            let mut split = Vec::with_capacity(self.raw_tail.len() + 3);
            split.extend_from_slice(&self.raw_tail);
            split.extend_from_slice(&input[..input.len().min(3)]);
            let erase_saved = memchr::memmem::find(&split, b"\x1b[3J").is_some()
                || memchr::memmem::find(input, b"\x1b[3J").is_some();
            // Rolling tail: last three bytes of (previous tail + this chunk),
            // so a sequence split over three chunks is still detected.
            if input.len() >= 3 {
                self.raw_tail.clear();
                self.raw_tail.extend_from_slice(&input[input.len() - 3..]);
            } else {
                let mut tail = std::mem::take(&mut self.raw_tail);
                tail.extend_from_slice(input);
                if tail.len() > 3 {
                    tail.drain(..tail.len() - 3);
                }
                self.raw_tail = tail;
            }
            if erase_saved {
                self.raw_tail.clear();
                self.bump_render_gen();
                self.drop_scroll_cache();
                self.view_offset = 0;
                self.term.selection = None;
                self.clear_overlines();
            }
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
                memchr::memmem::find(bytes, b"\x1b[H").is_some(),
                memchr::memmem::find(bytes, b"\x1b[2J").is_some()
                    || memchr::memmem::find(bytes, b"\x1b[J").is_some(),
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
            self.route_chunk(bytes);
        } else {
            let mut combined = std::mem::take(&mut self.sgr_buf);
            combined.extend_from_slice(bytes);
            self.route_chunk(&combined);
        }
    }

    /// 单遍快路径的入口：**决定这一块要不要在 SGR 处停顿**。
    ///
    /// 停顿（切段 + 逐段 `advance`）只服务两件事：记录 SGR 53 的起止位置、把 SGR 21
    /// 改写成 `4:2`。这两样都很罕见，而彩色输出每 64 KiB 有两万多个 SGR —— 逐段重入
    /// `advance` 两万多次正是彩色路径的热点。所以：
    ///
    /// | 这一块的情况 | 走哪条路 |
    /// |---|---|
    /// | 含 `53` / 含 `21` / 有未闭合的 overline 区间 | 旧的切段路径（`ingest_segments_inner`） |
    /// | 块尾是半截 CSI | 尾部留到下一块再判；前面单遍喂掉 |
    /// | 其它（普通输出、彩色日志、`ls --color`…） | **整块一次 `advance`** |
    ///
    /// 判据用的是 [`sgr_probe`]，与 `apply_sgr` 的分类规则同源（有测试对照）。
    fn route_chunk(&mut self, bytes: &[u8]) {
        if memchr::memchr(b'\x1b', bytes).is_none() {
            // 没有 ESC：不可能有 53 / 21，也没有半截序列。
            process_bytes(&mut self.processor, &mut self.term, bytes);
            return;
        }
        let probe = sgr_probe(bytes);
        if probe.has_53 || probe.has_21 || self.overline_active {
            self.ingest_segments_inner(bytes);
        } else if let Some(t) = probe.tail {
            if t > 0 {
                process_bytes(&mut self.processor, &mut self.term, &bytes[..t]);
            }
            self.buffer_sgr_tail(&bytes[t..]);
        } else {
            process_bytes(&mut self.processor, &mut self.term, bytes);
        }
    }

    /// 块尾半截 CSI 的缓存上限，与 `csi_pending` 的 64 字节同量级。
///
/// 为什么必须有上限：合法参数段远短于此（最长的真彩色 `38;2;R;G;B` 也才 ~20 字节），
/// 而损坏或恶意输入若在 `ESC[` 之后一直不发 final byte，`sgr_buf` 会**无界增长**，
/// 并且每一块都被重新拼到下一次 ingest 前面（重复搬运）。超限就整段丢弃 —— 与
/// `csi_pending` 超 64 字节时清空的行为一致。
const SGR_TAIL_CAP: usize = 64;

/// 缓存块尾半截 CSI（`route_chunk` 与 `ingest_segments_inner` 两个出口共用）。
fn buffer_sgr_tail(&mut self, tail: &[u8]) {
    if tail.len() <= Self::SGR_TAIL_CAP {
        self.sgr_buf = tail.to_vec();
    } else {
        // 病态输入：不是合法 CSI 参数段，整段丢弃（与 `csi_pending` 超限时一致）。
        self.sgr_buf.clear();
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
                self.buffer_sgr_tail(&bytes[t..]);
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
            if crate::terminal::is_extended_color_prefix(part) {
                // 与 `sgr_probe` 共用同一条规则（见 csi.rs）
                skip = crate::terminal::extended_color_skip(split.get(i + 1).map(|v| &**v));
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
        if alt || self.view_offset == 0 {
            self.render_live_view(rows, cols, cur_row, cur_col, alt)
        } else {
            self.render_scrolled_view(rows, cols)
        }
    }

    /// Live view (also alt-screen): render the current grid row by row.
    fn render_live_view(
        &mut self,
        rows: u16,
        cols: u16,
        cur_row: u16,
        cur_col: u16,
        alt: bool,
    ) -> BuiltScreen {
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
                    if self.overline_ranges.is_empty() {
                        cached
                            .merged
                            .iter()
                            .flat_map(|hs| render_term_span(hs, r as i32, self.is_dark))
                            .collect()
                    } else {
                        merge_runs(&refresh_overlines(
                            &cached.runs,
                            &self.overline_ranges,
                            &self.term,
                            r as i32,
                        ))
                        .iter()
                        .flat_map(|hs| render_term_span(hs, r as i32, self.is_dark))
                        .collect()
                    }
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
        BuiltScreen {
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
        }
    }

    /// Scrolled view: read directly from alacritty's native scrollback grid.
    fn render_scrolled_view(&mut self, rows: u16, cols: u16) -> BuiltScreen {
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
            // 一次查找：命中后直接借用，不再二次索引。
            if let Some(cached) = self.scroll_cache.get(&line_no) {
                // 文本相同还不够：相对行号可能落到"文本一样、样式不同"的另一行上，必须比对
                // 高亮前的 runs；overlines 也要按**当前**区间重新切（同 live 路径的理由）。
                if cached.generation == generation
                    && cached.plain_key == display
                    && cached.raw_runs == runs
                {
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
        let merged = merge_runs(&runs);
        let spans: Vec<_> = merged
            .iter()
            .flat_map(|hs| render_term_span(hs, row, self.is_dark))
            .collect();
        self.rendered[row as usize] = Some(RenderedLine {
            plain_key: plain_key.to_string(),
            raw_runs,
            runs,
            merged,
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
/// `sgr_probe` 的结论：这一块里有没有需要停顿的 SGR，以及块尾有没有半截 CSI。
#[derive(Debug, Default, PartialEq, Eq)]
struct SgrProbe {
    /// 含 SGR 53（overline）—— 需要记录起止位置。
    has_53: bool,
    /// 含 SGR 21（要改写成 `4:2`，vte 把它解析成 CancelBold）。
    has_21: bool,
    /// 块尾半截 CSI 的起点：留到下一块（可能凑成 53 / 21）。
    tail: Option<usize>,
}

/// 判断一块字节里是否存在"必须在 SGR 处停顿"的序列。
///
/// 分类规则与 `apply_sgr` 一致：`38;5;N` / `38;2;R;G;B` / `48…` / `58…` 这些扩展色的
/// **分量**即便等于 53 / 21 也不算（`38;2;53;100;200m` 那个 53 是颜色，不是 overline）。
/// 这里不分配、不构造参数表，只用 memchr 跳到下一个 ESC。
fn sgr_probe(bytes: &[u8]) -> SgrProbe {
    let mut out = SgrProbe::default();
    // 骨架（memchr 跳 ESC → 扫到 final byte → 块尾半截）与 `scan_csi_sequences`
    // 共用 `csi::scan_csi_visit`：两份规则漂移过一次，收敛后结构上不再可能。
    // 分类只对 `m` 做，且不用参数表（零分配 —— 彩色刷屏时每块都会跑这里）。
    out.tail = crate::terminal::scan_csi_visit(bytes, |i, j| {
        if bytes[j] != b'm' {
            return;
        }
        let params = &bytes[i + 2..j];
        let mut skip = 0usize;
        let mut parts = params.split(|&b| b == b';');
        while let Some(part) = parts.next() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            if crate::terminal::is_extended_color_prefix(part) {
                // 与 `apply_sgr` 共用同一条规则（见 csi.rs）
                skip = crate::terminal::extended_color_skip(parts.clone().next());
                if skip > 0 {
                    continue;
                }
            }
            if part == b"53" {
                out.has_53 = true;
            } else if part == b"21" {
                out.has_21 = true;
            }
        }
    });
    out
}

fn scan_csi_sequences(bytes: &[u8]) -> (Vec<(usize, usize)>, Option<usize>) {
    let mut seqs = Vec::new();
    // 与 `sgr_probe` 共用骨架（含"没有 ESC 就一次 memchr 返回"的快路径与块尾规则）。
    let tail = crate::terminal::scan_csi_visit(bytes, |i, j| {
        if bytes[j] == b'm' {
            seqs.push((i, j + 1));
        }
    });
    (seqs, tail)
}
#[cfg(test)]
#[path = "term_buffer_tests.rs"]
mod term_buffer_tests_aggregate;
