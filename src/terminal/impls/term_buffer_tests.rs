#[cfg(test)]
mod tests {
    use super::super::*;

    /// 探针的分类必须与 `apply_sgr` 同源：扩展色的**分量**等于 53 / 21 不算 overline / 21。
    #[test]
    fn sgr_probe_matches_apply_sgr_classification() {
        let p = |b: &[u8]| sgr_probe(b);
        assert!(p(b"\x1b[53m").has_53);
        assert!(p(b"\x1b[0;53;1m").has_53);
        assert!(p(b"\x1b[21m").has_21);
        assert!(p(b"\x1b[1;21m").has_21);
        assert!(
            !p(b"\x1b[38;2;53;100;200m").has_53,
            "真彩分量 53 不是 overline"
        );
        assert!(!p(b"\x1b[38;5;53m").has_53, "256 色索引 53 不是 overline");
        assert!(
            !p(b"\x1b[48;2;21;21;21m").has_21,
            "背景色分量 21 不是 SGR 21"
        );
        assert!(!p(b"\x1b[58;5;21m").has_21, "下划线色分量 21 不是 SGR 21");
        assert!(!p(b"\x1b[4:2m").has_21, "4:2 本来就是双下划线，不需要改写");
        assert!(!p(b"plain text, no escapes").has_53);
        // 块尾半截 CSI → 交给调用方缓存（可能凑成 53 / 21）
        assert_eq!(p(b"abc\x1b[38;5").tail, Some(3));
        assert_eq!(
            p(b"abc\x1b").tail,
            Some(3),
            "块尾孤立 ESC 要留给下一块（可能是 53 的一半）"
        );
    }

    /// **逐字节分块也要拦住 SGR 53** —— 半截序列（`ESC` 在前一块、`[53m` 在后几块）
    /// 必须被完整地看到，否则 overline 区间会丢。
    ///
    /// 这条测试抓过一次真 bug：`sgr_probe` 认得"块尾孤立 ESC"，而 `scan_csi_sequences`
    /// 不认 —— 两个扫描器规则不一致时，`ESC` 被裸喂进解析器，后面的 `[0m` 就再也不是
    /// 完整序列，区间永远闭合不了（1 字节分块下实测 `active=true ranges=0`）。
    #[test]
    fn one_byte_chunks_still_intercept_overline() {
        let seq: &[u8] = b"\x1b[53mover\x1b[0m";
        let mut b = make_buffer();
        for byte in seq {
            b.ingest(std::slice::from_ref(byte));
        }
        assert!(!b.overline_active, "闭合的 SGR 必须让 overline 状态复位");
        assert_eq!(b.overline_ranges.len(), 1, "over 四格应记成一个区间");
        assert_eq!(
            (b.overline_ranges[0].col_start, b.overline_ranges[0].col_end),
            (0, 4)
        );
    }

    /// **分块喂与整块喂必须等价** —— 这是 0.7.9-beta2 那次闪退的同类边界。
    ///
    /// 语料刻意包含：SGR、OSC 引子、SGR 53、SGR 21、真彩分量里出现的 53/21、分号参数、
    /// CR/LF、TAB；按 1..=7 字节切块重喂，逐格比对 cell 属性 + overline 区间。
    #[test]
    fn chunked_ingest_matches_whole_chunk() {
        let corpus: &[u8] = b"a\x1b[31mred\x1b[0m b\x1b]0;title\x07c\x1b[53mover\x1b[0m d\r\n\
x1b[21munder\x1b[4:2m\x1b[38;2;53;100;200mtrue\x1b[0m tail";

        /// 快照"真正画出来的东西"：每段（行/列/格宽/下划线/overline/emoji/文本）+ overline 区间数。
        /// 用 `render()` 而不是逐格 `CellAttr`，是因为后者没有 `PartialEq`。
        fn snapshot(buf: &mut TermBuffer) -> (Vec<String>, usize) {
            let drawn = buf
                .render()
                .spans
                .iter()
                .map(|s| {
                    format!(
                        "{}:{}:{}:{:?}:{}:{}:{}",
                        s.row, s.col, s.cells, s.underline, s.overline, s.emoji, s.text
                    )
                })
                .collect();
            (drawn, buf.overline_ranges.len())
        }

        let whole = {
            let mut b = make_buffer();
            b.ingest(corpus);
            snapshot(&mut b)
        };
        assert!(
            whole.1 > 0,
            "语料里应至少有一个 53 区间（否则测试没测到东西）"
        );
        for step in 1..=7 {
            let mut b = make_buffer();
            for part in corpus.chunks(step) {
                b.ingest(part);
            }
            assert_eq!(
                snapshot(&mut b),
                whole,
                "按 {step} 字节分块喂必须与整块喂等价"
            );
        }
    }

    /// 块尾刚好落在转义序列中间时**不能 panic**。
    ///
    /// 0.7.9-beta2 在彩色刷屏下崩过（SIGABRT + `range start index 32769 out of range for
    /// slice of length 32768`）：`ESC` 后面跟的不是 `[` 时 `i += 2` 会跨过块尾，下一轮
    /// `&bytes[i..]` 越界。32768 正是读缓冲的块大小。
    #[test]
    fn scan_csi_survives_escape_at_chunk_tail() {
        assert_eq!(
            scan_csi_sequences(b"abc\x1b"),
            (vec![], Some(3)),
            "块尾孤立 ESC → 留给下一块"
        );
        assert_eq!(
            scan_csi_sequences(b"abc\x1b]"),
            (vec![], None),
            "ESC ]（OSC 引子）"
        );
        assert_eq!(
            scan_csi_sequences(b"abc\x1b7"),
            (vec![], None),
            "ESC 7（保存光标）"
        );
        assert_eq!(
            scan_csi_sequences(b"\x1b"),
            (vec![], Some(0)),
            "整块只有一个 ESC → 留给下一块"
        );

        let mut big = vec![b'a'; 32768];
        big.push(0x1b);
        assert_eq!(
            scan_csi_sequences(&big),
            (vec![], Some(32768)),
            "32 KiB 块尾是 ESC → 留给下一块"
        );

        let mut big2 = vec![b'a'; 32767];
        big2.extend_from_slice(b"\x1b]");
        assert_eq!(
            scan_csi_sequences(&big2),
            (vec![], None),
            "32 KiB 块尾是 ESC ]"
        );
    }

    /// 未结束的 CSI 仍要交给调用方缓存 —— 这是 split-read 时保住 SGR 53 / 21 的机制。
    #[test]
    fn scan_csi_reports_unterminated_csi_tail_and_finds_sgr() {
        assert_eq!(scan_csi_sequences(b"abc\x1b[38;5"), (vec![], Some(3)));
        assert_eq!(scan_csi_sequences(b"abc\x1b[31mX"), (vec![(3, 8)], None));
    }
    use crate::terminal::{
        CsiState, OutputHighlightPreset, TermColor, UnderlineStyle, attr_from_cell, build_line,
        build_row, new_term,
    };
    use alacritty_terminal::index::{Column, Line, Point};

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
            mouse_tracked: false,
            output_seq: 0,
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
    /// 真实参数：SSH 侧把相邻输出合并到 64 KiB 再交给 `ingest`（`term_buffers::OUTPUT_MERGE_BYTE_CAP`），
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
            std::hint::black_box(crate::app::resource_ui::apply_rows_slice(
                &model,
                &screen.spans,
            ));
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
        assert!(
            buf.scroll_to_find_match("marker-10", false),
            "继续向后应能找到更旧的"
        );
        assert!(buf.view_offset > first, "视口应朝更旧的方向移动");

        // 向前（更新）找一个更下面的标记 → 视口朝更新方向回来
        assert!(
            buf.scroll_to_find_match("marker-30", true),
            "向前应能找到更新的"
        );
        assert!(buf.view_offset < first, "视口应朝更新的方向移动");

        // 回绕：向前找最旧的一行（在视口上方）→ 必须绕到最旧处，而不是原地返回 false
        assert!(
            buf.scroll_to_find_match("marker-0", true),
            "向前找不到时应回绕到最旧的行"
        );

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
            mouse_tracked: false,
            output_seq: 0,
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
    use super::super::*;
    use crate::terminal::{CsiState, OutputHighlightPreset, build_line, new_term};

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
            mouse_tracked: false,
            output_seq: 0,
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
    use super::super::*;
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
            mouse_tracked: false,
            output_seq: 0,
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

    /// 终端查询（DSR/CPR/DA1）的回复内容 —— 这段逻辑原先**没有任何测试**，
    /// 而它有两条调用路径（`session_event` 丢弃回复、`session_runtime` 回写 PTY），
    /// 所以先把"回复长什么样"钉死。
    #[test]
    fn terminal_queries_answer_status_cursor_and_device_attributes() {
        let mut buf = make_buffer();
        assert_eq!(buf.detect_terminal_queries(b"\x1b[5n"), b"\x1b[0n".to_vec());
        assert_eq!(
            buf.detect_terminal_queries(b"\x1b[c"),
            b"\x1b[?1;2c".to_vec()
        );
        assert_eq!(
            buf.detect_terminal_queries(b"\x1b[0c"),
            b"\x1b[?1;2c".to_vec()
        );
        // 光标在 (1,1)：普通 CPR 与私有 CPR 各一份
        assert_eq!(
            buf.detect_terminal_queries(b"\x1b[6n"),
            b"\x1b[1;1R".to_vec()
        );
        assert_eq!(
            buf.detect_terminal_queries(b"\x1b[?6n"),
            b"\x1b[?1;1R".to_vec()
        );
    }

    #[test]
    fn terminal_queries_survive_split_chunks_and_ignore_other_csi() {
        let mut buf = make_buffer();
        // 半截 CSI 跨块续扫（csi_pending）
        assert!(buf.detect_terminal_queries(b"\x1b[").is_empty());
        assert_eq!(buf.detect_terminal_queries(b"6n"), b"\x1b[1;1R".to_vec());
        // 普通 SGR / 纯文本不产生回复
        assert!(buf.detect_terminal_queries(b"\x1b[31m").is_empty());
        assert!(buf.detect_terminal_queries(b"plain").is_empty());
        // 异常超长 CSI（>64 字节）被丢弃：不回复、也不越界
        let junk = format!("\x1b[{}n", "9".repeat(80));
        assert!(buf.detect_terminal_queries(junk.as_bytes()).is_empty());
    }

    /// MCP 的读屏入口：必须是纯读，且 output_seq 随 ingest 增长。
    #[test]
    fn snapshot_text_reads_screen_without_touching_view() {
        let mut buf = make_buffer();
        buf.ingest(b"hello\r\nworld\r\n");
        let before_offset = buf.view_offset;
        let snapshot = buf.snapshot_text(10, false);
        assert!(snapshot.text.contains("hello"), "{}", snapshot.text);
        assert!(snapshot.text.contains("world"), "{}", snapshot.text);
        assert_eq!(snapshot.output_seq, 14);
        assert_eq!(buf.view_offset, before_offset, "读屏不能改变滚动位置");
        assert!(!snapshot.truncated);
    }

    /// 软换行是终端宽度造成的，不是真换行 —— 拼回去，否则整词搜索永远匹配不上。
    #[test]
    fn snapshot_text_joins_soft_wrapped_lines() {
        let mut buf = make_buffer(); // 40 列
        let token = "x".repeat(100);
        buf.ingest(format!("head {token} tail\r\n").as_bytes());
        let snapshot = buf.snapshot_text(10, false);
        assert!(
            snapshot.text.contains(&token),
            "soft wrap split the logical line: {}",
            snapshot.text
        );
        assert!(snapshot.text.contains(&format!("head {token} tail")));
    }

    /// 回滚模式取的是"最后 N 行"，行数不足时不能报截断。
    #[test]
    fn snapshot_text_reports_truncation_only_when_rows_are_dropped() {
        let mut buf = make_buffer();
        for index in 0..80 {
            buf.ingest(format!("line {index}\r\n").as_bytes());
        }
        let short = buf.snapshot_text(5, true);
        assert!(short.truncated, "80 行历史应被截断到 5 行");
        assert!(short.text.contains("line 79"), "{}", short.text);
        let tall = buf.snapshot_text(400, true);
        assert!(!tall.truncated, "行数够时不该报截断");
    }
}
