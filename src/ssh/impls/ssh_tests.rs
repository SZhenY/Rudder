#[cfg(test)]
mod prompt_setup_echo_tests {
    use super::super::{
        PROMPT_BODY, PROMPT_SETUP_HISTORY_MARKER, PROMPT_SETUP_PREFIX, accumulate_late_echo,
        discard_prompt_setup_tail, final_setup_guard, prompt_setup_echo_end,
        prompt_setup_supported, release_after_window_expiry, strip_late_prompt_setup_echo,
        strip_prompt_setup_echo,
    };

    #[test]
    fn only_bash_and_zsh_receive_prompt_setup() {
        assert_eq!(
            prompt_setup_supported("__MEATSHELL_SHELL__:bash\n"),
            Some(true)
        );
        assert_eq!(
            prompt_setup_supported("__MEATSHELL_SHELL__:zsh\n"),
            Some(true)
        );
        assert_eq!(
            prompt_setup_supported("__MEATSHELL_SHELL__:other\n"),
            Some(false)
        );
        assert_eq!(prompt_setup_supported("ash: syntax error\n"), None);
    }

    #[test]
    fn bash_setup_removes_current_and_stale_history_entries() {
        assert!(PROMPT_BODY.contains(PROMPT_SETUP_HISTORY_MARKER));
        assert!(PROMPT_BODY.contains("history 2>/dev/null"));
        assert!(PROMPT_BODY.contains("__ms7()"));
        assert!(PROMPT_BODY.contains("history -d \"$__mn\""));
        // Re-prime command capture only after deleting the setup entry, so the
        // previous real user command does not get reported as newly executed.
        assert!(PROMPT_BODY.find("history -d").unwrap() < PROMPT_BODY.rfind("__cl=").unwrap());
    }

    #[test]
    fn strips_oh_my_zsh_echo_without_newline() {
        let mut text = format!(
            "➜  ~  {} && eval 'body; __ms7'\rafter prompt",
            PROMPT_SETUP_PREFIX
        );
        let p = text.find(PROMPT_SETUP_PREFIX).unwrap();
        let end = prompt_setup_echo_end(&text, p).unwrap();
        strip_prompt_setup_echo(&mut text, p, end);
        assert_eq!(text, "\r\x1b[2Kafter prompt");
    }

    #[test]
    fn strips_echo_through_osc7() {
        let mut text = format!(
            "banner\n➜  ~  {} && eval 'body; __ms7'\r\u{1b}]7;file://host/home/jeff\u{07}prompt",
            PROMPT_SETUP_PREFIX
        );
        let p = text.find(PROMPT_SETUP_PREFIX).unwrap();
        let osc_end = text.find("prompt").unwrap();
        strip_prompt_setup_echo(&mut text, p, osc_end);
        assert_eq!(text, "banner\n\r\x1b[2Kprompt");
    }

    #[test]
    fn strips_late_echoed_setup_command() {
        let mut text = format!(
            "prompt\r\n{} && eval 'body; __ms7'\r\nafter",
            PROMPT_SETUP_PREFIX
        );
        assert!(strip_late_prompt_setup_echo(&mut text));
        assert_eq!(text, "prompt\r\n\r\x1b[2Kafter");
    }

    #[test]
    fn late_echo_accumulates_split_command_across_chunks() {
        // A slow PTY can deliver the echoed setup line split across several
        // chunks. Per-chunk stripping fails forever once the prefix has
        // already been released, so the accumulator must hold and combine.
        let echoed = format!(
            "banner\r\n{} && eval 'x; __ms7'\r\n\u{1b}]7;file://host/root\u{07}prompt",
            PROMPT_SETUP_PREFIX
        );
        let mut buf = String::new();
        let third = echoed.len() / 3;
        let mut out = String::new();
        let mut pending = true;
        for part in [
            &echoed[..third],
            &echoed[third..2 * third],
            &echoed[2 * third..],
        ] {
            if !pending {
                // Once the accumulator reports done, the caller (see the
                // late-echo handling in the session loop) stops routing
                // chunks through it and renders them as-is.
                out.push_str(part);
                continue;
            }
            let (release, d) = accumulate_late_echo(&mut buf, part);
            out.push_str(&release);
            if d {
                pending = false;
            }
        }
        assert!(!pending, "accumulator should complete the strip");
        assert!(!out.contains(PROMPT_SETUP_PREFIX));
        assert!(out.contains("banner"));
        assert!(out.contains("prompt"));
    }

    #[test]
    fn late_echo_matches_prefix_split_across_chunks() {
        // The prefix itself can be split by a chunk boundary; the overlap
        // window must let it match once the remainder arrives.
        let mut buf = String::new();
        let mid = PROMPT_SETUP_PREFIX.len() / 2;
        let (t1, done1) = accumulate_late_echo(&mut buf, &PROMPT_SETUP_PREFIX[..mid]);
        assert!(t1.is_empty(), "short fragment should be held");
        assert!(!done1);
        let rest = format!(
            "{} && eval 'x; __ms7'\r\nafter",
            &PROMPT_SETUP_PREFIX[mid..]
        );
        let (t2, done2) = accumulate_late_echo(&mut buf, &rest);
        assert!(done2);
        assert!(!t2.contains(PROMPT_SETUP_PREFIX));
        assert!(t2.contains("after"));
    }

    #[test]
    fn late_echo_holds_incomplete_line_until_suffix_lands() {
        let mut buf = String::new();
        // Prefix landed but the command line has not finished echoing.
        let head = format!("{} && eval 'x; __ms7", PROMPT_SETUP_PREFIX);
        let (t1, done1) = accumulate_late_echo(&mut buf, &head);
        assert!(t1.is_empty());
        assert!(!done1);
        assert!(!buf.is_empty());
        // Suffix + OSC 7 + prompt arrive: strip the whole span.
        let (t2, done2) =
            accumulate_late_echo(&mut buf, "'\r\n\u{1b}]7;file://host/root\u{07}prompt");
        assert!(done2);
        assert!(!t2.contains(PROMPT_SETUP_PREFIX));
        assert!(t2.contains("prompt"));
    }

    #[test]
    fn late_echo_releases_unrelated_output_keeping_overlap() {
        let mut buf = String::new();
        let (t1, done1) = accumulate_late_echo(&mut buf, &"x".repeat(100));
        assert!(t1.len() < 100, "trailing overlap window must be retained");
        assert!(!done1);
        assert!(!t1.contains(PROMPT_SETUP_PREFIX));
        let (t2, done2) = accumulate_late_echo(&mut buf, &"y".repeat(50));
        assert!(!done2);
        assert_eq!(t1.len() + t2.len() + buf.len(), 150, "no bytes lost");
    }

    #[test]
    fn late_echo_cap_fallback_releases_held_line() {
        let mut buf = String::new();
        let head = format!("{} && eval 'incomplete", PROMPT_SETUP_PREFIX);
        let (t1, _) = accumulate_late_echo(&mut buf, &head);
        assert!(t1.is_empty());
        let pad = "z".repeat(1 << 14); // exceeds the 16 KiB cap
        let (t2, done2) = accumulate_late_echo(&mut buf, &pad);
        assert!(done2, "cap fallback must end the late-echo state");
        assert!(!t2.contains(PROMPT_SETUP_PREFIX));
    }

    #[test]
    fn echo_end_is_none_while_line_incomplete() {
        // The suffix is the command's final characters; without it the line is
        // still in flight and the caller must keep waiting — never fall back
        // to a line end (the wrapped command has many rows).
        let text = format!("{} && eval 'x; __ms7", PROMPT_SETUP_PREFIX);
        let p = text.find(PROMPT_SETUP_PREFIX).unwrap();
        assert_eq!(prompt_setup_echo_end(&text, p), None);
        let complete = format!("{} && eval 'x; __ms7'\r\n", PROMPT_SETUP_PREFIX);
        let p = complete.find(PROMPT_SETUP_PREFIX).unwrap();
        assert!(prompt_setup_echo_end(&complete, p).is_some());
    }

    #[test]
    fn window_expiry_discards_wrapped_echo_but_keeps_prefix_free_text() {
        // Simulates the second-session failure mode: the ~1.4 KiB setup
        // command wraps across many rows and the OSC 7 never arrives before
        // the 10 s late-echo window expires. The old code cut at the first
        // newline and leaked the remaining wrapped rows; now the whole tail is
        // discarded, only text before the command line survives.
        let wrapped = format!(
            "banner\r\n{} && eval 'row1; row2; row3; __ms7", // incomplete: no suffix
            PROMPT_SETUP_PREFIX
        );
        let mut buf = String::new();
        let release = release_after_window_expiry(&mut buf, &wrapped);
        assert!(
            !release.contains(PROMPT_SETUP_PREFIX),
            "wrapped echo must not leak"
        );
        assert!(release.contains("banner"), "pre-echo text survives");
        assert_eq!(
            release.find("\r\x1b[2K"),
            Some(release.len() - "\r\x1b[2K".len())
        );
    }

    #[test]
    fn window_expiry_passes_normal_output_through() {
        let mut buf = String::new();
        let release = release_after_window_expiry(&mut buf, "normal output after window");
        assert_eq!(release, "normal output after window");
    }

    #[test]
    fn cap_discards_wrapped_echo_whole() {
        // Cap fallback with the prefix landed but no suffix: the accumulator
        // must discard the entire wrapped remainder, not cut at a newline.
        let mut buf = String::new();
        let head = format!("{} && eval 'x; __ms7", PROMPT_SETUP_PREFIX);
        let (t1, _) = accumulate_late_echo(&mut buf, &head);
        assert!(t1.is_empty());
        let pad = "z".repeat(1 << 14);
        let (t2, done2) = accumulate_late_echo(&mut buf, &pad);
        assert!(done2);
        assert!(!t2.contains(PROMPT_SETUP_PREFIX));
        assert_eq!(t2.find("\r\x1b[2K"), Some(t2.len() - "\r\x1b[2K".len()));
    }

    #[test]
    fn discard_tail_clears_from_line_start() {
        // The discard starts at the beginning of the line the prefix sits on,
        // so a prompt/banner already on that line is wiped along with it.
        let mut text = format!("➜  ~  {} && eval 'incomplete", PROMPT_SETUP_PREFIX);
        let p = text.find(PROMPT_SETUP_PREFIX).unwrap();
        discard_prompt_setup_tail(&mut text, p);
        assert_eq!(text, "\r\x1b[2K");
    }

    #[test]
    fn final_guard_strips_fully_landed_echo() {
        // The exact shape the user reported: the complete setup command echoed
        // back in one block. Must be stripped no matter what the suppression
        // state machine thinks.
        let mut text = format!(
            "banner\r\n{} && eval 'body; __ms7'\r\n\u{1b}]7;file://host/root\u{07}prompt",
            PROMPT_SETUP_PREFIX
        );
        assert!(final_setup_guard(&mut text));
        assert!(!text.contains(PROMPT_SETUP_PREFIX), "echo must not leak");
        assert!(text.contains("banner"), "pre-echo text survives");
        assert!(text.contains("prompt"));
    }

    #[test]
    fn final_guard_strips_wrapped_partial_without_suffix() {
        // The wrapped command is still in flight (no suffix yet): the guard
        // cuts to the end of the block rather than a newline, so no row of the
        // wrapped echo can leak. Only text before the command line survives.
        let mut text = format!(
            "banner\r\n{} && eval 'row1; row2; row3; __ms7",
            PROMPT_SETUP_PREFIX
        );
        assert!(final_setup_guard(&mut text));
        assert!(!text.contains(PROMPT_SETUP_PREFIX));
        assert!(text.contains("banner"));
    }

    #[test]
    fn final_guard_passes_normal_output() {
        let mut text = "ls -la\r\nhello world".to_string();
        assert!(!final_setup_guard(&mut text));
        assert_eq!(text, "ls -la\r\nhello world");
        // The prefix inside a larger unrelated word must not match either.
        let mut text2 = "x test -z \"$FISH_VERSION\" is not a real prefix".to_string();
        assert!(final_setup_guard(&mut text2));
        assert!(!text2.contains(PROMPT_SETUP_PREFIX));
    }

    #[test]
    fn hidden_setup_echo_resynchronizes_the_prompt_cursor() {
        let prompt = "root@host:~# ";
        let (mut term, mut processor) = crate::terminal::new_term(4, 80, 0);
        // The initial prompt is painted immediately before shell integration is
        // injected. The buffered setup echo must replace, not append to, it.
        crate::terminal::process_bytes(&mut processor, &mut term, prompt.as_bytes());
        let mut echoed = format!(
            "{prompt}{} && eval 'body; __ms7'\r\n\u{1b}]7;file://host/root\u{07}{prompt}",
            PROMPT_SETUP_PREFIX
        );
        let prefix = echoed.find(PROMPT_SETUP_PREFIX).unwrap();
        let osc_end = echoed.rfind(prompt).unwrap();
        strip_prompt_setup_echo(&mut echoed, prefix, osc_end);
        crate::terminal::process_bytes(&mut processor, &mut term, echoed.as_bytes());

        let lines = crate::terminal::vt_adapter::grid_to_lines(&term);
        // grid_to_lines trims trailing blanks (grid padding vs content spaces
        // can't be distinguished), so compare trimmed.
        assert_eq!(lines.first().map(|s| s.trim_end()), Some(prompt.trim_end()));
        assert_eq!(crate::terminal::cursor_pos(&term), (0, prompt.len() as u16));
    }
}

#[cfg(test)]
mod zmodem_detection_tests {
    use super::super::{ZmodemDirection, zmodem_direction};

    #[test]
    fn distinguishes_remote_sz_from_remote_rz() {
        assert_eq!(
            zmodem_direction(b"**\x18B00000000000000\r\n"),
            Some(ZmodemDirection::Download)
        );
        assert_eq!(
            zmodem_direction(b"rz waiting to receive.**\x18B0100000023be50\r\n\x11"),
            Some(ZmodemDirection::Upload)
        );
    }

    #[test]
    fn ignores_non_handshake_frames_and_ctrl_x() {
        assert_eq!(zmodem_direction(b"plain \x18 text"), None);
        assert_eq!(zmodem_direction(b"**\x18B08000000000000\r\n"), None);
    }
}

#[cfg(test)]
mod osc_command_tests {
    use super::super::extract_osc_command;

    #[test]
    fn extracts_and_locates_bel_terminated() {
        let text = "before\u{1b}]697;ls -la\u{07}after";
        let (cmd, range) = extract_osc_command(text).expect("found");
        assert_eq!(cmd, "ls -la");
        // Stripping the range leaves the surrounding text intact.
        let mut s = text.to_string();
        s.replace_range(range, "");
        assert_eq!(s, "beforeafter");
    }

    #[test]
    fn extracts_st_terminated() {
        let text = "\u{1b}]697;echo hi\u{1b}\\";
        let (cmd, _) = extract_osc_command(text).expect("found");
        assert_eq!(cmd, "echo hi");
    }

    #[test]
    fn ignores_other_osc_and_incomplete() {
        // OSC 7 (cwd) is not a command sequence.
        assert!(extract_osc_command("\u{1b}]7;file:///home\u{07}").is_none());
        // No terminator yet → wait for more.
        assert!(extract_osc_command("\u{1b}]697;ls").is_none());
        assert!(extract_osc_command("plain text").is_none());
    }
}

#[cfg(test)]
mod auxiliary_startup_tests {
    use super::super::{AuxiliaryChannelKind, AuxiliaryStartup, auxiliary_timeout};

    #[test]
    fn starts_channels_in_order_and_never_overlaps_them() {
        let mut startup = AuxiliaryStartup::new(true);

        assert_eq!(startup.begin(), Some(AuxiliaryChannelKind::Resources));
        assert_eq!(startup.begin(), None);
        startup.complete();

        assert_eq!(startup.begin(), Some(AuxiliaryChannelKind::Processes));
        assert_eq!(startup.begin(), None);
        startup.complete();

        assert_eq!(startup.begin(), Some(AuxiliaryChannelKind::SystemInfo));
        startup.complete();
        assert_eq!(startup.begin(), None);
    }

    #[test]
    fn disabled_shell_integration_skips_all_auxiliary_channels() {
        let mut startup = AuxiliaryStartup::new(false);

        assert_eq!(startup.begin(), None);
    }

    #[tokio::test]
    async fn auxiliary_channel_startup_timeout_releases_the_pump() {
        let result = auxiliary_timeout(
            std::time::Duration::from_millis(1),
            std::future::pending::<()>(),
        )
        .await;

        assert!(result.is_err());
    }
}

#[cfg(test)]
mod monitor_hardening_tests {
    use super::super::{parse_df_line, parse_monitor_block, parse_process_block};
    use std::collections::HashMap;
    use std::time::Instant;

    #[test]
    fn df_line_saturates_instead_of_overflowing() {
        // avail/total near u64::MAX must not panic on the KiB->bytes multiply.
        let line = "/dev/sda1 18446744073709551615 0 18446744073709551615 100% /";
        let (_, avail, total) = parse_df_line(line).expect("parses");
        assert_eq!(avail, u64::MAX);
        assert_eq!(total, u64::MAX);
    }

    #[test]
    fn cpu_overflow_values_do_not_panic() {
        let big = u64::MAX;
        let block =
            format!("cpu {big} {big} {big} {big} {big}\nMemTotal: 1000 kB\nMemAvailable: 500 kB");
        let mut prev = None;
        let mut prev_net = HashMap::new();
        let mut at = Instant::now();
        // Must not panic; with no baseline the first sample reports 0% CPU.
        assert!(parse_monitor_block(&block, &mut prev, &mut prev_net, &mut at).is_some());
    }

    #[test]
    fn floods_of_fake_interfaces_are_capped() {
        let mut block = String::from("MemTotal: 1000 kB\nMemAvailable: 500 kB\n");
        for i in 0..500 {
            block.push_str(&format!("eth{i}: 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16\n"));
        }
        let mut prev = None;
        let mut prev_net = HashMap::new();
        let mut at = Instant::now();
        assert!(parse_monitor_block(&block, &mut prev, &mut prev_net, &mut at).is_some());
        // The remembered interface set is capped, not 500.
        assert!(prev_net.len() <= 64, "prev_net held {}", prev_net.len());
    }

    #[test]
    fn lightweight_resource_sample_does_not_replace_system_details() {
        let block = "cpu 1 2 3 4\nMemTotal: 1000 kB\nMemAvailable: 500 kB\n__DF__\n";
        let mut prev = None;
        let mut prev_net = HashMap::new();
        let mut at = Instant::now();
        let event = parse_monitor_block(block, &mut prev, &mut prev_net, &mut at).unwrap();
        match event {
            super::super::SessionEvent::ResourceStats { sys, .. } => assert!(sys.is_none()),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn delayed_system_sample_carries_detailed_information() {
        let block = "cpu 1 2 3 4\nMemTotal: 1000 kB\nMemAvailable: 500 kB\n__DF__\n__SYS__\nOS=Debian GNU/Linux 12\nKERNEL=Linux\n";
        let mut prev = None;
        let mut prev_net = HashMap::new();
        let mut at = Instant::now();
        let event = parse_monitor_block(block, &mut prev, &mut prev_net, &mut at).unwrap();
        match event {
            super::super::SessionEvent::ResourceStats { sys, .. } => {
                let sys = sys.expect("delayed sample should include details");
                assert!(
                    sys.overview
                        .iter()
                        .any(|(_, value)| value == "Debian GNU/Linux 12")
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn dedicated_process_block_reports_user_and_rows() {
        let (user, procs) = parse_process_block(
            "__ME__\nalice\n__PS__\nPID USER %CPU %MEM COMMAND\n42 root 3.5 1.2 java -jar demo.jar\n",
        );
        assert_eq!(user, "alice");
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].pid, 42);
        assert_eq!(procs[0].user, "root");
        assert_eq!(procs[0].command, "java -jar demo.jar");
    }
}

#[cfg(test)]
mod process_control_tests {
    use super::super::{
        looks_like_sudo_password_prompt, process_control_log_text, process_kill_command,
    };

    #[test]
    fn own_process_uses_plain_term_signal() {
        assert_eq!(process_kill_command(4242, false), "kill -TERM 4242");
    }

    #[test]
    fn privileged_process_uses_root_su_without_embedding_password() {
        assert_eq!(
            process_kill_command(4242, true),
            "LC_ALL=C sudo -S -p 'Password:' -- kill -TERM 4242"
        );
    }

    #[test]
    fn recognizes_su_password_prompt() {
        assert!(looks_like_sudo_password_prompt("Password: "));
        assert!(looks_like_sudo_password_prompt("请输入密码："));
        assert!(!looks_like_sudo_password_prompt("Authentication failure"));
    }

    #[test]
    fn diagnostic_output_redacts_password_and_controls() {
        let safe =
            process_control_log_text("Password:\r\nsecret-value\x1b[0m", Some("secret-value"));
        assert!(!safe.contains("secret-value"));
        assert!(safe.contains("[REDACTED]"));
        assert!(!safe.contains('\r'));
        assert!(!safe.contains('\n'));
    }
}

#[cfg(test)]
mod mfa_tests {
    use super::super::looks_like_mfa;

    #[test]
    fn password_prompts_are_not_mfa() {
        // These should be answered automatically with the stored password.
        for p in [
            "Password: ",
            "password:",
            "jeff@host's password:",
            "请输入密码:",
            "Password for jeff:",
        ] {
            assert!(!looks_like_mfa(p), "wrongly flagged as MFA: {p:?}");
        }
    }

    #[test]
    fn verification_code_prompts_are_mfa() {
        // These must prompt the user (JumpServer / Google Authenticator / Duo …).
        for p in [
            "MFA code: ",
            "[MFA] Please enter 6 digit code: ",
            "Verification code: ",
            "Verification code (from your authenticator app): ",
            "One-time password (OATH-TOTP): ",
            "Enter passcode or select one of the following options:",
            "Duo two-factor login",
            "请输入验证码:",
            "动态口令:",
            "请输入令牌:",
        ] {
            assert!(looks_like_mfa(p), "missed an MFA prompt: {p:?}");
        }
    }
}

#[cfg(test)]
mod connect_failure_tests {
    use super::super::classify_connect_failure;

    #[test]
    fn connect_failures_are_classified() {
        use anyhow::anyhow;

        // Assertions are bilingual: t() returns zh or en per runtime locale.

        let auth = classify_connect_failure(&anyhow!("authentication failed"));
        assert!(auth.contains("认证失败") || auth.contains("Authentication"));

        let timeout = classify_connect_failure(&anyhow!(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "read timed out"
        )));
        assert!(timeout.contains("超时") || timeout.contains("Timed out"));

        let refused = classify_connect_failure(&anyhow!(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused"
        )));
        assert!(refused.contains("拒绝") || refused.contains("refused"));

        let dns = classify_connect_failure(&anyhow!(
            "failed to lookup address information: Name or service not known"
        ));
        assert!(dns.contains("解析") || dns.contains("DNS"));

        let hostkey = classify_connect_failure(&anyhow!("host key verification failed"));
        assert!(hostkey.contains("主机密钥") || hostkey.contains("Host key"));
    }
}

#[cfg(test)]
mod client_config_tests {
    use super::super::{COMPAT_CIPHER, COMPAT_KEX, COMPAT_MAC, ssh_client_config};

    /// russh 0.65 的默认 MAC 去掉了 SHA-1（0.49 有）。兼容表必须自带 SHA-1，
    /// 且排在最后；同时确认这份表真的被送到 Config，而不只是定义着。
    #[test]
    fn legacy_fallbacks_reach_the_client_config() {
        let macs: Vec<&str> = COMPAT_MAC.iter().map(|m| m.as_ref()).collect();
        assert_eq!(macs.last(), Some(&"hmac-sha1"));
        assert!(macs.contains(&"hmac-sha1-etm@openssh.com"));
        assert!(COMPAT_CIPHER.iter().any(|c| c.as_ref() == "3des-cbc"));
        assert!(
            COMPAT_KEX
                .iter()
                .any(|k| k.as_ref() == "diffie-hellman-group1-sha1")
        );

        let config = ssh_client_config();
        assert_eq!(config.preferred.mac.as_ref(), COMPAT_MAC);
        assert_eq!(config.preferred.kex.as_ref(), COMPAT_KEX);
        assert_eq!(config.preferred.cipher.as_ref(), COMPAT_CIPHER);
        assert_eq!(
            config.keepalive_interval,
            Some(std::time::Duration::from_secs(30))
        );
    }
}
