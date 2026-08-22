#[cfg(target_os = "linux")]
use std::sync::OnceLock;

use crate::terminal::TermBuffers;

#[cfg(any(target_os = "windows", test))]
use super::state::CtrlKeySide;

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
    let mut bytes = command.clone().into_bytes();
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
    workaround
        && ctrl
        && matches!(
            key.chars().collect::<Vec<_>>().as_slice(),
            ['\u{0011}'] | ['\u{0016}']
        )
}

#[cfg(target_os = "linux")]
pub(crate) fn bare_ctrl_marker_workaround_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let Ok(release) = std::fs::read_to_string("/etc/os-release") else {
            return false;
        };
        release.lines().any(|line| {
            let Some((key, value)) = line.split_once('=') else {
                return false;
            };
            let value = value.trim_matches('"');
            key == "ID" && value.eq_ignore_ascii_case("debian")
                || key == "ID_LIKE"
                    && value
                        .split_ascii_whitespace()
                        .any(|item| item.eq_ignore_ascii_case("debian"))
        })
    })
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
