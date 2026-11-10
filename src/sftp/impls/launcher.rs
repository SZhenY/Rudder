use super::*;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Spawn an SFTP worker on the Tokio runtime.
///
/// The worker opens its own SSH connection to the same server, authenticates,
/// and requests the `sftp` subsystem. Events (directory listings, progress,
/// errors) are sent back via `events`, which is the same sender used by the
/// terminal's shell session.
/// Turn an SFTP-worker failure into a status-bar message.
///
/// SFTP runs on its own SSH connection, fully separate from the shell PTY, so
/// when it can't connect the terminal keeps working — we just surface why in the
/// SFTP panel. The common bastion/jump-host case is "shell is allowed but the
/// `sftp` subsystem is not", which shows up as a failed subsystem request /
/// channel / handshake (or an explicit "permission denied"). For that family we
/// give a plain-language hint instead of the raw russh error (#190).
pub(crate) fn friendly_sftp_error(err: &anyhow::Error) -> String {
    let chain = err
        .chain()
        .map(|e| e.to_string().to_lowercase())
        .collect::<Vec<_>>()
        .join(" | ");
    let permission_like = [
        "subsystem",      // server refused the `sftp` subsystem request
        "sftp channel",   // channel_open_session refused
        "sftp handshake", // subsystem opened but no SFTP server behind it
        "permission",
        "denied",
        "prohibited", // "administratively prohibited"
        "not allowed",
    ]
    .iter()
    .any(|k| chain.contains(k));
    if permission_like {
        t(
            "SFTP 不可用,请检查是否有访问权限(服务器可能未开放 SFTP)",
            "SFTP unavailable — check whether you have permission (server may not allow SFTP)",
        )
        .to_string()
    } else {
        format!("{}: {err:#}", t("SFTP 错误", "SFTP error"))
    }
}

pub fn spawn_sftp(
    runtime: &tokio::runtime::Handle,
    session: Session,
    jump: Option<Session>,
    events: UnboundedSender<SessionEvent>,
) -> SftpHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let self_tx = cmd_tx.clone();
    let events_err = events.clone();
    // 丢弃 `JoinHandle` = detach（tokio 语义）：worker 照跑；要停它靠 `SftpHandle::drop`
    // 发的那条 `Close`。
    runtime.spawn(async move {
        if let Err(err) = run_sftp(session, jump, cmd_rx, self_tx, events).await {
            let _ = events_err.send(SessionEvent::SftpStatus(friendly_sftp_error(&err)));
        }
    });
    SftpHandle { commands: cmd_tx }
}

// ---------------------------------------------------------------------------
// 按需连接
// ---------------------------------------------------------------------------

/// 事件 pump 启动器，可重复调用以便重连。
pub(crate) type SftpPumpStarter =
    Arc<dyn Fn(UnboundedReceiver<SessionEvent>) + Send + Sync + 'static>;

pub(crate) static LAUNCHERS: OnceLock<Mutex<HashMap<String, Arc<SftpLauncher>>>> = OnceLock::new();
/// 面板折叠后待释放的代数：每次折叠 +1，在途定时器只对最新一代生效。
pub(crate) static RELEASE_GEN: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
/// 折叠后闲置多久才断开第二条 SSH 连接；窗口内重新展开会取消。
pub(crate) const SFTP_IDLE_RELEASE: Duration = Duration::from_secs(5);

pub(crate) fn launchers() -> &'static Mutex<HashMap<String, Arc<SftpLauncher>>> {
    LAUNCHERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn release_gen() -> &'static Mutex<HashMap<String, u64>> {
    RELEASE_GEN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 一个标签页的"待启动 SFTP"：只有用户真的用文件面板时才发起第二条 SSH 连接。
///
/// 展开面板（`collapse_sftp_default = true` 的会话）或手动刷新/导航时触发；
/// 连接建立前到达的请求先记下路径，连上后立刻列该目录。
/// 闲置 5 秒断开，再次展开自动重连。
pub(crate) struct SftpLauncher {
    pub(crate) tab_id: String,
    pub(crate) runtime: tokio::runtime::Handle,
    pub(crate) handles: crate::sftp::transfer::SftpHandles,
    pub(crate) last_cwd: crate::sftp::transfer::SftpLastCwd,
    /// 用 clone 而不是 take，断开后重连还能再建一次连接。
    pub(crate) session: Mutex<Option<(Session, Option<Session>)>>,
    pub(crate) events: UnboundedSender<SessionEvent>,
    /// 事件 pump 的 receiver 只在首次 `start()` 取走；启动器保留下来供重连用。
    pub(crate) pump_rx: Mutex<Option<UnboundedReceiver<SessionEvent>>>,
    pub(crate) pump_starter: SftpPumpStarter,
    pub(crate) connected: AtomicBool,
    pub(crate) wanted: AtomicBool,
    pub(crate) started: AtomicBool,
    pub(crate) pending: Mutex<Option<String>>,
}

impl SftpLauncher {
    pub(crate) fn want(&self, path: Option<String>) {
        if let Some(path) = path
            && let Ok(mut slot) = self.pending.lock()
        {
            *slot = Some(path);
        }
        self.wanted.store(true, Ordering::SeqCst);
        if self.connected.load(Ordering::SeqCst) {
            self.start();
        }
    }

    /// PTY 报 Connected 后调用；若此前已经有人请求过，立刻开连。
    pub(crate) fn mark_connected(&self) {
        self.connected.store(true, Ordering::SeqCst);
        if self.wanted.load(Ordering::SeqCst) {
            self.start();
        }
    }

    pub(crate) fn start(&self) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some((session, jump)) = self.session.lock().ok().and_then(|slot| slot.clone()) else {
            return;
        };
        // 首次用到 SFTP 才起事件 pump。
        if let Some(rx) = self.pump_rx.lock().ok().and_then(|mut slot| slot.take()) {
            (self.pump_starter)(rx);
        }
        // 懒启动时面板可能已经展开，给一条状态提示，避免几秒空白。
        let _ = self.events.send(SessionEvent::SftpStatus(
            t("正在连接文件服务…", "Connecting to the file service…").to_string(),
        ));
        let handle = spawn_sftp(&self.runtime, session, jump, self.events.clone());
        // 优先用请求里带的路径，其次用面板跟随到的 cwd，最后退回 "."。
        let path = self
            .pending
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .or_else(|| {
                self.last_cwd
                    .lock()
                    .ok()
                    .and_then(|map| map.get(&self.tab_id).cloned())
            })
            .unwrap_or_else(|| ".".to_string());
        handle.list_dir(path);
        if let Ok(mut handles) = self.handles.lock() {
            handles.insert(self.tab_id.clone(), handle);
        }
    }

    /// 断开连接并换成未启动的 launcher；旧 pump 随发送端释放而退出。
    pub(crate) fn disconnect(self: &Arc<Self>) {
        if let Ok(mut handles) = self.handles.lock()
            && let Some(handle) = handles.remove(&self.tab_id)
        {
            handle.close();
        }
        let (events, rx) = mpsc::unbounded_channel();
        let fresh = Arc::new(SftpLauncher {
            tab_id: self.tab_id.clone(),
            runtime: self.runtime.clone(),
            handles: self.handles.clone(),
            last_cwd: self.last_cwd.clone(),
            session: Mutex::new(self.session.lock().ok().and_then(|slot| slot.clone())),
            events,
            pump_rx: Mutex::new(Some(rx)),
            pump_starter: self.pump_starter.clone(),
            connected: AtomicBool::new(self.connected.load(Ordering::SeqCst)),
            wanted: AtomicBool::new(false),
            started: AtomicBool::new(false),
            pending: Mutex::new(None),
        });
        if let Ok(mut map) = launchers().lock() {
            map.insert(self.tab_id.clone(), fresh);
        }
    }
}

/// 登记一枚懒启动 SFTP 所需的全部上下文（收进结构体，避免长参数列表）。
pub(crate) struct SftpLaunchParams {
    pub tab_id: String,
    pub runtime: tokio::runtime::Handle,
    pub handles: crate::sftp::transfer::SftpHandles,
    pub last_cwd: crate::sftp::transfer::SftpLastCwd,
    pub session: Session,
    pub jump: Option<Session>,
    pub events: UnboundedSender<SessionEvent>,
    pub events_rx: UnboundedReceiver<SessionEvent>,
    pub pump_starter: SftpPumpStarter,
}

impl SftpLaunchParams {
    pub(crate) fn into_launcher(self) -> Arc<SftpLauncher> {
        Arc::new(SftpLauncher {
            tab_id: self.tab_id,
            runtime: self.runtime,
            handles: self.handles,
            last_cwd: self.last_cwd,
            session: Mutex::new(Some((self.session, self.jump))),
            events: self.events,
            pump_rx: Mutex::new(Some(self.events_rx)),
            pump_starter: self.pump_starter,
            connected: AtomicBool::new(false),
            wanted: AtomicBool::new(false),
            started: AtomicBool::new(false),
            pending: Mutex::new(None),
        })
    }
}

/// 登记一个懒启动的 SFTP。替换同标签的旧 launcher（重连场景）。
pub(crate) fn register_sftp_launcher(params: SftpLaunchParams) {
    cancel_sftp_release(&params.tab_id);
    let tab_id = params.tab_id.clone();
    let launcher = params.into_launcher();
    if let Ok(mut map) = launchers().lock() {
        map.insert(tab_id, launcher);
    }
}

/// 请求某个标签的 SFTP：未启动则启动（等 PTY Connected），已启动则只更新路径。
pub(crate) fn want_sftp(tab_id: &str, path: Option<String>) {
    // 展开面板 / 手动刷新都算"还要用"，撤销尚未触发的延迟断开。
    cancel_sftp_release(tab_id);
    let launcher = launchers()
        .lock()
        .ok()
        .and_then(|map| map.get(tab_id).cloned());
    if let Some(launcher) = launcher {
        launcher.want(path);
    }
}

pub(crate) fn sftp_launcher_connected(tab_id: &str) {
    let launcher = launchers()
        .lock()
        .ok()
        .and_then(|map| map.get(tab_id).cloned());
    if let Some(launcher) = launcher {
        launcher.mark_connected();
    }
}

/// 关标签/断线时丢弃 launcher：它持有的 Session 与事件通道一起释放。
pub(crate) fn forget_sftp_launcher(tab_id: &str) {
    cancel_sftp_release(tab_id);
    if let Ok(mut map) = launchers().lock() {
        map.remove(tab_id);
    }
}

/// 折叠后 5 秒断开；每次折叠重计，展开取消。须在 UI 线程调用。
pub(crate) fn schedule_sftp_release(tab_id: &str) {
    // 只处理按需连接的会话。
    let exists = launchers()
        .lock()
        .map(|map| map.contains_key(tab_id))
        .unwrap_or(false);
    if !exists {
        return;
    }
    let id = tab_id.to_string();
    let generation = bump_release_generation(&id);
    slint::Timer::single_shot(SFTP_IDLE_RELEASE, move || {
        // 代数不匹配 = 已被取消，或又被一次新的折叠重置了计时。
        if !release_is_current(&id, generation) {
            return;
        }
        release_gen()
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(&id);
        let launcher = launchers()
            .lock()
            .ok()
            .and_then(|map| map.get(&id).cloned());
        if let Some(launcher) = launcher {
            launcher.disconnect();
        }
    });
}

/// 取消未触发的延迟断开（展开面板 / 手动刷新 / 关标签 / 重新注册）。
pub(crate) fn cancel_sftp_release(tab_id: &str) {
    if let Ok(mut gens) = release_gen().lock() {
        gens.remove(tab_id);
    }
}

/// 每次折叠把代数 +1：先前的在途定时器随即失效，等价「重新计时 5 秒」。
pub(crate) fn bump_release_generation(tab_id: &str) -> u64 {
    let mut gens = release_gen().lock().unwrap_or_else(|err| err.into_inner());
    let entry = gens.entry(tab_id.to_string()).or_insert(0);
    *entry += 1;
    *entry
}

/// 该代定时器是否仍对应最新一次折叠（取消 / 重计都会让它返回 false）。
pub(crate) fn release_is_current(tab_id: &str, generation: u64) -> bool {
    release_gen()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .get(tab_id)
        .copied()
        == Some(generation)
}
