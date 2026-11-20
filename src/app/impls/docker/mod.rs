//! Docker 页控制器：按 `user@host` 维护一份容器缓存，刷新 / 切换主机都只走这里。
//!
//! 所有 SSH 命令都在 tokio worker 上执行；本模块的公开函数只在 Slint UI 线程调用，
//! 结果通过 `invoke_from_event_loop` 回到 UI 线程再写模型。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tokio::runtime::Runtime;

use crate::config::SessionKind;
use crate::docker::classify::classify as classify_docker;
use crate::docker::{
    command, parse, ContainerState, DockerContainerInfo, DockerFailure, DockerHostKey, HostStatus,
};
use crate::i18n::t;
use crate::resource::TabStatuses;
use crate::ssh::{ExecResult, SessionHandle};
use crate::ui::{AppWindow, DockerContainer, DockerHostTab, TabInfo};

const PS_TIMEOUT: Duration = Duration::from_secs(10);
const PS_MAX_OUTPUT: usize = 512 * 1024;
const FRESH_FOR: Duration = Duration::from_secs(10);
const BACKOFF: Duration = Duration::from_secs(30);

thread_local! {
    static HUB: RefCell<DockerHub> = RefCell::new(DockerHub::new());
    /// UI 线程上的刷新上下文；异步任务只带回结果，需要续跑时在这里取句柄。
    static REFRESH_CTX: RefCell<Option<RefreshCtx>> = const { RefCell::new(None) };
    /// 新开标签在 Connected 之前要注入的命令：tab_id -> 命令。
    static PENDING_TERMINAL: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

struct RefreshCtx {
    runtime: Arc<Runtime>,
    handles: Rc<RefCell<HashMap<String, SessionHandle>>>,
}

struct HostRuntime {
    tab_id: String,
    containers: Vec<DockerContainerInfo>,
    status: HostStatus,
    failure: Option<DockerFailure>,
    last_ok: Option<Instant>,
    last_attempt: Option<Instant>,
    generation: u64,
    busy: bool,
    queued: bool,
    backoff_until: Option<Instant>,
}

struct DockerHub {
    hosts: HashMap<String, HostRuntime>,
    active: Option<String>,
    filter: String,
    hosts_model: Rc<VecModel<DockerHostTab>>,
    containers_model: Rc<VecModel<DockerContainer>>,
}

impl DockerHub {
    fn new() -> Self {
        Self {
            hosts: HashMap::new(),
            active: None,
            filter: String::new(),
            hosts_model: Rc::new(VecModel::default()),
            containers_model: Rc::new(VecModel::default()),
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
) {
    REFRESH_CTX.with(|ctx| {
        *ctx.borrow_mut() = Some(RefreshCtx {
            runtime: runtime.clone(),
            handles: handles.clone(),
        });
    });
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        sync_hosts(&mut hub, window, tabs, statuses);
        let Some(key) = hub.active.clone() else {
            clear_view(window, &mut hub);
            return;
        };
        refresh_key(window, runtime, handles, &mut hub, &key, false);
    });
}

/// 切换主机标签：只切缓存；缓存缺失或过期才请求。
pub(crate) fn select_host(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    key: &str,
) {
    REFRESH_CTX.with(|ctx| {
        *ctx.borrow_mut() = Some(RefreshCtx {
            runtime: runtime.clone(),
            handles: handles.clone(),
        });
    });
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        if !hub.hosts.contains_key(key) {
            return;
        }
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
    let cmd = match action {
        0 => command::start(container_id),
        1 => command::stop(container_id),
        2 => command::restart(container_id),
        3 => command::remove_sequence(container_id),
        _ => return,
    };
    let Ok(cmd) = cmd else {
        window.set_docker_error(t("容器 ID 非法", "Invalid container id").into());
        return;
    };
    let timeout = match action {
        1 => Duration::from_secs(40),
        3 => Duration::from_secs(60),
        _ => Duration::from_secs(30),
    };
    run_mutation(window, runtime, handles, key, cmd, timeout);
}

/// 拉取日志尾部；结果写入日志抽屉模型。
pub(crate) fn fetch_logs(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    key: &str,
    container_id: &str,
) {
    let Ok(cmd) = command::logs(container_id, 200, false) else {
        window.set_docker_error(t("容器 ID 非法", "Invalid container id").into());
        return;
    };
    let Some(tab_id) = HUB.with(|hub| hub.borrow().hosts.get(key).map(|h| h.tab_id.clone())) else {
        return;
    };
    window.set_docker_log_open(true);
    window.set_docker_log_loading(true);
    window.set_docker_log_title(container_id.into());
    window.set_docker_log_lines(ModelRc::from(Rc::new(VecModel::<slint::SharedString>::default())));

    let rx = {
        let hs = handles.borrow();
        hs.get(&tab_id)
            .map(|h| h.exec(cmd, Duration::from_secs(20), PS_MAX_OUTPUT))
    };
    let Some(rx) = rx else { return };
    let weak = window.as_weak();
    runtime.spawn(async move {
        let result = rx.await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else { return };
            let Ok(res) = result else {
                window.set_docker_log_loading(false);
                return;
            };
            let (lines, truncated) = parse::parse_logs(&res.stdout, &res.stderr, 200);
            window.set_docker_log_truncated(truncated);
            window.set_docker_log_loading(false);
            window.set_docker_log_lines(ModelRc::from(Rc::new(VecModel::from(
                lines.into_iter().map(slint::SharedString::from).collect::<Vec<_>>(),
            ))));
        });
    });
}

fn run_mutation(
    window: &AppWindow,
    runtime: &Arc<Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    key: &str,
    cmd: String,
    timeout: Duration,
) {
    let Some(tab_id) = HUB.with(|hub| hub.borrow().hosts.get(key).map(|h| h.tab_id.clone())) else {
        return;
    };
    let rx = {
        let hs = handles.borrow();
        hs.get(&tab_id).map(|h| h.exec(cmd, timeout, 64 * 1024))
    };
    let Some(rx) = rx else { return };
    window.set_docker_pending_id(tab_id.clone().into());
    let weak = window.as_weak();
    runtime.spawn(async move {
        let result = rx.await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else { return };
            window.set_docker_pending_id("".into());
            let Ok(res) = result else { return };
            if res.timed_out || res.error.is_some() || res.exit_code.unwrap_or(1) != 0 {
                window.set_docker_error(
                    error_text(Some(classify_docker(&res.stderr, res.exit_code, res.timed_out)))
                        .into(),
                );
            }
            window.invoke_refresh_docker();
        });
    });
}

/// 进入容器终端：复制当前主机会话开新标签，连接成功后注入 `docker exec -it`。
pub(crate) fn open_terminal(window: &AppWindow, key: &str, container_id: &str) {
    let Some(tab_id) = HUB.with(|hub| hub.borrow().hosts.get(key).map(|h| h.tab_id.clone())) else {
        return;
    };
    let Ok(cmd) = command::exec_shell(container_id) else {
        window.set_docker_error(t("容器 ID 非法", "Invalid container id").into());
        return;
    };

    let before: std::collections::HashSet<String> = window
        .get_tabs()
        .iter()
        .map(|t| t.id.to_string())
        .collect();
    window.invoke_tab_duplicate(tab_id.into());

    let weak = window.as_weak();
    slint::Timer::single_shot(Duration::from_millis(150), move || {
        let Some(w) = weak.upgrade() else { return };
        for tab in w.get_tabs().iter() {
            let id = tab.id.to_string();
            if !before.contains(&id) {
                PENDING_TERMINAL.with(|p| p.borrow_mut().insert(id, cmd.clone()));
                return;
            }
        }
        // 手动复制的兜底：至少把命令放到日志抽屉里。
        w.set_docker_log_open(true);
        w.set_docker_log_title("docker exec".into());
        w.set_docker_log_lines(ModelRc::from(Rc::new(VecModel::from(vec![
            slint::SharedString::from(cmd),
        ]))));
    });
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

fn sync_hosts(
    hub: &mut DockerHub,
    window: &AppWindow,
    tabs: &VecModel<TabInfo>,
    statuses: &TabStatuses,
) {
    let statuses = match statuses.lock() {
        Ok(s) => s,
        Err(_) => return,
    };

    let mut by_host: HashMap<String, (usize, bool, Option<Instant>, String)> = HashMap::new();
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
        let entry = by_host.entry(key).or_insert((0, false, None, id.clone()));
        entry.0 += 1;
        entry.1 |= st.state == 1;
        if st.state == 1 && entry.2.is_none_or(|t| st.last_stats_at.is_some_and(|x| x > t)) {
            entry.2 = st.last_stats_at;
            entry.3 = id;
        }
    }

    let mut keys: Vec<_> = by_host.keys().cloned().collect();
    keys.sort();
    let mut rows = Vec::with_capacity(keys.len());
    for key in &keys {
        let (count, connected, _, tab_id) = by_host.get(key).cloned().unwrap_or((0, false, None, String::new()));
        let rt = hub.hosts.entry(key.clone()).or_insert_with(|| HostRuntime {
            tab_id: tab_id.clone(),
            containers: Vec::new(),
            status: HostStatus::Idle,
            failure: None,
            last_ok: None,
            last_attempt: None,
            generation: 0,
            busy: false,
            queued: false,
            backoff_until: None,
        });
        rt.tab_id = tab_id;
        if !connected {
            rt.status = HostStatus::Offline;
        }
        rows.push(DockerHostTab {
            key: key.as_str().into(),
            label: key.as_str().into(),
            connected,
            session_count: count as i32,
            count: if rt.containers.is_empty() && rt.last_ok.is_none() {
                -1
            } else {
                rt.containers.len() as i32
            },
        });
    }

    hub.hosts.retain(|key, _| by_host.contains_key(key));
    hub.hosts_model.set_vec(rows);

    if hub.active.as_ref().is_none_or(|key| !by_host.contains_key(key)) {
        hub.active = keys
            .iter()
            .find(|key| by_host.get(*key).is_some_and(|v| v.1))
            .cloned()
            .or_else(|| keys.first().cloned());
    }
    if let Some(key) = hub.active.clone() {
        window.set_docker_active_host(key.into());
    }
    window.set_docker_hosts(ModelRc::from(hub.hosts_model.clone()));
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
    let Some(rx) = ({
        let hs = handles.borrow();
        hs.get(&rt.tab_id)
            .map(|handle| handle.exec(command::ps(true), PS_TIMEOUT, PS_MAX_OUTPUT))
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
                    rt.containers = parsed.rows;
                    rt.failure = None;
                    rt.last_ok = Some(Instant::now());
                    rt.status = if rt.containers.is_empty() {
                        HostStatus::Empty
                    } else {
                        HostStatus::Ok
                    };
                    rt.backoff_until = None;
                }
            }
        }
        if rt.status == HostStatus::Error {
            rt.backoff_until = Some(Instant::now() + BACKOFF);
        }
        rt.queued
    };

    let active = hub.active.as_deref() == Some(key);
    if active {
        apply_active(window, hub);
    } else {
        sync_host_count(hub, key);
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

fn clear_view(window: &AppWindow, hub: &mut DockerHub) {
    hub.containers_model.set_vec(Vec::<DockerContainer>::new());
    window.set_docker_containers(ModelRc::from(hub.containers_model.clone()));
    window.set_docker_status("no-session".into());
    window.set_docker_error("".into());
    window.set_docker_summary("".into());
    window.set_docker_session("".into());
}

fn apply_active(window: &AppWindow, hub: &mut DockerHub) {
    let Some(key) = hub.active.clone() else {
        clear_view(window, hub);
        return;
    };
    let Some(rt) = hub.hosts.get(&key) else { return };
    window.set_docker_session(key.as_str().into());
    window.set_docker_status(rt.status.key().into());
    window.set_docker_error(error_text(rt.failure).into());
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
}

fn error_text(failure: Option<DockerFailure>) -> &'static str {
    match failure {
        Some(DockerFailure::NoPermission) => t(
            "当前用户没有 Docker 权限，需要加入 docker 组后重新登录",
            "No Docker permission — join the docker group and sign in again",
        ),
        Some(DockerFailure::NoCli) => t(
            "远端未安装 docker CLI",
            "docker CLI is not installed on the remote host",
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
        Some(DockerFailure::NotFound) => t("容器不存在", "Container not found"),
        Some(DockerFailure::Conflict) => t("容器状态冲突", "Container state conflict"),
        Some(DockerFailure::Parse) => t("无法解析 docker 输出", "Cannot parse docker output"),
        Some(DockerFailure::Other) => t("docker 命令执行失败，请查看远端日志", "docker command failed — check the remote logs"),
        None => "",
    }
}
