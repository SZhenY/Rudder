use super::*;

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
        // ⚠ 必须按**单元格宽度**算，不能按字符数：CJK / 全角字符占 2 格，
        // `"服务启动 ERROR"` 里的 4 个汉字按 4 计会让后面的标记整体左移 4 列
        // （同一文件里的 style_custom_matches 用的就是 text_cell_width）。
        let before_cells = text_cell_width(&before);
        let marker_cells = text_cell_width(&marker);

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

pub(crate) fn highlight_custom_output(
    mut runs: Vec<HistSpan>,
    rules: &[CompiledOutputRule],
) -> Vec<HistSpan> {
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

pub(crate) fn custom_rule_eligible(run: &HistSpan) -> bool {
    matches!(run.fg, TermColor::Default)
        && matches!(run.bg, TermColor::Default)
        && !run.bold
        && !run.inverse
}

pub(crate) fn style_custom_matches(
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
/// Shared marker scanner used by both the log-level and devops presets:
/// 1) scans for uppercase keywords (e.g. `ERROR`, `SUCCEEDED`), then
/// 2) falls back to structured `key=value` values in lowercase (`level=error`,
///    `"status": "ok"`). Only values after a real key are accepted, keeping
///    normal lowercase prose untouched.
pub(crate) fn find_marker(
    text: &str,
    max_chars: usize,
    words: &[(&str, u8)],
    keys: &[&str],
) -> Option<(usize, usize, u8)> {
    let bytes = text.as_bytes();
    let mut best: Option<(usize, usize, u8)> = None;
    for (word, colour) in words {
        for (start, _) in text.match_indices(word) {
            if text[..start].chars().count() >= max_chars
                || !ascii_word_boundary(bytes, start, start + word.len())
            {
                continue;
            }
            let candidate = (start, start + word.len(), *colour);
            if best.is_none_or(|current| start < current.0) {
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
    for key in keys {
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
            for (word, colour) in words {
                let word = word.to_ascii_lowercase();
                if lower[pos..].starts_with(&word)
                    && ascii_word_boundary(lower_bytes, pos, pos + word.len())
                {
                    return Some((pos, pos + word.len(), *colour));
                }
            }
        }
    }
    None
}

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
    find_marker(text, max_chars, &LEVELS, &["level"])
}

pub(crate) fn output_highlight_marker(
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
pub(crate) fn devops_marker(text: &str, max_chars: usize) -> Option<(usize, usize, u8)> {
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
    find_marker(text, max_chars, &STATES, &["status", "state", "result"])
}

pub(crate) fn ascii_word_boundary(bytes: &[u8], start: usize, end: usize) -> bool {
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    bytes
        .get(start.wrapping_sub(1))
        .is_none_or(|b| !is_word(*b))
        && bytes.get(end).is_none_or(|b| !is_word(*b))
}
