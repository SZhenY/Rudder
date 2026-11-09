use super::*;
use crate::ui::{ Theme };

pub(super) fn push_ring(buf: &mut Vec<f32>, val: f32) {
    if buf.len() != NET_HISTORY_LEN {
        *buf = vec![0.0; NET_HISTORY_LEN];
    }
    buf.remove(0);
    buf.push(val);
}

/// 折线图归一化：除以峰值（峰值从 1.0 起算，所以低流量不会被拉满整图）。
pub(super) fn normalize(buf: &[f32]) -> Vec<f32> {
    let max = buf.iter().cloned().fold(1.0_f32, f32::max);
    buf.iter().map(|v| (v / max).clamp(0.0, 1.0)).collect()
}

/// 归一化结果对应的**新**模型 —— 只在第一次（或模型被别人换过）时用一次。
/// （名字里的 graph 是为了避开 `history_model`：那是命令历史用的，glob 导入后会歧义。）
pub(super) fn graph_model(scaled: &[f32]) -> ModelRc<f32> {
    ModelRc::from(Rc::new(VecModel::from(scaled.to_vec())))
}

/// 把算好的一批行**增量**写进模型：逐行比对内容，只更新真正变化的行，长度变化时才在
/// 尾部 push / remove。
///
/// 系统信息面板原来每轮刷新都是 `set_vec` 整表替换 —— Repeater 会把每一行都当成新行
/// 重建（行内状态、悬停、动画全部重来）。这些面板是几秒一轮的定时刷新，内容多数时候
/// 没变，增量写法把这份重复工作去掉。
///
/// 注意：行数**从尾部**增减，中间插队（排序/过滤变化）会退化成逐行更新 —— 结果是
/// 正确的，只是没有增量收益。用 `set_row_data` 而非重建，也顺带避免了整块重绘。
pub(super) fn apply_rows<T: Clone + PartialEq + 'static>(vm: &VecModel<T>, next: Vec<T>) {
    apply_rows_slice(vm, &next);
}

/// `apply_rows` 的借用版：调用方只有 `&[T]` 时用（拿所有权会让每个调用点都得 clone 一遍）。
/// 返回"是否真的写了东西"—— 内容完全没变时一次通知都不发。
pub(crate) fn apply_rows_slice<T: Clone + PartialEq + 'static>(vm: &VecModel<T>, next: &[T]) -> bool {
    let old_len = vm.row_count();
    let new_len = next.len();
    let mut changed = old_len != new_len;

    for (i, item) in next.iter().enumerate().take(old_len.min(new_len)) {
        if vm.row_data(i).as_ref() != Some(item) {
            vm.set_row_data(i, item.clone());
            changed = true;
        }
    }
    for item in &next[old_len.min(new_len)..] {
        vm.push(item.clone());
    }
    for i in (new_len..old_len).rev() {
        vm.remove(i);
    }
    changed
}

/// 把 `next` **增量写进 model 背后那个 `VecModel`** —— Repeater 因此复用已有的项，而不是把
/// 每一项都当成新的重建（每帧 `VecModel::from(...)` + 整体替换就是这个代价）。
///
/// 返回 `false` 表示这个 model 不是 `VecModel`（首次调用，或被别人换过）：调用方应新建一个。
/// 把 `next` 增量写进 `model` 背后那个 `VecModel`，并报出**内容是否变化**
/// （`None` = 模型不可复用，调用方应新建；`Some(false)` = 内容完全没变）。
/// 调用方据此决定要不要 `request_redraw()` —— 一次重绘会让 Slint 重画整个窗口。
pub(super) fn try_write_rows_changed<T: Clone + PartialEq + 'static>(
    model: &ModelRc<T>,
    next: &[T],
) -> Option<bool> {
    let vm = model.as_any().downcast_ref::<VecModel<T>>()?;
    Some(apply_rows_slice(vm, next))
}

pub(super) fn disk_rows(
    disks: &[(String, u64, u64)],
    mount_filter: &str,
    hide_special: bool,
) -> Vec<DiskInfo> {
    let filters: Vec<&str> = if mount_filter.is_empty() {
        vec![]
    } else {
        mount_filter
            .split([' ', ',', ';'])
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect()
    };
    disks
        .iter()
        .filter(|(mount, _, _)| {
            // Hide pseudo-filesystems and tiny special partitions when enabled.
            if hide_special && is_special_partition(mount) {
                return false;
            }
            if filters.is_empty() {
                true
            } else {
                filters.contains(&mount.as_str())
            }
        })
        .map(|(mount, avail, total)| {
            let used = total.saturating_sub(*avail);
            let percent = if *total > 0 {
                used as f32 / *total as f32
            } else {
                0.0
            };
            DiskInfo {
                path: mount.clone().into(),
                detail: format!("{}/{}", format_size(*avail), format_size(*total)).into(),
                percent,
            }
        })
        .collect()
}

pub(super) fn disk_model(
    disks: &[(String, u64, u64)],
    mount_filter: &str,
    hide_special: bool,
) -> ModelRc<DiskInfo> {
    ModelRc::from(Rc::new(VecModel::from(disk_rows(
        disks,
        mount_filter,
        hide_special,
    ))))
}

pub(super) fn set_process_action_error(weak: &slint::Weak<ProcWindow>, message: &str) {
    if let Some(window) = weak.upgrade() {
        window.set_action_busy(false);
        window.set_action_error(true);
        window.set_action_status(message.into());
    }
}

pub(super) fn process_needs_root(current_user: &str, process_user: &str) -> bool {
    current_user != "root" && process_user != current_user
}

pub(super) fn proc_rows(procs: &[ProcInfo], current_user: &str, tab_id: &str) -> Vec<ProcRow> {
    procs
        .iter()
        .map(|p| ProcRow {
            tab_id: tab_id.into(),
            pid: p.pid.to_string().into(),
            user: p.user.clone().into(),
            cpu: format!("{:.1}", p.cpu).into(),
            mem: format!("{:.1}", p.mem).into(),
            command: p.command.clone().into(),
            cpu_frac: (p.cpu / 100.0).clamp(0.0, 1.0),
            own_process: !process_needs_root(current_user, &p.user),
        })
        .collect()
}

pub(super) fn metric_rows(
    cpu: f32,
    mem: f32,
    swap: f32,
    mem_detail: impl Into<SharedString>,
    swap_detail: impl Into<SharedString>,
) -> Vec<SysMetricRow> {
    vec![
        SysMetricRow {
            label: "CPU".into(),
            percent: cpu,
            detail: "".into(),
        },
        SysMetricRow {
            label: t("内存", "Memory").into(),
            percent: mem,
            detail: mem_detail.into(),
        },
        SysMetricRow {
            label: t("交换", "Swap").into(),
            percent: swap,
            detail: swap_detail.into(),
        },
    ]
}

pub(super) fn net_rows(net: &[(String, u64, u64)]) -> Vec<SysNetRow> {
    net.iter()
        .map(|(name, rx, tx)| SysNetRow {
            name: name.clone().into(),
            up: format_bytes_per_sec(*tx).into(),
            down: format_bytes_per_sec(*rx).into(),
        })
        .collect()
}

pub(super) fn pairs_to_overview_rows(pairs: &[(String, String)]) -> Vec<SysInfoRow> {
    pairs
        .chunks(2)
        .map(|chunk| {
            let first = &chunk[0];
            let second = chunk.get(1);
            SysInfoRow {
                c1: first.0.clone().into(),
                c2: first.1.clone().into(),
                c3: second.map(|p| p.0.clone()).unwrap_or_default().into(),
                c4: second.map(|p| p.1.clone()).unwrap_or_default().into(),
                c5: "".into(),
            }
        })
        .collect()
}

pub(super) fn pairs_to_one_row(pairs: &[(String, String)]) -> Vec<SysInfoRow> {
    let value = |idx: usize| {
        pairs
            .get(idx)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "-".to_string())
    };
    vec![SysInfoRow {
        c1: value(0).into(),
        c2: value(1).into(),
        c3: value(2).into(),
        c4: value(3).into(),
        c5: value(4).into(),
    }]
}

pub(super) fn pairs_to_rows(pairs: &[(String, String)], width: usize) -> Vec<SysInfoRow> {
    pairs
        .chunks(width)
        .filter(|chunk| {
            chunk
                .iter()
                .any(|(_, v)| !v.trim().is_empty() && v.trim() != "-")
        })
        .map(|chunk| {
            let value = |idx: usize| {
                chunk
                    .get(idx)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_else(|| "-".to_string())
            };
            SysInfoRow {
                c1: value(0).into(),
                c2: value(1).into(),
                c3: value(2).into(),
                c4: value(3).into(),
                c5: value(4).into(),
            }
        })
        .collect()
}

pub(super) fn cpu_usage_detail_rows(pairs: &[(String, String)]) -> Vec<SysInfoRow> {
    let value = |idx: usize| {
        pairs
            .get(idx)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "0.0%".to_string())
    };
    let extra = pairs
        .iter()
        .skip(4)
        .map(|(k, v)| format!("{k} {v}"))
        .collect::<Vec<_>>()
        .join(" / ");
    vec![SysInfoRow {
        c1: value(0).into(),
        c2: value(2).into(),
        c3: value(1).into(),
        c4: value(3).into(),
        c5: extra.into(),
    }]
}

pub(super) fn tuple5_rows(rows: &[(String, String, String, String, String)]) -> Vec<SysInfoRow> {
    rows.iter()
        .map(|r| SysInfoRow {
            c1: r.0.clone().into(),
            c2: r.1.clone().into(),
            c3: r.2.clone().into(),
            c4: r.3.clone().into(),
            c5: r.4.clone().into(),
        })
        .collect()
}

pub(super) fn sync_proc_theme(main: &AppWindow, proc: &ProcWindow) {
    let t = main.global::<Theme>();
    proc.set_dark_mode(t.get_dark());
    proc.set_ui_scale(t.get_ui_scale());
    proc.set_ui_font_family(t.get_ui_font_family());
    // Mirror the immersive wallpaper so the detached window shares the frosted
    // backdrop instead of a flat panel.
    proc.set_wallpaper_img(t.get_wallpaper());
    proc.set_wallpaper_active(t.get_wallpaper_active());
    proc.set_wp_accent(t.get_wp_accent());
    proc.set_wp_tint(t.get_wp_tint());
    proc.set_wp_base(t.get_wp_base());
}

pub(super) fn sync_system_info_theme(main: &AppWindow, sys: &SystemInfoWindow) {
    let t = main.global::<Theme>();
    sys.set_dark_mode(t.get_dark());
    sys.set_ui_scale(t.get_ui_scale());
    sys.set_ui_font_family(t.get_ui_font_family());
    sys.set_wallpaper_img(t.get_wallpaper());
    sys.set_wallpaper_active(t.get_wallpaper_active());
    sys.set_wp_accent(t.get_wp_accent());
    sys.set_wp_tint(t.get_wp_tint());
    sys.set_wp_base(t.get_wp_base());
}

/// 让子窗口真正画出第一帧。
///
/// 根因：macOS 上新映射的第二个窗口**不会自动产生首次渲染事件**，而 Slint 的布局是
/// 「渲染时惰性计算」的 —— 于是一直没有内容，直到外部原因（用户拖动窗口边缘）送来一次
/// `Resized` 才补上首帧。现场表现就是"只有交通灯、没有内容，拉伸一下就出来了"。
///
/// 正解不是去动尺寸，而是**显式请求重绘**：`Window::request_redraw()` 就是 Slint 为
/// 此提供的 API。第一次请求可能早于窗口映射完成（winit 会丢弃未映射窗口的重绘请求），
/// 所以在随后的两个 tick 里各补一次；三次都只是"请画一帧"，不改尺寸、不看内容。
pub(super) fn request_first_frame(w: &slint::Window) {
    w.request_redraw();
}

pub(super) fn place_system_info_window(main: &AppWindow, sys: &SystemInfoWindow) {
    use i_slint_backend_winit::winit::dpi::LogicalPosition;

    let Some((mon_x, mon_y, mon_w, mon_h, scale)) = main
        .window()
        .with_winit_window(|ww| {
            let scale = ww.scale_factor().max(0.01);
            let monitor = ww.current_monitor().or_else(|| ww.primary_monitor())?;
            let pos = monitor.position();
            let size = monitor.size();
            Some((
                pos.x as f64 / scale,
                pos.y as f64 / scale,
                size.width as f64 / scale,
                size.height as f64 / scale,
                scale,
            ))
        })
        .flatten()
    else {
        return;
    };

    let target_w = (mon_w * 0.5).clamp(760.0, (mon_w - 24.0).max(760.0));
    let target_h = (mon_h * 0.5).clamp(520.0, (mon_h - 24.0).max(520.0));
    let x = mon_x + (mon_w - target_w).max(0.0) / 2.0;
    let y = mon_y + (mon_h - target_h).max(0.0) / 2.0;

    // 位置：只能走 winit（Slint 没有设置窗口位置的 API）。移动不涉及重排，安全。
    sys.window().with_winit_window(|ww| {
        ww.set_outer_position(LogicalPosition::new(x, y));
    });
    // 尺寸：走 **Slint 自己的 API**。用 `with_winit_window(request_inner_size)` 改尺寸是
    // 在 Slint 背后动 OS 窗口，首帧布局会被跳过（现场表现：空白窗口，拖动边缘才有内容）。
    let _ = scale;
    sys.window().set_size(slint::PhysicalSize::new(
        (target_w * scale) as u32,
        (target_h * scale) as u32,
    ));
}

pub(super) fn place_process_window(main: &AppWindow, process: &ProcWindow) {
    use i_slint_backend_winit::winit::dpi::PhysicalPosition;

    let monitor = main
        .window()
        .with_winit_window(|ww| ww.current_monitor().or_else(|| ww.primary_monitor()))
        .flatten();
    let Some(monitor) = monitor else { return };
    let origin = monitor.position();
    let monitor_size = monitor.size();

    process.window().with_winit_window(|ww| {
        let window_size = ww.outer_size();
        let x = origin.x + monitor_size.width.saturating_sub(window_size.width) as i32 / 2;
        let y = origin.y + monitor_size.height.saturating_sub(window_size.height) as i32 / 2;
        ww.set_outer_position(PhysicalPosition::new(x, y));
    });
}

/// 设置窗口居中：优先与主窗口同心，主窗口贴边放不下时退回屏幕正中。
/// 按物理像素算（`outer_size` / `outer_position` 同一坐标系），跨 DPI 屏也成立。
pub(super) fn place_settings_window(main: &AppWindow, sw: &SettingsWindow) {
    use i_slint_backend_winit::winit::dpi::PhysicalPosition;

    const EDGE: i32 = 8;

    // 主窗口外框 + 所在屏幕；最小化后 `outer_position` 是哨兵值，跳过。
    // 闭包不能把借用带出来 → 只取数字。
    let Some((parent, monitor)) = main.window().with_winit_window(|ww| {
        let parent = if ww.is_minimized().unwrap_or(false) {
            None
        } else {
            ww.outer_position().ok().map(|p| {
                let size = ww.outer_size();
                (p.x, p.y, size.width as i32, size.height as i32)
            })
        };
        let monitor = ww
            .current_monitor()
            .or_else(|| ww.primary_monitor())
            .map(|m| {
                let pos = m.position();
                let size = m.size();
                (pos.x, pos.y, size.width as i32, size.height as i32)
            });
        (parent, monitor)
    }) else {
        return;
    };
    let Some((mon_x, mon_y, mon_w, mon_h)) = monitor else {
        return;
    };

    sw.window().with_winit_window(|ww| {
        let size = ww.outer_size();
        let (w, h) = (size.width as i32, size.height as i32);
        // 屏幕正中；屏幕比窗口还小时贴左上角，不做负偏移。
        let fallback = (
            mon_x + (mon_w - w).max(0) / 2,
            mon_y + (mon_h - h).max(0) / 2,
        );
        let (x, y) = match parent {
            Some((px, py, pw, ph)) => {
                let (cx, cy) = (px + (pw - w) / 2, py + (ph - h) / 2);
                let inside = cx >= mon_x + EDGE
                    && cy >= mon_y + EDGE
                    && cx + w <= mon_x + mon_w - EDGE
                    && cy + h <= mon_y + mon_h - EDGE;
                if inside { (cx, cy) } else { fallback }
            }
            None => fallback,
        };
        // 位置只能走 winit（Slint 没有这个 API）。
        ww.set_outer_position(PhysicalPosition::new(x, y));
    });
}
#[cfg(test)]
#[path = "resource_ui_tests.rs"]
mod resource_ui_tests_aggregate;
