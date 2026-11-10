use super::*;

/// Apply DIM (faint, SGR 2) and HIDDEN (conceal, SGR 8) to a resolved
/// foreground colour: faint text is drawn at reduced opacity, concealed text
/// at full transparency (its background fill still shows).
///
/// DIM is expressed as a family switch: embedded JetBrains Mono uses the
/// "JetBrains Mono ExtraLight" family, other fonts use "<family> Thin"
/// (see terminal_view.slint). The alpha here is a gentle 0.85 rather than a
/// heavy 0.55 — heavy translucency made faint text look washed out.
pub(crate) fn vt_apply_attr_alpha(color: slint::Color, dim: bool, hidden: bool) -> slint::Color {
    if hidden {
        color.with_alpha(0.0)
    } else if dim {
        color.with_alpha(0.85)
    } else {
        color
    }
}

/// 把相邻、样式完全相同、且"中文归属"一致的 run 合并成一条。
///
/// **为什么要合并**：`render_term_span` 是按 run 逐条产出 `TermSpan` 的，而每个 `TermSpan`
/// 在 UI 侧就是「一个 `Rectangle` + 一个 `Text`」—— 终端现在**逐单元格**产出 run
/// （`render.rs::make_span`），一屏因此上千个绘制项。同一句话里绝大多数相邻格子长得一模一样，
/// 合并后只剩一条，项数降一到两个数量级。
///
/// **两条不能跨的边界**：
/// * **中文 / 非中文**：`render_term_span` 用 `contains_cjk(整条文本)` 决定用哪套字体，混在一起
///   会让整条（含 ASCII 部分）改用 CJK 字体 —— 等宽格子的对齐全靠它（#54）；
/// * **样式**：颜色、粗体、暗显、斜体、下划线、删除线、上/下划线、反显、隐藏必须全等。
///   overline 区间在上游已按边界切好，同值合并不会越界。
pub(crate) fn merge_runs(runs: &[HistSpan]) -> Vec<HistSpan> {
    let mut out: Vec<HistSpan> = Vec::with_capacity(runs.len());
    for span in runs {
        let mergeable = out.last().is_some_and(|prev: &HistSpan| {
            prev.col + prev.cells == span.col
                && contains_cjk(&prev.text) == contains_cjk(&span.text)
                && prev.fg == span.fg
                && prev.bg == span.bg
                && prev.bold == span.bold
                && prev.dim == span.dim
                && prev.italic == span.italic
                && prev.underline == span.underline
                && prev.strike == span.strike
                && prev.overline == span.overline
                && prev.hidden == span.hidden
                && prev.inverse == span.inverse
        });
        if mergeable {
            let prev = out.last_mut().expect("just checked");
            prev.text.push_str(&span.text);
            prev.cells += span.cells;
        } else {
            out.push(span.clone());
        }
    }
    out
}

/// Split a styled terminal run only at complete Unicode grapheme boundaries.
/// Ordinary graphemes remain grouped into large Text spans; emoji with a
/// Twemoji asset become image spans so color survives Slint's monochrome font
/// rasterizers. Columns still come from terminal cells, not image pixels.
pub(crate) fn render_term_span(span: &HistSpan, row: i32, is_dark: bool) -> Vec<TermSpan> {
    // ASCII fast path: >95% of terminal output is plain ASCII.
    // Bypass grapheme segmentation, emoji lookup, and CJK detection entirely.
    if span.text.is_ascii() && span.cells > 0 {
        let (fg, bg) = vt_span_colors(span.fg, span.bg, span.bold, span.inverse, is_dark);
        return vec![TermSpan {
            text: span.text.clone().into(),
            fg: vt_apply_attr_alpha(fg, span.dim, span.hidden),
            bg,
            bold: span.bold,
            dim: span.dim,
            italic: span.italic,
            underline: span.underline as i32,
            strike: span.strike,
            overline: span.overline,
            row,
            col: span.col,
            cells: span.cells,
            cjk: false,
            emoji: false,
            emoji_image: slint::Image::default(),
        }];
    }

    use unicode_segmentation::UnicodeSegmentation as _;
    use unicode_width::UnicodeWidthStr as _;

    let graphemes: Vec<&str> = span.text.graphemes(true).collect();
    if graphemes.is_empty() {
        return Vec::new();
    }

    let (fg, bg) = vt_span_colors(span.fg, span.bg, span.bold, span.inverse, is_dark);
    let fg = vt_apply_attr_alpha(fg, span.dim, span.hidden);
    let mut result = Vec::new();
    let mut col = span.col;
    let mut remaining_cells = span.cells.max(0);
    let mut plain = String::new();
    let mut plain_col = col;
    let mut plain_cells = 0;

    for (index, grapheme) in graphemes.iter().enumerate() {
        let following = (graphemes.len() - index - 1) as i32;
        let desired = (*grapheme).width().clamp(1, 2) as i32;
        let cells = if following == 0 {
            remaining_cells.max(1)
        } else {
            desired.min((remaining_cells - following).max(1))
        };
        remaining_cells = remaining_cells.saturating_sub(cells);

        if let Some(emoji_image) = twemoji_image(grapheme) {
            if !plain.is_empty() {
                let plain_cjk = contains_cjk(&plain);
                result.push(TermSpan {
                    text: std::mem::take(&mut plain).into(),
                    fg,
                    bg,
                    bold: span.bold,
                    dim: span.dim,
                    italic: span.italic,
                    underline: span.underline as i32,
                    strike: span.strike,
                    overline: span.overline,
                    row,
                    col: plain_col,
                    cells: plain_cells,
                    cjk: plain_cjk,
                    emoji: false,
                    emoji_image: slint::Image::default(),
                });
                plain_cells = 0;
            }
            result.push(TermSpan {
                text: "".into(),
                fg,
                bg,
                bold: span.bold,
                dim: span.dim,
                italic: span.italic,
                underline: span.underline as i32,
                strike: span.strike,
                overline: span.overline,
                row,
                col,
                cells,
                cjk: false,
                emoji: true,
                emoji_image,
            });
            plain_col = col + cells;
        } else {
            if plain.is_empty() {
                plain_col = col;
            }
            plain.push_str(grapheme);
            plain_cells += cells;
        }
        col += cells;
    }

    if !plain.is_empty() {
        let cjk = contains_cjk(&plain);
        result.push(TermSpan {
            text: plain.into(),
            fg,
            bg,
            bold: span.bold,
            dim: span.dim,
            italic: span.italic,
            underline: span.underline as i32,
            strike: span.strike,
            overline: span.overline,
            row,
            col: plain_col,
            cells: plain_cells,
            cjk,
            emoji: false,
            emoji_image: slint::Image::default(),
        });
    }
    result
}
