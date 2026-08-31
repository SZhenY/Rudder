use crate::terminal::TermBuffers;
use crate::terminal::MouseReport;

#[cfg(any(target_os = "windows", test))]
use super::state::CtrlKeySide;

/// Encode a terminal mouse event for the PTY using the encoding the remote
/// application requested (X10 / SGR) — so btop, htop, mc and other mouse-aware
/// TUI apps get click/drag/wheel events (ported from upstream `d8eff40`).
///
/// `btn` follows the xterm conventions:
///   0/1/2 = left / middle / right button press,
///   32    = motion with no button,
///   35    = motion with a button held,
///   64/65 = wheel up / down.
/// `release` marks a button-release event (only meaningful for `btn` 0–2):
/// X10 encodes it as `btn + 3`, SGR keeps the same code but ends the report
/// with a lowercase `m`.
///
/// Coordinates are 1-based grid cells clamped into [1, 223] — the range the
/// classic X10 byte encoding can express — matching how the remote draws its
/// UI. `cols`/`rows` are the *screen* dimensions so the report always points
/// at the same cell the program rendered.
pub(crate) fn encode_mouse_event(
    btn: u8,
    release: bool,
    col: i32,
    row: i32,
    cols: u16,
    rows: u16,
    encoding: MouseReport,
) -> Vec<u8> {
    let c = (col.clamp(0, cols.saturating_sub(1) as i32) as u16 + 1).clamp(1, 223);
    let r = (row.clamp(0, rows.saturating_sub(1) as i32) as u16 + 1).clamp(1, 223);
    match encoding {
        MouseReport::Sgr => {
            let final_byte = if release { b'm' } else { b'M' };
            format!("\x1b[<{btn};{c};{r}{}", final_byte as char).into_bytes()
        }
        _ => {
            let cb = btn as u16 + if release { 3 } else { 0 } + 32;
            vec![0x1b, b'[', b'M', cb as u8, (c + 32) as u8, (r + 32) as u8]
        }
    }
}

/// Normalize clipboard line endings to the single CR byte expected for Enter
/// by a terminal, including inside bracketed-paste payloads.
/// When `convert_eol` is true, LF is converted to CR+LF for Windows programs.
pub(crate) fn normalize_pasted_newlines(text: &str, convert_eol: bool) -> String {
    if convert_eol {
        // Convert \r\n → placeholder, \n → \r\n, restore \r\n
        text.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r")
    }
}

pub(crate) fn encode_command_bar_input(command: &str) -> Option<(String, Vec<u8>)> {
    let command = command.trim_end().to_string();
    if command.is_empty() {
        return None;
    }
    let mut bytes = command.as_bytes().to_vec();
    bytes.push(b'\n');
    Some((command, bytes))
}

pub(crate) fn encode_pasted_text(text: &str, bracketed: bool, convert_eol: bool) -> Vec<u8> {
    let normalized = normalize_pasted_newlines(text, convert_eol);
    if !bracketed {
        return normalized.into_bytes();
    }

    // Do not allow pasted content to forge the bracketed-paste terminator or
    // inject Ctrl+C while the remote application is accepting the payload.
    let filtered = normalized.replace(['\x1b', '\x03'], "");
    let mut bytes = Vec::with_capacity(filtered.len() + 12);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(filtered.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

pub(crate) fn terminal_uses_bracketed_paste(bufs: &TermBuffers, tab_id: &str) -> bool {
    let buffer = bufs
        .lock()
        .ok()
        .and_then(|buffers| buffers.get(tab_id).cloned());
    buffer
        .and_then(|buffer| {
            buffer
                .lock()
                .ok()
                .map(|buffer| crate::terminal::bracketed_paste(&buffer.term))
        })
        .unwrap_or(false)
}

pub(crate) fn paste_requires_large_review(text: &str) -> bool {
    const COMPACT_CHAR_LIMIT: usize = 600;
    const COMPACT_LINE_LIMIT: usize = 12;
    let bytes = text.as_bytes();
    let mut lines = 1usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' => {
                lines += 1;
                if bytes.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
            }
            b'\n' => lines += 1,
            _ => {}
        }
        index += 1;
    }
    text.chars().count() > COMPACT_CHAR_LIMIT || lines > COMPACT_LINE_LIMIT
}

#[cfg(any(target_os = "windows", test))]
pub(crate) fn windows_process_ctrl_release(
    state: i_slint_backend_winit::winit::event::ElementState,
    logical_key: &i_slint_backend_winit::winit::keyboard::Key,
    physical_key: &i_slint_backend_winit::winit::keyboard::PhysicalKey,
) -> Option<CtrlKeySide> {
    use i_slint_backend_winit::winit::event::ElementState;
    use i_slint_backend_winit::winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

    if state != ElementState::Released || !matches!(logical_key, Key::Named(NamedKey::Process)) {
        return None;
    }

    match physical_key {
        PhysicalKey::Code(KeyCode::ControlLeft) => Some(CtrlKeySide::Left),
        PhysicalKey::Code(KeyCode::ControlRight) => Some(CtrlKeySide::Right),
        _ => None,
    }
}

pub(crate) fn should_drop_bare_ctrl_marker(key: &str, ctrl: bool, workaround: bool) -> bool {
    if !workaround || !ctrl {
        return false;
    }
    // Slint/winit on Linux (Debian, Fedora, …) and macOS emits these bare Ctrl
    // modifier markers before the actual Ctrl+letter event (#274, #369).
    if key == "\u{0011}" || key == "\u{0016}" {
        return true;
    }
    // macOS IME combinations may report bare physical Control as other C0
    // bytes: U+0017 opens nano search before Ctrl+X (#312), while U+0008 is
    // encoded as Backspace and deletes the preceding character during
    // Ctrl+Space input-method switching (#348). Genuine chords still arrive
    // through the final printable letter, so filtering these markers is safe.
    #[cfg(target_os = "macos")]
    if key == "\u{0017}" || key == "\u{0008}" {
        return true;
    }
    false
}

#[cfg(target_os = "linux")]
pub(crate) fn bare_ctrl_marker_workaround_enabled() -> bool {
    // Slint/winit can expose a physical Control press as U+0011 or U+0016 on
    // Linux. This was first observed on Debian (#274) and is now confirmed on
    // Fedora as well (#369), so it is a backend/platform behaviour rather than
    // a distribution-specific quirk. The final letter event still generates
    // genuine Ctrl+Q/Ctrl+V bytes through `key_to_pty_bytes`.
    true
}

#[cfg(target_os = "macos")]
pub(crate) fn bare_ctrl_marker_workaround_enabled() -> bool {
    true
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn bare_ctrl_marker_workaround_enabled() -> bool {
    false
}

pub(crate) fn key_to_pty_bytes(key: &str, ctrl: bool, alt: bool, app_cursor: bool) -> Vec<u8> {
    let special: Option<&[u8]> = match key {
        "\u{F700}" => Some(if app_cursor { b"\x1bOA" } else { b"\x1b[A" }),
        "\u{F701}" => Some(if app_cursor { b"\x1bOB" } else { b"\x1b[B" }),
        "\u{F702}" => Some(if app_cursor { b"\x1bOD" } else { b"\x1b[D" }),
        "\u{F703}" => Some(if app_cursor { b"\x1bOC" } else { b"\x1b[C" }),
        "\u{F729}" => Some(if app_cursor { b"\x1bOH" } else { b"\x1b[H" }),
        "\u{F72B}" => Some(if app_cursor { b"\x1bOF" } else { b"\x1b[F" }),
        "\u{F72C}" => Some(b"\x1b[5~"),
        "\u{F72D}" => Some(b"\x1b[6~"),
        "\u{007F}" | "\u{F728}" => Some(b"\x1b[3~"),
        "\u{F704}" => Some(b"\x1bOP"),
        "\u{F705}" => Some(b"\x1bOQ"),
        "\u{F706}" => Some(b"\x1bOR"),
        "\u{F707}" => Some(b"\x1bOS"),
        "\u{F708}" => Some(b"\x1b[15~"),
        "\u{F709}" => Some(b"\x1b[17~"),
        "\u{F70A}" => Some(b"\x1b[18~"),
        "\u{F70B}" => Some(b"\x1b[19~"),
        "\u{F70C}" => Some(b"\x1b[20~"),
        "\u{F70D}" => Some(b"\x1b[21~"),
        "\u{F70E}" => Some(b"\x1b[23~"),
        "\u{F70F}" => Some(b"\x1b[24~"),
        _ => None,
    };
    if let Some(sequence) = special {
        return sequence.to_vec();
    }

    if key == "\u{0008}" {
        return vec![0x7f];
    }
    if key == "\n" && !ctrl && !alt {
        return vec![0x0d];
    }
    if key.is_empty() {
        return Vec::new();
    }

    if let Some(character) = key.chars().next() {
        let codepoint = character as u32;
        if key.chars().count() == 1 && !ctrl && (0x10..=0x18).contains(&codepoint) {
            return Vec::new();
        }
    }

    if ctrl {
        if let Some(character) = key.chars().next() {
            let codepoint = character as u32;
            if key.chars().count() == 1 && (0x01..=0x1f).contains(&codepoint) {
                return vec![codepoint as u8];
            }
        }
        if let Some(character) = key.chars().next()
            && key.chars().count() == 1
        {
            let upper = character.to_ascii_uppercase() as u8;
            let control = match upper {
                b'A'..=b'Z' => Some(upper - b'A' + 1),
                b'[' => Some(0x1b),
                b'\\' => Some(0x1c),
                b']' => Some(0x1d),
                b'^' => Some(0x1e),
                b'_' => Some(0x1f),
                b'@' => Some(0x00),
                _ => None,
            };
            if let Some(byte) = control {
                return vec![byte];
            }
        }
    }

    if key
        .chars()
        .any(|character| (0xE000..=0xF8FF).contains(&(character as u32)))
    {
        return Vec::new();
    }
    if alt && !ctrl {
        let mut bytes = vec![0x1b];
        bytes.extend_from_slice(key.as_bytes());
        return bytes;
    }
    key.as_bytes().to_vec()
}

#[cfg(windows)]
pub(crate) fn c0_letter_key_down(codepoint: u32) -> bool {
    if !(0x01..=0x1a).contains(&codepoint) {
        return true;
    }
    let virtual_key = (codepoint + 0x40) as i32;
    #[allow(non_snake_case)]
    unsafe extern "system" {
        fn GetKeyState(nVirtKey: i32) -> i16;
    }
    unsafe { (GetKeyState(virtual_key) as u16) & 0x8000 != 0 }
}

#[cfg(test)]
mod interrupt_tests {
    use super::{encode_mouse_event, key_to_pty_bytes, should_drop_bare_ctrl_marker};
    use crate::terminal::MouseReport;

    /// Ctrl+C (ETX) is the terminal interrupt: it must reach the PTY whether or
    /// not the backend still reports the Control modifier alongside it. Upstream
    /// hit a regression here (#377) where a bare-modifier filter swallowed it.
    #[test]
    fn ctrl_c_interrupt_reaches_the_pty_with_and_without_the_modifier() {
        assert_eq!(
            key_to_pty_bytes("\u{0003}", true, false, false),
            vec![0x03],
            "Ctrl+C with the modifier held must reach the PTY"
        );
        assert_eq!(
            key_to_pty_bytes("\u{0003}", false, false, false),
            vec![0x03],
            "an already-translated ETX (ctrl=false) is still an interrupt"
        );
    }

    #[test]
    fn bare_ctrl_marker_filter_never_drops_the_interrupt() {
        // Enabled or not, the workaround only targets the physical Control
        // markers (Ctrl+Q / Ctrl+V, plus Ctrl+W / Ctrl+H on macOS).
        assert!(!should_drop_bare_ctrl_marker("\u{0003}", true, true));
        assert!(!should_drop_bare_ctrl_marker("\u{0003}", false, true));
        // …and it does still drop the markers it was written for.
        assert!(should_drop_bare_ctrl_marker("\u{0011}", true, true));
        assert!(should_drop_bare_ctrl_marker("\u{0016}", true, true));
    }

    #[test]
    fn mouse_events_encode_as_sgr() {
        // Press of the left button on the top-left cell: 1-based, uppercase M.
        assert_eq!(
            encode_mouse_event(0, false, 0, 0, 80, 24, MouseReport::Sgr),
            b"\x1b[<0;1;1M".to_vec()
        );
        // Release keeps the same button code but ends in lowercase m.
        assert_eq!(
            encode_mouse_event(0, true, 4, 9, 80, 24, MouseReport::Sgr),
            b"\x1b[<0;5;10m".to_vec()
        );
        // Wheel up / down are just button codes 64 / 65.
        assert_eq!(
            encode_mouse_event(64, false, 0, 0, 80, 24, MouseReport::Sgr),
            b"\x1b[<64;1;1M".to_vec()
        );
    }

    #[test]
    fn mouse_events_encode_as_x10() {
        // ESC [ M  Cb Cx Cy with every value offset by 32.
        assert_eq!(
            encode_mouse_event(0, false, 0, 0, 80, 24, MouseReport::X10),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
        // Release bumps the button code by 3 (X10 has no separate release form).
        assert_eq!(
            encode_mouse_event(0, true, 0, 0, 80, 24, MouseReport::X10),
            vec![0x1b, b'[', b'M', 35, 33, 33]
        );
    }

    #[test]
    fn mouse_coordinates_clamp_into_the_x10_range() {
        // Negative coordinates must not underflow into a bogus cell.
        assert_eq!(
            encode_mouse_event(0, false, -5, -5, 80, 24, MouseReport::Sgr),
            b"\x1b[<0;1;1M".to_vec()
        );
        // Past the right/bottom edge clamps to the last cell.
        assert_eq!(
            encode_mouse_event(0, false, 999, 999, 80, 24, MouseReport::Sgr),
            b"\x1b[<0;80;24M".to_vec()
        );
        // X10 can only express up to cell 223, so wider screens clamp there.
        assert_eq!(
            encode_mouse_event(0, false, 400, 0, 500, 24, MouseReport::X10),
            vec![0x1b, b'[', b'M', 32, 255, 33]
        );
    }

    #[test]
    fn mouse_event_with_no_tracking_still_encodes() {
        // The caller gates on MouseReport::None; encoding is a pure formatter
        // and falls back to the X10 byte form for anything that is not SGR.
        assert_eq!(
            encode_mouse_event(2, false, 0, 0, 80, 24, MouseReport::None),
            vec![0x1b, b'[', b'M', 34, 33, 33]
        );
    }
}

#[cfg(test)]
mod key_bytes_tests {
    use super::*;

    #[test]
    fn encode_command_bar_input_adds_newline_and_trims_tail() {
        assert_eq!(
            encode_command_bar_input("ls -la"),
            Some(("ls -la".to_string(), b"ls -la\n".to_vec()))
        );
        assert_eq!(
            encode_command_bar_input("git status  "),
            Some(("git status".to_string(), b"git status\n".to_vec()))
        );
        assert_eq!(encode_command_bar_input(""), None);
        assert_eq!(encode_command_bar_input("   "), None);
    }

    #[test]
    fn key_to_pty_bytes_maps_specials_and_application_cursor() {
        // Arrow keys: application-cursor mode switches to SS3 (ESC O …).
        assert_eq!(key_to_pty_bytes("\u{F700}", false, false, false), b"\x1b[A");
        assert_eq!(key_to_pty_bytes("\u{F700}", false, false, true), b"\x1bOA");
        assert_eq!(key_to_pty_bytes("\u{F702}", false, false, false), b"\x1b[D");
        assert_eq!(key_to_pty_bytes("\u{F702}", false, false, true), b"\x1bOD");
        assert_eq!(key_to_pty_bytes("\u{F72C}", false, false, false), b"\x1b[5~");
        assert_eq!(key_to_pty_bytes("\u{F704}", false, false, false), b"\x1bOP");
    }

    #[test]
    fn key_to_pty_bytes_maps_ctrl_and_alt() {
        assert_eq!(key_to_pty_bytes("c", true, false, false), b"\x03");
        assert_eq!(key_to_pty_bytes("C", true, false, false), b"\x03");
        assert_eq!(key_to_pty_bytes("[", true, false, false), b"\x1b");
        assert_eq!(key_to_pty_bytes("@", true, false, false), b"\x00");
        // Alt prefix.
        assert_eq!(key_to_pty_bytes("a", false, true, false), b"\x1ba");
        // Enter without modifiers becomes CR.
        assert_eq!(key_to_pty_bytes("\n", false, false, false), b"\x0d");
        // Backspace → DEL.
        assert_eq!(key_to_pty_bytes("\u{0008}", false, false, false), b"\x7f");
    }

    #[test]
    fn key_to_pty_bytes_drops_unsendable_keys() {
        // Bare C0 single chars (Ctrl-P..Ctrl-X without Ctrl) are dropped (#377).
        assert_eq!(key_to_pty_bytes("\u{0010}", false, false, false), b"");
        // Private-use range (Slint sentinel keys) never reaches the PTY.
        assert_eq!(key_to_pty_bytes("\u{E000}", false, false, false), b"");
        // Empty key.
        assert_eq!(key_to_pty_bytes("", false, false, false), b"");
    }

    #[test]
    fn encode_pasted_text_filters_forgery_bytes_in_bracketed_mode() {
        assert_eq!(
            encode_pasted_text("hello", false, false),
            b"hello".to_vec()
        );
        let bracketed = encode_pasted_text("a\x1bb\x03c", true, false);
        assert_eq!(bracketed, b"\x1b[200~abc\x1b[201~".to_vec());
    }
}
