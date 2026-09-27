use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use tokio::runtime::Runtime;

use crate::app::AppContext;
use crate::config::ConfigStore;
use crate::resource::{LocalSnap, NetHist, TabStatuses};
use crate::sftp::{SftpHandles, SftpLastCwd};
use crate::ssh::{CredentialResponder, HostKeyResponder, MfaResponder, SessionHandle};
use crate::terminal::{RenderGates, TermBuffers};
use crate::ui::AppWindow;

/// Shared dependencies for starting or reconnecting a session tab.
///
/// 全字段都是 `Rc` / `Arc` 句柄，克隆很便宜（共享同一份状态）—— 调用方常在闭包外
/// 装配一次、闭包内 `clone()` 使用（闭包不能捕获 `&AppContext`）。
#[derive(Clone)]
pub(crate) struct ConnectCtx {
    pub(crate) weak: slint::Weak<AppWindow>,
    pub(crate) runtime: Arc<Runtime>,
    pub(crate) handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
    pub(crate) sftp_handles: SftpHandles,
    pub(crate) sftp_last_cwd: SftpLastCwd,
    pub(crate) bufs: TermBuffers,
    pub(crate) render_gates: RenderGates,
    pub(crate) tab_statuses: TabStatuses,
    pub(crate) local_snap: LocalSnap,
    pub(crate) local_net_hist: NetHist,
    pub(crate) last_term_size: Arc<Mutex<(u32, u32)>>,
    pub(crate) sftp_follow_cd: Arc<AtomicBool>,
    pub(crate) store: Rc<RefCell<ConfigStore>>,
}

impl ConnectCtx {
    /// 从应用级上下文装配（`key_input` 与 `session_callbacks` 原先各自逐字写一遍
    /// 这 13 个字段 —— 漏掉一个就是静默丢功能，比如少 `sftp_follow_cd` 就跟不上
    /// 远端 cwd、少 `render_gates` 就不出帧）。
    pub(crate) fn from_app(weak: slint::Weak<AppWindow>, app: &AppContext) -> Self {
        Self {
            weak,
            runtime: app.runtime.clone(),
            handles: app.handles.clone(),
            sftp_handles: app.sftp_handles.clone(),
            sftp_last_cwd: app.sftp_last_cwd.clone(),
            bufs: app.bufs.clone(),
            render_gates: app.render_gates.clone(),
            tab_statuses: app.tab_statuses.clone(),
            local_snap: app.local_snap.clone(),
            local_net_hist: app.local_net_hist.clone(),
            last_term_size: app.last_term_size.clone(),
            sftp_follow_cd: app.sftp_follow_cd.clone(),
            store: app.store.clone(),
        }
    }
}

pub(crate) struct PendingHostKey {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) changed: bool,
    pub(crate) title: String,
    pub(crate) message: String,
    pub(crate) detail: String,
    pub(crate) confirm_label: String,
    pub(crate) responders: Vec<HostKeyResponder>,
}

pub(crate) struct PendingCred {
    pub(crate) session_id: String,
    pub(crate) host: String,
    pub(crate) user: String,
    pub(crate) need_user: bool,
    pub(crate) need_password: bool,
    pub(crate) responders: Vec<CredentialResponder>,
}

pub(crate) struct PendingMfa {
    pub(crate) session_id: String,
    pub(crate) host: String,
    pub(crate) prompt: String,
    pub(crate) echo: bool,
    pub(crate) responders: Vec<MfaResponder>,
}
