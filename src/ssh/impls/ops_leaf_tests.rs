use super::*;

#[test]
fn stage_names_cover_every_known_stage() {
    let expected = [
        (0, "open-channel"),
        (1, "request-pty"),
        (2, "exec-sudo"),
        (3, "wait-password-prompt"),
        (5, "wait-exit-status"),
        (6, "completed"),
    ];
    for (stage, name) in expected {
        assert_eq!(process_control_stage_name(stage), name);
    }
    assert_eq!(process_control_stage_name(4), "unknown");
    assert_eq!(process_control_stage_name(200), "unknown");
}

#[test]
fn failure_message_prefers_remote_detail() {
    let message = process_kill_failure_message("permission\r\ndenied\n", false);
    assert!(message.contains("permission denied"), "{message}");
}

#[test]
fn failure_message_falls_back_per_privilege() {
    let plain = process_kill_failure_message("  ", false);
    let sudo = process_kill_failure_message("", true);
    assert!(!plain.is_empty());
    assert!(!sudo.is_empty());
    assert_ne!(plain, sudo);
}

#[test]
fn diagnostic_text_caps_length_and_keeps_plain_output() {
    let long = "x".repeat(700);
    let safe = process_control_log_text(&long, None);
    assert_eq!(safe.chars().count(), 512);
    assert_eq!(
        process_control_log_text("  hello   world  ", None),
        "hello world"
    );
    assert_eq!(
        process_control_log_text("no secret here", Some("")),
        "no secret here"
    );
}
