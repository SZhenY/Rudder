use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

/// Commands sent to the SFTP worker task from the UI thread.
#[derive(Debug)]
pub enum SftpCommand {
    /// List the contents of a remote directory.
    ListDir(String),
    /// Refresh button: re-list the directory *and* re-sync the whole expanded
    /// left tree, so external/own changes (deleted/created dirs) show up without
    /// a reconnect (#189). Plain navigation uses `ListDir` to avoid the extra
    /// per-click tree round-trips.
    RefreshDir(String),
    /// Toggle a directory node in the tree (expand if collapsed, collapse if expanded).
    ToggleTreeNode(String),
    /// Download a remote file to a local directory.
    Download { remote: String, local_dir: String },
    /// Multi-select download (#100): tar the named entries under `remote_dir`
    /// into one archive on the remote, download it, then delete the temp.
    DownloadArchive {
        remote_dir: String,
        names: Vec<String>,
        local_dir: String,
    },
    /// Cancel an in-progress transfer by its id (#100). The partial local file
    /// (and any remote temp archive) are cleaned up.
    CancelTransfer(String),
    /// Upload a local file into a remote directory.
    Upload {
        local: PathBuf,
        remote_dir: String,
        cleanup_after: Option<PathBuf>,
    },
    /// Re-upload an externally edited temp copy to its exact original remote
    /// path (#318). Unlike `Upload`, the destination is the full remote file
    /// path rather than a directory, so the watcher cannot derive a wrong
    /// target from a prefixed temp filename.
    UploadEdited { local: PathBuf, remote: String },
    /// Copy remote entries from this session into another SFTP session.
    CopyTo {
        remotes: Vec<String>,
        target: UnboundedSender<SftpCommand>,
        target_dir: String,
    },
    /// Delete a remote file (falls back to removing an empty directory).
    Delete(String),
    /// Download a file to a temp dir and open it with the OS default app
    /// ("Open/Edit externally", #81). When `edit` is set, watch the temp copy
    /// and re-upload on every change.
    OpenTemp { remote: String, edit: bool },
    /// Rename / move a remote file or directory (#69).
    Rename { from: String, to: String },
    /// Change a remote path's permission bits (POSIX mode, e.g. 0o755) (#69).
    Chmod { path: String, mode: u32 },
    /// Create an empty remote directory (#69).
    MkDir(String),
    /// Create an empty remote file (#69).
    TouchFile(String),
    /// Read a remote file's text for the built-in viewer/editor (#70).
    ReadText { remote: String, edit: bool },
    /// Overwrite a remote file with text from the built-in editor (#70).
    WriteText { remote: String, content: String },
    /// Gracefully shut down the SFTP worker.
    Close,
}

/// Handle retained by the UI to drive a running SFTP worker.
pub struct SftpHandle {
    pub commands: UnboundedSender<SftpCommand>,
    #[allow(dead_code)]
    pub join: JoinHandle<()>,
}

pub(crate) type SftpHandles = Arc<Mutex<HashMap<String, SftpHandle>>>;

/// Last terminal cwd followed by each SFTP panel.
pub(crate) type SftpLastCwd = Arc<Mutex<HashMap<String, String>>>;
