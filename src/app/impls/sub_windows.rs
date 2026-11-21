//! 子窗口的按需创建与延迟销毁。
//!
//! 进程窗 / 系统信息窗 / 设置窗原先在启动时全部 `new()`，但其中只有主窗口在
//! 首帧是必须的：另外三个各自带一整棵 Slint 组件树（设置窗还含 TerminalPage）。
//! 首次打开才创建；关闭时 hide，下一轮事件循环再销毁（#323/#343）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, VecModel};

use i_slint_backend_winit::WinitWindowAccessor as _;

use super::resource_ui::{
    place_process_window, process_needs_root, request_first_frame,
    set_process_action_error, sync_proc_theme,
};
use super::settings_window;
use super::window::schedule_slint_pointer_ungrab;
use super::AppContext;
use crate::ui::{
    AppWindow, ProcRow, ProcWindow, SettingsWindow,
};

thread_local! {
    static PROC: RefCell<Option<Rc<ProcWindow>>> = const { RefCell::new(None) };
    static SETTINGS: RefCell<Option<Rc<SettingsWindow>>> = const { RefCell::new(None) };
    /// 已 hide、等下一轮事件循环 drop 的窗口，按类型一个槽。
    static PENDING: RefCell<HashMap<&'static str, PendingSlot>> = RefCell::new(HashMap::new());
    /// 主窗口弱引用，供关闭子窗口后归还键盘焦点。
    static MAIN_WEAK: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
}

/// 登记主窗口弱引用；`run()` 建窗后调用一次。
pub(super) fn watch_main_window(weak: slint::Weak<AppWindow>) {
    MAIN_WEAK.with(|slot| *slot.borrow_mut() = Some(weak));
}

/// 子窗口关闭后把焦点交还主窗口，恢复 1Hz 采样与光标闪烁的激活态。
fn refocus_main_window() {
    let weak = MAIN_WEAK.with(|slot| slot.borrow().clone());
    if let Some(main) = weak.and_then(|w| w.upgrade()) {
        main.window().with_winit_window(|ww| ww.focus_window());
    }
}

const KIND_PROC: &str = "proc";
const KIND_SETTINGS: &str = "settings";

/// 延迟销毁中的窗口。三种窗口不同型，用枚举装进同一张表。
enum PendingWindow {
    Proc(Rc<ProcWindow>),
    Settings(Rc<SettingsWindow>),
}

type PendingSlot = Rc<RefCell<Option<PendingWindow>>>;

/// 延迟销毁；reopen 时从槽里取回窗口复用。
fn drop_after_tick(kind: &'static str, window: PendingWindow) {
    let slot: PendingSlot = Rc::new(RefCell::new(Some(window)));
    PENDING.with(|pending| {
        pending.borrow_mut().insert(kind, slot.clone());
    });
    let callback_slot = slot.clone();
    slint::Timer::single_shot(std::time::Duration::ZERO, move || {
        PENDING.with(|pending| {
            pending.borrow_mut().remove(kind);
        });
        // 未被取走才真正销毁。
        drop(callback_slot.borrow_mut().take());
    });
}

/// reopen：撤销尚未触发的延迟销毁并取回窗口。
fn take_pending(kind: &'static str) -> Option<PendingWindow> {
    let slot = PENDING.with(|pending| pending.borrow_mut().remove(kind))?;
    slot.borrow_mut().take()
}

/// 任一子窗口仍可见。深度空闲裁剪前用它确认没有窗口在用。
pub(super) fn any_visible() -> bool {
    SETTINGS.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|w| w.window().is_visible())
    }) || PROC.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|w| w.window().is_visible())
    })
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

/// 已创建的系统信息窗弱引用；未创建时返回默认（不可升级）的 `Weak`。
/// 首次打开进程窗时创建并接好回调；之后直接复用。
pub(super) fn ensure_process(
    _main: &AppWindow,
    rows: &Rc<VecModel<ProcRow>>,
    ctx: &AppContext,
) -> Option<Rc<ProcWindow>> {
    if let Some(proc) = PROC.with(|slot| slot.borrow().clone()) {
        return Some(proc);
    }
    // 关闭后一帧内又打开：撤销待触发的销毁，复用旧组件树。
    if let Some(PendingWindow::Proc(proc)) = take_pending(KIND_PROC) {
        PROC.with(|slot| *slot.borrow_mut() = Some(proc.clone()));
        return Some(proc);
    }
    let proc = match ProcWindow::new() {
        Ok(proc) => Rc::new(proc),
        Err(err) => {
            tracing::error!("failed to build process window: {err}");
            return None;
        }
    };
    proc.set_custom_titlebar(cfg!(not(target_os = "macos")));
    super::apply_chrome_on_first_frame(proc.as_ref());
    proc.set_proc_list(ModelRc::from(rows.clone()));
    proc.on_request_close(release_process);
    proc.window().on_close_requested(|| {
        release_process();
        slint::CloseRequestResponse::HideWindow
    });
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

/// 关闭进程窗并归还 wgpu 设备。
pub(super) fn release_process() {
    let proc = PROC.with(|slot| slot.borrow_mut().take());
    if let Some(proc) = proc {
        let _ = proc.hide();
        drop_after_tick(KIND_PROC, PendingWindow::Proc(proc));
        super::mem::reclaim_after_release(super::mem::ReclaimReason::SubWindowClosed);
        refocus_main_window();
    }
}


/// 首次打开设置窗时创建并接好回调；之后直接复用。
///
/// 失败返回错误文本而不是 `Option`：调用方要能给用户一次提示，否则"点设置没反应"
/// 没有任何线索（`error.log` 在用户机器上没人看）。
pub(super) fn ensure_settings(main: &AppWindow) -> Result<Rc<SettingsWindow>, String> {
    if let Some(sw) = SETTINGS.with(|slot| slot.borrow().clone()) {
        return Ok(sw);
    }
    if let Some(PendingWindow::Settings(sw)) = take_pending(KIND_SETTINGS) {
        settings_window::remember_handle(&sw);
        SETTINGS.with(|slot| *slot.borrow_mut() = Some(sw.clone()));
        return Ok(sw);
    }
    let sw = match SettingsWindow::new() {
        Ok(sw) => Rc::new(sw),
        Err(err) => {
            tracing::error!("failed to build settings window: {err}");
            return Err(err.to_string());
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
        sw.on_close_requested(move || close_settings(&main_weak));
    }
    {
        // 系统级关窗（Alt+F4 / 原生标题栏）不经过组件的 close-requested，
        // 不接这条就只 hide，整套 wgpu 设备会一直留在进程里。
        let main_weak = main.as_weak();
        sw.window().on_close_requested(move || {
            close_settings(&main_weak);
            slint::CloseRequestResponse::HideWindow
        });
    }
    SETTINGS.with(|slot| *slot.borrow_mut() = Some(sw.clone()));
    Ok(sw)
}

/// 设置窗的统一关闭入口：组件级与窗口级回调共用，重复调用由 `take()` 幂等吸收。
fn close_settings(main_weak: &slint::Weak<AppWindow>) {
    release_settings();
    if let Some(m) = main_weak.upgrade() {
        m.set_interface_open(false);
    }
}

/// 关闭设置窗并归还 wgpu 设备。
pub(super) fn release_settings() {
    let sw = SETTINGS.with(|slot| slot.borrow_mut().take());
    if let Some(sw) = sw {
        let _ = sw.hide();
        super::settings_window::forget_handle();
        drop_after_tick(KIND_SETTINGS, PendingWindow::Settings(sw));
        // wgpu 设备随窗口销毁；只回收堆页，主窗口仍可见，不裁剪进程工作集。
        super::mem::reclaim_after_release(super::mem::ReclaimReason::SubWindowClosed);
        refocus_main_window();
    }
}
