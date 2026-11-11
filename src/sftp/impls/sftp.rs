//! SFTP subsystem worker.
//!
//! Each terminal tab that spawns an SSH shell also spawns a *separate* SSH
//! connection for SFTP. This keeps the shell PTY completely unblocked: large
//! file transfers cannot stall readline or vim.
//!
//! The public API is a simple command channel (`SftpHandle::commands`) that
//! accepts `SftpCommand` messages. Results and status updates are pushed back
//! via the shared `UnboundedSender<SessionEvent>` that already exists for the
//! terminal tab.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use uuid::Uuid;

use anyhow::{Context, Result, anyhow};
use futures::stream::{FuturesUnordered, StreamExt};
use russh::Disconnect;
use russh::client::{self, Handler};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::HashAlg;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::{RawSftpSession, SftpSession};
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use super::transfer::{DownloadConflict, SftpCommand, SftpHandle};
use crate::config::{AuthMethod, Session};
use crate::i18n::t;
use crate::ssh::{RemoteEntry, RemoteTreeNode, SessionEvent};



#[path = "handles.rs"] mod handles;
#[path = "launcher.rs"] mod launcher;
pub(crate) use launcher::*;
#[path = "tree.rs"] mod tree;
pub(crate) use tree::*;
#[path = "editor.rs"] mod editor;
pub(crate) use editor::*;
#[path = "remote.rs"] mod remote;
pub(crate) use remote::*;
#[path = "paths.rs"] mod paths;
pub(crate) use paths::*;
#[path = "listing.rs"] mod listing;
pub(crate) use listing::*;
#[path = "download.rs"] mod download;
pub(crate) use download::*;
#[path = "dir_ops.rs"] mod dir_ops;
pub(crate) use dir_ops::*;
#[path = "upload.rs"] mod upload;
pub(crate) use upload::*;
#[path = "handler.rs"] mod handler;
pub(crate) use handler::*;

#[cfg(test)]
mod release_tests {
    use super::*;

    #[test]
    fn repeated_collapse_restarts_timer_and_cancel_invalidates_it() {
        let id = "sftp-release-test-tab";
        cancel_sftp_release(id);

        let first = bump_release_generation(id);
        assert!(release_is_current(id, first));

        // 再折叠一次 = 重新计时；第一次的定时器必须失效。
        let second = bump_release_generation(id);
        assert_eq!(second, first + 1);
        assert!(!release_is_current(id, first), "旧定时器不能再断开连接");
        assert!(release_is_current(id, second));

        // 5 秒内展开（或手动刷新）→ 待释放被撤销。
        cancel_sftp_release(id);
        assert!(!release_is_current(id, second));
        let pending = release_gen().lock().unwrap_or_else(|err| err.into_inner());
        assert!(!pending.contains_key(id));
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tree state helpers
// ---------------------------------------------------------------------------

/// Recursively build the flat node list from tree state (DFS pre-order).
fn build_tree_nodes(
    path: &str,
    depth: u32,
    expanded: &std::collections::HashSet<String>,
    tree_dirs: &std::collections::HashMap<String, Vec<(String, String)>>,
    nodes: &mut Vec<RemoteTreeNode>,
) {
    let name = if path == "/" {
        "/".to_string()
    } else {
        path.rsplit('/').next().unwrap_or(path).to_string()
    };
    let children = tree_dirs.get(path);
    let has_children = children.map(|c| !c.is_empty()).unwrap_or(true);
    let is_expanded = expanded.contains(path);
    nodes.push(RemoteTreeNode {
        path: path.to_string(),
        name,
        depth,
        expanded: is_expanded,
        has_children,
    });
    if is_expanded && let Some(ch) = children {
        for (_, child_path) in ch {
            build_tree_nodes(child_path, depth + 1, expanded, tree_dirs, nodes);
        }
    }
}


// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

#[path = "sftp_pump.rs"] mod sftp_pump;
pub(crate) use sftp_pump::*;








// ---------------------------------------------------------------------------
// russh client handler — verifies the host key against known_hosts, reusing the
// shell session's prompt path (#109-5). The UI de-duplicates by host:port, so a
// fresh host confirmed for the shell won't prompt again for SFTP.

#[cfg(test)]
mod sanitize_tests {
    use super::{
        EditorTextRejection, MAX_BUILTIN_EDITOR_BYTES, MAX_BUILTIN_EDITOR_LINE_BYTES,
        MAX_BUILTIN_EDITOR_LINES, available_download_path, download_target_path,
        external_edit_local_name, sanitize_filename, validate_editor_text,
    };

    #[test]
    fn external_edits_include_the_host_in_the_local_name() {
        assert_eq!(
            external_edit_local_name("192.168.1.10", "nginx.conf", "edit-1"),
            "192.168.1.10_edit-1_nginx.conf"
        );
        assert_ne!(
            external_edit_local_name("server-a", "nginx.conf", "edit-1"),
            external_edit_local_name("server-b", "nginx.conf", "edit-1")
        );
        assert_ne!(
            external_edit_local_name("server-a", "nginx.conf", "edit-1"),
            external_edit_local_name("server-a", "nginx.conf", "edit-2")
        );
    }

    #[test]
    fn plain_names_pass_through() {
        assert_eq!(sanitize_filename("report.txt"), "report.txt");
        assert_eq!(sanitize_filename("my-file_v2.tar.gz"), "my-file_v2.tar.gz");
        assert_eq!(sanitize_filename("数据.csv"), "数据.csv");
        // Unix dotfiles keep their leading dot.
        assert_eq!(sanitize_filename(".bashrc"), ".bashrc");
    }

    #[test]
    fn strips_path_separators_and_traversal() {
        // base_name already strips dirs, but sanitize is defence-in-depth: the
        // result must never keep a separator that could escape the target dir.
        assert_eq!(sanitize_filename("a/b\\c"), "a_b_c");
        let traversal = sanitize_filename("../../etc/passwd");
        assert!(!traversal.contains('/') && !traversal.contains('\\'));
        let win = sanitize_filename("..\\..\\Windows\\System32");
        assert!(!win.contains('/') && !win.contains('\\'));
    }

    #[test]
    fn replaces_shell_and_windows_special_chars() {
        assert_eq!(sanitize_filename("foo&calc.exe"), "foo_calc.exe");
        assert_eq!(sanitize_filename("a|b>c<d:e?f*g"), "a_b_c_d_e_f_g");
        assert_eq!(sanitize_filename("$(whoami)"), "_(whoami)");
        assert_eq!(sanitize_filename("a`b'c"), "a_b_c");
    }

    #[test]
    fn trims_whitespace_and_trailing_dots() {
        assert_eq!(sanitize_filename("   spaced.txt  "), "spaced.txt");
        assert_eq!(sanitize_filename("name..."), "name");
        // control chars become underscores, not trimmed
        assert_eq!(sanitize_filename("a\tb"), "a_b");
    }

    #[test]
    fn neutralises_windows_reserved_device_names() {
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("nul"), "_nul");
        assert_eq!(sanitize_filename("COM1"), "_COM1");
        assert_eq!(sanitize_filename("LPT9.txt"), "_LPT9.txt"); // reserved even with ext
        assert_eq!(sanitize_filename("Aux.log"), "_Aux.log");
        // Not reserved: a name that merely starts with the same letters.
        assert_eq!(sanitize_filename("console.txt"), "console.txt");
        assert_eq!(sanitize_filename("COM10"), "COM10");
    }

    #[test]
    fn empty_or_all_bad_falls_back() {
        assert_eq!(sanitize_filename(""), "file");
        assert_eq!(sanitize_filename("   "), "file");
        assert_eq!(sanitize_filename("..."), "file");
    }

    #[test]
    fn keep_both_uses_the_first_available_numbered_name() {
        let dir = std::env::temp_dir().join(format!(
            "rudder-download-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let requested = download_target_path("/remote/report.txt", dir.to_str().unwrap());
        std::fs::write(&requested, b"old").unwrap();
        std::fs::write(dir.join("report (1).txt"), b"older").unwrap();

        assert_eq!(
            available_download_path(&requested),
            dir.join("report (2).txt")
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keep_both_numbers_extensionless_names_too() {
        // The no-extension branch used to be reachable only via the same
        // unreachable!() tail; lock its "(n)" shape down as well.
        let dir = std::env::temp_dir().join(format!(
            "rudder-download-noext-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let requested = dir.join("Makefile");
        std::fs::write(&requested, b"old").unwrap();

        assert_eq!(
            available_download_path(&requested),
            dir.join("Makefile (1)")
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn download_target_path_sanitises_and_joins() {
        let dir = std::env::temp_dir().join("rudder-target-test");
        std::fs::create_dir_all(&dir).unwrap();
        // A hostile remote name is sanitised before it hits the local fs (#26).
        let path = download_target_path("/etc/../evil name?.txt", dir.to_str().unwrap());
        assert_eq!(
            path,
            dir.join("evil name_.txt"),
            "slash and '?' must be neutralised"
        );
        // A trailing slash on the directory is tolerated.
        let path2 = download_target_path("/remote/ok.txt", &format!("{}/", dir.display()));
        assert_eq!(path2, dir.join("ok.txt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn editor_rejects_content_over_the_safe_byte_limit() {
        let bytes = vec![b'a'; MAX_BUILTIN_EDITOR_BYTES + 1];
        assert_eq!(
            validate_editor_text(bytes),
            Err(EditorTextRejection::TooLarge)
        );
    }

    #[test]
    fn editor_rejects_pathological_text_layouts() {
        let lines = vec![b'\n'; MAX_BUILTIN_EDITOR_LINES];
        assert_eq!(
            validate_editor_text(lines),
            Err(EditorTextRejection::TooManyLines)
        );
        let line = vec![b'a'; MAX_BUILTIN_EDITOR_LINE_BYTES + 1];
        assert_eq!(
            validate_editor_text(line),
            Err(EditorTextRejection::LineTooLong)
        );
    }

    #[test]
    fn editor_accepts_regular_utf8_text_within_limits() {
        assert_eq!(
            validate_editor_text("第一行\nsecond line\n".as_bytes().to_vec()),
            Ok("第一行\nsecond line\n".into())
        );
    }
}
