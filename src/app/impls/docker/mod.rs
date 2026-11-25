//! Docker 页控制器：按 `user@host` 维护一份容器缓存，刷新 / 切换主机都只走这里。
//!
//! 所有 SSH 命令都在 tokio worker 上执行；本模块的公开函数只在 Slint UI 线程调用，
//! 结果通过 `invoke_from_event_loop` 回到 UI 线程再写模型。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tokio::runtime::Runtime;

use crate::config::{ConfigStore, Secret, SessionKind};
use crate::docker::classify::classify as classify_docker;
use crate::docker::{
    command, parse, ComposeProjectInfo, ContainerState, DockerContainerInfo, DockerFailure,
    DockerHostKey, DockerImageInfo, DockerNetworkInfo, HostStatus, RegistryRepo,
};

#[path = "registry.rs"]
mod registry;
use crate::i18n::t;
use crate::resource::system::format_bytes_per_sec;
use crate::resource::TabStatuses;
use crate::ssh::{ExecResult, SessionHandle};
use crate::ui::{
    AppWindow, DockerComposeRow, DockerContainer, DockerHostTab, DockerImage, DockerNetworkRow,
    RegistryRepoRow, SessionInfo, TabInfo,
};

const PS_TIMEOUT: Duration = Duration::from_secs(10);
const PS_MAX_OUTPUT: usize = 512 * 1024;
const FRESH_FOR: Duration = Duration::from_secs(10);
const BACKOFF: Duration = Duration::from_secs(30);

thread_local! {
    static HUB: RefCell<DockerHub> = RefCell::new(DockerHub::new());
    /// UI 线程上的刷新上下文；异步任务只带回结果，需要续跑时在这里取句柄。
    static REFRESH_CTX: RefCell<Option<RefreshCtx>> = const { RefCell::new(None) };
    /// 最近一次刷新用的资源快照表：概览页的曲线按选中主机的 tab_id 从这里取。
    static LAST_STATUSES: RefCell<Option<TabStatuses>> = const { RefCell::new(None) };
    /// 概览曲线已应用的 `stats_seq`（按 tab_id）：只有快照变了才重建路径。
    static APPLIED_MONITOR: RefCell<HashMap<String, u64>> = RefCell::new(HashMap::new());
    /// 日志正文：窗口可先开、结果后回填；窗口销毁后文本仍在。
    static LOG_TEXT: RefCell<Option<Rc<RefCell<String>>>> = const { RefCell::new(None) };
    /// 每次请求 +1，丢弃「切到别的容器之后才回来」的旧结果。
    static LOG_SEQ: Cell<u64> = const { Cell::new(0) };
    /// 新开标签在 Connected 之前要注入的命令：tab_id -> 命令。
    static PENDING_TERMINAL: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    /// 欢迎页发起的连接：tab_id -> 待打开的主机。连上后由 sync_hosts 消费。
    static PENDING_OPEN: RefCell<HashMap<String, PendingOpen>> = RefCell::new(HashMap::new());
    /// 操作结果条的序号，旧定时器不能清掉新消息。
    static ACTION_SEQ: Cell<u64> = const { Cell::new(0) };
}

#[derive(Clone)]
struct RefreshCtx {
    runtime: Arc<Runtime>,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
}

/// UI 线程上的刷新上下文；派生数据的异步任务也用它。
fn refresh_ctx() -> Option<RefreshCtx> {
    REFRESH_CTX.with(|ctx| ctx.borrow().clone())
}

#[derive(Clone)]
struct PendingOpen {
    session_id: String,
    key: String,
    name: String,
}

struct HostRuntime {
    tab_id: String,
    /// 会话显示名（`TabInfo.title`），标签上显示这个而不是 `user@host`。
    title: String,
    title_len: i32,
    session_count: i32,
    containers: Vec<DockerContainerInfo>,
    status: HostStatus,
    failure: Option<DockerFailure>,
    last_ok: Option<Instant>,
    last_attempt: Option<Instant>,
    generation: u64,
    busy: bool,
    queued: bool,
    backoff_until: Option<Instant>,
    /// 本轮同步时是否有 state == 1 的会话。
    connected: bool,
    /// 代表标签对应的已保存会话 id；凭据按会话存。
    session_id: String,
    /// `docker ps` 能否直接用登录用户跑通；None = 尚未探测。
    direct: Option<bool>,
    /// `sudo -n true` 是否可用；None = 尚未探测。
    sudo_nopass: Option<bool>,
    /// 最近一次 docker 命令实际用的提权方式，错误文案按它分支。
    last_mode: command::Mode,
    /// Docker 提权方式："" / "sudo" / "su"。
    docker_elevation: String,
    /// sudo 目标用户；空 = root。
    docker_sudo_user: String,
    /// 提权密码（来自会话配置，加密落盘）。
    docker_sudo_password: Secret,
    /// `systemctl is-active docker`：None = 未知（未采集 / 非 systemd）。
    service_up: Option<bool>,
    /// `systemctl is-enabled docker`：None = 未知。
    service_enabled: Option<bool>,
    /// `docker version` 的 Server.Version；空 = 未采集 / 取不到。
    docker_version: String,
    service_pending: bool,
    /// 已经弹过的不可用原因；同类原因不重复弹。
    blocker_kind: Option<DockerFailure>,
    /// 用户主动关掉过的弹窗原因，重试或成功后才清。
    blocker_dismissed: Option<DockerFailure>,

    // ---- 第一批只读页的缓存（每类独立节流与代次，互不阻塞）------------
    images: Vec<DockerImageInfo>,
    images_status: HostStatus,
    images_failure: Option<DockerFailure>,
    images_last_ok: Option<Instant>,
    images_busy: bool,
    images_generation: u64,

    compose_projects: Vec<ComposeProjectInfo>,
    compose_status: HostStatus,
    compose_failure: Option<DockerFailure>,
    compose_last_ok: Option<Instant>,
    compose_busy: bool,
    compose_generation: u64,

    networks: Vec<DockerNetworkInfo>,
    networks_status: HostStatus,
    networks_failure: Option<DockerFailure>,
    networks_last_ok: Option<Instant>,
    networks_busy: bool,
    networks_generation: u64,

    // ---- 由容器快照派生的数据（ps 成功且快照有变化时重算，与二级页无关）----
    /// 容器快照代次：首次 ps 成功、以及之后每次行内容变化时 +1。
    snapshot_epoch: u64,
    /// 镜像短 ID → 引用它的运行中容器数；None = 从没算成功过。
    image_usage: Option<HashMap<String, u32>>,
    image_usage_epoch: u64,
    /// 网络名 → 关联容器名（含已停止）。
    network_members: Option<HashMap<String, Vec<String>>>,
    network_members_epoch: u64,
}

struct DockerHub {
    hosts: HashMap<String, HostRuntime>,
    /// 当前可见的 Docker 主机标签，顺序即标签顺序。
    open: Vec<String>,
    /// 用户手动关掉的 host；阻止自动重开，直到手动打开或 host 消失。
    suppressed: HashSet<String>,
    active: Option<String>,
    filter: String,
    /// 上一轮看到的自动探索开关，用于识别上升/下降沿。
    auto_last: Option<bool>,
    /// 用户明确停在「欢迎」标签；有打开的标签也保持欢迎页，不被自动刷新拉走。
    welcome_selected: bool,
    hosts_model: Rc<VecModel<DockerHostTab>>,
    containers_model: Rc<VecModel<DockerContainer>>,
    welcome_model: Rc<VecModel<SessionInfo>>,
    /// 当前二级页，用于定时刷新只拉当前页的数据。
    section: String,
    images_model: Rc<VecModel<DockerImage>>,
    compose_model: Rc<VecModel<DockerComposeRow>>,
    networks_model: Rc<VecModel<DockerNetworkRow>>,
    registry_model: Rc<VecModel<RegistryRepoRow>>,
    registry_rows: Vec<RegistryRepo>,
    registry_query: String,
    registry_page: u32,
    registry_status: HostStatus,
    registry_error: String,
    image_filter: String,
    compose_filter: String,
}

impl DockerHub {
    fn new() -> Self {
        Self {
            hosts: HashMap::new(),
            open: Vec::new(),
            suppressed: HashSet::new(),
            active: None,
            filter: String::new(),
            auto_last: None,
            welcome_selected: false,
            hosts_model: Rc::new(VecModel::default()),
            containers_model: Rc::new(VecModel::default()),
            welcome_model: Rc::new(VecModel::default()),
            section: "overview".to_string(),
            images_model: Rc::new(VecModel::default()),
            compose_model: Rc::new(VecModel::default()),
            networks_model: Rc::new(VecModel::default()),
            registry_model: Rc::new(VecModel::default()),
            registry_rows: Vec::new(),
            registry_query: String::new(),
            registry_page: 1,
            registry_status: HostStatus::Idle,
            registry_error: String::new(),
            image_filter: String::new(),
            compose_filter: String::new(),
        }
    }
}

/// 进入 Docker 页 / 点刷新：同步主机标签，刷新当前主机。
pub(crate) fn refresh(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    tabs: &Rc<VecModel<TabInfo>>,
    statuses: &TabStatuses,
    store: &ConfigStore,
) {
    REFRESH_CTX.with(|ctx| {
        *ctx.borrow_mut() = Some(RefreshCtx {
            runtime: runtime.clone(),
            handles: handles.clone(),
        });
    });
    LAST_STATUSES.with(|s| *s.borrow_mut() = Some(statuses.clone()));
    let key = HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        sync_hosts(&mut hub, window, tabs, statuses, store);
        let Some(key) = hub.active.clone() else {
            clear_view(window, &mut hub);
            return None;
        };
        refresh_key(window, runtime, handles, &mut hub, &key, false);
        Some(key)
    });
    if let Some(key) = key {
        probe_service(window, runtime, handles, &key, false);
        // 连接建立 / 10s 拍：三类二级页一起拉，别等用户点进去。
        HUB.with(|hub| {
            let mut hub = hub.borrow_mut();
            refresh_all_sections(window, runtime, handles, &mut hub, &key, false);
        });
    }
}

/// 强制刷新当前主机：跳过失败退避，供刷新按钮 / 重试 / 操作完成使用。
pub(crate) fn refresh_forced(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    tabs: &Rc<VecModel<TabInfo>>,
    statuses: &TabStatuses,
    store: &ConfigStore,
) {
    REFRESH_CTX.with(|ctx| {
        *ctx.borrow_mut() = Some(RefreshCtx {
            runtime: runtime.clone(),
            handles: handles.clone(),
        });
    });
    LAST_STATUSES.with(|s| *s.borrow_mut() = Some(statuses.clone()));
    let key = HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        sync_hosts(&mut hub, window, tabs, statuses, store);
        let Some(key) = hub.active.clone() else {
            clear_view(window, &mut hub);
            return None;
        };
        if let Some(rt) = hub.hosts.get_mut(&key) {
            rt.backoff_until = None;
        }
        refresh_key(window, runtime, handles, &mut hub, &key, true);
        Some(key)
    });
    if let Some(key) = key {
        probe_service(window, runtime, handles, &key, true);
        // 操作完成 / 手动刷新：跳过 10s 节流，三类二级页一起重拉。
        HUB.with(|hub| {
            let mut hub = hub.borrow_mut();
            refresh_all_sections(window, runtime, handles, &mut hub, &key, true);
        });
    }
}

/// 切换主机标签：只切缓存；缓存缺失或过期才请求。
pub(crate) fn select_host(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    key: &str,
) {
    if key.is_empty() {
        return;
    }
    REFRESH_CTX.with(|ctx| {
        *ctx.borrow_mut() = Some(RefreshCtx {
            runtime: runtime.clone(),
            handles: handles.clone(),
        });
    });
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        if !hub.open.iter().any(|k| k == key) {
            return;
        }
        hub.welcome_selected = false;
        hub.active = Some(key.to_string());
        window.set_docker_active_host(key.into());
        apply_active(window, &mut hub);
        let stale = hub
            .hosts
            .get(key)
            .is_none_or(|h| h.last_ok.is_none_or(|t| t.elapsed() > FRESH_FOR));
        if stale {
            refresh_key(window, runtime, handles, &mut hub, key, false);
        }
        // 切到这台主机：三类二级页也补一次（各自的 10s 节流兜底）。
        refresh_all_sections(window, runtime, handles, &mut hub, key, false);
    });
    probe_service(window, runtime, handles, key, false);
}

/// 打开（或聚焦）某台主机的 Docker 标签。欢迎页手动打开走这里。
pub(crate) fn open_host(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    tabs: &Rc<VecModel<TabInfo>>,
    statuses: &TabStatuses,
    store: &ConfigStore,
    key: &str,
) {
    REFRESH_CTX.with(|ctx| {
        *ctx.borrow_mut() = Some(RefreshCtx {
            runtime: runtime.clone(),
            handles: handles.clone(),
        });
    });
    LAST_STATUSES.with(|s| *s.borrow_mut() = Some(statuses.clone()));
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        sync_hosts(&mut hub, window, tabs, statuses, store);
        if !hub.hosts.contains_key(key) {
            return;
        }
        if !hub.open.iter().any(|k| k == key) {
            hub.open.push(key.to_string());
        }
        hub.suppressed.remove(key);
        hub.welcome_selected = false;
        hub.active = Some(key.to_string());
        if let Some(rt) = hub.hosts.get_mut(key) {
            rt.blocker_dismissed = None;
        }
        window.set_docker_active_host(key.into());
        rebuild_hosts_model(&mut hub, window);
        apply_active(window, &mut hub);
        let stale = hub
            .hosts
            .get(key)
            .is_none_or(|h| h.last_ok.is_none_or(|t| t.elapsed() > FRESH_FOR));
        if stale {
            refresh_key(window, runtime, handles, &mut hub, key, false);
        }
        refresh_all_sections(window, runtime, handles, &mut hub, key, false);
    });
    probe_service(window, runtime, handles, key, true);
}

/// 关闭某台主机的 Docker 标签（不影响 SSH 会话）；关掉后阻止自动重开。
pub(crate) fn close_host(window: &AppWindow, store: &ConfigStore, key: &str) {
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        let Some(index) = hub.open.iter().position(|k| k == key) else {
            return;
        };
        hub.open.remove(index);
        hub.suppressed.insert(key.to_string());
        PENDING_OPEN.with(|pending| {
            pending.borrow_mut().retain(|_, v| v.key != key);
        });
        if hub.open.is_empty() {
            hub.welcome_selected = true;
        }
        if hub.active.as_deref() == Some(key) {
            hub.active = hub
                .open
                .get(index)
                .cloned()
                .or_else(|| hub.open.last().cloned());
        }
        super::sub_windows::release_docker_log();
        if window.get_docker_blocker_host().as_str() == key {
            clear_blocker(window);
        }
        window.set_docker_active_host(hub.active.clone().unwrap_or_default().into());
        rebuild_hosts_model(&mut hub, window);
        if hub.active.is_some() {
            apply_active(window, &mut hub);
        } else {
            clear_view(window, &mut hub);
            refresh_welcome(window, &mut hub, store);
        }
    });
}

/// 回到固定的「欢迎」标签。
pub(crate) fn goto_welcome(window: &AppWindow, store: &ConfigStore) {
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        hub.welcome_selected = true;
        hub.active = None;
        window.set_docker_active_host("".into());
        super::sub_windows::release_docker_log();
        clear_blocker(window);
        clear_view(window, &mut hub);
        refresh_welcome(window, &mut hub, store);
    });
}

/// 打开某会话的 Docker 提权凭据弹窗。
pub(crate) fn open_credentials(window: &AppWindow, store: &ConfigStore, session_id: &str) {
    let Some(session) = store.get(session_id) else { return };
    let label = if session.name.is_empty() {
        format!("{}@{}", session.user, session.host)
    } else {
        session.name.clone()
    };
    window.set_docker_cred_session(session_id.into());
    window.set_docker_cred_host(label.as_str().into());
    window.set_docker_cred_mode(
        if session.docker_elevation == "su" { "su" } else { "sudo" }.into(),
    );
    window.set_docker_cred_user(session.docker_sudo_user.as_str().into());
    window.set_docker_cred_has_password(!session.docker_sudo_password.is_empty());
    window.set_docker_cred_testing(false);
    window.set_docker_cred_test_text("".into());
    window.set_docker_cred_test_ok(false);
    window.set_docker_cred_open(true);
}

pub(crate) fn dismiss_credentials(window: &AppWindow) {
    window.set_docker_cred_open(false);
}

fn write_credentials(
    store: &mut ConfigStore,
    session_id: &str,
    mode: &str,
    user: &str,
    password: &str,
) -> Result<(), &'static str> {
    let mode = if mode == "su" { "su" } else { "sudo" };
    let user = user.trim();
    if mode == "su" && user.is_empty() {
        return Err(t(
            "su 模式需要填写目标账号",
            "su needs a target account",
        ));
    }
    let Some(mut session) = store.get(session_id).cloned() else {
        return Err(t("会话不存在", "The session no longer exists"));
    };
    session.docker_elevation = mode.to_string();
    session.docker_sudo_user = user.to_string();
    // 留空 = 保留已保存的密码。
    if !password.is_empty() {
        session.docker_sudo_password = Secret::new(password.to_string());
    }
    store.upsert(session);
    store.save_logging();
    Ok(())
}

/// 保存凭据；返回是否需要刷新（成功写入）。
pub(crate) fn save_credentials(
    window: &AppWindow,
    store: &mut ConfigStore,
    session_id: &str,
    mode: &str,
    user: &str,
    password: &str,
) -> bool {
    match write_credentials(store, session_id, mode, user, password) {
        Ok(()) => {
            // 留空保存时沿用旧密码，状态要按落盘后的真实值回显。
            let has_password = store
                .get(session_id)
                .is_some_and(|s| !s.docker_sudo_password.is_empty());
            window.set_docker_cred_has_password(has_password);
            window.set_docker_cred_test_text("".into());
            window.set_docker_cred_test_ok(false);
            window.set_docker_cred_open(false);
            true
        }
        Err(message) => {
            window.set_docker_cred_test_text(message.into());
            window.set_docker_cred_test_ok(false);
            false
        }
    }
}

pub(crate) fn clear_credentials(
    window: &AppWindow,
    store: &mut ConfigStore,
    session_id: &str,
) {
    if let Some(mut session) = store.get(session_id).cloned() {
        session.docker_elevation.clear();
        session.docker_sudo_user.clear();
        session.docker_sudo_password = Secret::default();
        store.upsert(session);
        store.save_logging();
    }
    window.set_docker_cred_has_password(false);
    window.set_docker_cred_test_text(t("已清除", "Cleared").into());
    window.set_docker_cred_test_ok(true);
}

/// 弹窗里的输入；打包成一个结构，避免回调函数参数过多。
pub(crate) struct CredentialInput<'a> {
    pub session_id: &'a str,
    pub mode: &'a str,
    pub user: &'a str,
    pub password: &'a str,
}

/// 用当前输入试跑一条只读命令，结果回弹窗。
pub(crate) fn test_credentials(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    statuses: &TabStatuses,
    store: &ConfigStore,
    input: CredentialInput<'_>,
) {
    let CredentialInput {
        session_id,
        mode,
        user,
        password,
    } = input;
    let saved = store
        .get(session_id)
        .map(|s| s.docker_sudo_password.clone())
        .unwrap_or_default();
    let secret = if !password.is_empty() {
        Secret::new(password.to_string())
    } else {
        saved
    };
    if secret.is_empty() {
        window.set_docker_cred_testing(false);
        window.set_docker_cred_test_text(t("请先输入密码", "Enter the password first").into());
        window.set_docker_cred_test_ok(false);
        return;
    }

    let tab_id = {
        let Ok(statuses) = statuses.lock() else { return };
        statuses
            .iter()
            .find(|(_, st)| st.session_id == session_id && st.state == 1)
            .map(|(id, _)| id.clone())
    };
    let Some(tab_id) = tab_id else {
        window.set_docker_cred_testing(false);
        window.set_docker_cred_test_text(t(
            "该主机尚未连接，先连接再测试",
            "Connect the host before testing",
        ).into());
        window.set_docker_cred_test_ok(false);
        return;
    };

    let elev = if mode == "su" {
        command::Elevation::su(user)
    } else {
        command::Elevation::sudo_password(user)
    };
    let cmd = command::ps(false, &elev);
    let mode = elev.mode;
    let rx = {
        let hs = handles.borrow();
        hs.get(&tab_id)
            .map(|handle| handle.exec_privileged(cmd, secret, Duration::from_secs(15), 64 * 1024))
    };
    let Some(rx) = rx else {
        window.set_docker_cred_testing(false);
        window.set_docker_cred_test_text(
            t("会话不可用，无法测试", "The session is unavailable").into(),
        );
        window.set_docker_cred_test_ok(false);
        return;
    };

    window.set_docker_cred_testing(true);
    window.set_docker_cred_test_text("".into());
    let weak = window.as_weak();
    runtime.spawn(async move {
        let result = rx.await;
        let (text, ok) = match result {
            Ok(res) if !res.timed_out && res.error.is_none() && res.exit_code.unwrap_or(1) == 0 => {
                (t("认证成功", "Authentication succeeded").to_string(), true)
            }
            Ok(res) => (
                error_text(
                    Some(classify_docker(&res.stderr, res.exit_code, res.timed_out)),
                    mode,
                )
                .to_string(),
                false,
            ),
            Err(_) => (
                t("会话已关闭，无法测试", "The session closed during the test").to_string(),
                false,
            ),
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak.upgrade() {
                window.set_docker_cred_testing(false);
                window.set_docker_cred_test_text(text.into());
                window.set_docker_cred_test_ok(ok);
            }
        });
    });
}

/// 欢迎页点击某个已保存的 SSH 会话：已连接直接开标签，未连接先建会话。
pub(crate) fn open_saved_session(
    window: &AppWindow,
    store: &ConfigStore,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    tabs: &Rc<VecModel<TabInfo>>,
    statuses: &TabStatuses,
    session_id: &str,
) {
    let Some(session) = store
        .sessions()
        .iter()
        .find(|s| s.id.as_str() == session_id)
        .cloned()
    else {
        return;
    };
    if session.kind != SessionKind::Ssh {
        return;
    }
    let Some(key) = DockerHostKey::parse(&session.host, &session.user) else {
        return;
    };
    let key = key.as_str().to_string();

    // 先用最新的 SSH 标签表刷新一次，避免把“已经连上”误判成“未连接”。
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        sync_hosts(&mut hub, window, tabs, statuses, store);
    });

    // 已经连上：直接开/聚焦标签。
    let connected = HUB.with(|hub| {
        hub.borrow()
            .hosts
            .get(&key)
            .is_some_and(|rt| rt.connected)
    });
    if connected {
        open_host(window, runtime, handles, tabs, statuses, store, &key);
        return;
    }

    // 已在连接中：不重复发起。
    let already_pending = PENDING_OPEN.with(|p| {
        p.borrow()
            .values()
            .any(|v| v.session_id == session_id)
    });
    if already_pending {
        return;
    }

    let before: HashSet<String> = (0..tabs.row_count())
        .filter_map(|i| tabs.row_data(i).map(|t| t.id.to_string()))
        .collect();
    window.invoke_connect_session(session_id.into());
    let new_tab = (0..tabs.row_count())
        .filter_map(|i| tabs.row_data(i).map(|t| t.id.to_string()))
        .find(|id| !before.contains(id));
    if let Some(tab_id) = new_tab {
        PENDING_OPEN.with(|p| {
            p.borrow_mut().insert(
                tab_id,
                PendingOpen {
                    session_id: session_id.to_string(),
                    key,
                    name: session.name.clone(),
                },
            );
        });
    }
}

/// 欢迎页分组折叠：与主机页共用同一份 store 状态。
pub(crate) fn toggle_group(window: &AppWindow, store: &mut ConfigStore, group: &str) {
    let collapsed = store
        .collapsed_session_groups()
        .is_some_and(|groups| groups.iter().any(|g| g == group));
    store.set_session_group_collapsed(group, !collapsed);
    if let Err(err) = store.save() {
        tracing::warn!("failed to save Docker folder state: {err:#}");
    }
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        refresh_welcome(window, &mut hub, store);
    });
}

/// SSH 连接成功：触发一次同步；如果是欢迎页发起的连接，连上后切回 Docker 页。
pub(crate) fn on_session_connected(window: &AppWindow, tab_id: &str) {
    let from_welcome = PENDING_OPEN.with(|p| p.borrow().contains_key(tab_id));
    window.invoke_refresh_docker();
    if from_welcome {
        window.set_nav_page(3);
    }
}

/// SSH 连接失败：如果这条连接是欢迎页发起的，弹「无法连接」并清 pending。
pub(crate) fn on_session_closed(window: &AppWindow, tab_id: &str, reason: &str) {
    let pending = PENDING_OPEN.with(|p| p.borrow_mut().remove(tab_id));
    let Some(pending) = pending else { return };
    let detail = t(
        "SSH 会话未能建立：{reason}",
        "The SSH session could not be established: {reason}",
    )
    .replace("{reason}", reason);
    // 从欢迎页发起时连接过程会把界面切到终端，失败要把用户带回 Docker 页看弹窗。
    window.set_nav_page(3);
    window.set_docker_blocker_kind("connect-failed".into());
    window.set_docker_blocker_host(pending.key.clone().into());
    window.set_docker_blocker_title(pending.name.into());
    window.set_docker_blocker_detail(detail.into());
    window.set_docker_blocker_session(pending.session_id.into());
}

/// 标签在 Connected 之前被关掉：清掉待打开记录。
pub(crate) fn cancel_pending(tab_id: &str) {
    PENDING_OPEN.with(|p| {
        p.borrow_mut().remove(tab_id);
    });
}

fn blocker_kind_key(kind: DockerFailure) -> &'static str {
    match kind {
        DockerFailure::NoCli => "no-cli",
        DockerFailure::NoPermission => "no-permission",
        DockerFailure::SudoAuthFailed => "sudo-auth",
        DockerFailure::DaemonDown => "daemon-down",
        _ => "",
    }
}

fn clear_blocker(window: &AppWindow) {
    window.set_docker_blocker_kind("".into());
    window.set_docker_blocker_host("".into());
    window.set_docker_blocker_title("".into());
    window.set_docker_blocker_detail("".into());
    window.set_docker_blocker_session("".into());
}

/// 关掉不可用弹窗；同一原因在本次打开期间不再重弹。
pub(crate) fn dismiss_blocker(window: &AppWindow) {
    let host = window.get_docker_blocker_host().to_string();
    HUB.with(|hub| {
        if let Some(rt) = hub.borrow_mut().hosts.get_mut(&host) {
            rt.blocker_dismissed = rt.blocker_kind;
            rt.blocker_kind = None;
        }
    });
    clear_blocker(window);
}

/// 弹窗里的「重试」：清掉已弹标记后强制刷新当前主机。
pub(crate) fn retry_blocker(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    tabs: &Rc<VecModel<TabInfo>>,
    statuses: &TabStatuses,
    store: &ConfigStore,
) {
    let host = window.get_docker_blocker_host().to_string();
    let session = window.get_docker_blocker_session().to_string();
    window.set_docker_blocker_kind("".into());
    if !session.is_empty() {
        open_saved_session(window, store, runtime, handles, tabs, statuses, &session);
        return;
    }
    if host.is_empty() {
        return;
    }
    HUB.with(|hub| {
        if let Some(rt) = hub.borrow_mut().hosts.get_mut(&host) {
            rt.blocker_kind = None;
            rt.blocker_dismissed = None;
            rt.backoff_until = None;
        }
    });
    refresh_forced(window, runtime, handles, tabs, statuses, store);
}

/// 切到该主机已有的 SSH 终端标签（弹窗里的「打开终端」）。
pub(crate) fn focus_host_terminal(window: &AppWindow, key: &str) {
    let tab_id = HUB.with(|hub| {
        hub.borrow()
            .hosts
            .get(key)
            .map(|rt| rt.tab_id.clone())
            .filter(|id| !id.is_empty())
    });
    let Some(tab_id) = tab_id else {
        // 连接失败场景：没有可用标签，退回主机页让用户重连。
        window.set_nav_page(0);
        return;
    };
    window.set_nav_page(1);
    let panes = window.get_panes();
    for index in 0..panes.row_count() {
        let Some(pane) = panes.row_data(index) else { continue };
        let found = (0..pane.tabs.row_count())
            .any(|i| pane.tabs.row_data(i).is_some_and(|t| t.id.as_str() == tab_id));
        if found {
            window.invoke_pane_tab_selected(pane.id, tab_id.into());
            return;
        }
    }
    window.set_active_tab_id(tab_id.into());
}

/// 查询 `systemctl is-active / is-enabled docker` 与 `docker version`；失败只把状态留在「未知」。
///
/// 与容器列表刷新分开走：daemon 没起来时 `docker ps` 会失败，但服务状态仍然要能显示。
/// 版本命令单独发一条并跟随用户设置的提权方式（`probe()` 本身不提权）；
/// 版本失败只清空版本号，绝不影响在线判定。
fn probe_service(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    key: &str,
    force_version: bool,
) {
    let (tab_id, generation, need_version, elev, password) = HUB.with(|hub| {
        let hub = hub.borrow();
        let Some(rt) = hub.hosts.get(key) else {
            return (None, 0, false, None, None);
        };
        let need_version = force_version || rt.docker_version.is_empty();
        let elev = elevation_for(rt, window.get_docker_use_sudo(), false);
        let password = password_for(rt, &elev);
        (
            Some(rt.tab_id.clone()),
            rt.generation,
            need_version,
            Some(elev),
            password,
        )
    });
    let Some(tab_id) = tab_id else { return };
    let rx = {
        let hs = handles.borrow();
        hs.get(&tab_id)
            .map(|handle| handle.exec(command::probe(), Duration::from_secs(8), 16 * 1024))
    };
    let Some(rx) = rx else { return };

    let version_rx = match (need_version, elev) {
        (true, Some(elev)) => {
            let cmd = command::version(&elev);
            let hs = handles.borrow();
            hs.get(&tab_id).map(|handle| match password {
                Some(password) => {
                    handle.exec_privileged(cmd, password, Duration::from_secs(8), 8 * 1024)
                }
                None => handle.exec(cmd, Duration::from_secs(8), 8 * 1024),
            })
        }
        _ => None,
    };

    let weak = window.as_weak();
    let key_owned = key.to_string();
    runtime.spawn(async move {
        let Ok(res) = rx.await else { return };
        let version_res = match version_rx {
            Some(rx) => rx.await.ok(),
            None => None,
        };
        let probe = (!res.timed_out)
            .then(|| command::parse_probe(&res.stdout))
            .filter(|probe| !probe.is_empty());
        let version = version_res
            .as_ref()
            .filter(|res| !res.timed_out && res.error.is_none() && res.exit_code.unwrap_or(1) == 0)
            .and_then(|res| parse::parse_version(&res.stdout));
        // 探测超时且不需要版本时无事可做；需要版本时即使版本取不到也要清掉旧值。
        if probe.is_none() && !need_version {
            return;
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else { return };
            HUB.with(|hub| {
                let mut hub = hub.borrow_mut();
                {
                    let Some(rt) = hub.hosts.get_mut(&key_owned) else { return };
                    if rt.generation != generation {
                        return;
                    }
                    if let Some(probe) = probe {
                        rt.service_up = probe.service_up;
                        rt.service_enabled = probe.service_enabled;
                        rt.direct = probe.direct;
                        rt.sudo_nopass = probe.sudo_nopass;
                    }
                    if need_version {
                        rt.docker_version = version.unwrap_or_default();
                    }
                }
                apply_active(&window, &mut hub);
            });
        });
    });
}

/// 启动 / 停止 Docker 服务。`sudo -n` 没有免密授权时直接失败，不弹提权框。
pub(crate) fn toggle_service(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    key: &str,
    start: bool,
) {
    let (tab_id, pending) = HUB.with(|hub| {
        let hub = hub.borrow();
        hub.hosts
            .get(key)
            .map_or((None, true), |h| (Some(h.tab_id.clone()), h.service_pending))
    });
    if pending {
        return;
    }
    let Some(tab_id) = tab_id else { return };

    let (elev, password) = HUB.with(|hub| {
        let hub = hub.borrow();
        hub.hosts.get(key).map_or_else(
            || (command::Elevation::direct(), None),
            |rt| {
                let elev = elevation_for(rt, window.get_docker_use_sudo(), true);
                let password = password_for(rt, &elev);
                (elev, password)
            },
        )
    });
    let mode = elev.mode;
    HUB.with(|hub| {
        if let Some(rt) = hub.borrow_mut().hosts.get_mut(key) {
            rt.last_mode = mode;
        }
    });
    let cmd = if start {
        command::service_start(&elev)
    } else {
        command::service_stop(&elev)
    };
    let rx = {
        let hs = handles.borrow();
        hs.get(&tab_id).map(|handle| match password {
            Some(password) => {
                handle.exec_privileged(cmd, password, Duration::from_secs(25), 32 * 1024)
            }
            None => handle.exec(cmd, Duration::from_secs(25), 32 * 1024),
        })
    };
    let Some(rx) = rx else {
        show_action(
            window,
            t("当前主机没有可用的 SSH 会话", "No live SSH session for this host"),
            true,
        );
        return;
    };

    HUB.with(|hub| {
        if let Some(rt) = hub.borrow_mut().hosts.get_mut(key) {
            rt.service_pending = true;
        }
    });
    window.set_docker_service_pending(true);

    let weak = window.as_weak();
    let key_owned = key.to_string();
    runtime.spawn(async move {
        let result = rx.await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else { return };
            let failure = match &result {
                Ok(res) if !res.timed_out && res.error.is_none() && res.exit_code == Some(0) => None,
                Ok(res) => Some(classify_docker(&res.stderr, res.exit_code, res.timed_out)),
                Err(_) => Some(DockerFailure::SessionLost),
            };
            HUB.with(|hub| {
                let mut hub = hub.borrow_mut();
                {
                    let Some(rt) = hub.hosts.get_mut(&key_owned) else { return };
                    rt.service_pending = false;
                    if failure.is_none() {
                        rt.service_up = Some(start);
                    }
                }
                apply_active(&window, &mut hub);
            });
            window.set_docker_service_pending(false);
            match failure {
                None => {
                    show_action(
                        &window,
                        if start {
                            t("Docker 服务已启动", "Docker service started")
                        } else {
                            t("Docker 服务已停止", "Docker service stopped")
                        },
                        false,
                    );
                    window.invoke_refresh_docker_now();
                }
                Some(DockerFailure::NoPermission) => show_action(
                    &window,
                    error_text(Some(DockerFailure::NoPermission), mode),
                    true,
                ),
                Some(other) => show_action(&window, error_text(Some(other), mode), true),
            }
        });
    });
}

/// 启动 / 停止 / 重启 / 删除。删除走「先 stop 再 rm」，不默认 `-f`。
pub(crate) fn act(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    key: &str,
    container_id: &str,
    action: i32,
) {
    let container_id = container_id.trim();
    // 同一容器已有操作在飞：忽略重复点击，避免连点下发多条命令。
    if !container_id.is_empty() && window.get_docker_pending_id().as_str() == container_id {
        return;
    }
    let (elev, password) = HUB.with(|hub| {
        let hub = hub.borrow();
        hub.hosts.get(key).map_or_else(
            || (command::Elevation::direct(), None),
            |rt| {
                let elev = elevation_for(rt, window.get_docker_use_sudo(), false);
                let password = password_for(rt, &elev);
                (elev, password)
            },
        )
    });
    let cmd = match action {
        0 => command::start(container_id, &elev),
        1 => command::stop(container_id, &elev),
        2 => command::restart(container_id, &elev),
        3 => command::remove_sequence(container_id, &elev),
        _ => return,
    };
    let Ok(cmd) = cmd else {
        show_action(window, t("容器 ID 非法", "Invalid container id"), true);
        return;
    };
    let timeout = match action {
        1 => Duration::from_secs(40),
        3 => Duration::from_secs(60),
        _ => Duration::from_secs(30),
    };
    let mutation = Mutation {
        cmd,
        password,
        done: action_done(action),
        timeout,
        mode: elev.mode,
    };
    run_mutation(window, runtime, handles, key, container_id, mutation);
}

/// 拉取日志尾部；结果写入独立日志窗的共享模型。失败也写进窗口，不留「加载中」。
pub(crate) fn fetch_logs(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    key: &str,
    container_id: &str,
) {
    let (elev, password) = HUB.with(|hub| {
        let hub = hub.borrow();
        hub.hosts.get(key).map_or_else(
            || (command::Elevation::direct(), None),
            |rt| {
                let elev = elevation_for(rt, window.get_docker_use_sudo(), false);
                let password = password_for(rt, &elev);
                (elev, password)
            },
        )
    });
    let mode = elev.mode;
    let Ok(cmd) = command::logs(container_id, 200, false, &elev) else {
        show_action(window, t("容器 ID 非法", "Invalid container id"), true);
        return;
    };
    let Some((tab_id, host_label, container_label)) = HUB.with(|hub| {
        let hub = hub.borrow();
        hub.hosts.get(key).map(|rt| {
            let host = if rt.title.is_empty() {
                key.to_string()
            } else {
                rt.title.clone()
            };
            let container = rt
                .containers
                .iter()
                .find(|c| c.id == container_id)
                .map(|c| c.name.clone())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| container_id.to_string());
            (rt.tab_id.clone(), host, container)
        })
    }) else {
        show_action(
            window,
            t("当前主机没有可用的 SSH 会话", "No live SSH session for this host"),
            true,
        );
        return;
    };

    let seq = LOG_SEQ.with(|s| {
        let next = s.get() + 1;
        s.set(next);
        next
    });
    let text_cell = log_text_cell();
    *text_cell.borrow_mut() = String::new();
    super::sub_windows::show_docker_log(
        window,
        &text_cell,
        &host_label,
        &container_label,
        true,
    );

    let rx = {
        let hs = handles.borrow();
        hs.get(&tab_id).map(|h| match password {
            Some(password) => {
                h.exec_privileged(cmd, password, Duration::from_secs(20), PS_MAX_OUTPUT)
            }
            None => h.exec(cmd, Duration::from_secs(20), PS_MAX_OUTPUT),
        })
    };
    let Some(rx) = rx else {
        if let Some(log) = super::sub_windows::docker_log_weak().upgrade() {
            log.set_loading(false);
        }
        set_log_error(
            &text_cell,
            t("会话已关闭，无法读取日志", "The session closed; cannot read logs"),
        );
        return;
    };
    runtime.spawn(async move {
        let result = rx.await;
        let _ = slint::invoke_from_event_loop(move || {
            // 已切到别的容器：丢弃这次结果，避免旧日志覆盖新容器。
            if LOG_SEQ.with(|s| s.get()) != seq {
                return;
            }
            let text_cell = log_text_cell();
            let log = super::sub_windows::docker_log_weak().upgrade();
            if let Some(log) = log.as_ref() {
                log.set_loading(false);
            }
            let Ok(res) = result else {
                set_log_error(
                    &text_cell,
                    t("会话已关闭，无法读取日志", "The session closed; cannot read logs"),
                );
                return;
            };
            if res.timed_out || res.error.is_some() || res.exit_code.unwrap_or(1) != 0 {
                let why = error_text(
                    Some(classify_docker(&res.stderr, res.exit_code, res.timed_out)),
                    mode,
                );
                set_log_error(&text_cell, why);
                return;
            }
            let (lines, truncated) = parse::parse_logs(&res.stdout, &res.stderr, 200);
            let joined = lines.join("\n");
            *text_cell.borrow_mut() = joined.clone();
            if let Some(log) = log.as_ref() {
                log.set_log_text(joined.into());
                log.set_truncated(truncated);
            }
        });
    });
}

/// 复制容器的 12 位短 ID，并在 Docker 页右下角给出提示。
pub(crate) fn copy_container_id(window: &AppWindow, id: &str) {
    copy_container_text(window, id, t("已复制容器 ID", "Container ID copied"));
}

/// 复制容器名，并在 Docker 页右下角给出提示。
pub(crate) fn copy_container_name(window: &AppWindow, name: &str) {
    copy_container_text(window, name, t("已复制容器名", "Container name copied"));
}

fn copy_container_text(window: &AppWindow, text: &str, message: &'static str) {
    let text = text.to_string();
    // arboard 在 Windows 会泵消息循环，必须离开 UI 线程。
    std::thread::spawn(move || super::clipboard_set_text(text));
    show_action(window, message, false);
}

/// 双击端口列：按容器 id 取出映射，交给独立窗口显示。
pub(crate) fn open_port_map(window: &AppWindow, key: &str, container_id: &str) {
    let found = HUB.with(|hub| -> Option<(String, String, Vec<String>)> {
        let hub = hub.borrow();
        let rt = hub.hosts.get(key)?;
        let container = rt.containers.iter().find(|c| c.id == container_id)?;
        Some((
            container.name.clone(),
            rt.title.clone(),
            parse::split_ports(&container.ports),
        ))
    });
    let Some((name, host, lines)) = found else {
        return;
    };
    super::sub_windows::show_port_map(window, &name, &host, &lines);
}

fn log_text_cell() -> Rc<RefCell<String>> {
    LOG_TEXT.with(|slot| {
        slot.borrow_mut()
            .get_or_insert_with(|| Rc::new(RefCell::new(String::new())))
            .clone()
    })
}

fn set_log_error(text_cell: &Rc<RefCell<String>>, message: &str) {
    *text_cell.borrow_mut() = message.to_string();
    if let Some(log) = super::sub_windows::docker_log_weak().upgrade() {
        log.set_log_text(message.into());
    }
}

/// 操作结果条：4 秒后自动清空，序号防止旧定时器清掉新消息。
fn show_action(window: &AppWindow, text: impl Into<slint::SharedString>, is_error: bool) {
    let seq = ACTION_SEQ.with(|s| {
        let next = s.get() + 1;
        s.set(next);
        next
    });
    window.set_docker_action_text(text.into());
    window.set_docker_action_error(is_error);
    let weak = window.as_weak();
    slint::Timer::single_shot(Duration::from_secs(4), move || {
        let Some(window) = weak.upgrade() else { return };
        if ACTION_SEQ.with(|s| s.get()) == seq {
            window.set_docker_action_text("".into());
            window.set_docker_action_error(false);
        }
    });
}

fn action_done(action: i32) -> &'static str {
    match action {
        0 => t("已启动", "Started"),
        1 => t("已停止", "Stopped"),
        2 => t("已重启", "Restarted"),
        _ => t("已删除", "Removed"),
    }
}

/// 一次容器操作：命令、成功文案与超时打包，避免函数参数过多。
struct Mutation {
    cmd: String,
    /// 提权密码；None = 不需要密码。
    password: Option<Secret>,
    done: &'static str,
    timeout: Duration,
    /// 本次命令实际用的提权方式，失败文案按它分支。
    mode: command::Mode,
}

fn run_mutation(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    key: &str,
    container_id: &str,
    mutation: Mutation,
) {
    let Mutation {
        cmd,
        password,
        done,
        timeout,
        mode,
    } = mutation;
    let Some(tab_id) = HUB.with(|hub| hub.borrow().hosts.get(key).map(|h| h.tab_id.clone())) else {
        show_action(
            window,
            t("当前主机没有可用的 SSH 会话", "No live SSH session for this host"),
            true,
        );
        return;
    };
    let rx = {
        let hs = handles.borrow();
        hs.get(&tab_id).map(|h| match password {
            Some(password) => h.exec_privileged(cmd, password, timeout, 64 * 1024),
            None => h.exec(cmd, timeout, 64 * 1024),
        })
    };
    let Some(rx) = rx else {
        show_action(
            window,
            t("会话已关闭，操作未下发", "The session closed; the action was not sent"),
            true,
        );
        return;
    };
    window.set_docker_pending_id(container_id.into());
    let weak = window.as_weak();
    let id = container_id.to_string();
    runtime.spawn(async move {
        let result = rx.await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else { return };
            window.set_docker_pending_id("".into());
            match result {
                Err(_) => show_action(
                    &window,
                    t("会话已关闭，操作结果未知", "The session closed before the action finished"),
                    true,
                ),
                Ok(res) => {
                    let ok =
                        !res.timed_out && res.error.is_none() && res.exit_code.unwrap_or(1) == 0;
                    if ok {
                        show_action(&window, format!("{} {}", done, id), false);
                    } else {
                        let why = error_text(
                            Some(classify_docker(&res.stderr, res.exit_code, res.timed_out)),
                            mode,
                        );
                        show_action(
                            &window,
                            format!("{}: {}", t("操作失败", "Action failed"), why),
                            true,
                        );
                    }
                }
            }
            window.invoke_refresh_docker_now();
        });
    });
}

/// 进入容器终端：复制当前主机会话开新标签，连接成功后注入 `docker exec -it`。
pub(crate) fn open_terminal(window: &AppWindow, key: &str, container_id: &str) {
    let Some(tab_id) = HUB.with(|hub| hub.borrow().hosts.get(key).map(|h| h.tab_id.clone())) else {
        return;
    };
    let elev = HUB.with(|hub| {
        hub.borrow()
            .hosts
            .get(key)
            .map_or_else(command::Elevation::direct, |rt| {
                // 交互式终端不自动喂密码：需要密码时让 sudo 在终端里提示，用户自己输入。
                elevation_for(rt, window.get_docker_use_sudo(), false)
            })
    });
    let Ok(cmd) = command::exec_shell(container_id, &elev) else {
        show_action(window, t("容器 ID 非法", "Invalid container id"), true);
        return;
    };

    let before: std::collections::HashSet<String> = window
        .get_tabs()
        .iter()
        .map(|t| t.id.to_string())
        .collect();
    // `invoke_tab_duplicate` 是同步回调：返回时新标签已经在模型里，不必等定时器。
    window.invoke_tab_duplicate(tab_id.into());

    let new_tab = window
        .get_tabs()
        .iter()
        .map(|t| t.id.to_string())
        .find(|id| !before.contains(id));
    match new_tab {
        Some(id) => {
            PENDING_TERMINAL.with(|p| p.borrow_mut().insert(id, cmd));
        }
        None => {
            // 兜底：把命令放进日志窗，用户可复制后自己粘贴。
            let text_cell = log_text_cell();
            *text_cell.borrow_mut() = cmd.clone();
            super::sub_windows::show_docker_log(window, &text_cell, "", "docker exec", false);
            show_action(
                window,
                t(
                    "未能自动打开新标签，命令已放到日志面板",
                    "Could not open a tab automatically; the command is shown in the log panel",
                ),
                true,
            );
        }
    }
}

/// Connected 时取走待注入命令；没有则返回 None。
pub(crate) fn take_pending_terminal(tab_id: &str) -> Option<String> {
    PENDING_TERMINAL.with(|p| p.borrow_mut().remove(tab_id))
}

/// 搜索过滤：只重绘当前主机，不触发任何 exec。
pub(crate) fn set_filter(window: &AppWindow, text: &str) {
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        hub.filter = text.trim().to_ascii_lowercase();
        apply_active(window, &mut hub);
    });
}

struct HostFacts {
    count: i32,
    connected: bool,
    tab_id: String,
    session_id: String,
    title: String,
    title_len: i32,
    last: Option<Instant>,
}

fn new_host_runtime(tab_id: &str) -> HostRuntime {
    HostRuntime {
        tab_id: tab_id.to_string(),
        title: String::new(),
        title_len: 0,
        session_count: 0,
        containers: Vec::new(),
        status: HostStatus::Idle,
        failure: None,
        last_ok: None,
        last_attempt: None,
        generation: 0,
        busy: false,
        queued: false,
        backoff_until: None,
        connected: false,
        session_id: String::new(),
        direct: None,
        sudo_nopass: None,
        last_mode: command::Mode::Direct,
        docker_elevation: String::new(),
        docker_sudo_user: String::new(),
        docker_sudo_password: Secret::default(),
        service_up: None,
        service_enabled: None,
        docker_version: String::new(),
        service_pending: false,
        blocker_kind: None,
        blocker_dismissed: None,
        images: Vec::new(),
        images_status: HostStatus::Idle,
        images_failure: None,
        images_last_ok: None,
        images_busy: false,
        images_generation: 0,
        compose_projects: Vec::new(),
        compose_status: HostStatus::Idle,
        compose_failure: None,
        compose_last_ok: None,
        compose_busy: false,
        compose_generation: 0,
        networks: Vec::new(),
        networks_status: HostStatus::Idle,
        networks_failure: None,
        networks_last_ok: None,
        networks_busy: false,
        networks_generation: 0,
        snapshot_epoch: 0,
        image_usage: None,
        image_usage_epoch: 0,
        network_members: None,
        network_members_epoch: 0,
    }
}

fn sync_hosts(
    hub: &mut DockerHub,
    window: &AppWindow,
    tabs: &VecModel<TabInfo>,
    statuses: &TabStatuses,
    store: &ConfigStore,
) {
    let Ok(statuses) = statuses.lock() else { return };

    let mut by_host: HashMap<String, HostFacts> = HashMap::new();
    for index in 0..tabs.row_count() {
        let Some(tab) = tabs.row_data(index) else { continue };
        let id = tab.id.to_string();
        let Some(st) = statuses.get(&id) else { continue };
        if st.kind != SessionKind::Ssh || st.is_local {
            continue;
        }
        let Some(key) = DockerHostKey::parse(&st.host, &st.user) else {
            continue;
        };
        let key = key.as_str().to_string();
        let title = tab.title.to_string();
        let title_len = tab.title_len;
        let session_id = st.session_id.clone();
        let entry = by_host.entry(key).or_insert_with(|| HostFacts {
            count: 0,
            connected: false,
            tab_id: id.clone(),
            session_id: session_id.clone(),
            title: title.clone(),
            title_len,
            last: None,
        });
        entry.count += 1;
        entry.connected |= st.state == 1;
        if st.state == 1 && entry.last.is_none_or(|t| st.last_stats_at.is_some_and(|x| x > t)) {
            entry.last = st.last_stats_at;
            entry.tab_id = id;
            entry.session_id = session_id;
            entry.title = title;
            entry.title_len = title_len;
        }
    }
    drop(statuses);

    // 主机彻底消失（SSH 标签关闭）：连缓存、关闭抑制、待打开一起清掉。
    let vanished: Vec<String> = hub
        .hosts
        .keys()
        .filter(|key| !by_host.contains_key(*key))
        .cloned()
        .collect();
    for key in vanished {
        hub.hosts.remove(&key);
        hub.open.retain(|k| k != &key);
        hub.suppressed.remove(&key);
    }
    PENDING_OPEN.with(|p| p.borrow_mut().retain(|_, v| by_host.contains_key(&v.key)));

    let mut became_connected: HashSet<String> = HashSet::new();
    for (key, facts) in &by_host {
        let rt = hub
            .hosts
            .entry(key.clone())
            .or_insert_with(|| new_host_runtime(&facts.tab_id));
        let was = rt.connected;
        rt.connected = facts.connected;
        rt.tab_id = facts.tab_id.clone();
        rt.session_id = facts.session_id.clone();
        let (elevation, sudo_user, sudo_password) =
            load_credentials(store, &facts.session_id, key);
        rt.docker_elevation = elevation;
        rt.docker_sudo_user = sudo_user;
        rt.docker_sudo_password = sudo_password;
        rt.title = facts.title.clone();
        rt.title_len = facts.title_len;
        rt.session_count = facts.count;
        if facts.connected && !was {
            became_connected.insert(key.clone());
        }
        if !facts.connected {
            rt.status = HostStatus::Offline;
        }
    }

    // 断开后重连：清掉“用户关过”的抑制，允许按自动探索重新打开。
    for key in &became_connected {
        hub.suppressed.remove(key);
    }

    let auto = window.get_docker_auto_discover();
    if hub.auto_last != Some(auto) {
        hub.auto_last = Some(auto);
        if auto {
            hub.welcome_selected = false;
            for (key, facts) in &by_host {
                if facts.connected && !hub.suppressed.contains(key) && !hub.open.contains(key) {
                    hub.open.push(key.clone());
                }
            }
        } else {
            // 关闭自动探索：回到欢迎页；手动打开由 open_host 负责。
            hub.open.clear();
            hub.suppressed.clear();
            hub.active = None;
            hub.welcome_selected = true;
        }
    }

    let pending_keys: Vec<String> = PENDING_OPEN.with(|p| {
        p.borrow().values().map(|v| v.key.clone()).collect()
    });
    for (key, facts) in &by_host {
        if !facts.connected {
            continue;
        }
        let auto_open = auto && !hub.suppressed.contains(key) && became_connected.contains(key);
        let pending_open = pending_keys.iter().any(|pending| pending == key);
        if (auto_open || pending_open) && !hub.open.contains(key) {
            hub.open.push(key.clone());
            if pending_open {
                hub.welcome_selected = false;
            }
        }
    }
    PENDING_OPEN.with(|pending| {
        pending.borrow_mut().retain(|_, v| {
            by_host.get(&v.key).is_some_and(|f| f.connected) && !hub.open.contains(&v.key)
        });
    });

    if hub.welcome_selected {
        hub.active = None;
    } else if hub
        .active
        .as_ref()
        .is_none_or(|key| !hub.open.contains(key))
    {
        hub.active = hub
            .open
            .iter()
            .find(|key| hub.hosts.get(*key).is_some_and(|rt| rt.connected))
            .cloned()
            .or_else(|| hub.open.first().cloned());
    }
    window.set_docker_active_host(hub.active.clone().unwrap_or_default().into());
    rebuild_hosts_model(hub, window);
    if hub.active.is_none() {
        super::sub_windows::release_docker_log();
        refresh_welcome(window, hub, store);
    }
}

/// 按 `open` 的顺序重建标签模型（不同步整个 hosts 表）。
fn rebuild_hosts_model(hub: &mut DockerHub, window: &AppWindow) {
    let rows: Vec<DockerHostTab> = hub
        .open
        .iter()
        .filter_map(|key| {
            let rt = hub.hosts.get(key)?;
            Some(DockerHostTab {
                key: key.as_str().into(),
                label: if rt.title.is_empty() {
                    key.as_str().into()
                } else {
                    rt.title.as_str().into()
                },
                connected: rt.connected,
                session_count: rt.session_count,
                count: if rt.containers.is_empty() && rt.last_ok.is_none() {
                    -1
                } else {
                    rt.containers.len() as i32
                },
                title_len: rt.title_len,
                session_id: rt.session_id.as_str().into(),
            })
        })
        .collect();

    let unchanged = hub.hosts_model.row_count() == rows.len()
        && (0..rows.len()).all(|i| hub.hosts_model.row_data(i).as_ref() == rows.get(i));
    if !unchanged {
        hub.hosts_model.set_vec(rows);
    }
    window.set_docker_hosts(ModelRc::from(hub.hosts_model.clone()));
}

/// 欢迎页的已保存 SSH 会话（分组/折叠与主机页共用）。
fn refresh_welcome(window: &AppWindow, hub: &mut DockerHub, store: &ConfigStore) {
    let rows = super::session_models::docker_session_rows(store);
    let unchanged = hub.welcome_model.row_count() == rows.len()
        && (0..rows.len()).all(|i| hub.welcome_model.row_data(i).as_ref() == rows.get(i));
    if !unchanged {
        hub.welcome_model.set_vec(rows);
    }
    window.set_docker_sessions(ModelRc::from(hub.welcome_model.clone()));
}

/// 选提权方式：开关关闭一律直连；否则免密 sudo > 直连 > 回退。
///
/// 直连探测为 `None`（探测失败 / 还没跑）时也先直连：宁可跑一次真实命令拿到
/// 准确错误，也不要凭猜测强上 `sudo -n` 报「需要免密 sudo」。
fn elevation_for(rt: &HostRuntime, setting_on: bool, needs_root: bool) -> command::Elevation {
    let mode = choose_mode(
        setting_on,
        rt.direct,
        rt.sudo_nopass,
        !rt.docker_sudo_password.is_empty(),
        needs_root,
    );
    match mode {
        command::Mode::Direct => command::Elevation::direct(),
        command::Mode::SudoNoPass => command::Elevation::sudo_nopass(&rt.docker_sudo_user),
        command::Mode::SudoPassword => {
            if rt.docker_elevation == "su" {
                command::Elevation::su(&rt.docker_sudo_user)
            } else {
                command::Elevation::sudo_password(&rt.docker_sudo_user)
            }
        }
        command::Mode::SuPassword => command::Elevation::su(&rt.docker_sudo_user),
    }
}

/// 纯策略：给定开关与探测结果，选哪种执行方式。
///
/// 顺序即优先级：免密 sudo > 直连 > 密码凭据 > root 命令回退。
fn choose_mode(
    setting_on: bool,
    direct: Option<bool>,
    sudo_nopass: Option<bool>,
    has_password: bool,
    needs_root: bool,
) -> command::Mode {
    if !setting_on {
        return command::Mode::Direct;
    }
    if sudo_nopass == Some(true) {
        return command::Mode::SudoNoPass;
    }
    // 服务启停必须 root：能直连 docker 不代表能 systemctl。
    if needs_root {
        return if has_password {
            command::Mode::SudoPassword
        } else {
            command::Mode::SudoNoPass
        };
    }
    // 直连已知不可用、sudo 还没探测：先用 sudo 试一次（首次进页面的兜底）。
    if direct == Some(false) && sudo_nopass.is_none() {
        return command::Mode::SudoNoPass;
    }
    // 探测未知也先直连：别凭猜测强上 sudo 报「需要免密 sudo」。
    if direct != Some(false) {
        return command::Mode::Direct;
    }
    if has_password {
        return command::Mode::SudoPassword;
    }
    command::Mode::Direct
}

/// 概览页显示的权限状态（Rust 侧翻译，Slint 只负责排版）。
fn permission_text(rt: &HostRuntime, setting_on: bool) -> &'static str {
    if !setting_on {
        return t("按登录用户执行", "Runs as the signed-in user");
    }
    if rt.sudo_nopass == Some(true) {
        return t("免密 sudo 可用", "Passwordless sudo available");
    }
    if rt.direct == Some(true) {
        return t("已可直接访问 docker", "Direct docker access");
    }
    if !rt.docker_sudo_password.is_empty() {
        return t("已保存提权凭据", "Elevation credentials saved");
    }
    if rt.direct == Some(false) {
        return t("没有权限，需配置凭据", "No permission — configure credentials");
    }
    t("未知（刷新可重新探测）", "Unknown — refresh to probe")
}

/// 凭据优先取代表会话；同一 `user@host` 的其它会话配置了也能用上。
fn load_credentials(
    store: &ConfigStore,
    session_id: &str,
    host_key: &str,
) -> (String, String, Secret) {
    let configured = |s: &crate::config::Session| {
        !s.docker_elevation.is_empty()
            || !s.docker_sudo_user.is_empty()
            || !s.docker_sudo_password.is_empty()
    };
    if let Some(session) = store.get(session_id)
        && configured(session)
    {
        return (
            session.docker_elevation.clone(),
            session.docker_sudo_user.clone(),
            session.docker_sudo_password.clone(),
        );
    }
    let (user, host) = host_key.split_once('@').unwrap_or(("", host_key));
    store
        .sessions()
        .iter()
        .find(|s| {
            s.kind == SessionKind::Ssh
                && s.user == user
                && s.host == host
                && !s.docker_sudo_password.is_empty()
        })
        .map(|s| {
            (
                s.docker_elevation.clone(),
                s.docker_sudo_user.clone(),
                s.docker_sudo_password.clone(),
            )
        })
        .unwrap_or_default()
}

/// 本机是否要带密码执行：仅当命令需要提权且配置了密码。
fn password_for(rt: &HostRuntime, elev: &command::Elevation) -> Option<Secret> {
    if elev.mode.uses_password() && !rt.docker_sudo_password.is_empty() {
        Some(rt.docker_sudo_password.clone())
    } else {
        None
    }
}

fn refresh_key(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    hub: &mut DockerHub,
    key: &str,
    force: bool,
) {
    let Some(rt) = hub.hosts.get_mut(key) else { return };
    if rt.busy {
        rt.queued = true;
        return;
    }
    if !force && rt.backoff_until.is_some_and(|t| Instant::now() < t) {
        apply_active(window, hub);
        return;
    }
    let elev = elevation_for(rt, window.get_docker_use_sudo(), false);
    rt.last_mode = elev.mode;
    let password = password_for(rt, &elev);
    let cmd = command::ps(true, &elev);
    let Some(rx) = ({
        let hs = handles.borrow();
        hs.get(&rt.tab_id).map(|handle| match password {
            Some(password) => {
                handle.exec_privileged(cmd, password, PS_TIMEOUT, PS_MAX_OUTPUT)
            }
            None => handle.exec(cmd, PS_TIMEOUT, PS_MAX_OUTPUT),
        })
    }) else {
        rt.status = HostStatus::Offline;
        rt.failure = Some(DockerFailure::SessionLost);
        apply_active(window, hub);
        return;
    };

    rt.busy = true;
    rt.queued = false;
    rt.generation += 1;
    rt.last_attempt = Some(Instant::now());
    let generation = rt.generation;
    rt.status = HostStatus::Loading;
    apply_active(window, hub);

    let weak = window.as_weak();
    let key_owned = key.to_string();
    runtime.spawn(async move {
        let result = rx.await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else { return };
            HUB.with(|hub| {
                let mut hub = hub.borrow_mut();
                apply_result(&window, &mut hub, &key_owned, generation, result);
            });
        });
    });
}

/// 统一选择普通 / 带密码的执行通道。
fn exec_on(
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    tab_id: &str,
    cmd: String,
    password: Option<Secret>,
    timeout: Duration,
    max_output: usize,
) -> Option<tokio::sync::oneshot::Receiver<ExecResult>> {
    let hs = handles.borrow();
    hs.get(tab_id).map(|handle| match password {
        Some(password) => handle.exec_privileged(cmd, password, timeout, max_output),
        None => handle.exec(cmd, timeout, max_output),
    })
}

/// 失败时把 stderr 头部写进 debug 日志，便于定位远端命令的真实报错。
fn debug_failure(what: &str, res: &ExecResult) {
    let stderr = res.stderr.trim();
    if stderr.is_empty() {
        return;
    }
    let head: String = stderr.chars().take(400).collect();
    tracing::debug!(target: "rudder::docker", "{what} failed: {head}");
}

fn refresh_images(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    hub: &mut DockerHub,
    key: &str,
    force: bool,
) {
    let request = {
        let Some(rt) = hub.hosts.get_mut(key) else { return };
        if rt.images_busy {
            return;
        }
        if !force && rt.images_last_ok.is_some_and(|t| t.elapsed() < FRESH_FOR) {
            return;
        }
        let elev = elevation_for(rt, window.get_docker_use_sudo(), false);
        let password = password_for(rt, &elev);
        match exec_on(
            handles,
            &rt.tab_id,
            command::images(&elev),
            password,
            PS_TIMEOUT,
            PS_MAX_OUTPUT,
        ) {
            Some(images) => {
                rt.images_busy = true;
                rt.images_generation += 1;
                rt.images_status = HostStatus::Loading;
                rt.images_failure = None;
                Some((images, rt.images_generation))
            }
            None => {
                rt.images_status = HostStatus::Offline;
                rt.images_failure = Some(DockerFailure::SessionLost);
                None
            }
        }
    };
    apply_section_lists(window, hub);
    let Some((images, generation)) = request else {
        return;
    };
    let weak = window.as_weak();
    let key_owned = key.to_string();
    runtime.spawn(async move {
        let result = images.await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else { return };
            HUB.with(|hub| {
                let mut hub = hub.borrow_mut();
                apply_images_result(&window, &mut hub, &key_owned, generation, result);
            });
        });
    });
}

fn apply_images_result(
    window: &AppWindow,
    hub: &mut DockerHub,
    key: &str,
    generation: u64,
    result: Result<ExecResult, tokio::sync::oneshot::error::RecvError>,
) {
    {
        let Some(rt) = hub.hosts.get_mut(key) else { return };
        if rt.images_generation != generation {
            return;
        }
        rt.images_busy = false;
        match result {
            Err(_) => {
                rt.images_status = HostStatus::Error;
                rt.images_failure = Some(DockerFailure::SessionLost);
            }
            Ok(res) => {
                let lost = res.exit_code.is_none() && !res.timed_out && res.error.is_none();
                if res.timed_out || res.error.is_some() || lost || res.exit_code.unwrap_or(1) != 0 {
                    debug_failure("docker images", &res);
                    rt.images_status = HostStatus::Error;
                    rt.images_failure = Some(if lost {
                        DockerFailure::SessionLost
                    } else {
                        classify_docker(&res.stderr, res.exit_code, res.timed_out)
                    });
                } else {
                    let parsed = parse::parse_images(&res.stdout);
                    tracing::debug!(
                        target: "rudder::docker",
                        "docker images: {} rows",
                        parsed.rows.len()
                    );
                    rt.images = parsed.rows;
                    rt.images_status = if rt.images.is_empty() {
                        HostStatus::Empty
                    } else {
                        HostStatus::Ok
                    };
                    rt.images_failure = None;
                    rt.images_last_ok = Some(Instant::now());
                }
            }
        }
    }
    if hub.active.as_deref() == Some(key) {
        apply_section_lists(window, hub);
    }
}

fn refresh_compose(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    hub: &mut DockerHub,
    key: &str,
    force: bool,
) {
    let request = {
        let Some(rt) = hub.hosts.get_mut(key) else { return };
        if rt.compose_busy {
            return;
        }
        if !force && rt.compose_last_ok.is_some_and(|t| t.elapsed() < FRESH_FOR) {
            return;
        }
        let elev = elevation_for(rt, window.get_docker_use_sudo(), false);
        let password = password_for(rt, &elev);
        let cmd = command::compose_ls(true, &elev);
        match exec_on(handles, &rt.tab_id, cmd, password, PS_TIMEOUT, PS_MAX_OUTPUT) {
            Some(rx) => {
                rt.compose_busy = true;
                rt.compose_generation += 1;
                rt.compose_status = HostStatus::Loading;
                rt.compose_failure = None;
                Some((rx, rt.compose_generation))
            }
            None => {
                rt.compose_status = HostStatus::Offline;
                rt.compose_failure = Some(DockerFailure::SessionLost);
                None
            }
        }
    };
    apply_section_lists(window, hub);
    let Some((rx, generation)) = request else { return };
    let weak = window.as_weak();
    let key_owned = key.to_string();
    runtime.spawn(async move {
        let result = rx.await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else { return };
            HUB.with(|hub| {
                let mut hub = hub.borrow_mut();
                apply_compose_result(&window, &mut hub, &key_owned, generation, result);
            });
        });
    });
}

fn apply_compose_result(
    window: &AppWindow,
    hub: &mut DockerHub,
    key: &str,
    generation: u64,
    result: Result<ExecResult, tokio::sync::oneshot::error::RecvError>,
) {
    {
        let Some(rt) = hub.hosts.get_mut(key) else { return };
        if rt.compose_generation != generation {
            return;
        }
        rt.compose_busy = false;
        match result {
            Err(_) => {
                rt.compose_status = HostStatus::Error;
                rt.compose_failure = Some(DockerFailure::SessionLost);
            }
            Ok(res) => {
                let lost = res.exit_code.is_none() && !res.timed_out && res.error.is_none();
                if res.timed_out || res.error.is_some() || lost || res.exit_code.unwrap_or(1) != 0 {
                    debug_failure("docker compose ls", &res);
                    rt.compose_status = HostStatus::Error;
                    rt.compose_failure = Some(if lost {
                        DockerFailure::SessionLost
                    } else {
                        classify_docker(&res.stderr, res.exit_code, res.timed_out)
                    });
                } else {
                    let parsed = parse::parse_compose_ls(&res.stdout);
                    rt.compose_projects = parsed.rows;
                    rt.compose_status = if rt.compose_projects.is_empty() {
                        HostStatus::Empty
                    } else {
                        HostStatus::Ok
                    };
                    rt.compose_failure = None;
                    rt.compose_last_ok = Some(Instant::now());
                }
            }
        }
    }
    if hub.active.as_deref() == Some(key) {
        apply_section_lists(window, hub);
    }
}

fn refresh_networks(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    hub: &mut DockerHub,
    key: &str,
    force: bool,
) {
    let request = {
        let Some(rt) = hub.hosts.get_mut(key) else { return };
        if rt.networks_busy {
            return;
        }
        if !force && rt.networks_last_ok.is_some_and(|t| t.elapsed() < FRESH_FOR) {
            return;
        }
        let elev = elevation_for(rt, window.get_docker_use_sudo(), false);
        let password = password_for(rt, &elev);
        match exec_on(
            handles,
            &rt.tab_id,
            command::network_ls(&elev),
            password,
            PS_TIMEOUT,
            PS_MAX_OUTPUT,
        ) {
            Some(ls) => {
                rt.networks_busy = true;
                rt.networks_generation += 1;
                rt.networks_status = HostStatus::Loading;
                rt.networks_failure = None;
                Some((ls, rt.networks_generation))
            }
            None => {
                rt.networks_status = HostStatus::Offline;
                rt.networks_failure = Some(DockerFailure::SessionLost);
                None
            }
        }
    };
    apply_section_lists(window, hub);
    let Some((ls, generation)) = request else {
        return;
    };
    let weak = window.as_weak();
    let key_owned = key.to_string();
    runtime.spawn(async move {
        let ls_result = ls.await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else { return };
            HUB.with(|hub| {
                let mut hub = hub.borrow_mut();
                apply_networks_result(&window, &mut hub, &key_owned, generation, ls_result);
            });
        });
    });
}

fn apply_networks_result(
    window: &AppWindow,
    hub: &mut DockerHub,
    key: &str,
    generation: u64,
    ls_result: Result<ExecResult, tokio::sync::oneshot::error::RecvError>,
) {
    {
        let Some(rt) = hub.hosts.get_mut(key) else { return };
        if rt.networks_generation != generation {
            return;
        }
        rt.networks_busy = false;
        match ls_result {
            Err(_) => {
                rt.networks_status = HostStatus::Error;
                rt.networks_failure = Some(DockerFailure::SessionLost);
            }
            Ok(res) => {
                let lost = res.exit_code.is_none() && !res.timed_out && res.error.is_none();
                if res.timed_out || res.error.is_some() || lost || res.exit_code.unwrap_or(1) != 0 {
                    debug_failure("docker network ls", &res);
                    rt.networks_status = HostStatus::Error;
                    rt.networks_failure = Some(if lost {
                        DockerFailure::SessionLost
                    } else {
                        classify_docker(&res.stderr, res.exit_code, res.timed_out)
                    });
                } else {
                    let parsed = parse::parse_network_ls(&res.stdout);
                    rt.networks = parsed.rows;
                    rt.networks_status = if rt.networks.is_empty() {
                        HostStatus::Empty
                    } else {
                        HostStatus::Ok
                    };
                    rt.networks_failure = None;
                    rt.networks_last_ok = Some(Instant::now());
                }
            }
        }
    }
    if hub.active.as_deref() == Some(key) {
        apply_section_lists(window, hub);
    }
}

pub(crate) fn set_section_filter(window: &AppWindow, section: &str, text: &str) {
    let needle = text.trim().to_ascii_lowercase();
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        match section {
            "images" => hub.image_filter = needle,
            "compose" => hub.compose_filter = needle,
            _ => return,
        }
        apply_section_lists(window, &mut hub);
    });
}

/// Docker Hub 搜索：输入去抖后请求（本机 HTTP）。
pub(crate) fn search_registry(window: &AppWindow, query: &str) {
    registry::search(window, query);
}

/// 镜像仓库刷新按钮。
pub(crate) fn refresh_registry(window: &AppWindow) {
    registry::refresh(window);
}

/// 镜像仓库上一页 / 下一页。
pub(crate) fn registry_page(window: &AppWindow, delta: i32) {
    registry::page(window, delta);
}

/// 二级页切换：只记录当前页；三页数据在连上/每拍已拉过，切过去直接读缓存。
pub(crate) fn set_section(window: &AppWindow, section: &str) {
    HUB.with(|hub| hub.borrow_mut().section = section.to_string());
    refresh_active_section(window, false);
}

/// 页面刷新按钮：强制重拉容器 + 该页列表（派生数据随容器快照重算）。
pub(crate) fn refresh_section_now(window: &AppWindow, section: &str) {
    let Some(ctx) = refresh_ctx() else { return };
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        hub.section = section.to_string();
        let Some(key) = hub.active.clone() else { return };
        if let Some(rt) = hub.hosts.get_mut(&key) {
            rt.backoff_until = None;
        }
        refresh_key(window, &ctx.runtime, &ctx.handles, &mut hub, &key, true);
        refresh_section_lists(window, &ctx.runtime, &ctx.handles, &mut hub, &key, true);
    });
}

/// 定时器 / 进入 Docker 页时按当前二级页刷新。
pub(crate) fn refresh_active_section(window: &AppWindow, force: bool) {
    let Some(ctx) = refresh_ctx() else { return };
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        let Some(key) = hub.active.clone() else { return };
        refresh_section_lists(window, &ctx.runtime, &ctx.handles, &mut hub, &key, force);
    });
}

/// 按 `hub.section` 拉当前二级页的列表。调用方已持有 `hub` 的借用。
fn refresh_section_lists(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    hub: &mut DockerHub,
    key: &str,
    force: bool,
) {
    match hub.section.as_str() {
        "images" => refresh_images(window, runtime, handles, hub, key, force),
        "compose" => refresh_compose(window, runtime, handles, hub, key, force),
        "networks" => refresh_networks(window, runtime, handles, hub, key, force),
        _ => {}
    }
}

/// 三类二级页一起刷：连上、10s 拍、操作完成都走它。
fn refresh_all_sections(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    hub: &mut DockerHub,
    key: &str,
    force: bool,
) {
    refresh_images(window, runtime, handles, hub, key, force);
    refresh_compose(window, runtime, handles, hub, key, force);
    refresh_networks(window, runtime, handles, hub, key, force);
}

fn apply_result(
    window: &AppWindow,
    hub: &mut DockerHub,
    key: &str,
    generation: u64,
    result: Result<ExecResult, tokio::sync::oneshot::error::RecvError>,
) {
    let queued = {
        let Some(rt) = hub.hosts.get_mut(key) else { return };
        if rt.generation != generation {
            return;
        }
        rt.busy = false;

        match result {
            Err(_) => {
                rt.status = HostStatus::Offline;
                rt.failure = Some(DockerFailure::SessionLost);
            }
            Ok(res) if res.timed_out => {
                rt.status = HostStatus::Error;
                rt.failure = Some(DockerFailure::Timeout);
            }
            Ok(res) if res.error.is_some() => {
                rt.status = HostStatus::Error;
                rt.failure = Some(DockerFailure::Other);
            }
            Ok(res) if res.exit_code.unwrap_or(1) != 0 => {
                rt.status = HostStatus::Error;
                rt.failure = Some(classify_docker(&res.stderr, res.exit_code, false));
            }
            Ok(res) => {
                let parsed = parse::parse_ps(&res.stdout);
                if parsed.rows.is_empty() && parsed.bad_lines > 0 {
                    rt.status = HostStatus::Error;
                    rt.failure = Some(DockerFailure::Parse);
                } else {
                    let changed = rt.containers != parsed.rows;
                    rt.containers = parsed.rows;
                    rt.failure = None;
                    rt.last_ok = Some(Instant::now());
                    rt.status = if rt.containers.is_empty() {
                        HostStatus::Empty
                    } else {
                        HostStatus::Ok
                    };
                    rt.backoff_until = None;
                    // 首次成功、或容器集合有变化才换代：没变化时派生数据无需重算。
                    if changed || rt.snapshot_epoch == 0 {
                        rt.snapshot_epoch += 1;
                    }
                }
            }
        }
        // 首次还没探测过权限：直连被拒就立刻用 sudo -n 再试一次，避免
        // 「NOPASSWD 用户第一次进页面先闪一次无权限」。只重试一次。
        if rt.status == HostStatus::Error
            && rt.last_mode == command::Mode::Direct
            && rt.direct.is_none()
            && matches!(rt.failure, Some(DockerFailure::NoPermission))
        {
            rt.direct = Some(false);
            rt.queued = true;
        }
        if rt.status == HostStatus::Error {
            rt.backoff_until = Some(Instant::now() + BACKOFF);
            if let Some(kind) = rt.failure
                && matches!(
                    kind,
                    DockerFailure::NoCli
                        | DockerFailure::NoPermission
                        | DockerFailure::SudoAuthFailed
                        | DockerFailure::DaemonDown
                )
                && rt.blocker_kind != Some(kind)
                && rt.blocker_dismissed != Some(kind)
            {
                rt.blocker_kind = Some(kind);
            }
        } else {
            rt.blocker_kind = None;
            rt.blocker_dismissed = None;
        }
        rt.queued
    };

    let active = hub.active.as_deref() == Some(key);
    if active {
        apply_active(window, hub);
    } else {
        sync_host_count(hub, key);
    }

    // `docker ps` 刚成功：用这份快照重算「使用中」与「网络成员」。
    // 快照没换代时内部直接跳过，不会重复下发命令。
    spawn_derived_refresh(window, hub, key);

    // 不可用弹窗：只在该主机是当前标签、且这一轮确实失败时弹一次。
    let blocker = hub
        .hosts
        .get(key)
        .filter(|_| hub.active.as_deref() == Some(key))
        .and_then(|rt| {
        if rt.status == HostStatus::Error {
            rt.blocker_kind
                .map(|kind| (kind, rt.title.clone(), rt.last_mode))
        } else {
            None
        }
    });
    if let Some((kind, title, mode)) = blocker {
        window.set_docker_blocker_kind(blocker_kind_key(kind).into());
        window.set_docker_blocker_host(key.into());
        window.set_docker_blocker_title(title.into());
        window.set_docker_blocker_detail(error_text(Some(kind), mode).into());
        window.set_docker_blocker_session("".into());
    } else if hub
        .hosts
        .get(key)
        .is_some_and(|rt| rt.status != HostStatus::Error)
        && window.get_docker_blocker_host().as_str() == key
    {
        clear_blocker(window);
    }
    if queued {
        REFRESH_CTX.with(|ctx| {
            if let Some(ctx) = ctx.borrow().as_ref() {
                refresh_key(window, &ctx.runtime, &ctx.handles, hub, key, true);
            }
        });
    }
}

fn sync_host_count(hub: &mut DockerHub, key: &str) {
    let count = hub.hosts.get(key).map_or(-1, |h| {
        if h.containers.is_empty() && h.last_ok.is_none() {
            -1
        } else {
            h.containers.len() as i32
        }
    });
    for index in 0..hub.hosts_model.row_count() {
        if let Some(mut row) = hub.hosts_model.row_data(index)
            && row.key.as_str() == key
        {
            row.count = count;
            hub.hosts_model.set_row_data(index, row);
            break;
        }
    }
}

/// 把当前主机的三份只读列表写进 Slint 模型，并更新概览计数。
/// ps 成功后：用这份快照重算「使用中」与「网络成员」。
///
/// 调用时已持有 `HUB` 的借用，函数内部不得再 `HUB.with`；异步回写时才借。
fn spawn_derived_refresh(window: &AppWindow, hub: &mut DockerHub, key: &str) {
    let Some(ctx) = refresh_ctx() else { return };
    let use_sudo = window.get_docker_use_sudo();
    let mut redraw = false;
    let (epoch, image_req, network_req) = {
        let Some(rt) = hub.hosts.get_mut(key) else { return };
        if rt.snapshot_epoch == 0 || !matches!(rt.status, HostStatus::Ok | HostStatus::Empty) {
            return;
        }
        let epoch = rt.snapshot_epoch;
        let elev = elevation_for(rt, use_sudo, false);
        let password = password_for(rt, &elev);
        let tab_id = rt.tab_id.clone();

        // 镜像：只算运行中的容器；没有运行中的容器就是已知的 0，不必下发命令。
        let image_req = if rt.image_usage_epoch == epoch && rt.image_usage.is_some() {
            None
        } else {
            let running = parse::running_container_ids(&rt.containers);
            if running.is_empty() {
                rt.image_usage = Some(HashMap::new());
                rt.image_usage_epoch = epoch;
                redraw = true;
                None
            } else {
                command::image_usage(&running, &elev).and_then(|cmd| {
                    exec_on(
                        &ctx.handles,
                        &tab_id,
                        cmd,
                        password.clone(),
                        PS_TIMEOUT,
                        PS_MAX_OUTPUT,
                    )
                })
            }
        };

        // 网络：全部容器（含已停止）。`docker network connect` 不改容器 id，
        // 所以每个新代次都重算，不能按 id 集合复用。
        let network_req = if rt.network_members_epoch == epoch && rt.network_members.is_some() {
            None
        } else {
            let all: Vec<String> = rt.containers.iter().map(|item| item.id.clone()).collect();
            if all.is_empty() {
                rt.network_members = Some(HashMap::new());
                rt.network_members_epoch = epoch;
                redraw = true;
                None
            } else {
                command::container_networks(&all, &elev).and_then(|cmd| {
                    exec_on(&ctx.handles, &tab_id, cmd, password, PS_TIMEOUT, PS_MAX_OUTPUT)
                })
            }
        };
        (epoch, image_req, network_req)
    };

    if image_req.is_none() && network_req.is_none() {
        if redraw && hub.active.as_deref() == Some(key) {
            apply_section_lists(window, hub);
        }
        return;
    }

    let weak = window.as_weak();
    let key_owned = key.to_string();
    ctx.runtime.spawn(async move {
        let image_result = match image_req {
            Some(rx) => Some(rx.await),
            None => None,
        };
        let network_result = match network_req {
            Some(rx) => Some(rx.await),
            None => None,
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else { return };
            HUB.with(|hub| {
                let mut hub = hub.borrow_mut();
                apply_derived_result(
                    &window,
                    &mut hub,
                    &key_owned,
                    epoch,
                    image_result,
                    network_result,
                );
            });
        });
    });
}

fn apply_derived_result(
    window: &AppWindow,
    hub: &mut DockerHub,
    key: &str,
    epoch: u64,
    image_result: Option<Result<ExecResult, tokio::sync::oneshot::error::RecvError>>,
    network_result: Option<Result<ExecResult, tokio::sync::oneshot::error::RecvError>>,
) {
    {
        let Some(rt) = hub.hosts.get_mut(key) else { return };
        // 快照已换代：这批结果作废，等新快照重算。
        if rt.snapshot_epoch != epoch {
            return;
        }
        if let Some(result) = image_result
            && let Ok(res) = &result
        {
            if inspect_usable(res) {
                rt.image_usage = Some(parse::parse_image_usage(&res.stdout));
                rt.image_usage_epoch = epoch;
            } else {
                debug_failure("docker inspect (image usage)", res);
            }
        }
        if let Some(result) = network_result
            && let Ok(res) = &result
        {
            if inspect_usable(res) {
                rt.network_members = Some(parse::network_members_by_network(&res.stdout));
                rt.network_members_epoch = epoch;
            } else {
                debug_failure("docker inspect (network membership)", res);
            }
        }
    }
    if hub.active.as_deref() == Some(key) {
        apply_section_lists(window, hub);
    }
}

/// inspect 结果能否用于派生：没超时/本地错误，且要么退出码 0，要么有部分输出
/// （容器刚被删会让 inspect 非 0 退出，其余容器仍然有效）。
fn inspect_usable(res: &ExecResult) -> bool {
    !res.timed_out
        && res.error.is_none()
        && (res.exit_code == Some(0) || !res.stdout.trim().is_empty())
}

fn apply_section_lists(window: &AppWindow, hub: &mut DockerHub) {
    let Some(key) = hub.active.clone() else { return };
    let Some(rt) = hub.hosts.get(&key) else { return };

    let image_needle = hub.image_filter.clone();
    // 「使用中」来自容器快照派生的 map：查得到就是已知值，map 本身缺失才是未知。
    let usage = rt.image_usage.as_ref();
    hub.images_model.set_vec(
        rt.images
            .iter()
            .filter(|image| {
                image_needle.is_empty()
                    || image.display_name.to_ascii_lowercase().contains(&image_needle)
                    || image.repository.to_ascii_lowercase().contains(&image_needle)
            })
            .map(|image| {
                let count = usage.and_then(|map| {
                    map.get(&parse::normalize_image_id(&image.id)).copied()
                });
                DockerImage {
                    id: image.id.as_str().into(),
                    name: if image.dangling {
                        "<none>".into()
                    } else {
                        image.repository.as_str().into()
                    },
                    tag: image.tag.as_str().into(),
                    size: image.size.as_str().into(),
                    created: image.created.as_str().into(),
                    in_use: count.is_some_and(|count| count > 0),
                    use_known: usage.is_some(),
                    use_count: count.unwrap_or(0).to_string().into(),
                }
            })
            .collect::<Vec<_>>(),
    );
    window.set_docker_image_list(ModelRc::from(hub.images_model.clone()));
    window.set_docker_image_status(rt.images_status.key().into());
    window.set_docker_image_error(error_text(rt.images_failure, rt.last_mode).into());

    // 容器数按 compose 项目标签统计（含已停止），`Status` 里的 running(N) 只算主状态。
    let mut compose_counts: HashMap<&str, usize> = HashMap::new();
    for container in &rt.containers {
        if let Some(project) = container.compose_project.as_deref() {
            *compose_counts.entry(project).or_insert(0) += 1;
        }
    }
    let compose_needle = hub.compose_filter.clone();
    hub.compose_model.set_vec(
        rt.compose_projects
            .iter()
            .filter(|project| {
                compose_needle.is_empty()
                    || project.name.to_ascii_lowercase().contains(&compose_needle)
                    || project.config_files.to_ascii_lowercase().contains(&compose_needle)
            })
            .map(|project| DockerComposeRow {
                name: project.name.as_str().into(),
                running: project.running,
                containers_text: compose_counts
                    .get(project.name.as_str())
                    .copied()
                    .unwrap_or(project.containers.max(0) as usize)
                    .to_string()
                    .into(),
                config: project.config_files.as_str().into(),
            })
            .collect::<Vec<_>>(),
    );
    window.set_docker_compose_list(ModelRc::from(hub.compose_model.clone()));
    window.set_docker_compose_status(rt.compose_status.key().into());
    window.set_docker_compose_error(error_text(rt.compose_failure, rt.last_mode).into());

    let members = rt.network_members.as_ref();
    hub.networks_model.set_vec(
        rt.networks
            .iter()
            .map(|network| {
                let names: &[String] = members
                    .and_then(|map| map.get(&network.name))
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                DockerNetworkRow {
                    name: network.name.as_str().into(),
                    driver: network.driver.as_str().into(),
                    count_text: names.len().to_string().into(),
                    containers: names.join(", ").into(),
                }
            })
            .collect::<Vec<_>>(),
    );
    window.set_docker_network_list(ModelRc::from(hub.networks_model.clone()));
    window.set_docker_network_status(rt.networks_status.key().into());
    window.set_docker_network_error(error_text(rt.networks_failure, rt.last_mode).into());

    window.set_docker_images(if rt.images_last_ok.is_some() {
        rt.images.len() as i32
    } else {
        -1
    });
    window.set_docker_networks(if rt.networks_last_ok.is_some() {
        rt.networks.len() as i32
    } else {
        -1
    });
    window.set_docker_compose_projects(if rt.compose_last_ok.is_some() {
        rt.compose_projects.len() as i32
    } else {
        -1
    });
}

fn clear_view(window: &AppWindow, hub: &mut DockerHub) {
    hub.containers_model.set_vec(Vec::<DockerContainer>::new());
    window.set_docker_containers(ModelRc::from(hub.containers_model.clone()));
    window.set_docker_status("no-session".into());
    window.set_docker_error("".into());
    window.set_docker_summary("".into());
    window.set_docker_session("".into());
    window.set_docker_active_session("".into());
    window.set_docker_permission("".into());
    window.set_docker_running(-1);
    window.set_docker_stopped(-1);
    hub.images_model.set_vec(Vec::<DockerImage>::new());
    window.set_docker_image_list(ModelRc::from(hub.images_model.clone()));
    window.set_docker_image_status("idle".into());
    window.set_docker_image_error("".into());
    hub.compose_model.set_vec(Vec::<DockerComposeRow>::new());
    window.set_docker_compose_list(ModelRc::from(hub.compose_model.clone()));
    window.set_docker_compose_status("idle".into());
    window.set_docker_compose_error("".into());
    hub.networks_model.set_vec(Vec::<DockerNetworkRow>::new());
    window.set_docker_network_list(ModelRc::from(hub.networks_model.clone()));
    window.set_docker_network_status("idle".into());
    window.set_docker_network_error("".into());
    window.set_docker_images(-1);
    window.set_docker_networks(-1);
    window.set_docker_compose_projects(-1);
    window.set_docker_service_state("unknown".into());
    window.set_docker_version("".into());
    window.set_docker_service_pending(false);
    window.set_docker_service_autostart("".into());
    clear_monitor(window);
}

fn apply_active(window: &AppWindow, hub: &mut DockerHub) {
    let Some(key) = hub.active.clone() else {
        clear_view(window, hub);
        return;
    };
    let Some(rt) = hub.hosts.get(&key) else { return };
    window.set_docker_session(key.as_str().into());
    window.set_docker_active_session(rt.session_id.as_str().into());
    window.set_docker_permission(permission_text(rt, window.get_docker_use_sudo()).into());
    window.set_docker_status(rt.status.key().into());
    window.set_docker_error(error_text(rt.failure, rt.last_mode).into());
    let running = rt
        .containers
        .iter()
        .filter(|c| c.state == ContainerState::Running)
        .count();
    let summary = if rt.containers.is_empty() {
        String::new()
    } else {
        t("{running} 运行 / {total} 共", "{running} running / {total} total")
            .replace("{running}", &running.to_string())
            .replace("{total}", &rt.containers.len().to_string())
    };
    window.set_docker_summary(summary.into());

    // 概览统计用**未过滤**列表：搜索框只影响容器页的可见行，不该改变数字。
    let (run_count, stop_count) = if rt.last_ok.is_some() {
        let r = running as i32;
        (r, rt.containers.len() as i32 - r)
    } else {
        (-1, -1)
    };
    window.set_docker_running(run_count);
    window.set_docker_stopped(stop_count);
    window.set_docker_service_state(
        match rt.service_up {
            Some(true) => "running",
            Some(false) => "stopped",
            None => "unknown",
        }
        .into(),
    );
    window.set_docker_version(rt.docker_version.as_str().into());
    window.set_docker_service_pending(rt.service_pending);
    window.set_docker_service_autostart(
        match rt.service_enabled {
            Some(true) => t("已启用", "Enabled"),
            Some(false) => t("未启用", "Disabled"),
            None => "",
        }
        .into(),
    );
    apply_monitor(window, &rt.tab_id);

    let needle = hub.filter.as_str();
    hub.containers_model.set_vec(
        rt.containers
            .iter()
            .filter(|c| {
                needle.is_empty()
                    || c.name.to_ascii_lowercase().contains(needle)
                    || c.id.to_ascii_lowercase().contains(needle)
                    || c.image.to_ascii_lowercase().contains(needle)
                    || c.ports.contains(needle)
            })
            .map(|c| DockerContainer {
                id: c.id.as_str().into(),
                name: c.name.as_str().into(),
                image: c.image.as_str().into(),
                state: c.state.key().into(),
                status: c.status.as_str().into(),
                ports: c.ports.as_str().into(),
                created: c.created.as_str().into(),
            })
            .collect::<Vec<_>>(),
    );
    window.set_docker_containers(ModelRc::from(hub.containers_model.clone()));
    apply_section_lists(window, hub);
}

/// Rust 侧 500ms 驱动：按当前选中 Docker 主机的 tab_id 刷新概览曲线。
///
/// 只在 `stats_seq` 变化时重建路径，避免每拍重复 `smooth_path` 与重绘。
pub(crate) fn tick_monitor(window: &AppWindow) {
    let tab_id = HUB.with(|hub| {
        let hub = hub.borrow();
        hub.active
            .as_ref()
            .and_then(|key| hub.hosts.get(key))
            .map(|rt| rt.tab_id.clone())
    });
    let Some(tab_id) = tab_id else {
        clear_monitor_once(window);
        return;
    };
    let seq = LAST_STATUSES.with(|slot| {
        slot.borrow().as_ref().and_then(|statuses| {
            statuses
                .lock()
                .ok()
                .and_then(|map| map.get(&tab_id).map(|st| st.stats_seq))
        })
    });
    let Some(seq) = seq else {
        clear_monitor_once(window);
        return;
    };
    if APPLIED_MONITOR.with(|m| m.borrow().get(&tab_id).copied() == Some(seq)) {
        return;
    }
    APPLIED_MONITOR.with(|m| {
        m.borrow_mut().insert(tab_id.clone(), seq);
    });
    apply_monitor(window, &tab_id);
}

/// 只在确实应用过数据时清空一次，避免每拍重复写属性触发重绘。
fn clear_monitor_once(window: &AppWindow) {
    let had = APPLIED_MONITOR.with(|m| !m.borrow().is_empty());
    if had {
        APPLIED_MONITOR.with(|m| m.borrow_mut().clear());
        clear_monitor(window);
    }
}

/// 概览页的监控曲线：按**选中 Docker 主机自己的 tab_id** 取快照，不能用
/// `active-tab-id` —— 切 Docker 主机标签时，活动 SSH 标签可能属于另一台主机。
fn apply_monitor(window: &AppWindow, tab_id: &str) {
    let snapshot = LAST_STATUSES.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|statuses| statuses.lock().ok().and_then(|map| map.get(tab_id).cloned()))
    });
    let Some(st) = snapshot.filter(|st| st.state == 1 && !st.is_local) else {
        clear_monitor(window);
        return;
    };

    let cpu: Vec<f32> = st.cpu_hist.iter().map(|v| v / 100.0).collect();
    window.set_docker_cpu_path(super::resource_ui::smooth_path(&cpu, 60).into());
    window.set_docker_mem_path(super::resource_ui::smooth_path(&st.mem_hist, 60).into());
    // 上下行共用同一个峰值：两条线画在同一刻度上才可比（与综合页一致）。
    let peak = st
        .net_hist
        .iter()
        .chain(st.net_up_hist.iter())
        .fold(1.0_f32, |m, v| m.max(*v));
    let down: Vec<f32> = st.net_hist.iter().map(|v| (v / peak).clamp(0.0, 1.0)).collect();
    let up: Vec<f32> = st.net_up_hist.iter().map(|v| (v / peak).clamp(0.0, 1.0)).collect();
    window.set_docker_net_down_path(super::resource_ui::smooth_path(&down, 60).into());
    window.set_docker_net_up_path(super::resource_ui::smooth_path(&up, 60).into());
    window.set_docker_cpu_percent(st.cpu);
    window.set_docker_mem_percent(super::sidebar::usage_pct(st.mem_used_kib, st.mem_total_kib));
    let (_, rx, tx) = super::terminal_ui::selected_iface(&st);
    window.set_docker_net_top_down(format_bytes_per_sec(rx).into());
    window.set_docker_net_top_up(format_bytes_per_sec(tx).into());
    window.set_docker_stats_ready(!st.cpu_hist.is_empty() && !st.mem_hist.is_empty());
}

fn clear_monitor(window: &AppWindow) {
    window.set_docker_cpu_path("".into());
    window.set_docker_mem_path("".into());
    window.set_docker_net_up_path("".into());
    window.set_docker_net_down_path("".into());
    window.set_docker_cpu_percent(0.0);
    window.set_docker_mem_percent(0.0);
    window.set_docker_net_top_up("".into());
    window.set_docker_net_top_down("".into());
    window.set_docker_stats_ready(false);
}

fn error_text(failure: Option<DockerFailure>, mode: command::Mode) -> &'static str {
    match failure {
        Some(DockerFailure::SudoAuthFailed) => t(
            "提权认证失败：密码错误，或该账号没有 sudo 权限",
            "Elevation was rejected — wrong password, or the account may not use sudo",
        ),
        Some(DockerFailure::NoPermission) => match mode {
            command::Mode::SudoPassword | command::Mode::SuPassword => t(
                "提权认证失败：密码错误，或该账号没有 sudo 权限",
                "Elevation was rejected — wrong password, or the account may not use sudo",
            ),
            command::Mode::SudoNoPass => t(
                "已开启 sudo，但该用户需要密码：请配置提权凭据，或为当前用户配置免密 sudo（NOPASSWD）",
                "sudo is enabled but needs a password — configure elevation credentials, or set up NOPASSWD",
            ),
            command::Mode::Direct => t(
                "当前用户没有 Docker 权限：配置提权凭据、开启「优先使用 sudo」，或把用户加入 docker 组后重新登录",
                "No Docker permission — configure elevation credentials, enable \"Prefer sudo\", or add the user to the docker group",
            ),
        },
        Some(DockerFailure::NoCli) => t(
            "远端未安装 docker CLI",
            "docker CLI is not installed on the remote host",
        ),
        Some(DockerFailure::NoCompose) => t(
            "远端未安装 Docker Compose v2 插件",
            "Docker Compose v2 plugin is not installed on the remote host",
        ),
        Some(DockerFailure::RegistryAuth) => t(
            "镜像仓库需要登录：请先执行 docker login，或检查凭据",
            "Registry authentication required — run docker login or check credentials",
        ),
        Some(DockerFailure::DaemonDown) => t(
            "远端 Docker daemon 未运行或 socket 不可访问",
            "Remote Docker daemon is down or the socket is unreachable",
        ),
        Some(DockerFailure::Timeout) => t("docker 命令执行超时", "docker command timed out"),
        Some(DockerFailure::SessionLost) => t(
            "会话已关闭，无法读取容器列表",
            "Session closed — cannot list containers",
        ),
        Some(DockerFailure::NotFound) => t(
            "目标不存在，可能已被删除",
            "Target not found — it may have been removed",
        ),
        Some(DockerFailure::Conflict) => t("容器状态冲突", "Container state conflict"),
        Some(DockerFailure::Parse) => t("无法解析 docker 输出", "Cannot parse docker output"),
        Some(DockerFailure::Other) => t("docker 命令执行失败，请查看远端日志", "docker command failed — check the remote logs"),
        None => "",
    }
}

#[cfg(test)]
mod elevation_tests {
    use super::*;

    #[test]
    fn docker_group_user_falls_back_to_direct() {
        // 用户已在 docker 组、没有免密 sudo：开关打开也必须直连。
        assert_eq!(
            choose_mode(true, Some(true), Some(false), false, false),
            command::Mode::Direct
        );
    }

    #[test]
    fn passwordless_sudo_wins_over_direct() {
        assert_eq!(
            choose_mode(true, Some(true), Some(true), false, false),
            command::Mode::SudoNoPass
        );
    }

    #[test]
    fn saved_password_is_used_only_when_direct_is_denied() {
        assert_eq!(
            choose_mode(true, Some(false), Some(false), true, false),
            command::Mode::SudoPassword
        );
        assert_eq!(
            choose_mode(true, Some(true), Some(false), true, false),
            command::Mode::Direct
        );
    }

    #[test]
    fn unprobed_sudo_is_tried_after_direct_failure() {
        assert_eq!(
            choose_mode(true, Some(false), None, false, false),
            command::Mode::SudoNoPass
        );
    }

    #[test]
    fn unknown_probe_defaults_to_direct() {
        assert_eq!(
            choose_mode(true, None, None, false, false),
            command::Mode::Direct
        );
    }

    #[test]
    fn switch_off_always_runs_direct() {
        assert_eq!(
            choose_mode(false, Some(false), Some(true), true, true),
            command::Mode::Direct
        );
    }

    #[test]
    fn service_control_falls_back_to_sudo_for_root() {
        // 直连 docker 可用也必须走 sudo：systemctl 需要 root。
        assert_eq!(
            choose_mode(true, Some(true), Some(false), false, true),
            command::Mode::SudoNoPass
        );
        assert_eq!(
            choose_mode(true, Some(true), Some(false), true, true),
            command::Mode::SudoPassword
        );
    }
}
