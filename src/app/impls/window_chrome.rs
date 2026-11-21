//! Custom title-bar window controls (#119): minimize / maximize / close on
//! the frameless window, plus the close-confirmation flow.

use slint::ComponentHandle as _;
use i_slint_backend_winit::WinitWindowAccessor;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::config::ConfigStore;
use crate::ssh::SessionHandle;
use crate::ui::AppWindow;
use std::collections::HashMap;

use super::{center_window, schedule_slint_pointer_ungrab, should_block_close};

pub(crate) fn wire_window_chrome(
    window: &AppWindow,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    store: &Rc<RefCell<ConfigStore>>,
    exit_confirmed: &Rc<Cell<bool>>,
    sftp_handles: &crate::sftp::SftpHandles,
) {
    // --- Custom title-bar window controls (#119) --------------------------
    {
        let weak = window.as_weak();
        window.on_win_minimize(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| ww.set_minimized(true));
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_win_maximize_toggle(move || {
            if let Some(w) = weak.upgrade() {
                // 自绘标题栏的最大化按钮 / 双击走这里；macOS 保留原生装饰，
                // 顶部留白条的双击由 win-zoom-toggle 接管，避免和 AppKit 的 zoom 抢。
                let now = w.window().with_winit_window(|ww| {
                    let m = !ww.is_maximized();
                    ww.set_maximized(m);
                    tracing::debug!("maximize-toggle: target={m} after={}", ww.is_maximized());
                    m
                });
                if let Some(m) = now {
                    w.set_window_maximized(m);
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_win_zoom_toggle(move || {
            let Some(w) = weak.upgrade() else { return };
            // 目标状态从窗口系统读，不用 Slint 属性：原生标题栏双击、Fill / Zoom
            // 系统设置都会改变 OS 状态，属性只有在 Resized 事件里才同步。
            let os_state = w.window().with_winit_window(|ww| ww.is_maximized());
            let prop_state = w.get_window_maximized();
            let target = !os_state.unwrap_or(prop_state);
            tracing::info!(
                "[ZOOM] callback: property={prop_state} os={os_state:?} target={target}"
            );
            let weak2 = w.as_weak();
            // AppKit 的 zoom 可能和我们的 toggle 同时触发：等一拍再检查，
            // 状态已经变成目标值就说明系统处理过了，不再补一次。
            slint::Timer::single_shot(std::time::Duration::from_millis(180), move || {
                let Some(w) = weak2.upgrade() else { return };
                let before = w.window().with_winit_window(|ww| ww.is_maximized());
                let applied = w.window().with_winit_window(|ww| {
                    if before == Some(target) {
                        return false;
                    }
                    ww.set_maximized(target);
                    true
                });
                // 无论是否补 toggle，都把 Slint 属性同步到实际窗口状态。
                let after = w
                    .window()
                    .with_winit_window(|ww| ww.is_maximized())
                    .unwrap_or(target);
                w.set_window_maximized(after);
                tracing::info!(
                    "[ZOOM] timer: before={before:?} applied={applied:?} after={after}"
                );
            });
        });
    }
    {
        let weak = window.as_weak();
        let close_handles = handles.clone();
        let wc_store = store.clone();
        let wc_sftp = sftp_handles.clone();
        let wc_exit_confirmed = exit_confirmed.clone();
        window.on_win_close(move || {
            if let Some(w) = weak.upgrade() {
                // Mirror the native-X behaviour: confirm if sessions are open.
                if !should_block_close(wc_exit_confirmed.get(), !close_handles.borrow().is_empty())
                {
                    wc_exit_confirmed.set(true);
                    // 与原生 X / 确认弹窗同一条收尾路径。
                    super::begin_shutdown(&w, &wc_store, &close_handles, &wc_sftp);
                } else {
                    w.set_confirm_close_open(true);
                }
            }
        });
    }
    {
        // 自绘标题栏拖动：交给窗口系统，并补一次合成的指针释放 —— Linux 的 WM /
        // 合成器可能吃掉 up 事件，Slint 若一直抓着指针就会卡在移动光标状态。
        let weak = window.as_weak();
        window.on_win_drag(move || {
            if let Some(w) = weak.upgrade() {
                w.window().with_winit_window(|ww| {
                    if let Err(e) = ww.drag_window() {
                        tracing::warn!("drag_window failed: {e}");
                    }
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_win_resize(move |dir: i32| {
            if let Some(w) = weak.upgrade() {
                use i_slint_backend_winit::winit::window::ResizeDirection;
                let d = match dir {
                    0 => ResizeDirection::North,
                    1 => ResizeDirection::South,
                    2 => ResizeDirection::East,
                    3 => ResizeDirection::West,
                    4 => ResizeDirection::NorthEast,
                    5 => ResizeDirection::NorthWest,
                    6 => ResizeDirection::SouthEast,
                    _ => ResizeDirection::SouthWest,
                };
                w.window().with_winit_window(|ww| {
                    if let Err(e) = ww.drag_resize_window(d) {
                        tracing::warn!("drag_resize_window({d:?}) failed: {e}");
                    }
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }

    // 配置里有尺寸时，位置已经在建窗前定好了（`place_main_window_before_show`），这里再居中
    // 会多出一次可见位移；只有首次运行（没存过尺寸）才需要这 30ms 的兜底居中。
    let has_saved_size = {
        let (w, h) = store.borrow().window_size();
        w > 0.0 && h > 0.0
    };
    if !has_saved_size {
        let weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(30), move || {
            if let Some(w) = weak.upgrade() {
                center_window(&w);
            }
        });
    }

}

