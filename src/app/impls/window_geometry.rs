//! Window geometry helpers: centering, hit-testing and platform quirks.
//!
//! Maps pointer/scroll positions onto the frameless window's logical regions
//! (terminal panes, SFTP list). The Win32 FFI is confined to the two functions
//! that need OS geometry (centering, cursor position).

use std::collections::HashMap;

use slint::{ComponentHandle as _, Model as _, VecModel};

use crate::ui::Theme;

#[cfg(windows)]
use i_slint_backend_winit::WinitWindowAccessor;

use crate::layout::{LogicalRect, TerminalWheelHit};
use crate::sftp::SftpHandles;
use crate::terminal::TermBuffers;

use crate::app::terminal_sftp_paths;
use super::term_buf;
use crate::ui::{AppWindow, TerminalState};

/// 主屏工作区（物理像素）：左、上、宽、高。取不到返回 `None`。
#[cfg(windows)]
fn primary_work_area() -> Option<(i32, i32, u32, u32)> {
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

    let mut wa = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa, 0) } == 0 {
        return None;
    }
    Some((
        wa.left,
        wa.top,
        (wa.right - wa.left).max(0) as u32,
        (wa.bottom - wa.top).max(0) as u32,
    ))
}

/// 系统 DPI。建窗之前拿不到窗口自己的 scale factor，物理像素换算只能用它。
#[cfg(windows)]
fn system_dpi() -> u32 {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetDpiForSystem() -> u32;
    }
    let dpi = unsafe { GetDpiForSystem() };
    if dpi == 0 { 96 } else { dpi }
}

/// Center the window on the primary monitor's work area (Windows).
#[cfg(windows)]
pub(crate) fn center_window(win: &AppWindow) {
    let Some((wa_x, wa_y, area_w, area_h)) = primary_work_area() else {
        return;
    };
    let size = win.window().size(); // physical pixels
    let x = wa_x + ((area_w.saturating_sub(size.width)) / 2) as i32;
    let y = wa_y + ((area_h.saturating_sub(size.height)) / 2) as i32;
    win.window()
        .set_position(slint::PhysicalPosition::new(x, y));
}

#[cfg(not(windows))]
pub(crate) fn center_window(_win: &AppWindow) {}

/// 建窗之前把主窗口定到配置里的尺寸与位置。
///
/// Slint 的 winit 后端在原生窗口还没创建时，会把 `set_size` / `set_position` 记进 winit 的
/// `WindowAttributes`，建窗那一刻生效（`winitwindowadapter.rs` 的 `WinitWindowOrNone::None`
/// 分支）；`set_size` 同时置上 `has_explicit_size`，之后 `show` 不再按
/// `preferred-width/height` 覆盖。于是窗口第一次出现就是最终几何 —— 不会再有"先按
/// 1440×900 显示、随后缩回配置尺寸并居中"的两段跳。
#[cfg(windows)]
pub(crate) fn place_main_window_before_show(win: &AppWindow, size: (f32, f32)) {
    let window = win.window();
    let Some((wa_x, wa_y, area_w, area_h)) = primary_work_area() else {
        window.set_size(slint::LogicalSize::new(size.0, size.1));
        return;
    };

    let scale = (system_dpi() as f32 / 96.0).max(0.01);
    // 与 .slint 的 min-width/min-height 一致；上限留 16px 给屏幕边缘。
    let max_w = (area_w as f32 / scale - 16.0).max(1.0);
    let max_h = (area_h as f32 / scale - 16.0).max(1.0);
    let w = size.0.clamp(960.0_f32.min(max_w), max_w);
    let h = size.1.clamp(600.0_f32.min(max_h), max_h);

    window.set_size(slint::LogicalSize::new(w, h));

    let phys_w = (w * scale).round() as i32;
    let phys_h = (h * scale).round() as i32;
    window.set_position(slint::PhysicalPosition::new(
        wa_x + (area_w as i32 - phys_w).max(0) / 2,
        wa_y + (area_h as i32 - phys_h).max(0) / 2,
    ));
}

#[cfg(not(windows))]
pub(crate) fn place_main_window_before_show(win: &AppWindow, size: (f32, f32)) {
    // 其它平台的位置交给窗口管理器，这里只钉尺寸。
    win.window().set_size(slint::LogicalSize::new(size.0, size.1));
}

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
    let term_x = term.x;
    let term_y = term.y;
    let mut term_w = term.w;
    let mut term_h = term.h;

    // 状态行已移除（`ui/terminal_view.slint`）：dock-region 直接从 pane 顶边开始。

    // SFTP 面板固定在右侧：终端可用宽度直接扣掉它（折叠时只剩 36px 折叠条）。
    // 非 SFTP 会话不扣 —— 与 Slint 的 `sf-taken` 门控保持一致。
    // 面板与终端之间没有间隙（分隔条压在终端的右内边距上），所以不额外加常数。
    let sftp_take = if !term_state.overview_available {
        0.0
    } else if term_state.overview_collapsed {
        36.0
    } else {
        term_state.overview_panel_width
    };
    term_w = (term_w - sftp_take).max(0.0);

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

pub(crate) fn contains_logical(rect: LogicalRect, x: f32, y: f32) -> bool {
    x >= rect.x && x <= rect.x + rect.w && y >= rect.y && y <= rect.y + rect.h
}


pub(crate) fn app_content_area(win: &AppWindow) -> LogicalRect {
    let size = win.window().size();
    let scale = win.window().scale_factor().max(0.01);
    // 高度从 Theme 读，不在 Rust 里留第二份字面量。
    let titlebar = win.global::<Theme>().get_titlebar_height();
    let mut area = LogicalRect {
        x: 0.0,
        y: if win.get_custom_titlebar() {
            titlebar
        } else if win.get_is_mac() {
            28.0
        } else {
            0.0
        },
        w: size.width as f32 / scale,
        h: 0.0,
    };
    area.h = size.height as f32 / scale - area.y;

    // 左侧导航在 dock-area 之外固定占左缘，先扣掉它的宽（展开 200 / 收起 54）。
    let nav = win.get_nav_width();
    area.x += nav;
    area.w = (area.w - nav).max(0.0);

    // 快捷命令面板固定在右侧。
    if win.get_quick_panel_open() {
        let quick_take = if win.get_quick_panel_collapsed() {
            36.0
        } else {
            win.get_quick_panel_width() + 2.0
        };
        area.w = (area.w - quick_take).max(0.0);
    }
    area
}

pub(crate) fn active_terminal_panel_rects(win: &AppWindow) -> Option<(String, LogicalRect, TerminalState)> {
    let active = win.get_active_tab_id().to_string();
    if active.is_empty() || active == "welcome" {
        return None;
    }
    // Docker / 笔记页整页接管内容区：标签栏与终端都已隐藏，算出来的矩形是错的。
    // 不拦会让该页的滚轮喂给看不见的终端、拖文件进窗口触发一次不可见的上传。
    if matches!(win.get_nav_page(), 3 | 5) {
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
            // 标签栏 36px（`ui/tabs.slint` 的 height），不是旧的 40（36 + 4）。
            y: area.y + pane.y + 36.0,
            w: pane.w,
            h: (pane.h - 36.0).max(0.0),
        },
        term_state,
    ))
}

/// 当前标签页 SFTP 文件列表在窗口内的逻辑矩形（拖放落点判定用）。
///
/// 纯 Slint 模型几何，与平台无关 —— 之前只被 Windows 的拖放处理器用到，才挂着
/// `#[cfg(windows)]` 躲 dead-code 警告；现在非 Windows 的拖放也走这条路径。
pub(crate) fn active_sftp_file_list_rect(win: &AppWindow) -> Option<LogicalRect> {
    // 状态页与隧道页没有文件列表，拖到这里不该当成上传落点。
    if win.get_overview_mode().as_str() != "sftp" {
        return None;
    }
    let (_active, term, term_state) = active_terminal_panel_rects(win)?;
    if term_state.overview_collapsed {
        return None;
    }

    // 状态行已移除，dock-region 覆盖整个 term 矩形（与 ui/terminal_view.slint 一致）。
    let dock_region = LogicalRect {
        x: term.x,
        y: term.y,
        w: term.w,
        h: term.h,
    };
    // 面板固定在右侧、不显示目录树：占满 dock-region 的高度，宽度取面板宽。
    let mut panel = LogicalRect {
        x: dock_region.x + (dock_region.w - term_state.overview_panel_width).max(0.0),
        y: dock_region.y,
        w: term_state.overview_panel_width,
        h: dock_region.h,
    };

    // OverviewPanel layout: toolbar, then file headers 20 + separator 1.
    // 工具栏高度随 `narrow-toolbar` 变化（`ui/sftp_panel.slint`）：宽度 < 480px
    // （含 255 下限）时是两行 62px，不是 34px。写死 34 会让落点偏高 28px。
    let toolbar_h = if term_state.overview_panel_width < 480.0 {
        62.0
    } else {
        34.0
    };
    panel.y += toolbar_h + 20.0 + 1.0;
    panel.h = (panel.h - toolbar_h - 20.0 - 1.0).max(0.0);
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
    _hovered_pos: Option<(f32, f32)>,
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
    let point = _hovered_pos;

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
