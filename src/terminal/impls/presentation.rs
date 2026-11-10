use crate::terminal::{
    CompiledOutputRule, HistSpan, OutputHighlightPreset, TermColor, builtin_rules,
};
use crate::ui::TermSpan;
use std::cell::RefCell;
use std::collections::HashMap;




#[path = "presentation_highlight.rs"] mod highlight;
pub(crate) use highlight::*;
#[path = "presentation_emoji.rs"] mod emoji;
pub(crate) use emoji::*;
#[path = "presentation_runs.rs"] mod runs;
pub(crate) use runs::*;
#[path = "presentation_colors.rs"] mod colors;
pub(crate) use colors::*;

#[cfg(test)]
mod color_emoji_tests {
    use super::*;

    fn run(text: &str, cells: i32) -> HistSpan {
        HistSpan {
            text: text.to_string(),
            fg: TermColor::Default,
            bg: TermColor::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: Default::default(),
            hidden: false,
            strike: false,
            overline: false,
            inverse: false,
            col: 4,
            cells,
        }
    }

    #[test]
    fn replaces_emoji_without_changing_terminal_columns() {
        let spans = render_term_span(&run("A😀B", 4), 2, true);
        assert_eq!(spans.len(), 3);
        assert_eq!((spans[0].col, spans[0].cells), (4, 1));
        assert!(!spans[0].emoji);
        assert_eq!((spans[1].col, spans[1].cells), (5, 2));
        assert!(spans[1].emoji);
        assert_eq!((spans[2].col, spans[2].cells), (7, 1));
        assert!(!spans[2].emoji);
    }

    #[test]
    fn keeps_zwj_sequence_as_one_color_image() {
        let spans = render_term_span(&run("👨‍👩‍👧‍👦", 2), 0, true);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].emoji);
        assert_eq!(spans[0].cells, 2);
    }

    #[test]
    fn supports_common_composed_emoji_sequences() {
        for emoji in ["👍🏽", "🇨🇳", "👨‍💻", "❤️"] {
            let spans = render_term_span(&run(emoji, 2), 0, true);
            assert_eq!(spans.len(), 1, "unexpected split for {emoji}");
            assert!(spans[0].emoji, "missing color asset for {emoji}");
            assert_eq!(spans[0].cells, 2);
        }
    }

    #[test]
    fn respects_explicit_text_presentation_selector() {
        let spans = render_term_span(&run("♥\u{fe0e}", 1), 0, true);
        assert_eq!(spans.len(), 1);
        assert!(!spans[0].emoji);
        assert_eq!(spans[0].text.as_str(), "♥\u{fe0e}");
    }

    #[test]
    fn light_theme_lightens_dark_cube_backgrounds() {
        // 17 = (0,0,95) deep blue: on a light theme the cube background must
        // be lightened like true-colour RGB backgrounds, or it reads as a
        // near-black block (matches how [2] of the char test suite renders).
        let (r, g, b) = idx_to_rgb_bg(17, false);
        assert!(
            r > 100 && b > 100,
            "cube colour must be lightened on light theme: ({r},{g},{b})"
        );
        // Dark mode keeps the exact xterm cube value.
        assert_eq!(idx_to_rgb_bg(17, true), (0, 0, 95));
    }

    #[test]
    fn dark_theme_keeps_exact_xterm_cube_values() {
        // The standard 256-colour palette must render exactly in dark mode —
        // 53 = (95,0,95) dark magenta, exactly like PowerShell / Windows
        // Terminal. No lifting, no shifting (a lifted 53 visually collided
        // with cube 91 = (135,0,135)).
        let c17 = vt_bg_to_slint(TermColor::Idx(17), true); // (0,0,95)
        assert_eq!((c17.red(), c17.green(), c17.blue()), (0, 0, 95));

        let c53 = vt_bg_to_slint(TermColor::Idx(53), true); // (95,0,95)
        assert_eq!((c53.red(), c53.green(), c53.blue()), (95, 0, 95));

        let c58 = vt_bg_to_slint(TermColor::Idx(58), true); // (95,95,0)
        assert_eq!((c58.red(), c58.green(), c58.blue()), (95, 95, 0));

        let c91 = vt_bg_to_slint(TermColor::Idx(91), true); // (135,0,175)
        assert_eq!((c91.red(), c91.green(), c91.blue()), (135, 0, 175));

        let c16 = vt_bg_to_slint(TermColor::Idx(16), true); // (0,0,0)
        assert_eq!(
            (c16.red(), c16.green(), c16.blue()),
            (0, 0, 0),
            "true black stays black"
        );

        let g232 = vt_bg_to_slint(TermColor::Idx(232), true); // (8,8,8)
        assert_eq!((g232.red(), g232.green(), g232.blue()), (8, 8, 8));
    }

    /// P1：相邻、样式相同、且中文归属一致的 run 合并成一条 —— UI 侧的绘制项数因此降一到两个
    /// 数量级（终端是**逐单元格**产出 run 的，`render.rs::make_span`）。
    #[test]
    fn merge_runs_joins_adjacent_identical_cells_into_one_run() {
        let cells: Vec<HistSpan> = (0..40)
            .map(|i| {
                let mut hs = run("x", 1);
                hs.col = i;
                hs
            })
            .collect();
        let merged = merge_runs(&cells);
        assert_eq!(merged.len(), 1, "同一行同一样式的 40 个格子应合并成 1 条");
        assert_eq!(merged[0].text.len(), 40);
        assert_eq!(merged[0].col, 0);
        assert_eq!(merged[0].cells, 40);
    }

    /// 合并的三条边界：样式不同、中文/非中文不同、位置不连续 —— 都不能跨。
    #[test]
    fn merge_runs_stops_at_style_cjk_and_position_boundaries() {
        let mut a = run("a", 1);
        a.col = 0;

        let mut bold = run("b", 1);
        bold.col = 1;
        bold.bold = true;
        assert_eq!(merge_runs(&[a.clone(), bold]).len(), 2, "样式不同不合并");

        let mut cjk = run("中", 2);
        cjk.col = 1;
        assert_eq!(merge_runs(&[a.clone(), cjk]).len(), 2, "中文与非中文不合并（会选错字体）");

        let mut far = run("z", 1);
        far.col = 5;
        assert_eq!(merge_runs(&[a, far]).len(), 2, "位置不连续（中间是默认样式空白）不合并");
    }

    #[test]
    fn keeps_plain_text_grouped() {
        let spans = render_term_span(&run("plain text", 10), 0, true);
        assert_eq!(spans.len(), 1);
        assert!(!spans[0].emoji);
        assert_eq!(spans[0].text.as_str(), "plain text");
    }

    #[test]
    fn dim_and_hidden_fade_the_foreground() {
        let mut span = run("text", 4);
        span.dim = true;
        let spans = render_term_span(&span, 0, true);
        let alpha = spans[0].fg.alpha();
        assert!(
            alpha > 0 && alpha < 255,
            "SGR 2 dim → partially transparent fg (got {alpha})"
        );

        let mut span = run("text", 4);
        span.hidden = true;
        let spans = render_term_span(&span, 0, true);
        assert_eq!(
            spans[0].fg.alpha(),
            0,
            "SGR 8 conceal → fully transparent fg"
        );
    }

    #[test]
    fn style_flags_flow_into_term_spans() {
        let mut span = run("AB", 2);
        span.italic = true;
        span.strike = true;
        span.overline = true;
        span.underline = crate::terminal::UnderlineStyle::Double;
        let spans = render_term_span(&span, 3, true);
        assert!(spans[0].italic);
        assert!(spans[0].strike);
        assert!(spans[0].overline);
        assert_eq!(spans[0].underline, 2);
        assert_eq!(spans[0].row, 3);
    }

    /// flood 压测里 `cjk` 那行的原样语料：CJK + 全角字母 + 组合重音 + TAB + ZWJ 家庭 emoji。
    ///
    /// 钉住四件事：① ZWJ 家庭序列是**一张 2 格 emoji 图片**（不被拆成 4 个）；② 组合重音
    /// 留在基础字母**同一段文本**里（交给整形器，不要自己拆）；③ TAB 原样交给渲染器；
    /// ④ 各段列号首尾相接、合计格宽 = 终端列宽。
    ///
    /// 顺带记一笔**不是 bug 的观感问题**：`twemoji-assets` 17.x 把 `👨‍👩‍👧‍👦` 这类 ZWJ 家庭
    /// 序列画成了"蓝底白色一家四口"的**象形图标**（Unicode 已把家庭从 RGI 推荐里移除），
    /// 与 Apple 系统 emoji 字体的"四个人物"观感不同 —— 那是素材本身，不是渲染错误。
    #[test]
    fn cjk_corpus_line_keeps_grid_width_and_keeps_zwj_whole() {
        let line = "汉字宽字符 mixed ＡＢＣ ｅｍｏｊｉ 👨‍👩‍👧‍👦 e\u{301} tab\tend";
        let spans = render_term_span(&run(line, 60), 0, true);
        for sp in spans.iter() {
            println!(
                "  col={:<3} cells={:<3} emoji={:<5} cjk={:<5} text={:?}",
                sp.col, sp.cells, sp.emoji, sp.cjk, sp.text.as_str()
            );
        }
        assert_eq!(spans.len(), 3, "CJK 文本 / emoji 图片 / 其余文本");
        assert!(spans[0].cjk && !spans[0].emoji);
        assert!(spans[1].emoji, "ZWJ 家庭序列应走 emoji 图片通路");
        assert_eq!(spans[1].cells, 2, "2 格宽带");
        assert!(
            spans[2].text.as_str().contains("e\u{301}"),
            "组合重音留在基础字母同一段文本里"
        );
        assert!(spans[2].text.as_str().contains('\t'), "TAB 原样交给渲染器");
        let mut col = spans[0].col;
        for sp in spans.iter() {
            assert_eq!(sp.col, col, "span 列号必须首尾相接");
            col += sp.cells;
        }
        assert_eq!(col - spans[0].col, 60, "合计格宽 = 终端列宽");
    }

    #[test]
    fn emoji_row_columns_conserve_grid_width() {
        // [8] row 5 of the char test suite: 2 bars + 20 emoji × 2 cells = 42.
        let text = "|😀😃😄😁😆😅😂🤣😊😇😍😘🥰😗😙😚🙂🤗😜😝|";
        let spans = render_term_span(&run(text, 42), 0, true);
        assert_eq!(spans.len(), 22, "2 plain bars + 20 emoji images");
        let total: i32 = spans.iter().map(|s| s.cells).sum();
        assert_eq!(total, 42, "column width must match the terminal grid");
        // Columns must be contiguous with no gaps or overlaps.
        let mut col = spans[0].col;
        for span in &spans {
            assert_eq!(span.col, col, "span columns must line up");
            col += span.cells;
        }
        assert_eq!(col, spans[0].col + 42);
    }
}


#[cfg(test)]
mod real_file_cube_tests {
    use super::*;
    use crate::terminal::{TermColor, build_line, new_term, process_bytes, term_size};
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::Line as GridLine;

    /// End-to-end regression for the user-reported "cube index 53 renders
    /// black" issue: feeding the *whole* real char-test file, the 53 swatch
    /// (dark magenta) must come out as a lifted magenta — never the theme
    /// background, never black.
    #[test]
    fn real_file_cube_53_is_magenta() {
        // Embedded at compile time — see the note in `real_file_overline_verify`
        // (term_buffer.rs): a CWD-relative read here always failed, so this test
        // never actually ran.
        const DATA: &[u8] = include_bytes!("../../../tests/terminal_chars_test.txt");
        let (mut term, mut proc) = new_term(40, 100, 2000);
        process_bytes(&mut proc, &mut term, DATA);
        let (rows, cols) = term_size(&term);
        let mut found = false;
        // [2] lives in scrollback after the whole file is fed; scan every
        // grid line including history.
        let hist = term.grid().history_size() as i32;
        for line in (-hist..0).chain(0..rows as i32) {
            let (_plain, runs, _) = build_line(&term, GridLine(line), cols, &[]);
            for run in runs.iter() {
                if matches!(run.bg, TermColor::Idx(53)) {
                    found = true;
                    let (_fg, bg) = vt_span_colors(run.fg, run.bg, run.bold, run.inverse, true);
                    assert_eq!(
                        (bg.red(), bg.green(), bg.blue()),
                        (95, 0, 95),
                        "cube 53 must render exact dark magenta (95,0,95), got ({},{},{})",
                        bg.red(),
                        bg.green(),
                        bg.blue()
                    );
                }
            }
        }
        assert!(found, "no span with bg=Idx(53) found in the whole file");
    }
}
