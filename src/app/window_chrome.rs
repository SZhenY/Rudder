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

use super::{center_window, save_layout, schedule_slint_pointer_ungrab, should_block_close};

pub(crate) fn wire_window_chrome(
    window: &AppWindow,
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    store: &Rc<RefCell<ConfigStore>>,
    exit_confirmed: &Rc<Cell<bool>>,
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
                // ⚠️ macOS 上**不要**再自己 toggle：那块留白区的双击本来就是 AppKit 在处理
                // （它把窗口 zoom 成屏幕大小、还原时回到 `standard_frame`，两个方向都对），
                // 我们再补一次 `set_maximized`/`performZoom` 就等于"跟系统抢" —— 表现为
                // 第一次能最大化、第二次"回弹一下又最大化"（0.7.9 起的老 bug，2026-09-21
                // 用 `isZoomed` 日志抓到的：系统 zoom 完 ~0.5s 后我们的 handler 才跑）。
                // macOS 上这个回调现在只有 Windows/Linux 的小窗口按钮用得到，
                // 上面那条 `titlebar-inset-strip` 已不再派发它。
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
        let close_handles = handles.clone();
        let wc_store = store.clone();
        let wc_exit_confirmed = exit_confirmed.clone();
        window.on_win_close(move || {
            if let Some(w) = weak.upgrade() {
                // Mirror the native-X behaviour: confirm if sessions are open.
                if !should_block_close(wc_exit_confirmed.get(), !close_handles.borrow().is_empty())
                {
                    wc_exit_confirmed.set(true);
                    save_layout(&w, &wc_store);
                    let _ = slint::quit_event_loop();
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
                    let _ = ww.drag_window();
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }
    {
        use i_slint_backend_winit::winit::window::ResizeDirection;
        let weak = window.as_weak();
        window.on_win_resize(move |dir: i32| {
            if let Some(w) = weak.upgrade() {
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
                    let _ = ww.drag_resize_window(d);
                });
                schedule_slint_pointer_ungrab(weak.clone());
            }
        });
    }

    {
        // 在系统文件管理器里显示日志目录（设置 → 关于）。
        window.on_open_log_dir(move || {
            let dir = crate::config::log_dir();
            let _ = std::fs::create_dir_all(&dir);
            #[cfg(target_os = "macos")]
            let _ = std::process::Command::new("open").arg(&dir).spawn();
            #[cfg(target_os = "windows")]
            let _ = std::process::Command::new("explorer").arg(&dir).spawn();
            #[cfg(all(unix, not(target_os = "macos")))]
            let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
        });
    }

    // 兜底居中：常规启动路径（`app::run` 的尺寸/位置预设）已在窗口**显示前**落位
    // 居中；这条 30ms 定时器保留两个用途 —— 首次启动没有保存尺寸、预设算不出位置时
    // 靠它居中；首帧后拿到真实窗口尺寸再校正一次（幂等：位置已正确时算出的坐标
    // 相同，无视觉变化）。
    {
        let weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(30), move || {
            if let Some(w) = weak.upgrade() {
                center_window(&w);
            }
        });
    }

}
