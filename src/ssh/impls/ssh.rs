//! SSH session manager.
//!
//! Each open terminal tab maps to exactly one `SshSession`. The session runs
//! on the shared Tokio runtime; commands come in via an MPSC channel and
//! output lines are pushed back via an `UnboundedSender<SessionEvent>`.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use russh::client::{self, Handle, Handler, Msg};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::{HashAlg, PrivateKey, PublicKey, decode_secret_key, load_secret_key};
use russh::{Channel, ChannelId, ChannelMsg, Disconnect};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::config::{AuthMethod, PortForward, Secret, Session, SessionTrigger};
use crate::i18n::t;


#[path = "helpers.rs"] mod helpers;
pub(crate) use helpers::*;
#[path = "format.rs"] mod format;
pub(crate) use format::*;
#[path = "prompt.rs"] mod prompt;
pub(crate) use prompt::*;
#[path = "../struct/model.rs"] mod model;
pub(crate) use model::*;
#[path = "ops.rs"] mod ops;
pub(crate) use ops::*;
#[path = "monitor.rs"] mod monitor;
pub(crate) use monitor::*;
#[path = "handler.rs"] mod handler;
pub(crate) use handler::*;

#[cfg(test)]
mod size_format_tests {
    use super::format_size;

    #[test]
    fn formats_large_storage_units_through_exabytes() {
        const GIB: u64 = 1024_u64.pow(3);
        const TIB: u64 = 1024_u64.pow(4);
        const PIB: u64 = 1024_u64.pow(5);
        const EIB: u64 = 1024_u64.pow(6);

        assert_eq!(format_size(GIB), "1.0 GB");
        assert_eq!(format_size(TIB), "1.00 TB");
        assert_eq!(format_size(PIB), "1.00 PB");
        assert_eq!(format_size(EIB), "1.00 EB");
        assert_eq!(format_size(u64::MAX), "16.00 EB");
    }
}

#[cfg(test)]
mod session_trigger_tests {
    use super::TriggerEngine;
    use crate::config::{Secret, SessionTrigger};

    fn trigger(expect: &str, response: &str, repeat: bool) -> SessionTrigger {
        SessionTrigger {
            expect: expect.to_string(),
            response: Secret::new(response),
            append_enter: true,
            repeat,
        }
    }

    #[test]
    fn session_trigger_matches_across_output_chunks_once() {
        let mut engine = TriggerEngine::new(&[trigger("Password:", "secret", false)]);
        assert!(engine.feed("Pass").is_empty());
        let replies = engine.feed("word:");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].0.as_str(), "secret");
        assert!(replies[0].1);
        assert!(engine.feed("Password:").is_empty());
    }

    #[test]
    fn repeating_session_trigger_consumes_each_match() {
        let mut engine = TriggerEngine::new(&[trigger("continue?", "y", true)]);
        assert_eq!(engine.feed("continue?").len(), 1);
        assert!(engine.feed("unrelated output").is_empty());
        assert_eq!(engine.feed("continue?").len(), 1);
    }

    #[test]
    fn incomplete_rules_are_ignored() {
        // An empty response would otherwise fire on every chunk.
        let blank = SessionTrigger::default();
        let no_response = SessionTrigger {
            expect: "Password:".into(),
            ..SessionTrigger::default()
        };
        assert!(TriggerEngine::new(&[blank, no_response])
            .feed("Password:")
            .is_empty());
    }
}


#[cfg(test)]
mod permission_format_tests {
    use super::*;

    #[test]
    fn permissions_read_like_ls() {
        assert_eq!(format_permissions(0o755), "rwxr-xr-x");
        assert_eq!(format_permissions(0o644), "rw-r--r--");
        assert_eq!(format_permissions(0o600), "rw-------");
        assert_eq!(format_permissions(0), "---------");
    }

    #[test]
    fn setuid_setgid_sticky_override_the_execute_letter() {
        assert_eq!(format_permissions(0o4755), "rwsr-xr-x");
        assert_eq!(format_permissions(0o2755), "rwxr-sr-x");
        assert_eq!(format_permissions(0o1777), "rwxrwxrwt");
        // 特殊位在、但没有执行位 → 大写（与 ls 一致）。
        assert_eq!(format_permissions(0o4644), "rwSr--r--");
        assert_eq!(format_permissions(0o1666), "rw-rw-rwT");
    }

    #[test]
    fn file_type_comes_from_the_high_bits() {
        assert_eq!(file_type_label(0o040_755), "dir");
        assert_eq!(file_type_label(0o100_644), "file");
        assert_eq!(file_type_label(0o120_777), "link");
        assert_eq!(file_type_label(0o140_777), "socket");
        assert_eq!(file_type_label(0o010_644), "fifo");
        assert_eq!(file_type_label(0o020_644), "char");
        assert_eq!(file_type_label(0o060_644), "block");
        // 服务器没报权限 → 什么都不猜。
        assert_eq!(file_type_label(0), "?");
    }
}




#[path = "session_pump.rs"] mod session_pump;
pub(crate) use session_pump::*;


#[cfg(test)]
#[path = "ssh_tests.rs"]
mod ssh_tests_aggregate;
