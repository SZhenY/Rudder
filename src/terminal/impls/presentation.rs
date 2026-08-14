use std::cell::RefCell;
use std::collections::HashMap;
use crate::terminal::{builtin_rules, CompiledOutputRule, HistSpan, OutputHighlightPreset, TermColor};
use crate::ui::TermSpan;

/// Highlight the first recognisable log-level token in each otherwise unstyled
/// terminal run. Uppercase standalone levels cover conventional text logs;
/// lowercase values are accepted only in a structured `level=...` / JSON field
/// to avoid colouring ordinary prose that happens to contain words like "error".
pub(crate) fn highlight_plain_output(
    runs: Vec<HistSpan>,
    preset: OutputHighlightPreset,
    custom_rules: &[CompiledOutputRule],
) -> Vec<HistSpan> {
    if preset == OutputHighlightPreset::Off {
        return runs;
    }
    // Built-in (tailspin-style) preset: custom rules first, then the embedded
    // pattern set. No log-level marker pass — it's an independent rule family.
    if preset == OutputHighlightPreset::Builtin {
        let runs = highlight_custom_output(runs, custom_rules);
        return highlight_custom_output(runs, builtin_rules());
    }
    let runs = highlight_custom_output(runs, custom_rules);
    const SEARCH_COLS: i32 = 96;

    let mut out = Vec::with_capacity(runs.len() + 2);
    for run in runs {
        let eligible = run.col < SEARCH_COLS
            && matches!(run.fg, TermColor::Default)
            && matches!(run.bg, TermColor::Default)
            && !run.bold
            && !run.inverse;
        let max_chars = SEARCH_COLS.saturating_sub(run.col) as usize;
        let Some((start, end, ansi_index)) = eligible
            .then(|| output_highlight_marker(&run.text, max_chars, preset))
            .flatten()
        else {
            out.push(run);
            continue;
        };

        let before = run.text[..start].to_string();
        let marker = run.text[start..end].to_string();
        let after = run.text[end..].to_string();
        let before_cells = before.chars().count() as i32;
        let marker_cells = marker.chars().count() as i32;

        if !before.is_empty() {
            let mut part = run.clone();
            part.text = before;
            part.cells = before_cells;
            out.push(part);
        }

        let mut level = run.clone();
        level.text = marker;
        level.fg = TermColor::Idx(ansi_index);
        level.bold = true;
        level.col += before_cells;
        level.cells = marker_cells;
        out.push(level);

        if !after.is_empty() {
            let mut part = run;
            part.text = after;
            part.col += before_cells + marker_cells;
            part.cells = part.cells.saturating_sub(before_cells + marker_cells);
            out.push(part);
        }
    }
    out
}

fn highlight_custom_output(mut runs: Vec<HistSpan>, rules: &[CompiledOutputRule]) -> Vec<HistSpan> {
    for rule in rules {
        if rule.whole_line
            && runs
                .iter()
                .any(|run| custom_rule_eligible(run) && rule.matcher.is_match(&run.text))
        {
            for run in &mut runs {
                if custom_rule_eligible(run) {
                    run.fg = TermColor::Idx(rule.ansi_index);
                    run.bold = true;
                }
            }
            continue;
        }

        let mut next = Vec::with_capacity(runs.len() + 2);
        for run in runs {
            if !custom_rule_eligible(&run) {
                next.push(run);
                continue;
            }
            let matches: Vec<(usize, usize)> = rule
                .matcher
                .find_iter(&run.text)
                .filter(|m| !m.is_empty())
                .map(|m| (m.start(), m.end()))
                .collect();
            if matches.is_empty() {
                next.push(run);
            } else {
                next.extend(style_custom_matches(run, &matches, rule.ansi_index));
            }
        }
        runs = next;
    }
    runs
}

fn custom_rule_eligible(run: &HistSpan) -> bool {
    matches!(run.fg, TermColor::Default)
        && matches!(run.bg, TermColor::Default)
        && !run.bold
        && !run.inverse
}

fn style_custom_matches(
    run: HistSpan,
    matches: &[(usize, usize)],
    ansi_index: u8,
) -> Vec<HistSpan> {
    let mut out = Vec::with_capacity(matches.len() * 2 + 1);
    let mut byte_pos = 0usize;
    let mut col = run.col;
    for &(start, end) in matches {
        if start < byte_pos || end > run.text.len() {
            continue;
        }
        if start > byte_pos {
            let text = &run.text[byte_pos..start];
            let cells = text_cell_width(text);
            let mut part = run.clone();
            part.text = text.to_string();
            part.col = col;
            part.cells = cells;
            out.push(part);
            col += cells;
        }

        let text = &run.text[start..end];
        let cells = text_cell_width(text);
        let mut hit = run.clone();
        hit.text = text.to_string();
        hit.fg = TermColor::Idx(ansi_index);
        hit.bold = true;
        hit.col = col;
        hit.cells = cells;
        out.push(hit);
        col += cells;
        byte_pos = end;
    }
    if byte_pos < run.text.len() {
        let mut part = run;
        part.text = part.text[byte_pos..].to_string();
        part.col = col;
        // Recompute instead of relying on subtraction: wide/combining glyphs
        // can make byte/character counts differ from terminal grid cells.
        part.cells = text_cell_width(&part.text);
        out.push(part);
    }
    out
}

pub(crate) fn text_cell_width(text: &str) -> i32 {
    use unicode_width::UnicodeWidthChar;
    text.chars().map(|ch| ch.width().unwrap_or(0) as i32).sum()
}

/// Return `(byte_start, byte_end, xterm_256_index)` for a log severity marker.
pub(crate) fn log_level_marker(text: &str, max_chars: usize) -> Option<(usize, usize, u8)> {
    const LEVELS: [(&str, u8); 10] = [
        ("CRITICAL", 9),
        ("WARNING", 11),
        ("ERROR", 9),
        ("FATAL", 9),
        ("PANIC", 9),
        ("TRACE", 8),
        ("DEBUG", 8),
        ("NOTICE", 14),
        ("INFO", 14),
        ("WARN", 11),
    ];

    let bytes = text.as_bytes();
    let mut best: Option<(usize, usize, u8)> = None;
    for (word, colour) in LEVELS {
        for (start, _) in text.match_indices(word) {
            if text[..start].chars().count() >= max_chars
                || !ascii_word_boundary(bytes, start, start + word.len())
            {
                continue;
            }
            let candidate = (start, start + word.len(), colour);
            if best.map_or(true, |current| start < current.0) {
                best = Some(candidate);
            }
            break;
        }
    }
    if best.is_some() {
        return best;
    }

    // Structured logging commonly emits `level=error`, `level: warn`, or
    // `{"level":"info"}` using lowercase values. Only accept those values
    // after a real `level` key, keeping normal lowercase prose untouched.
    let lower = text.to_ascii_lowercase();
    let lower_bytes = lower.as_bytes();
    for (key_start, _) in lower.match_indices("level") {
        if text[..key_start].chars().count() >= max_chars
            || !ascii_word_boundary(lower_bytes, key_start, key_start + 5)
        {
            continue;
        }
        let mut pos = key_start + 5;
        if lower_bytes.get(pos) == Some(&b'"') {
            pos += 1;
        }
        while lower_bytes.get(pos).is_some_and(u8::is_ascii_whitespace) {
            pos += 1;
        }
        if !matches!(lower_bytes.get(pos).copied(), Some(b'=') | Some(b':')) {
            continue;
        }
        pos += 1;
        while lower_bytes.get(pos).is_some_and(u8::is_ascii_whitespace) {
            pos += 1;
        }
        if matches!(lower_bytes.get(pos).copied(), Some(b'"') | Some(b'\'')) {
            pos += 1;
        }
        for (word, colour) in LEVELS {
            let word = word.to_ascii_lowercase();
            if lower[pos..].starts_with(&word)
                && ascii_word_boundary(lower_bytes, pos, pos + word.len())
            {
                return Some((pos, pos + word.len(), colour));
            }
        }
    }
    None
}

fn output_highlight_marker(
    text: &str,
    max_chars: usize,
    preset: OutputHighlightPreset,
) -> Option<(usize, usize, u8)> {
    let log = log_level_marker(text, max_chars);
    if preset != OutputHighlightPreset::DevOps {
        return log;
    }
    let ops = devops_marker(text, max_chars);
    match (log, ops) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(marker), None) | (None, Some(marker)) => Some(marker),
        (None, None) => None,
    }
}

/// Additional deployment/operations states used by the DevOps preset. The list
/// intentionally avoids ambiguous short words such as OK/UP/DOWN.
fn devops_marker(text: &str, max_chars: usize) -> Option<(usize, usize, u8)> {
    const STATES: [(&str, u8); 15] = [
        ("UNHEALTHY", 9),
        ("SUCCEEDED", 10),
        ("SUCCESS", 10),
        ("FAILURE", 9),
        ("FAILED", 9),
        ("TIMEOUT", 9),
        ("DENIED", 9),
        ("DEGRADED", 11),
        ("RETRYING", 11),
        ("PENDING", 11),
        ("HEALTHY", 10),
        ("READY", 10),
        ("PASSED", 10),
        ("RETRY", 11),
        ("FAIL", 9),
    ];

    let bytes = text.as_bytes();
    let mut best: Option<(usize, usize, u8)> = None;
    for (word, colour) in STATES {
        for (start, _) in text.match_indices(word) {
            if text[..start].chars().count() >= max_chars
                || !ascii_word_boundary(bytes, start, start + word.len())
            {
                continue;
            }
            let candidate = (start, start + word.len(), colour);
            if best.map_or(true, |current| start < current.0) {
                best = Some(candidate);
            }
            break;
        }
    }
    if best.is_some() {
        return best;
    }

    let lower = text.to_ascii_lowercase();
    let lower_bytes = lower.as_bytes();
    for key in ["status", "state", "result"] {
        for (key_start, _) in lower.match_indices(key) {
            if text[..key_start].chars().count() >= max_chars
                || !ascii_word_boundary(lower_bytes, key_start, key_start + key.len())
            {
                continue;
            }
            let mut pos = key_start + key.len();
            if lower_bytes.get(pos) == Some(&b'"') {
                pos += 1;
            }
            while lower_bytes.get(pos).is_some_and(u8::is_ascii_whitespace) {
                pos += 1;
            }
            if !matches!(lower_bytes.get(pos).copied(), Some(b'=') | Some(b':')) {
                continue;
            }
            pos += 1;
            while lower_bytes.get(pos).is_some_and(u8::is_ascii_whitespace) {
                pos += 1;
            }
            if matches!(lower_bytes.get(pos).copied(), Some(b'"') | Some(b'\'')) {
                pos += 1;
            }
            for (word, colour) in STATES {
                let word = word.to_ascii_lowercase();
                if lower[pos..].starts_with(&word)
                    && ascii_word_boundary(lower_bytes, pos, pos + word.len())
                {
                    return Some((pos, pos + word.len(), colour));
                }
            }
        }
    }
    None
}

fn ascii_word_boundary(bytes: &[u8], start: usize, end: usize) -> bool {
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    bytes
        .get(start.wrapping_sub(1))
        .map_or(true, |b| !is_word(*b))
        && bytes.get(end).map_or(true, |b| !is_word(*b))
}

thread_local! {
    /// Decoded images are retained only for emoji actually seen in terminal
    /// output. A full 72x72 RGBA Twemoji is ~20 KiB; this avoids decoding on
    /// every redraw without eagerly allocating the entire emoji collection.
    static TWEMOJI_CACHE: RefCell<HashMap<String, Option<slint::Image>>> =
        RefCell::new(HashMap::new());
}

fn twemoji_image(grapheme: &str) -> Option<slint::Image> {
    TWEMOJI_CACHE.with(|cache| {
        if let Some(image) = cache.borrow().get(grapheme) {
            return image.clone();
        }

        // U+FE0E explicitly requests text presentation. U+FE0F requests emoji
        // presentation, but Twemoji stores some legacy symbols (for example
        // ❤️) under a key without VS16, so retry lookup with VS16 removed.
        let normalized;
        let asset = if grapheme.contains('\u{fe0e}') {
            None
        } else {
            normalized = grapheme.replace('\u{fe0f}', "");
            twemoji_assets::png::PngTwemojiAsset::from_emoji(grapheme).or_else(|| {
                (normalized != grapheme)
                    .then(|| twemoji_assets::png::PngTwemojiAsset::from_emoji(&normalized))
                    .flatten()
            })
        };
        let image = asset
            .and_then(|asset| image::load_from_memory(asset.data.0).ok())
            .map(|decoded| {
                let rgba = decoded.into_rgba8();
                let (width, height) = rgba.dimensions();
                let mut pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
                pixels.make_mut_bytes().copy_from_slice(rgba.as_raw());
                slint::Image::from_rgba8(pixels)
            });
        cache
            .borrow_mut()
            .insert(grapheme.to_string(), image.clone());
        image
    })
}

/// Apply DIM (faint, SGR 2) and HIDDEN (conceal, SGR 8) to a resolved
/// foreground colour: faint text is drawn at reduced opacity, concealed text
/// at full transparency (its background fill still shows).
///
/// DIM is mostly a *weight* change these days: the UI switches the span to
/// the font's " Thin" variant (see terminal_view.slint), so the alpha here is
/// a gentle 0.85 rather than a heavy 0.55 — heavy translucency made faint
/// text look washed out.
fn vt_apply_attr_alpha(color: slint::Color, dim: bool, hidden: bool) -> slint::Color {
    if hidden {
        color.with_alpha(0.0)
    } else if dim {
        color.with_alpha(0.85)
    } else {
        color
    }
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
            hidden: span.hidden,
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
                    fg: fg.clone(),
                    bg: bg.clone(),
                    bold: span.bold,
                    dim: span.dim,
                    italic: span.italic,
                    underline: span.underline as i32,
                    hidden: span.hidden,
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
                fg: fg.clone(),
                bg: bg.clone(),
                bold: span.bold,
                dim: span.dim,
                italic: span.italic,
                underline: span.underline as i32,
                hidden: span.hidden,
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
            hidden: span.hidden,
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
        assert!(r > 100 && b > 100, "cube colour must be lightened on light theme: ({r},{g},{b})");
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
        assert_eq!((c16.red(), c16.green(), c16.blue()), (0, 0, 0), "true black stays black");

        let g232 = vt_bg_to_slint(TermColor::Idx(232), true); // (8,8,8)
        assert_eq!((g232.red(), g232.green(), g232.blue()), (8, 8, 8));
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
        assert!(alpha > 0 && alpha < 255, "SGR 2 dim → partially transparent fg (got {alpha})");

        let mut span = run("text", 4);
        span.hidden = true;
        let spans = render_term_span(&span, 0, true);
        assert_eq!(spans[0].fg.alpha(), 0, "SGR 8 conceal → fully transparent fg");
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

/// True if a terminal span contains any CJK character — ideograph, kana, or
/// (crucially) CJK punctuation like 、。，. The mono terminal font has no CJK
/// glyphs and Slint's per-script fallback tofu's *isolated* CJK punctuation
/// (it renders fine only when adjacent to a Han char), so these spans are drawn
/// with the CJK-capable UI font instead (#54). Box-drawing / powerline glyphs
/// are deliberately excluded so they keep the aligned monospace font.
fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c as u32,
            0x2E80..=0x2EFF       // CJK radicals
            | 0x3000..=0x303F     // CJK symbols & punctuation (、。「」…)
            | 0x3040..=0x30FF     // hiragana + katakana
            | 0x3100..=0x312F     // bopomofo
            | 0x3400..=0x4DBF     // CJK ext A
            | 0x4E00..=0x9FFF     // CJK unified ideographs
            | 0xF900..=0xFAFF     // CJK compatibility ideographs
            | 0xFF00..=0xFFEF     // fullwidth / halfwidth forms (，！？：；)
            | 0x20000..=0x2FA1F) // CJK ext B–F + compat supplement
    })
}

/// 16-colour ANSI palette for **dark** terminals (VS Code "Dark+" values).
const ANSI16_DARK: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00), // 0  black
    (0xcd, 0x31, 0x31), // 1  red
    (0x0d, 0xbc, 0x79), // 2  green
    (0xe5, 0xe5, 0x10), // 3  yellow
    (0x24, 0x72, 0xc8), // 4  blue
    (0xbc, 0x3f, 0xbc), // 5  magenta
    (0x11, 0xa8, 0xcd), // 6  cyan
    (0xe5, 0xe5, 0xe5), // 7  white        (light grey on dark bg)
    (0x66, 0x66, 0x66), // 8  bright black
    (0xf1, 0x4c, 0x4c), // 9  bright red
    (0x23, 0xd1, 0x8b), // 10 bright green
    (0xf5, 0xf5, 0x43), // 11 bright yellow
    (0x3b, 0x8e, 0xea), // 12 bright blue
    (0xd6, 0x70, 0xd6), // 13 bright magenta
    (0x29, 0xb8, 0xdb), // 14 bright cyan
    (0xff, 0xff, 0xff), // 15 bright white
];

/// 16-colour ANSI palette for **light** terminal **foreground** (text) use.
///
/// On a near-white (#fafafa) background, the standard "white" (slot 7) and
/// "bright white" (slot 15) are nearly invisible.  We remap them to dark greys
/// so `ls`, `git` and other tools that use colour 7 for regular text stay
/// perfectly readable.  Saturated hues are darkened for contrast.
const ANSI16_LIGHT: [(u8, u8, u8); 16] = [
    (0x1c, 0x1c, 0x1e), // 0  black        → Apple near-black
    (0xc0, 0x39, 0x2b), // 1  red
    (0x1a, 0x7f, 0x37), // 2  green        → darker for white bg
    (0x85, 0x64, 0x04), // 3  yellow       → dark amber, readable
    (0x04, 0x51, 0xa5), // 4  blue         → VS Code light blue
    (0x80, 0x00, 0x80), // 5  magenta
    (0x0e, 0x72, 0x5c), // 6  cyan         → darker teal
    (0x3a, 0x3a, 0x3c), // 7  white        → dark grey (was 0xe5e5e5, near-invisible)
    (0x55, 0x55, 0x55), // 8  bright black
    (0xe7, 0x4c, 0x3c), // 9  bright red
    (0x27, 0xae, 0x60), // 10 bright green
    (0xd4, 0xac, 0x0d), // 11 bright yellow
    (0x2e, 0x86, 0xc1), // 12 bright blue
    (0x9b, 0x59, 0xb6), // 13 bright magenta
    (0x1a, 0xbc, 0x9c), // 14 bright cyan
    (0x2c, 0x2c, 0x2e), // 15 bright white → dark (was 0xffffff, near-invisible)
];

/// 16-colour ANSI palette for **light** terminal **background** (fill) use.
///
/// When TUI programs (btop, htop, vim) paint cell backgrounds in light mode,
/// each colour maps to a light-tinted variant so the overall UI feels light.
/// "Black" (slot 0) becomes a very light grey rather than near-black, so
/// dark-background TUI apps naturally inherit a light appearance.  Foreground
/// text always uses `ANSI16_LIGHT` so readability is unaffected.
const ANSI16_LIGHT_BG: [(u8, u8, u8); 16] = [
    (0xe8, 0xe8, 0xed), // 0  black        → Apple system-grey-6 (very light)
    (0xff, 0xd5, 0xd5), // 1  red          → light rose
    (0xd5, 0xf5, 0xd5), // 2  green        → light mint
    (0xff, 0xf8, 0xd5), // 3  yellow       → light cream
    (0xd5, 0xe8, 0xf8), // 4  blue         → light sky
    (0xf5, 0xd5, 0xf5), // 5  magenta      → light lilac
    (0xd5, 0xf5, 0xf8), // 6  cyan         → light aqua
    (0xf5, 0xf5, 0xf7), // 7  white        → Apple bg (near-white)
    (0xd1, 0xd1, 0xd6), // 8  bright black → Apple system-grey-4
    (0xff, 0xbe, 0xbe), // 9  bright red   → light salmon
    (0xbe, 0xf5, 0xbe), // 10 bright green
    (0xf5, 0xf5, 0xbe), // 11 bright yellow
    (0xbe, 0xdd, 0xff), // 12 bright blue  → light periwinkle
    (0xf0, 0xbe, 0xff), // 13 bright magenta → light violet
    (0xbe, 0xf5, 0xff), // 14 bright cyan
    (0xff, 0xff, 0xff), // 15 bright white → white
];

/// Convert a terminal foreground colour (+ bold) to a Slint colour.
/// Bold + a base colour (0–7) maps to the bright variant (8–15), matching
/// how terminals render `ls --color` (bold-green executables, bold-blue dirs).
///
/// In light mode, true-colour RGB foregrounds that are light (HSL lightness
/// ≥ 0.55) are darkened so they remain readable on a near-white background.
fn vt_color_to_slint(color: TermColor, bold: bool, is_dark: bool) -> slint::Color {
    let (r, g, b) = match color {
        TermColor::Default => {
            if is_dark {
                (0xd4, 0xd4, 0xd4)
            } else {
                (0x2d, 0x2d, 0x2f)
            }
        }
        TermColor::Idx(i) => idx_to_rgb(i, bold, is_dark),
        TermColor::Rgb(r, g, b) => {
            if is_dark {
                (r, g, b)
            } else {
                darken_light_fg(r, g, b)
            }
        }
    };
    slint::Color::from_rgb_u8(r, g, b)
}

fn vt_default_fg_rgb(is_dark: bool) -> (u8, u8, u8) {
    if is_dark {
        (0xd4, 0xd4, 0xd4)
    } else {
        (0x2d, 0x2d, 0x2f)
    }
}

fn vt_default_bg_rgb(is_dark: bool) -> (u8, u8, u8) {
    if is_dark {
        (0x0e, 0x0f, 0x13)
    } else {
        (0xfa, 0xfa, 0xfa)
    }
}

pub(crate) fn vt_span_colors(
    fg: TermColor,
    bg: TermColor,
    bold: bool,
    inverse: bool,
    is_dark: bool,
) -> (slint::Color, slint::Color) {
    if !inverse {
        return (
            vt_color_to_slint(fg, bold, is_dark),
            vt_bg_to_slint(bg, is_dark),
        );
    }

    let fg_color = match bg {
        TermColor::Default => {
            let (r, g, b) = vt_default_bg_rgb(is_dark);
            slint::Color::from_rgb_u8(r, g, b)
        }
        _ => vt_color_to_slint(bg, false, is_dark),
    };
    let bg_color = match fg {
        TermColor::Default => {
            let (r, g, b) = vt_default_fg_rgb(is_dark);
            slint::Color::from_rgb_u8(r, g, b)
        }
        _ => vt_bg_to_slint(fg, is_dark),
    };
    (fg_color, bg_color)
}

/// In light mode, remap light true-colour foregrounds to dark so they are
/// readable on a near-white background.  Colours already dark (L < 0.55)
/// pass through unchanged.
fn darken_light_fg(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (h, s, l) = rgb_to_hsl(r, g, b);
    if l < 0.55 {
        return (r, g, b);
    }
    // L=0.55 → 0.40 (readable dark grey), L=1.0 (white) → ~0.15 (near-black).
    let new_l = (0.40 - (l - 0.55) * 0.56).max(0.10);
    hsl_to_rgb(h, s, new_l)
}

/// Convert a terminal *background* colour to Slint.  The default background maps
/// to fully transparent so we don't paint a fill over the terminal's own bg.
/// Non-default backgrounds (btop/htop bars, selected rows) become opaque.
///
/// In light mode:
/// - ANSI 16 colours use `ANSI16_LIGHT_BG` (light pastels).
/// - True-colour RGB backgrounds that are dark (HSL lightness < 0.45) are
///   remapped to light pastels so programs like btop feel light-themed.
fn vt_bg_to_slint(color: TermColor, is_dark: bool) -> slint::Color {
    match color {
        TermColor::Default => slint::Color::from_argb_u8(0, 0, 0, 0), // transparent
        TermColor::Idx(i) => {
            // Exact xterm 256-colour value in both themes — the standard cube
            // values must never be shifted (53 = (95,0,95) stays dark magenta,
            // exactly as Windows Terminal / PowerShell render it).
            let (r, g, b) = idx_to_rgb_bg(i, is_dark);
            slint::Color::from_rgb_u8(r, g, b)
        }
        TermColor::Rgb(r, g, b) => {
            if is_dark {
                slint::Color::from_rgb_u8(r, g, b)
            } else {
                let (nr, ng, nb) = lighten_dark_bg(r, g, b);
                slint::Color::from_rgb_u8(nr, ng, nb)
            }
        }
    }
}

/// In light mode, remap dark true-colour backgrounds to light pastels.
/// Colours whose HSL lightness is already ≥ 0.45 pass through unchanged
/// (the program chose a light colour deliberately).
fn lighten_dark_bg(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (h, s, l) = rgb_to_hsl(r, g, b);
    if l >= 0.45 {
        return (r, g, b);
    }
    // Remap: darkest (l≈0) → very light (l≈0.92); l=0.45 → l≈0.84.
    // Reduce saturation to pastel so colours don't look garish on white.
    let new_l = 0.92 - l * 0.18;
    let new_s = (s * 0.35).min(0.25);
    hsl_to_rgb(h, new_s, new_l)
}

fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-6 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < 1e-6 {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if (max - g).abs() < 1e-6 {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    (h, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    if s < 1e-6 {
        let v = (l * 255.0).round() as u8;
        return (v, v, v);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let hue = |mut t: f32| -> f32 {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            return p + (q - p) * 6.0 * t;
        }
        if t < 0.5 {
            return q;
        }
        if t < 2.0 / 3.0 {
            return p + (q - p) * (2.0 / 3.0 - t) * 6.0;
        }
        p
    };
    (
        (hue(h + 1.0 / 3.0) * 255.0).round() as u8,
        (hue(h) * 255.0).round() as u8,
        (hue(h - 1.0 / 3.0) * 255.0).round() as u8,
    )
}

/// Map an xterm-256 palette index to RGB (16 ANSI + 6×6×6 cube + grayscale).
fn idx_to_rgb(i: u8, bold: bool, is_dark: bool) -> (u8, u8, u8) {
    let i = if bold && i < 8 { i + 8 } else { i };
    let palette = if is_dark { &ANSI16_DARK } else { &ANSI16_LIGHT };
    match i {
        0..=15 => palette[i as usize],
        16..=231 => {
            let n = i - 16;
            let to = |v: u8| -> u8 {
                if v == 0 {
                    0
                } else {
                    55 + v * 40
                }
            };
            (to(n / 36), to((n % 36) / 6), to(n % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

/// Same as [`idx_to_rgb`] but for **background** fills in light mode: the 16
/// ANSI base colours use `ANSI16_LIGHT_BG` (light pastels) so TUI program
/// backgrounds feel light, and colour-cube / grayscale entries (16+) are
/// lightened like true-colour RGB backgrounds — otherwise dark cube colours
/// (e.g. 17 = (0,0,95)) would read as near-black blocks on a light theme.
/// 256-colour cube / grayscale are used as-is in dark mode.
fn idx_to_rgb_bg(i: u8, is_dark: bool) -> (u8, u8, u8) {
    if !is_dark && i < 16 {
        return ANSI16_LIGHT_BG[i as usize];
    }
    let (r, g, b) = idx_to_rgb(i, false, is_dark);
    if is_dark {
        (r, g, b)
    } else {
        lighten_dark_bg(r, g, b)
    }
}

#[cfg(test)]
mod real_file_cube_tests {
    use super::*;
    use crate::terminal::{build_line, new_term, process_bytes, term_size, TermColor};
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::Line as GridLine;

    /// End-to-end regression for the user-reported "cube index 53 renders
    /// black" issue: feeding the *whole* real char-test file, the 53 swatch
    /// (dark magenta) must come out as a lifted magenta — never the theme
    /// background, never black.
    #[test]
    fn real_file_cube_53_is_magenta() {
        let Ok(data) = std::fs::read("../terminal_chars_test.txt") else {
            eprintln!("skipping: terminal_chars_test.txt not found");
            return;
        };
        let (mut term, mut proc) = new_term(40, 100, 2000);
        process_bytes(&mut proc, &mut term, &data);
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
