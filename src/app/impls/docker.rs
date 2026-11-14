//! Docker 页：在当前会话对应的主机上执行 `docker ps --all`，只读展示容器列表。
//!
//! 复用现有 SSH 会话的 exec 通道（`SessionHandle::exec`），不引入 Docker 客户端依赖。
//! 目标会话取 `active-tab-id`；它被左侧导航打回 `"welcome"` 时，回退到「最近仍有
//! 存活采样」的 SSH 会话 —— 否则点一下 Hosts 再进 Docker 页就会误判成无会话。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::config::SessionKind;
use crate::i18n::t;
use crate::resource::TabStatuses;
use crate::ssh::{ExecResult, SessionHandle};
use crate::ui::{AppWindow, DockerContainer, TabInfo};

/// 固定模板：每行一个 JSON 对象。PATH 前缀与 `session_pump.rs` 一致 —— SSH exec
/// 是非交互 shell，默认 PATH 往往不含 `/usr/local/bin`。
const DOCKER_PS_CMD: &str = "PATH=/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/snap/bin; export PATH; docker ps --all --format '{{json .}}'";
const TIMEOUT: Duration = Duration::from_secs(6);
const MAX_OUTPUT: usize = 256 * 1024;

/// 并发刷新时只认最后一次请求的结果。
static GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(serde::Deserialize)]
struct PsRow {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Names")]
    names: String,
    #[serde(rename = "Image")]
    image: String,
    #[serde(rename = "State")]
    state: String,
    #[serde(rename = "Status")]
    status: String,
    #[serde(rename = "Ports", default)]
    ports: String,
}

pub(crate) fn refresh(
    window: &AppWindow,
    runtime: &Arc<tokio::runtime::Runtime>,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    tabs: &Rc<VecModel<TabInfo>>,
    statuses: &TabStatuses,
) {
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;

    let Some(tab_id) = pick_target(window, tabs, statuses) else {
        let has_any = !handles.borrow().is_empty();
        let status = if has_any { "disconnected" } else { "no-session" };
        set_status(window, status, "", "");
        return;
    };

    let rx = {
        let hs = handles.borrow();
        match hs.get(&tab_id) {
            Some(handle) => handle.exec(DOCKER_PS_CMD.to_string(), TIMEOUT, MAX_OUTPUT),
            None => {
                drop(hs);
                set_status(window, "disconnected", "", "");
                return;
            }
        }
    };

    window.set_docker_status("loading".into());
    window.set_docker_error("".into());
    window.set_docker_summary("".into());
    window.set_docker_session(session_label(&tab_id, statuses).into());

    let weak = window.as_weak();
    runtime.spawn(async move {
        let result = rx.await;
        let _ = slint::invoke_from_event_loop(move || {
            if GENERATION.load(Ordering::Relaxed) != generation {
                return;
            }
            if let Some(window) = weak.upgrade() {
                apply(&window, result);
            }
        });
    });
}

/// 目标会话：先用活动标签；不是存活 SSH 会话时，回退到「采样时间最新」的那个。
fn pick_target(
    window: &AppWindow,
    tabs: &VecModel<TabInfo>,
    statuses: &TabStatuses,
) -> Option<String> {
    let active = window.get_active_tab_id().to_string();
    if is_live_ssh(&active, statuses) {
        return Some(active);
    }

    let statuses = statuses.lock().ok()?;
    let mut best: Option<(String, Option<std::time::Instant>, usize)> = None;
    for index in 0..tabs.row_count() {
        let Some(tab) = tabs.row_data(index) else {
            continue;
        };
        let id = tab.id.to_string();
        let Some(st) = statuses.get(&id) else {
            continue;
        };
        if st.kind != SessionKind::Ssh || st.state != 1 || st.is_local {
            continue;
        }
        let candidate = (id, st.last_stats_at, index);
        let better = best
            .as_ref()
            .is_none_or(|current| (candidate.1, candidate.2) > (current.1, current.2));
        if better {
            best = Some(candidate);
        }
    }
    best.map(|(id, _, _)| id)
}

fn is_live_ssh(tab_id: &str, statuses: &TabStatuses) -> bool {
    statuses
        .lock()
        .ok()
        .and_then(|s| s.get(tab_id).cloned())
        .is_some_and(|st| st.kind == SessionKind::Ssh && st.state == 1 && !st.is_local)
}

fn session_label(tab_id: &str, statuses: &TabStatuses) -> String {
    let Some(st) = statuses.lock().ok().and_then(|s| s.get(tab_id).cloned()) else {
        return String::new();
    };
    if st.user.is_empty() {
        st.host
    } else {
        format!("{}@{}", st.user, st.host)
    }
}

fn set_status(window: &AppWindow, status: &str, error: &str, session: &str) {
    window.set_docker_status(status.into());
    window.set_docker_error(error.into());
    window.set_docker_session(session.into());
    window.set_docker_summary("".into());
    window.set_docker_containers(ModelRc::from(Rc::new(VecModel::from(Vec::<
        DockerContainer,
    >::new()))));
}

fn apply(
    window: &AppWindow,
    result: Result<ExecResult, tokio::sync::oneshot::error::RecvError>,
) {
    let Ok(res) = result else {
        window.set_docker_status("error".into());
        window.set_docker_error(t("会话已关闭，无法读取容器列表", "Session closed — cannot list containers").into());
        return;
    };
    if let Some(err) = res.error {
        window.set_docker_status("error".into());
        window.set_docker_error(err.into());
        return;
    }
    if res.timed_out {
        window.set_docker_status("error".into());
        window.set_docker_error(t("docker ps 执行超时", "docker ps timed out").into());
        return;
    }
    if res.exit_code.unwrap_or(1) != 0 {
        window.set_docker_status("error".into());
        window.set_docker_error(docker_error_text(&res.stderr).into());
        return;
    }

    let mut rows = Vec::new();
    let mut bad = 0usize;
    for line in res.stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<PsRow>(line) {
            Ok(row) => rows.push(DockerContainer {
                id: row.id.into(),
                name: first_name(&row.names).into(),
                image: row.image.into(),
                state: row.state.clone().into(),
                status: row.status.into(),
                ports: row.ports.into(),
            }),
            Err(_) => bad += 1,
        }
    }

    if rows.is_empty() && bad > 0 {
        let head: String = res.stdout.chars().take(200).collect();
        tracing::warn!(
            bad,
            stdout_head = %head,
            "docker ps output could not be parsed"
        );
        window.set_docker_status("error".into());
        window.set_docker_error(
            t("无法解析 docker 输出（{bad} 行）", "Cannot parse docker output ({bad} lines)")
                .replace("{bad}", &bad.to_string())
                .into(),
        );
        return;
    }

    let running = rows
        .iter()
        .filter(|r| r.state.as_str() == "running")
        .count();
    let summary = if rows.is_empty() {
        String::new()
    } else {
        t("{running} 运行 / {total} 共", "{running} running / {total} total")
            .replace("{running}", &running.to_string())
            .replace("{total}", &rows.len().to_string())
    };
    window.set_docker_summary(summary.into());
    window.set_docker_error("".into());
    window.set_docker_status(if rows.is_empty() { "empty" } else { "ok" }.into());
    window.set_docker_containers(ModelRc::from(Rc::new(VecModel::from(rows))));
}

/// `docker ps` 的 `Names` 可能是逗号分隔的多个别名，列表只显示第一个。
fn first_name(names: &str) -> &str {
    names.split(',').next().unwrap_or("").trim()
}

fn docker_error_text(stderr: &str) -> &'static str {
    let msg = stderr.to_ascii_lowercase();
    if msg.contains("permission denied") {
        t("当前用户没有 Docker 权限，需要加入 docker 组后重新登录", "No Docker permission — join the docker group and sign in again")
    } else if msg.contains("command not found") || msg.contains("not found") {
        t("远端未安装 docker CLI", "docker CLI is not installed on the remote host")
    } else if msg.contains("cannot connect") || msg.contains("daemon") {
        t("远端 Docker daemon 未运行或 socket 不可访问", "Remote Docker daemon is down or the socket is unreachable")
    } else {
        t("docker ps 执行失败，请查看远端日志", "docker ps failed — check the remote logs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_name_takes_the_leading_alias() {
        assert_eq!(first_name("web,web-1"), "web");
        assert_eq!(first_name("solo"), "solo");
        assert_eq!(first_name(""), "");
    }

    #[test]
    fn error_text_maps_the_common_failures() {
        assert!(
            docker_error_text("permission denied while trying to connect").contains("docker 组")
        );
        assert!(docker_error_text("bash: docker: command not found").contains("未安装"));
        assert!(docker_error_text("Cannot connect to the Docker daemon").contains("daemon"));
        assert!(docker_error_text("something else").contains("执行失败"));
    }
}
