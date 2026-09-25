//! Window geometry helpers: centering, hit-testing and platform quirks.
//!
//! Maps pointer/scroll positions onto the frameless window's logical regions
//! (terminal panes, SFTP list). The Win32 FFI is confined to the two functions
//! that need OS geometry (centering, cursor position).

use std::collections::HashMap;

use slint::{ComponentHandle as _, Model as _, VecModel};

#[cfg(windows)]
use i_slint_backend_winit::WinitWindowAccessor;

use crate::layout::{LogicalRect, TerminalWheelHit};
use crate::sftp::SftpHandles;
use crate::terminal::TermBuffers;

use crate::app::terminal_sftp_paths;
use super::term_buf;
use crate::ui::{AppWindow, TerminalState};

/// Center the window on the primary monitor's work area (Windows).
#[cfg(windows)]
pub(crate) fn center_window(win: &AppWindow) {
    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SystemParametersInfoW(action: u32, uiparam: u32, pvparam: *mut Rect, winini: u32)
        -> i32;
    }
    const SPI_GETWORKAREA: u32 = 0x0030;

    let size = win.window().size(); // physical pixels
    let mut wa = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa, 0) };
    if ok == 0 {
        return;
    }
    let area_w = (wa.right - wa.left).max(0) as u32;
    let area_h = (wa.bottom - wa.top).max(0) as u32;
    let x = wa.left + ((area_w.saturating_sub(size.width)) / 2) as i32;
    let y = wa.top + ((area_h.saturating_sub(size.height)) / 2) as i32;
    win.window()
        .set_position(slint::PhysicalPosition::new(x, y));
}

#[cfg(not(windows))]
pub(crate) fn center_window(_win: &AppWindow) {}

/// The active terminal tab's current SFTP directory ("" if unknown).
pub(crate) fn active_sftp_path(win: &AppWindow, tab_id: &str) -> String {
    let model = win.get_terminals();
    if let Some(m) = model.as_any().downcast_ref::<VecModel<TerminalState>>() {
        for i in 0..m.row_count() {
            if let Some(row) = m.row_data(i)
                && row.id.as_str() == tab_id
            {
                return row.sftp_path.to_string();
            }
        }
    }
    String::new()
}

pub(crate) fn handle_macos_terminal_wheel(
    win: &AppWindow,
    bufs: &TermBuffers,
    x: f32,
    y: f32,
    lines: i32,
) -> bool {
    let Some(hit) = terminal_wheel_hit(win, bufs, x, y) else {
        return false;
    };
    if hit.is_alt {
        win.invoke_terminal_wheel(hit.tab_id.into(), lines.signum(), hit.col, hit.row);
    } else {
        win.invoke_terminal_scroll(hit.tab_id.into(), lines);
    }
    true
}

// 原先这里有个 `macos_terminal_wheel_can_target_terminal(interface_open)`：设置页还叠在
// 主窗口里时，要用它挡住"设置打开期间滚轮喂给终端"。设置页改成**独立窗口**后，滚轮由系统
// 按指针位置派发到对应窗口，这个守卫反而会让设置开着时主窗口滚不动 —— 已删除。

pub(crate) fn terminal_wheel_hit(
    win: &AppWindow,
    bufs: &TermBuffers,
    x: f32,
    y: f32,
) -> Option<TerminalWheelHit> {
    let (active, term, term_state) = active_terminal_panel_rects(win)?;
    let mut term_x = term.x;
    let mut term_y = term.y;
    let mut term_w = term.w;
    let mut term_h = term.h;

    // TerminalView starts with a 24px status line, then the SFTP dock-region.
    term_y += 24.0;
    term_h = (term_h - 24.0).max(0.0);

    let sftp_dock = win.get_sftp_dock().to_string();
    let sftp_take = if term_state.sftp_collapsed {
        36.0
    } else if sftp_dock == "left" || sftp_dock == "right" {
        term_state.sftp_panel_width + 4.0
    } else {
        term_state.sftp_panel_height + 4.0
    };
    shrink_edge(
        &mut term_x,
        &mut term_y,
        &mut term_w,
        &mut term_h,
        &sftp_dock,
        sftp_take,
    );

    // Leave the command bar to TextInput/history handling; wheel fallback is for
    // terminal output only.
    term_h = (term_h - 34.0).max(0.0);
    if !contains_logical(
        LogicalRect {
            x: term_x,
            y: term_y,
            w: term_w,
            h: term_h,
        },
        x,
        y,
    ) {
        return None;
    }

    let h = term_buf(bufs, &active)?;
    let guard = h.lock().ok()?;
    let (rows, cols) = crate::terminal::term_size(&guard.term);
    let is_alt = crate::terminal::is_alt(&guard.term);
    let cell_w = (term_w / cols.max(1) as f32).max(1.0);
    let cell_h = (term_h / rows.max(1) as f32).max(1.0);
    Some(TerminalWheelHit {
        tab_id: active,
        is_alt,
        col: ((x - term_x) / cell_w).floor() as i32,
        row: ((y - term_y) / cell_h).floor() as i32,
    })
}

pub(crate) fn shrink_edge(x: &mut f32, y: &mut f32, w: &mut f32, h: &mut f32, dock: &str, amount: f32) {
    let amount = amount.max(0.0);
    match dock {
        "left" => {
            *x += amount;
            *w = (*w - amount).max(0.0);
        }
        "right" => *w = (*w - amount).max(0.0),
        "top" => {
            *y += amount;
            *h = (*h - amount).max(0.0);
        }
        "bottom" => *h = (*h - amount).max(0.0),
        _ => {}
    }
}

pub(crate) fn contains_logical(rect: LogicalRect, x: f32, y: f32) -> bool {
    x >= rect.x && x <= rect.x + rect.w && y >= rect.y && y <= rect.y + rect.h
}


pub(crate) fn app_content_area(win: &AppWindow) -> LogicalRect {
    let size = win.window().size();
    let scale = win.window().scale_factor().max(0.01);
    let mut area = LogicalRect {
        x: 0.0,
        y: if win.get_custom_titlebar() {
            38.0
        } else if win.get_is_mac() {
            28.0
        } else {
            0.0
        },
        w: size.width as f32 / scale,
        h: 0.0,
    };
    area.h = size.height as f32 / scale - area.y;

    if win.get_welcome_as_sidebar() {
        let dock = win.get_welcome_sidebar_dock().to_string();
        let sidebar_strip_outside = !win.get_welcome_collapsed()
            && win.get_sidebar_collapsed()
            && win.get_sidebar_dock().as_str() == dock.as_str();
        let welcome_taken = (if win.get_welcome_collapsed() {
            36.0
        } else {
            win.get_welcome_sidebar_width()
        }) + if sidebar_strip_outside { 36.0 } else { 0.0 };
        shrink_edge(
            &mut area.x,
            &mut area.y,
            &mut area.w,
            &mut area.h,
            &dock,
            welcome_taken,
        );
    }

    let side_dock = win.get_sidebar_dock().to_string();
    let side_take = if win.get_sidebar_collapsed() {
        36.0
    } else if side_dock == "left" || side_dock == "right" {
        win.get_sidebar_width() + 4.0
    } else {
        win.get_sidebar_height() + 4.0
    };
    shrink_edge(
        &mut area.x,
        &mut area.y,
        &mut area.w,
        &mut area.h,
        &side_dock,
        side_take,
    );
    if win.get_quick_panel_open() {
        let quick_dock = win.get_quick_panel_dock().to_string();
        let quick_merged = win.get_quick_panel_collapsed()
            && ((win.get_welcome_as_sidebar()
                && win.get_welcome_collapsed()
                && win.get_welcome_sidebar_dock().as_str() == quick_dock.as_str())
                || (win.get_sidebar_collapsed() && side_dock.as_str() == quick_dock.as_str()));
        if quick_merged {
            return area;
        }
        let quick_take = if win.get_quick_panel_collapsed() {
            36.0
        } else if quick_dock == "left" || quick_dock == "right" {
            win.get_quick_panel_width() + 4.0
        } else {
            win.get_quick_panel_height() + 4.0
        };
        shrink_edge(
            &mut area.x,
            &mut area.y,
            &mut area.w,
            &mut area.h,
            &quick_dock,
            quick_take,
        );
    }
    area
}

pub(crate) fn active_terminal_panel_rects(win: &AppWindow) -> Option<(String, LogicalRect, TerminalState)> {
    let active = win.get_active_tab_id().to_string();
    if active.is_empty() || active == "welcome" {
        return None;
    }

    let area = app_content_area(win);
    let panes = win.get_panes();
    let pane = (0..panes.row_count())
        .filter_map(|i| panes.row_data(i))
        .find(|p| p.active_id.as_str() == active.as_str())?;

    let terms = win.get_terminals();
    let term_state = (0..terms.row_count())
        .filter_map(|i| terms.row_data(i))
        .find(|t| t.id.as_str() == active.as_str())?;

    Some((
        active,
        LogicalRect {
            x: area.x + pane.x,
            y: area.y + pane.y + 40.0,
            w: pane.w,
            h: (pane.h - 40.0).max(0.0),
        },
        term_state,
    ))
}

/// 当前标签页 SFTP 文件列表在窗口内的逻辑矩形（拖放落点判定用）。
///
/// 纯 Slint 模型几何，与平台无关 —— 之前只被 Windows 的拖放处理器用到，才挂着
/// `#[cfg(windows)]` 躲 dead-code 警告；现在非 Windows 的拖放也走这条路径。
pub(crate) fn active_sftp_file_list_rect(win: &AppWindow) -> Option<LogicalRect> {
    let (_active, term, term_state) = active_terminal_panel_rects(win)?;
    if term_state.sftp_collapsed {
        return None;
    }

    // TerminalView starts with a 24px connection-status line; SFTP docks inside
    // the remaining dock-region. This mirrors ui/terminal_view.slint.
    let dock_region = LogicalRect {
        x: term.x,
        y: term.y + 24.0,
        w: term.w,
        h: (term.h - 24.0).max(0.0),
    };
    let dock = win.get_sftp_dock().to_string();
    let mut panel = LogicalRect {
        x: dock_region.x,
        y: dock_region.y,
        w: if dock == "left" || dock == "right" {
            term_state.sftp_panel_width
        } else {
            dock_region.w
        },
        h: if dock == "left" || dock == "right" {
            dock_region.h
        } else {
            term_state.sftp_panel_height
        },
    };
    if dock == "right" {
        panel.x = dock_region.x + (dock_region.w - panel.w).max(0.0);
    } else if dock == "bottom" {
        panel.y = dock_region.y + (dock_region.h - panel.h).max(0.0);
    }

    // SftpPanel layout: toolbar 34, then file headers 20 + separator 1; when the
    // tree is shown (top/bottom docks), the file list starts after tree 160 + sep.
    let show_tree = dock != "left" && dock != "right";
    panel.y += 34.0 + 20.0 + 1.0;
    panel.h = (panel.h - 34.0 - 20.0 - 1.0).max(0.0);
    if show_tree {
        panel.x += 160.0 + 1.0;
        panel.w = (panel.w - 160.0 - 1.0).max(0.0);
    }
    Some(panel)
}

/// Current mouse cursor position in physical screen pixels (Windows).
#[cfg(windows)]
pub(crate) fn cursor_pos() -> Option<(i32, i32)> {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    unsafe extern "system" {
        fn GetCursorPos(p: *mut Point) -> i32;
    }
    let mut p = Point { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut p) } != 0 {
        Some((p.x, p.y))
    } else {
        None
    }
}

/// Handle an OS file drop: if it landed over the SFTP file-list area of the
/// active session tab, upload the file to that tab's current remote directory.
///
/// `hovered_pos` 是最后一次已知的指针位置（**逻辑客户区坐标**）。Windows 忽略它 ——
/// OLE 拖放会抑制 `WM_MOUSEMOVE`，传进来的位置是过期的 —— 改问系统拿；其它平台没有
/// 别的办法知道落点，只能靠它（上游 0aeba62：此前非 Windows 是空实现，拖放完全无效）。
pub(crate) fn handle_file_drop(
    win: &AppWindow,
    sftp_handles: &SftpHandles,
    path: std::path::PathBuf,
    hovered_pos: Option<(f32, f32)>,
) {
    #[cfg(windows)]
    let point = {
        let w = win.window();
        let scale = w.scale_factor().max(0.01);
        w.with_winit_window(|ww| ww.inner_position().ok())
            .flatten()
            .and_then(|inner| {
                cursor_pos().map(|(cx, cy)| {
                    (
                        (cx - inner.x) as f32 / scale,
                        (cy - inner.y) as f32 / scale,
                    )
                })
            })
    };
    #[cfg(not(windows))]
    let point = hovered_pos;

    let Some((client_x, client_y)) = point else {
        return;
    };
    handle_file_drop_at(win, sftp_handles, path, client_x, client_y);
}

/// 落点命中 SFTP 文件列表就上传（平台无关的公共部分）。
fn handle_file_drop_at(
    win: &AppWindow,
    sftp_handles: &SftpHandles,
    path: std::path::PathBuf,
    client_x: f32,
    client_y: f32,
) {
    let active = win.get_active_tab_id().to_string();
    if active == "welcome" {
        return;
    }
    let Some(file_list) = active_sftp_file_list_rect(win) else {
        return;
    };
    if !contains_logical(file_list, client_x, client_y) {
        return; // dropped outside the file list — ignore
    }

    let dir = active_sftp_path(win, &active);
    if dir.is_empty() {
        return;
    }
    // Session-sync (#sync): when both toggles are on, also mirror the drop to
    // every other online session — each into *its own* current SFTP dir. This
    // matches the upload button's behaviour (drag-and-drop is a separate path).
    let sync = win.get_sync_input() && win.get_sync_upload_enabled();
    let other_dirs = if sync {
        terminal_sftp_paths(win)
    } else {
        HashMap::new()
    };
    if let Ok(handles) = sftp_handles.lock() {
        if let Some(h) = handles.get(&active) {
            win.set_download_open(true);
            h.upload(path.clone(), dir);
        }
        if sync {
            for (id, h) in handles.iter() {
                if id == &active {
                    continue;
                }
                if let Some(d) = other_dirs.get(id).filter(|d| !d.is_empty()) {
                    h.upload(path.clone(), d.clone());
                }
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> LogicalRect {
        LogicalRect { x, y, w, h }
    }

    /// 四种停靠边各自让出空间的方向（left/top 还要把起点挪开）。
    #[test]
    fn shrink_edge_shrinks_per_dock_side() {
        let (mut x, mut y, mut w, mut h) = (10.0, 20.0, 100.0, 50.0);
        shrink_edge(&mut x, &mut y, &mut w, &mut h, "left", 8.0);
        assert_eq!((x, y, w, h), (18.0, 20.0, 92.0, 50.0), "left：起点右移 + 减宽");

        let (mut x, mut y, mut w, mut h) = (10.0, 20.0, 100.0, 50.0);
        shrink_edge(&mut x, &mut y, &mut w, &mut h, "right", 8.0);
        assert_eq!((x, y, w, h), (10.0, 20.0, 92.0, 50.0), "right：只减宽");

        let (mut x, mut y, mut w, mut h) = (10.0, 20.0, 100.0, 50.0);
        shrink_edge(&mut x, &mut y, &mut w, &mut h, "top", 8.0);
        assert_eq!((x, y, w, h), (10.0, 28.0, 100.0, 42.0), "top：起点下移 + 减高");

        let (mut x, mut y, mut w, mut h) = (10.0, 20.0, 100.0, 50.0);
        shrink_edge(&mut x, &mut y, &mut w, &mut h, "bottom", 8.0);
        assert_eq!((x, y, w, h), (10.0, 20.0, 100.0, 42.0), "bottom：只减高");
    }

    /// 未知停靠值不动（停靠边是配置里的字符串，拼错不应被当成某个方向）。
    #[test]
    fn shrink_edge_ignores_unknown_dock() {
        let (mut x, mut y, mut w, mut h) = (10.0, 20.0, 100.0, 50.0);
        shrink_edge(&mut x, &mut y, &mut w, &mut h, "center", 8.0);
        assert_eq!((x, y, w, h), (10.0, 20.0, 100.0, 50.0));
    }

    /// 负的 amount 视作 0；缩过头时宽高夹在 0（不能出现负数 —— 后面要拿它算
    /// 单元格宽高，负值会传染成 NaN）。
    #[test]
    fn shrink_edge_clamps_amount_and_dimensions() {
        let (mut x, mut y, mut w, mut h) = (10.0, 20.0, 100.0, 50.0);
        shrink_edge(&mut x, &mut y, &mut w, &mut h, "left", -5.0);
        assert_eq!((x, w), (10.0, 100.0), "负数 = 不缩");

        let (mut x, mut y, mut w, mut h) = (10.0, 20.0, 100.0, 50.0);
        shrink_edge(&mut x, &mut y, &mut w, &mut h, "left", 500.0);
        assert_eq!(w, 0.0, "宽度缩到 0 为止");
        assert_eq!(x, 510.0, "起点仍按 amount 移动（被夹的只是宽高）");
    }

    /// 命中判定是**闭区间**：正好落在右/下边缘也算命中。
    #[test]
    fn contains_logical_includes_the_border() {
        let r = rect(10.0, 20.0, 100.0, 50.0);
        assert!(contains_logical(r, 10.0, 20.0), "左上角");
        assert!(contains_logical(r, 60.0, 45.0), "中间");
        assert!(contains_logical(r, 110.0, 70.0), "右下边缘");
        assert!(!contains_logical(r, 9.9, 45.0), "左侧外出");
        assert!(!contains_logical(r, 60.0, 70.1), "下方外出");
    }
}
