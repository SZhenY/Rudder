//! 镜像仓库搜索：通过远端 SSH 主机执行 `docker search`。
//!
//! 远端可能配置了 registry mirror / 代理 / 加速源，所以搜索必须走主机，
//! 不能由本机直接请求 Docker Hub。分页在 Rust 侧对结果切片。

use std::cell::RefCell;
use std::time::Duration;

use slint::{ComponentHandle, ModelRc};

use crate::docker::{command, parse};
use crate::i18n::t;
use crate::ui::{AppWindow, RegistryRepoRow};

use super::{
    elevation_for, error_text, exec_on, password_for, DockerFailure, DockerHub, HostStatus,
    REFRESH_CTX, HUB, PS_MAX_OUTPUT,
};

const PAGE_SIZE: usize = 20;
const SEARCH_LIMIT: u32 = 100;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);

thread_local! {
    /// 连续输入只发最后一次请求。
    static DEBOUNCE: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
}

/// 搜索框输入：300ms 去抖后从第 1 页开始。
pub(super) fn search(window: &AppWindow, query: &str) {
    let query = query.trim().to_string();
    let weak = window.as_weak();
    DEBOUNCE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let timer = slot.get_or_insert_with(slint::Timer::default);
        timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(300),
            move || {
                if let Some(window) = weak.upgrade() {
                    do_search(&window, &query);
                }
            },
        );
    });
}

/// 刷新按钮：用当前关键词重新搜索。
pub(super) fn refresh(window: &AppWindow) {
    let query = HUB.with(|hub| hub.borrow().registry_query.clone());
    do_search(window, &query);
}

/// 上一页 / 下一页：只切本地结果，不重新请求。
pub(super) fn page(window: &AppWindow, delta: i32) {
    HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        let pages = hub.registry_rows.len().div_ceil(PAGE_SIZE).max(1) as i32;
        let next = (hub.registry_page as i32 + delta).clamp(1, pages);
        hub.registry_page = next as u32;
        apply_registry(window, &mut hub);
    });
}

/// 星标数：上万后压成 `21K`，避免右侧列被长数字撑开。
fn format_stars(stars: i64) -> String {
    let n = stars.max(0);
    if n >= 1_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}

fn do_search(window: &AppWindow, query: &str) {
    let Some((runtime, handles)) = REFRESH_CTX.with(|ctx| {
        ctx.borrow()
            .as_ref()
            .map(|c| (c.runtime.clone(), c.handles.clone()))
    }) else {
        return;
    };

    let rx = HUB.with(|hub| {
        let mut hub = hub.borrow_mut();
        hub.registry_query = query.to_string();
        hub.registry_page = 1;
        hub.registry_status = HostStatus::Loading;
        hub.registry_error.clear();

        if query.is_empty() {
            hub.registry_rows.clear();
            hub.registry_status = HostStatus::Idle;
            apply_registry(window, &mut hub);
            return None;
        }

        let Some(key) = hub.active.clone() else {
            hub.registry_status = HostStatus::Error;
            hub.registry_error = t("请先选择 Docker 主机", "Select a Docker host first").to_string();
            apply_registry(window, &mut hub);
            return None;
        };
        let Some(rt) = hub.hosts.get_mut(&key) else {
            hub.registry_status = HostStatus::Error;
            hub.registry_error = t("请先选择 Docker 主机", "Select a Docker host first").to_string();
            apply_registry(window, &mut hub);
            return None;
        };

        let elev = elevation_for(rt, window.get_docker_use_sudo(), false);
        let password = password_for(rt, &elev);
        match command::search_images(query, SEARCH_LIMIT, &elev) {
            Ok(cmd) => {
                let rx = exec_on(
                    &handles,
                    &rt.tab_id,
                    cmd,
                    password,
                    SEARCH_TIMEOUT,
                    PS_MAX_OUTPUT,
                );
                if rx.is_none() {
                    hub.registry_status = HostStatus::Error;
                    hub.registry_error =
                        t("当前主机没有可用的 SSH 会话", "No SSH session for this host").to_string();
                }
                apply_registry(window, &mut hub);
                rx
            }
            Err(()) => {
                hub.registry_status = HostStatus::Error;
                hub.registry_error = t("搜索词包含非法字符", "Invalid search term").to_string();
                apply_registry(window, &mut hub);
                None
            }
        }
    });

    let Some(rx) = rx else { return };
    let weak = window.as_weak();
    runtime.spawn(async move {
        let result = rx.await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else { return };
            HUB.with(|hub| {
                let mut hub = hub.borrow_mut();
                match result {
                    Err(_) => {
                        hub.registry_status = HostStatus::Error;
                        hub.registry_error = t(
                            "会话已关闭，搜索未完成",
                            "Session closed before the search finished",
                        )
                        .to_string();
                    }
                    Ok(res) => {
                        let lost = res.exit_code.is_none() && !res.timed_out && res.error.is_none();
                        if res.timed_out || res.error.is_some() || lost || res.exit_code.unwrap_or(1) != 0
                        {
                            let failure = if lost {
                                DockerFailure::SessionLost
                            } else {
                                crate::docker::classify::classify(
                                    &res.stderr,
                                    res.exit_code,
                                    res.timed_out,
                                )
                            };
                            hub.registry_status = HostStatus::Error;
                            hub.registry_error = error_text(Some(failure), command::Mode::Direct)
                                .to_string();
                        } else {
                            let parsed = parse::parse_docker_search(&res.stdout);
                            if parsed.bad_lines > 0 {
                                tracing::debug!(
                                    "docker search: {} unparsable lines",
                                    parsed.bad_lines
                                );
                            }
                            hub.registry_rows = parsed.rows;
                            hub.registry_page = 1;
                            hub.registry_status = if hub.registry_rows.is_empty() {
                                HostStatus::Empty
                            } else {
                                HostStatus::Ok
                            };
                            hub.registry_error.clear();
                        }
                    }
                }
                apply_registry(&window, &mut hub);
            });
        });
    });
}

fn apply_registry(window: &AppWindow, hub: &mut DockerHub) {
    let start = ((hub.registry_page.max(1) - 1) as usize) * PAGE_SIZE;
    let slice: Vec<RegistryRepoRow> = hub
        .registry_rows
        .iter()
        .skip(start)
        .take(PAGE_SIZE)
        .map(|repo| RegistryRepoRow {
            name: repo.name.as_str().into(),
            description: repo.description.as_str().into(),
            pulls: repo.display_pulls.as_str().into(),
            stars_text: format_stars(repo.stars).into(),
            official: repo.official,
        })
        .collect();
    hub.registry_model.set_vec(slice);
    window.set_docker_registry_list(ModelRc::from(hub.registry_model.clone()));
    window.set_docker_registry_status(hub.registry_status.key().into());
    window.set_docker_registry_error(hub.registry_error.as_str().into());

    let pages = hub.registry_rows.len().div_ceil(PAGE_SIZE).max(1);
    let page = (hub.registry_page.max(1) as usize).min(pages);
    window.set_docker_registry_page_label(format!("{page} / {pages}").into());
    window.set_docker_registry_prev_enabled(page > 1);
    window.set_docker_registry_next_enabled(page < pages);
    window.set_docker_registry_searched(!hub.registry_query.is_empty());
}
