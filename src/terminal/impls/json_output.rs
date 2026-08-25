//! Automatic pretty-print + syntax colouring of complete JSON lines (#338,
//! upstream 39cd57c). Applied at the TermBuffer byte-ingest point so both the
//! live grid and the resize-replay stream stay consistent.

use std::borrow::Cow;

const JSON_LINE_LIMIT: usize = 1024 * 1024;

/// Pretty-print complete JSON objects/arrays that occupy a whole terminal line.
/// Partial chunks, prompts, scalar values, and non-SGR terminal control sequences
/// are preserved byte-for-byte so normal interactive applications are untouched.
pub(crate) fn format_json_output(input: &[u8]) -> Cow<'_, [u8]> {
    let Ok(text) = std::str::from_utf8(input) else {
        return Cow::Borrowed(input);
    };
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    for segment in text.split_inclusive('\n') {
        let has_lf = segment.ends_with('\n');
        let line = segment
            .strip_suffix('\n')
            .unwrap_or(segment)
            .strip_suffix('\r')
            .unwrap_or_else(|| segment.strip_suffix('\n').unwrap_or(segment));
        let Some(plain) = strip_sgr(line) else {
            out.push_str(segment);
            continue;
        };
        let trimmed = plain.trim();
        if !has_lf
            || trimmed.len() > JSON_LINE_LIMIT
            || !(trimmed.starts_with('{') || trimmed.starts_with('['))
        {
            out.push_str(segment);
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            out.push_str(segment);
            continue;
        };
        if !matches!(
            value,
            serde_json::Value::Object(_) | serde_json::Value::Array(_)
        ) {
            out.push_str(segment);
            continue;
        }
        let pretty = pretty_json_preserving_order(trimmed);
        out.push_str(&colour_json(&pretty).replace('\n', "\r\n"));
        out.push_str("\r\n");
        changed = true;
    }
    if changed {
        Cow::Owned(out.into_bytes())
    } else {
        Cow::Borrowed(input)
    }
}

/// Re-indent a single-line JSON document without reordering keys (which a
/// `serde_json::Value` round-trip would do).
fn pretty_json_preserving_order(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + input.len() / 2);
    let mut indent = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let chars: Vec<char> = input.chars().collect();
    for (index, &ch) in chars.iter().enumerate() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            '{' | '[' => {
                out.push(ch);
                indent += 1;
                let closing = if ch == '{' { '}' } else { ']' };
                let next = chars[index + 1..]
                    .iter()
                    .copied()
                    .find(|next| !next.is_whitespace());
                if next != Some(closing) {
                    out.push('\n');
                    out.push_str(&"  ".repeat(indent));
                }
            }
            '}' | ']' => {
                indent = indent.saturating_sub(1);
                if !matches!(out.chars().last(), Some('{') | Some('[')) {
                    out.push('\n');
                    out.push_str(&"  ".repeat(indent));
                }
                out.push(ch);
            }
            ',' => {
                out.push(',');
                out.push('\n');
                out.push_str(&"  ".repeat(indent));
            }
            ':' => {
                out.push_str(": ");
            }
            ch if ch.is_whitespace() => {}
            other => out.push(other),
        }
    }
    out
}

/// Remove ANSI SGR colour sequences only. Any other ESC sequence makes the line
/// ineligible for rewriting because it may carry cursor/layout semantics.
fn strip_sgr(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        out.push_str(&input[start..i]);
        if bytes.get(i + 1) != Some(&b'[') {
            return None;
        }
        let mut end = i + 2;
        while bytes
            .get(end)
            .is_some_and(|b| b.is_ascii_digit() || *b == b';')
        {
            end += 1;
        }
        if bytes.get(end) != Some(&b'm') {
            return None;
        }
        i = end + 1;
        start = i;
    }
    out.push_str(&input[start..]);
    Some(out)
}

/// Wrap JSON tokens in SGR sequences: keys cyan (36), strings green (32),
/// numbers magenta (35), booleans yellow (33), null bright-black (90).
fn colour_json(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len() + input.len() / 2);
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let start = i;
            i += 1;
            let mut escaped = false;
            while i < bytes.len() {
                let byte = bytes[i];
                i += 1;
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    break;
                }
            }
            let mut next = i;
            while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
                next += 1;
            }
            let colour = if bytes.get(next) == Some(&b':') {
                36
            } else {
                32
            };
            out.push_str(&format!("\x1b[{colour}m{}\x1b[0m", &input[start..i]));
            continue;
        }
        if bytes[i].is_ascii_digit() || bytes[i] == b'-' {
            let start = i;
            i += 1;
            while bytes.get(i).is_some_and(|b| {
                b.is_ascii_digit() || matches!(*b, b'.' | b'e' | b'E' | b'+' | b'-')
            }) {
                i += 1;
            }
            out.push_str(&format!("\x1b[35m{}\x1b[0m", &input[start..i]));
            continue;
        }
        let token = if input[i..].starts_with("true") {
            Some((4, 33))
        } else if input[i..].starts_with("false") {
            Some((5, 33))
        } else if input[i..].starts_with("null") {
            Some((4, 90))
        } else {
            None
        };
        if let Some((len, colour)) = token {
            out.push_str(&format!("\x1b[{colour}m{}\x1b[0m", &input[i..i + len]));
            i += len;
        } else {
            let ch = input[i..].chars().next().expect("valid UTF-8 boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::format_json_output;

    #[test]
    fn formats_and_colours_complete_json_lines() {
        let output = format_json_output(b"{\"name\":\"api\",\"ok\":false,\"count\":2}\r\n");
        let text = String::from_utf8(output.into_owned()).unwrap();
        assert!(text.contains("\r\n  \x1b[36m\"count\"\x1b[0m: \x1b[35m2\x1b[0m"));
        assert!(text.contains("\x1b[33mfalse\x1b[0m"));
    }

    #[test]
    fn preserves_prompts_partial_json_and_cursor_controls() {
        for input in [
            b"user@host:~$ ".as_slice(),
            b"{\"partial\":true}".as_slice(),
            b"\x1b[2J{\"ok\":true}\n".as_slice(),
        ] {
            assert!(matches!(
                format_json_output(input),
                std::borrow::Cow::Borrowed(_)
            ));
        }
    }
}
