//! Built-in output-highlight rule set, adapted from the regex patterns used by
//! the `tailspin` log highlighter (https://github.com/bensadeh/tailspin).
//!
//! We do **not** depend on the tailspin crate — it renders ANSI escape strings
//! and keeps its regexes private.  Instead we embed the same pattern families
//! here and reuse Rudder's existing `CompiledOutputRule` / `highlight_custom_output`
//! pipeline, which already colours `HistSpan::fg` while preserving `bg` and the
//! bold/italic flags.
//!
//! Order matters: `highlight_custom_output` only recolours runs whose `fg` is
//! still `Default`, so earlier rules win.  Specific rules (URL, IP, UUID, date,
//! sized numbers, paths) therefore run before generic ones (bare number) so a
//! generic rule never claims the digits inside a specific token.

use std::sync::LazyLock;

use crate::terminal::CompiledOutputRule;

/// xterm-256 indices matching `highlight_color_index` in `output_highlight.rs`.
/// 8 = gray, 9 = red, 10 = green, 11 = yellow, 12 = blue, 13 = magenta, 14 = cyan.
static BUILTIN_RULES: LazyLock<Vec<CompiledOutputRule>> = LazyLock::new(|| {
    // (pattern, colour, case_insensitive)
    const RULES: &[(&str, u8, bool)] = &[
        // --- structured tokens (most specific first) -----------------------
        // URL: http(s)://host[:port][/path]
        (
            r"https?://[A-Za-z0-9._~\-]+(?::\d{1,5})?(?:/[^\s]*)?",
            12,
            false,
        ),
        // email address (requires a dotted domain, so a bare `user@host`
        // prompt is not mistaken for an email)
        (
            r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b",
            12,
            false,
        ),
        // IPv4 (dotted quad; tailspin additionally range-checks octets, but a
        // permissive match is enough for colouring)
        (r"\b(?:\d{1,3}\.){3}\d{1,3}\b", 12, false),
        // IPv6 (heuristic: 2+ colon-separated hex groups)
        (r"\b(?:[0-9a-fA-F]{1,4}:){2,7}[0-9a-fA-F]{1,4}\b", 12, false),
        // UUID (8-4-4-4-12)
        (
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
            13,
            false,
        ),
        // ISO date (2026-08-14)
        (r"\b\d{4}-\d{2}-\d{2}\b", 11, false),
        // clock time (HH:MM:SS, optional fractional)
        (
            r"\b(?:[01]?\d|2[0-3]):[0-5]\d:[0-5]\d(?:[.,:]\d+)?\b",
            11,
            false,
        ),
        // --- sized numbers (must run before the bare-number rule, otherwise
        //     `1.9G` matches only `1` because `9G` is not a word boundary) ---
        // Storage size: 1.9G / 391M / 6.7G / 3.2G / 40M / 1.5K / 2.3GiB
        (r"\b\d+(?:\.\d+)?[KMGTPE](?:i?B)?\b", 14, false),
        // Duration: 150ms / 2.5s / 30min / 1h / 3d
        (
            r"\b\d+(?:\.\d+)?\s?(?:ns|us|µs|ms|s|sec|min|hr|h|d)\b",
            11,
            false,
        ),
        // memory pointer (0x7ffe...)
        (r"\b0x[0-9a-fA-F]+\b", 8, false),
        // --- Unix path: accepts the bare root `/` (a common mount point in
        //     `df`/`mount` output) and paths following a prompt colon, e.g.
        //     `ne@fnnas:/vol1/1000/...`.  `:` and `=` anchors cover prompt and
        //     `key=/path` forms. -------------------------------------------
        (r"(?:^|[\s:=])/(?:[\w.\-]+(?:/[\w.\-]+)*)?", 14, false),
        // key=value (the key and `=`; tailspin styles only `key=`)
        (r"(?:^|\s)\w+=", 13, false),
        // quoted strings
        (r#""[^"\n]*""#, 10, false),
        (r"'[^'\n]*'", 10, false),
        // --- severity keywords ---------------------------------------------
        (
            r"\b(?:error|errors|fatal|critical|panic|crash|exception|failed|failure|denied|rejected|refused|timeout|timed out|abort|aborted)\b",
            9,
            true,
        ),
        (
            r"\b(?:warning|warn|deprecated|retry|retrying|pending|degraded|unhealthy)\b",
            11,
            true,
        ),
        (
            r"\b(?:success|succeeded|ok|okay|ready|healthy|passed|completed|connected|started)\b",
            10,
            true,
        ),
        (
            r"\b(?:info|debug|trace|notice|verbose)\b",
            14,
            true,
        ),
        // --- bare number (last: earlier rules already claimed the digits
        //     inside IPs / UUIDs / dates / sized numbers) -------------------
        (r"\b\d+(?:\.\d+)?\b", 14, false),
    ];

    RULES
        .iter()
        .map(|(pattern, colour, case_insensitive)| CompiledOutputRule {
            matcher: regex::RegexBuilder::new(pattern)
                .case_insensitive(*case_insensitive)
                .build()
                .expect("built-in output-highlight regex must compile"),
            whole_line: false,
            ansi_index: *colour,
        })
        .collect()
});

/// The precompiled tailspin-style built-in rule set.
pub(crate) fn builtin_rules() -> &'static [CompiledOutputRule] {
    &BUILTIN_RULES
}
