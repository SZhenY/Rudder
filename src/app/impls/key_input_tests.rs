use super::*;
use crate::app::key_input::{parse_tunnel_forward, redact_key};

#[test]
fn windows_process_key_ctrl_release_keeps_physical_side() {
    use i_slint_backend_winit::winit::event::ElementState;
    use i_slint_backend_winit::winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

    let process = Key::Named(NamedKey::Process);
    assert_eq!(
        windows_process_ctrl_release(
            ElementState::Released,
            &process,
            &PhysicalKey::Code(KeyCode::ControlLeft),
        ),
        Some(CtrlKeySide::Left)
    );
    assert_eq!(
        windows_process_ctrl_release(
            ElementState::Released,
            &process,
            &PhysicalKey::Code(KeyCode::ControlRight),
        ),
        Some(CtrlKeySide::Right)
    );
}

#[test]
fn windows_process_key_recovery_ignores_other_key_events() {
    use i_slint_backend_winit::winit::event::ElementState;
    use i_slint_backend_winit::winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

    let process = Key::Named(NamedKey::Process);
    let left_ctrl = PhysicalKey::Code(KeyCode::ControlLeft);
    assert_eq!(
        windows_process_ctrl_release(ElementState::Pressed, &process, &left_ctrl),
        None
    );
    assert_eq!(
        windows_process_ctrl_release(
            ElementState::Released,
            &Key::Named(NamedKey::Control),
            &left_ctrl,
        ),
        None
    );
    assert_eq!(
        windows_process_ctrl_release(
            ElementState::Released,
            &process,
            &PhysicalKey::Code(KeyCode::KeyC),
        ),
        None
    );
}

#[test]
fn bare_alt_is_not_forwarded() {
    // Slint sends Alt-alone as key=0x12 with alt=true. It must produce no
    // bytes — otherwise it becomes ESC+0x12 and clears the input (issue #43).
    assert_eq!(
        key_to_pty_bytes("\u{0012}", false, true, false),
        Vec::<u8>::new()
    );
}

#[test]
fn bare_modifier_codes_are_dropped() {
    // Shift..MetaR (0x10..=0x18) pressed alone (ctrl=false) → nothing sent.
    for cp in 0x10u32..=0x18 {
        let s = char::from_u32(cp).unwrap().to_string();
        assert_eq!(
            key_to_pty_bytes(&s, false, false, false),
            Vec::<u8>::new(),
            "code point {:#04x} should be dropped",
            cp
        );
    }
}

#[test]
fn home_and_end_follow_application_cursor_mode() {
    // Normal cursor mode: CSI sequences.
    assert_eq!(key_to_pty_bytes("\u{F729}", false, false, false), b"\x1b[H");
    assert_eq!(key_to_pty_bytes("\u{F72B}", false, false, false), b"\x1b[F");
    // Application cursor mode (DECCKM): SS3 sequences, required by
    // oh-my-zsh / ZLE for beginning-/end-of-line movement.
    assert_eq!(key_to_pty_bytes("\u{F729}", false, false, true), b"\x1bOH");
    assert_eq!(key_to_pty_bytes("\u{F72B}", false, false, true), b"\x1bOF");
}

#[test]
fn ctrl_letter_c0_still_passes() {
    // A real Ctrl+R encoded as the C0 byte 0x12 with ctrl=true must still be
    // forwarded; the #274/#312 fix filters only bare Ctrl/CtrlR markers.
    assert_eq!(key_to_pty_bytes("\u{0012}", true, false, false), vec![0x12]);
    // Ctrl+X as C0 0x18.
    assert_eq!(key_to_pty_bytes("\u{0018}", true, false, false), vec![0x18]);
}

#[test]
fn platform_bare_ctrl_markers_do_not_reach_nano() {
    // Slint on Debian and macOS emits these before the actual Ctrl+letter event.
    assert!(should_drop_bare_ctrl_marker("\u{0011}", true, true));
    assert!(should_drop_bare_ctrl_marker("\u{0016}", true, true));
    // Other platforms retain their existing direct-C0 behaviour.
    assert!(!should_drop_bare_ctrl_marker("\u{0011}", true, false));
    assert!(!should_drop_bare_ctrl_marker("x", true, true));
    // Genuine Ctrl+Q/Ctrl+V chords still encode from the final letter
    // event (#369).
    assert_eq!(key_to_pty_bytes("q", true, false, false), vec![0x11]);
    assert_eq!(key_to_pty_bytes("v", true, false, false), vec![0x16]);
    // The following Ctrl+X must still become CAN (0x18), which nano uses
    // for Exit.
    assert_eq!(key_to_pty_bytes("x", true, false, false), vec![0x18]);
}

#[cfg(target_os = "macos")]
#[test]
fn macos_ime_bare_ctrl_backspace_marker_is_filtered() {
    // #348: macOS IMEs may report bare Control as U+0008; it must not be
    // sent as Backspace. A genuine Ctrl+H still arrives through the final
    // printable letter and is encoded at the PTY boundary.
    assert!(should_drop_bare_ctrl_marker("\u{0008}", true, true));
    assert!(!should_drop_bare_ctrl_marker("\u{0008}", true, false));
    assert!(!should_drop_bare_ctrl_marker("\u{0008}", false, true));
    assert_eq!(key_to_pty_bytes("h", true, false, false), vec![0x08]);
}

#[test]
fn alt_letter_still_sends_esc_prefix() {
    // Alt+a (a real Meta combo) must still send ESC + 'a'.
    assert_eq!(key_to_pty_bytes("a", false, true, false), vec![0x1b, b'a']);
}

#[test]
fn split_proxy_recognises_schemes() {
    assert_eq!(split_proxy(""), ("none".into(), "".into()));
    assert_eq!(
        split_proxy("http://10.0.0.1:1022"),
        ("http".into(), "10.0.0.1:1022".into())
    );
    assert_eq!(
        split_proxy("socks5://127.0.0.1:1080"),
        ("socks5".into(), "127.0.0.1:1080".into())
    );
    // user:pass survive in the host:port part.
    assert_eq!(
        split_proxy("http://u:p@host:8080"),
        ("http".into(), "u:p@host:8080".into())
    );
    // bare host:port (legacy) → treated as socks5.
    assert_eq!(
        split_proxy("127.0.0.1:1080"),
        ("socks5".into(), "127.0.0.1:1080".into())
    );
}

#[test]
fn compose_proxy_splits_credentials_out_of_the_url() {
    // 老配置把凭据写在 URL 里：保存时拆到独立字段，URL 只剩 host:port。
    let (url, user, pass) = compose_proxy("http", "u:p@host:8080", "", "", None);
    assert_eq!(url, "http://host:8080");
    assert_eq!(user, "u");
    assert_eq!(pass.as_str(), "p");

    // 独立字段优先于 URL 里的用户名。
    let (url, user, pass) = compose_proxy("socks5", "u:p@host:1080", "bob", "", None);
    assert_eq!(url, "socks5://host:1080");
    assert_eq!(user, "bob");
    assert_eq!(pass.as_str(), "p", "密码仍取自 URL");
}

#[test]
fn compose_proxy_keeps_saved_password_only_for_the_same_user() {
    let mut existing = Session::new_empty();
    existing.proxy = "socks5://host:1080".into();
    existing.proxy_user = "bob".into();
    existing.proxy_pass = Secret::new("s3cret");

    // 密码框留空 + 用户名没变 → 沿用旧密码（与登录密码同一口径）。
    let (_, user, pass) =
        compose_proxy("socks5", "host:1080", "bob", "", Some(&existing));
    assert_eq!(user, "bob");
    assert_eq!(pass.as_str(), "s3cret");

    // 换了用户名还留空 → 旧密码不能跟过去。
    let (_, user, pass) =
        compose_proxy("socks5", "host:1080", "alice", "", Some(&existing));
    assert_eq!(user, "alice");
    assert!(pass.is_empty());

    // 显式输入覆盖一切。
    let (_, _, pass) =
        compose_proxy("socks5", "host:1080", "bob", "newpass", Some(&existing));
    assert_eq!(pass.as_str(), "newpass");
}

#[test]
fn compose_proxy_none_clears_credentials() {
    let mut existing = Session::new_empty();
    existing.proxy_user = "bob".into();
    existing.proxy_pass = Secret::new("s3cret");
    let (url, user, pass) = compose_proxy("none", "", "bob", "", Some(&existing));
    assert!(url.is_empty());
    assert!(user.is_empty());
    assert!(pass.is_empty());
}

#[test]
fn paste_normalizes_newlines_to_cr() {
    // CRLF (Windows clipboard) and LF both collapse to a single CR so a
    // backslash-continued multi-line command pastes intact.
    assert_eq!(
        normalize_pasted_newlines("sudo apt install \\\r\n  docker-ce", false),
        "sudo apt install \\\r  docker-ce"
    );
    assert_eq!(normalize_pasted_newlines("a\nb\nc", false), "a\rb\rc");
    // A lone CR is left as-is; no doubling.
    assert_eq!(normalize_pasted_newlines("a\rb", false), "a\rb");
    // No newlines → unchanged.
    assert_eq!(normalize_pasted_newlines("echo hi", false), "echo hi");
}

#[test]
fn command_bar_preserves_multiline_heredoc() {
    let command = "cat <<'EOF'\nHEREDOC-1\n中文-HEREDOC-2\nEOF\n";
    let (history, bytes) = encode_command_bar_input(command).unwrap();
    assert_eq!(history, command.trim_end());
    assert_eq!(bytes, command.as_bytes());
    assert!(!history.lines().any(|line| line.starts_with(' ')));
}

#[test]
fn paste_uses_remote_bracketed_paste_mode() {
    assert_eq!(
        encode_pasted_text("first\r\n  second", true, false),
        b"\x1b[200~first\r  second\x1b[201~"
    );
    assert_eq!(
        encode_pasted_text("safe\x1b[201~\x03text", true, false),
        b"\x1b[200~safe[201~text\x1b[201~"
    );
    assert_eq!(
        encode_pasted_text("first\r\nsecond", false, false),
        b"first\rsecond"
    );
}

#[test]
fn long_pastes_switch_to_large_review() {
    assert!(!paste_requires_large_review("short prompt\nsecond line"));
    assert!(!paste_requires_large_review(&"a".repeat(600)));
    assert!(paste_requires_large_review(&"a".repeat(601)));
    assert!(!paste_requires_large_review(&["line"; 12].join("\r\n")));
    assert!(paste_requires_large_review(&["line"; 13].join("\r\n")));
}

#[test]
fn confirmed_exit_never_reopens_close_prompt() {
    assert!(should_block_close(false, true));
    assert!(!should_block_close(false, false));
    assert!(!should_block_close(true, true));
    assert!(!should_block_close(true, false));
}

#[test]
fn redact_key_hides_secrets_but_keeps_control_codes() {
    // Empty key.
    assert_eq!(redact_key(""), "(empty)");
    // Control codes are shown as U+XXXX, printable chars are redacted.
    assert_eq!(redact_key("\x03"), "U+0003");
    assert_eq!(redact_key("secret"), "<6 printable redacted>");
    // Mixed: only the control bytes leak their codepoints.
    let mixed = redact_key("a\x1b[b\x7f");
    assert!(mixed.contains("U+001B"));
    assert!(mixed.contains("U+007F"));
    assert!(mixed.contains("printable redacted"));
    // Multi-byte printable chars count individually.
    assert_eq!(redact_key("密码"), "<2 printable redacted>");
}

#[test]
fn parse_tunnel_forward_validates_fields() {
    let f = parse_tunnel_forward(
        "local", "  web  ", " 127.0.0.1 ", "8080", "db.internal", "5432",
    )
    .unwrap();
    assert_eq!(f.kind, "local");
    assert_eq!(f.name, "web");
    assert_eq!(f.bind_addr, "127.0.0.1");
    assert_eq!(f.bind_port, 8080);
    assert_eq!(f.host, "db.internal");
    assert_eq!(f.host_port, 5432);

    let d = parse_tunnel_forward(
        "dynamic", "socks", "0.0.0.0", "1080", "", "ignored",
    )
    .unwrap();
    assert_eq!(d.kind, "dynamic");
    assert_eq!(d.host_port, 0);

    assert!(parse_tunnel_forward("remote", "x", "h", "1", "h", "2").is_none());
    assert!(parse_tunnel_forward("local", "x", "h", "abc", "h", "2").is_none());
    assert!(parse_tunnel_forward("local", "x", "h", "8080", "h", "x").is_none());
    assert!(parse_tunnel_forward("local", "x", "h", "65536", "h", "2").is_none());
}

#[test]
fn tab_title_len_counts_wide_chars_as_two() {
    assert_eq!(tab_title_len(""), 0);
    assert_eq!(tab_title_len("abc"), 3);
    assert_eq!(tab_title_len("中文"), 4);
    assert_eq!(tab_title_len("a中文b"), 1 + 4 + 1);
}

#[test]
fn is_special_partition_filters_pseudo_fs() {
    assert!(is_special_partition("/proc"));
    assert!(is_special_partition("/proc/cpuinfo"));
    assert!(is_special_partition("/sys"));
    assert!(is_special_partition("/dev/sda1"));
    assert!(is_special_partition("/run"));
    assert!(is_special_partition("/tmp"));
    assert!(is_special_partition("/snap/core20/1234"));
    assert!(is_special_partition("/var/snap/foo"));
    assert!(is_special_partition("/boot/efi"));
    // Real user mounts stay visible.
    assert!(!is_special_partition("/"));
    assert!(!is_special_partition("/home"));
    assert!(!is_special_partition("/mnt/data"));
    assert!(!is_special_partition("/Users/zheny"));
}
