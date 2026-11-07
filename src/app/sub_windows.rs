//! 子窗口的按需创建与隐藏。
//!
//! 进程窗 / 系统信息窗 / 设置窗原先在启动时全部 `new()`，但其中只有主窗口在
//! 首帧是必须的：另外三个各自带一整棵 Slint 组件树（设置窗还含 TerminalPage）。
//! 这里把"创建 + 接回调"收进工厂，首次打开时才执行；关闭仍只 hide，不销毁
//! （避免在事件派发栈上 drop 正在处理事件的子树，#323/#343）。

use std::cell::RefCell;
use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, VecModel};

use i_slint_backend_winit::WinitWindowAccessor as _;

use super::resource_ui::{
    place_process_window, place_system_info_window, process_needs_root, request_first_frame,
    set_process_action_error, sync_proc_theme, sync_system_info_theme,
};
use super::settings_window;
use super::window::schedule_slint_pointer_ungrab;
use super::AppContext;
use crate::ui::{
    AppWindow, ProcRow, ProcWindow, SettingsWindow, SysInfoRow, SysMetricRow, SysNetRow,
    SystemInfoWindow,
};

/// 系统信息窗与主窗口共享的行模型（主窗口侧已经在用，工厂只持有 `Rc` 副本）。
pub(super) struct SysModels {
    pub(super) metrics: Rc<VecModel<SysMetricRow>>,
    pub(super) nets: Rc<VecModel<SysNetRow>>,
    pub(super) overview: Rc<VecModel<SysInfoRow>>,
    pub(super) cpu_info: Rc<VecModel<SysInfoRow>>,
    pub(super) gpu_info: Rc<VecModel<SysInfoRow>>,
    pub(super) cpu_usage: Rc<VecModel<SysInfoRow>>,
    pub(super) memory: Rc<VecModel<SysInfoRow>>,
    pub(super) swap: Rc<VecModel<SysInfoRow>>,
    pub(super) network: Rc<VecModel<SysInfoRow>>,
    pub(super) filesystem: Rc<VecModel<SysInfoRow>>,
}

thread_local! {
    static SYS: RefCell<Option<Rc<SystemInfoWindow>>> = const { RefCell::new(None) };
    static PROC: RefCell<Option<Rc<ProcWindow>>> = const { RefCell::new(None) };
    static SETTINGS: RefCell<Option<Rc<SettingsWindow>>> = const { RefCell::new(None) };
}

/// 已创建的进程窗弱引用；未创建时返回默认（不可升级）的 `Weak`。
/// 外观回调在**触发时**取它，而不是注册时捕获，这样后创建的窗口也能同步到主题。
pub(super) fn proc_weak() -> slint::Weak<ProcWindow> {
    PROC.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|w| w.as_weak())
            .unwrap_or_default()
    })
}

/// 首次打开进程窗时创建并接好回调；之后直接复用。
pub(super) fn ensure_process(
    _main: &AppWindow,
    rows: &Rc<VecModel<ProcRow>>,
    ctx: &AppContext,
) -> Option<Rc<ProcWindow>> {
    if let Some(proc) = PROC.with(|slot| slot.borrow().clone()) {
        return Some(proc);
    }
    let proc = match ProcWindow::new() {
        Ok(proc) => Rc::new(proc),
        Err(err) => {
            tracing::warn!("failed to build process window: {err}");
            return None;
        }
    };
    proc.set_custom_titlebar(cfg!(not(target_os = "macos")));
    super::apply_chrome_on_first_frame(proc.as_ref());
    proc.set_proc_list(ModelRc::from(rows.clone()));
    proc.on_request_close(release_process);
    {
        proc.on_copy_pid(move |pid: slint::SharedString| {
            let text = pid.to_string();
            std::thread::spawn(move || super::clipboard_set_text(text));
        });
    }
    {
        let weak = proc.as_weak();
        proc.on_win_drag(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_window();
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        use i_slint_backend_winit::winit::window::ResizeDirection;
        let weak = proc.as_weak();
        proc.on_win_resize_se(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_resize_window(ResizeDirection::SouthEast);
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        let proc_weak = proc.as_weak();
        let handles = ctx.handles.clone();
        let statuses = ctx.tab_statuses.clone();
        let runtime = ctx.runtime.clone();
        proc.on_terminate_process(
            move |tab_id: slint::SharedString,
                  pid: slint::SharedString,
                  password: slint::SharedString| {
                let tab_id = tab_id.to_string();
                let Ok(pid) = pid.parse::<u32>() else {
                    set_process_action_error(
                        &proc_weak,
                        crate::i18n::t("无效的 PID", "Invalid PID"),
                    );
                    return;
                };

                // Re-check the source tab, PID, and owner against the latest sample;
                // the main window may have switched tabs since the menu was opened.
                let ownership = {
                    let states = statuses.lock().unwrap_or_else(|e| e.into_inner());
                    states.get(&tab_id).map_or_else(
                        || {
                            Err(crate::i18n::t(
                                "当前会话不可用",
                                "The current session is unavailable",
                            ))
                        },
                        |status| {
                            status
                                .procs
                                .iter()
                                .find(|p| p.pid == pid)
                                .map(|process| process_needs_root(&status.user, &process.user))
                                .ok_or_else(|| {
                                    crate::i18n::t(
                                        "进程已退出",
                                        "The process has already exited",
                                    )
                                })
                        },
                    )
                };
                let needs_root = match ownership {
                    Ok(value) => value,
                    Err(message) => {
                        set_process_action_error(&proc_weak, message);
                        return;
                    }
                };
                if needs_root && password.is_empty() {
                    set_process_action_error(
                        &proc_weak,
                        crate::i18n::t(
                            "请输入管理员（sudo）密码",
                            "Enter the administrator (sudo) password",
                        ),
                    );
                    return;
                }

                let root_password =
                    needs_root.then(|| crate::config::Secret::new(password.to_string()));
                let response = handles
                    .borrow()
                    .get(&tab_id)
                    .map(|handle| handle.kill_process(pid, root_password));
                let Some(response) = response else {
                    set_process_action_error(
                        &proc_weak,
                        crate::i18n::t("SSH 会话不可用", "The SSH session is unavailable"),
                    );
                    return;
                };

                let done_weak = proc_weak.clone();
                runtime.spawn(async move {
                    let result = response
                        .await
                        .unwrap_or_else(|_| crate::ssh::ProcessKillResult {
                            success: false,
                            message: crate::i18n::t(
                                "SSH 会话已关闭",
                                "The SSH session has closed",
                            )
                            .to_string(),
                        });
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(pw) = done_weak.upgrade() {
                            pw.set_action_busy(false);
                            pw.set_action_error(!result.success);
                            pw.set_action_status(result.message.into());
                        }
                    });
                });
            },
        );
    }
    PROC.with(|slot| *slot.borrow_mut() = Some(proc.clone()));
    Some(proc)
}

/// 打开进程窗：创建（首次）→ 同步主题 → 显示 → 位置与首帧补救。
pub(super) fn show_process(main: &AppWindow, rows: &Rc<VecModel<ProcRow>>, ctx: &AppContext) {
    let Some(proc) = ensure_process(main, rows, ctx) else {
        return;
    };
    proc.set_host(main.get_connection_state());
    sync_proc_theme(main, &proc);
    settings_window::resync_if_open(main);
    let _ = proc.show();
    place_process_window(main, &proc);
    // 首帧：macOS 上新映射的第二个窗口不会自动产生首次渲染事件，布局是渲染时
    // 惰性计算的 —— 0 / 50 / 200ms 各请求一次。
    request_first_frame(proc.window());
    let f1 = proc.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(50), move || {
        if let Some(w) = f1.upgrade() {
            request_first_frame(w.window());
        }
    });
    let f2 = proc.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(200), move || {
        if let Some(w) = f2.upgrade() {
            request_first_frame(w.window());
        }
    });
    proc.window().with_winit_window(|ww| ww.focus_window());
}

pub(super) fn hide_process() {
    PROC.with(|slot| {
        if let Some(proc) = slot.borrow().as_ref() {
            let _ = proc.hide();
        }
    });
}

/// 关闭进程窗并归还 wgpu 设备；销毁时机见 `release_system_info`。
pub(super) fn release_process() {
    let proc = PROC.with(|slot| slot.borrow_mut().take());
    if let Some(proc) = proc {
        let _ = proc.hide();
        drop_after_tick(proc);
    }
}

/// 首次打开系统信息窗时创建并接好回调；之后直接复用。
pub(super) fn ensure_system_info(
    _main: &AppWindow,
    models: &SysModels,
) -> Option<Rc<SystemInfoWindow>> {
    if let Some(sys) = SYS.with(|slot| slot.borrow().clone()) {
        return Some(sys);
    }
    let sys = match SystemInfoWindow::new() {
        Ok(sys) => Rc::new(sys),
        Err(err) => {
            tracing::warn!("failed to build system info window: {err}");
            return None;
        }
    };
    sys.set_custom_titlebar(cfg!(not(target_os = "macos")));
    super::apply_chrome_on_first_frame(sys.as_ref());
    sys.set_metrics(ModelRc::from(models.metrics.clone()));
    sys.set_nets(ModelRc::from(models.nets.clone()));
    sys.set_overview_rows(ModelRc::from(models.overview.clone()));
    sys.set_cpu_info_rows(ModelRc::from(models.cpu_info.clone()));
    sys.set_gpu_info_rows(ModelRc::from(models.gpu_info.clone()));
    sys.set_cpu_usage_rows(ModelRc::from(models.cpu_usage.clone()));
    sys.set_memory_rows(ModelRc::from(models.memory.clone()));
    sys.set_swap_rows(ModelRc::from(models.swap.clone()));
    sys.set_network_rows(ModelRc::from(models.network.clone()));
    sys.set_filesystem_rows(ModelRc::from(models.filesystem.clone()));
    sys.on_request_close(release_system_info);
    {
        let weak = sys.as_weak();
        sys.on_win_drag(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_window();
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        use i_slint_backend_winit::winit::window::ResizeDirection;
        let weak = sys.as_weak();
        sys.on_win_resize_se(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    let _ = ww.drag_resize_window(ResizeDirection::SouthEast);
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    SYS.with(|slot| *slot.borrow_mut() = Some(sys.clone()));
    Some(sys)
}

/// 系统信息窗当前是否已创建（用于退出时统一隐藏）。
pub(super) fn hide_system_info() {
    SYS.with(|slot| {
        if let Some(sys) = slot.borrow().as_ref() {
            let _ = sys.hide();
        }
    });
}

/// 关闭系统信息窗并归还它那套 wgpu 设备：先 hide，再把 `Rc` 挪到下一轮事件循环
/// 的定时器里 drop —— 不在属性/回调派发栈上销毁子树（#323/#343）。
pub(super) fn release_system_info() {
    let sys = SYS.with(|slot| slot.borrow_mut().take());
    if let Some(sys) = sys {
        let _ = sys.hide();
        drop_after_tick(sys);
    }
}

fn drop_after_tick<T: 'static>(handle: Rc<T>) {
    slint::Timer::single_shot(std::time::Duration::ZERO, move || drop(handle));
}

/// 首次打开设置窗时创建并接好回调；之后直接复用。
pub(super) fn ensure_settings(main: &AppWindow) -> Option<Rc<SettingsWindow>> {
    if let Some(sw) = SETTINGS.with(|slot| slot.borrow().clone()) {
        return Some(sw);
    }
    let sw = match SettingsWindow::new() {
        Ok(sw) => Rc::new(sw),
        Err(err) => {
            tracing::warn!("failed to build settings window: {err}");
            return None;
        }
    };
    settings_window::wire_settings_window(&main.as_weak(), &sw);
    sw.set_custom_titlebar(cfg!(not(target_os = "macos")));
    super::apply_chrome_on_first_frame(sw.as_ref());
    {
        let sw_weak = sw.as_weak();
        sw.on_win_drag(move || {
            if let Some(w) = sw_weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    if let Err(e) = ww.drag_window() {
                        tracing::warn!("settings drag_window failed: {e}");
                    }
                });
                schedule_slint_pointer_ungrab(sw_weak.clone());
            }
        });
    }
    {
        let sw_weak = sw.as_weak();
        sw.on_win_resize_se(move || {
            if let Some(w) = sw_weak.upgrade() {
                use i_slint_backend_winit::winit::window::ResizeDirection;
                w.window().with_winit_window(|ww| {
                    if let Err(e) = ww.drag_resize_window(ResizeDirection::SouthEast) {
                        tracing::warn!("settings drag_resize_window failed: {e}");
                    }
                });
                schedule_slint_pointer_ungrab(sw_weak.clone());
            }
        });
    }
    {
        let main_weak = main.as_weak();
        sw.on_close_requested(move || {
            release_settings();
            if let Some(m) = main_weak.upgrade() {
                m.set_interface_open(false);
            }
        });
    }
    SETTINGS.with(|slot| *slot.borrow_mut() = Some(sw.clone()));
    Some(sw)
}

/// 关闭设置窗并归还 wgpu 设备；销毁时机见 `release_system_info`。
pub(super) fn release_settings() {
    let sw = SETTINGS.with(|slot| slot.borrow_mut().take());
    if let Some(sw) = sw {
        let _ = sw.hide();
        drop_after_tick(sw);
    }
}

/// 打开系统信息窗：创建（首次）→ 灌主窗口状态 → 显示 → 位置与首帧补救。
pub(super) fn show_system_info(main: &AppWindow, models: &SysModels) {
    if !main.get_system_info_available() {
        return;
    }
    let Some(sys) = ensure_system_info(main, models) else {
        return;
    };
    sys.set_host(main.get_conn_host());
    sys.set_connection_state(main.get_connection_state());
    sys.set_resource_title(main.get_resource_title());
    sync_system_info_theme(main, &sys);
    let _ = sys.show();
    place_system_info_window(main, &sys);
    // 首帧：macOS 上新映射的第二个窗口不会自动产生首次渲染事件，布局是渲染时
    // 惰性计算的 —— 0 / 50 / 200ms 各请求一次，首次可能早于窗口映射被丢弃。
    request_first_frame(sys.window());
    let f1 = sys.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(50), move || {
        if let Some(w) = f1.upgrade() {
            request_first_frame(w.window());
        }
    });
    let f2 = sys.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(200), move || {
        if let Some(w) = f2.upgrade() {
            request_first_frame(w.window());
        }
    });
    sys.window().with_winit_window(|ww| ww.focus_window());
}
