//! Built-in output-highlight rule set, adapted from the regex patterns used by
//! the `tailspin` log highlighter (https://github.com/bensadeh/tailspin).
//!
//! We do **not** depend on the tailspin crate — it renders ANSI escape strings
//! and keeps its regexes private.  Instead we embed the same pattern families
//! here and reuse Rudder's existing `CompiledOutputRule` / `highlight_custom_output`
//! pipeline, which already colours `HistSpan::fg` while preserving `bg` and the
//! bold/italic flags.
//!
//! Colour scheme (xterm-256 indices, matching `highlight_color_index`):
//!   9 = red (errors), 10 = green (values/success), 11 = yellow (time/warning),
//!   12 = blue (network), 13 = magenta (identifiers), 14 = cyan (paths/info).
//! Grey is deliberately **not** used — it reads as "disabled" and hurts the
//! otherwise colourful terminal palette.
//!
//! Order matters: `highlight_custom_output` only recolours runs whose `fg` is
//! still `Default`, so earlier rules win.  Specific rules (URL, IP, UUID, date,
//! sized numbers, paths) therefore run before generic ones, and there is no
//! catch-all "bare number" rule — plain digits stay in the default colour so a
//! date or IPv6 hex group is never split into an ugly grey fragment.

use std::sync::LazyLock;

use crate::terminal::CompiledOutputRule;

static BUILTIN_RULES: LazyLock<Vec<CompiledOutputRule>> = LazyLock::new(|| {
    // (pattern, colour, case_insensitive)
    const RULES: &[(&str, u8, bool)] = &[
        // --- network (blue) -------------------------------------------------
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
        // Bare domain without a scheme: `www.baidu.com`, `mail.qq.com`.
        // Requires at least two dots so a bare filename (`querylog.json`,
        // `sessions.db`) with a single dot is never mistaken for a domain.
        (
            r"\b(?:[A-Za-z0-9](?:[A-Za-z0-9\-]*[A-Za-z0-9])?\.){2,}[A-Za-z]{2,}\b",
            12,
            false,
        ),
        // IPv4 (dotted quad)
        (r"\b(?:\d{1,3}\.){3}\d{1,3}\b", 12, false),
        // IPv6 — supports the `::` zero-compression form.  Requires at least
        // three `:group` segments so a plain clock (`10:44:22`) is never
        // mistaken for an address.  `fe80::f0da:145:b458:4e3e` matches whole.
        (r"\b[0-9a-fA-F]{1,4}(?::[0-9a-fA-F]{0,4}){3,}\b", 12, false),
        // UUID (8-4-4-4-12)
        (
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
            13,
            false,
        ),
        // --- date & time (yellow) ------------------------------------------
        // ISO date (2026-08-15)
        (r"\b\d{4}-\d{2}-\d{2}\b", 11, false),
        // Month-name date (Aug 15, Sep 3) — e.g. `ll` / `date` output
        (
            r"\b(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)[a-z]*\.?\s+\d{1,2}\b",
            11,
            true,
        ),
        // clock time (HH:MM:SS, optional fractional)
        (
            r"\b(?:[01]?\d|2[0-3]):[0-5]\d:[0-5]\d(?:[.,:]\d+)?\b",
            11,
            false,
        ),
        // 4-digit year (1900-2099) — `date` prints a trailing year
        (r"\b(?:19|20)\d{2}\b", 11, false),
        // --- sized numbers (green, distinct from paths) ---------------------
        // Storage size: 1.9G / 391M / 6.7G / 40M / 1.5K / 2.3GiB
        (r"\b\d+(?:\.\d+)?[KMGTPE](?:i?B)?\b", 10, false),
        // Duration: 150ms / 2.5s / 30min / 1h / 3d
        (
            r"\b\d+(?:\.\d+)?\s?(?:ns|us|µs|ms|s|sec|min|hr|h|d)\b",
            11,
            false,
        ),
        // Percentage: 48% / 11% / 0%
        (r"\b\d+(?:\.\d+)?%\b", 10, false),
        // --- paths, split by context so scenarios get distinct colours ------
        // Prompt current-directory path: `user@host:/path` or `user@host:~`.
        // Anchored on `@host:` so it only fires inside a shell prompt.
        // (magenta)
        (r"@[A-Za-z0-9._\-]+:(?:~|/[\w.\-]+(?:/[\w.\-]+)*)", 13, false),
        // Plain absolute path / mount point: `/dev`, `/run`, `/dev/mmcblk0p2`,
        // and the bare root `/` in `df`/`mount` output.  `=` covers `key=/path`.
        // (cyan)
        (r"(?:^|[\s=])/(?:[\w.\-]+(?:/[\w.\-]+)*)?", 14, false),
        // key=value (the key and `=`; tailspin styles only `key=`)
        (r"(?:^|\s)\w+=", 13, false),
        // quoted strings (green)
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
