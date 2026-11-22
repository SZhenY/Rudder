use super::*;
use crate::ui::{ Theme };

/// 预设跨度（秒），索引与 overview 卡片里的下拉一致。
pub(crate) const SPAN_SECS: [u32; 6] = [15, 60, 300, 1800, 3600, 21600];

/// 0 表示「未设置」，回落到 1 分钟。
pub(crate) fn span_or_default(secs: u32) -> u32 {
    if SPAN_SECS.contains(&secs) { secs } else { 60 }
}

/// 跨度 → 下拉索引（0..5）。
pub(crate) fn span_index(secs: u32) -> i32 {
    SPAN_SECS
        .iter()
        .position(|v| *v == span_or_default(secs))
        .unwrap_or(1) as i32
}

/// 每条曲线自己的采样步长（秒）：跨度越大越稀，点数恒定 ≤ 60。
///
/// 两卡独立——CPU 切 15 秒只改自己的步长，网络仍按原步长累积。
pub(crate) fn card_step(span_secs: u32) -> u32 {
    match span_or_default(span_secs) {
        15 => 1,
        60 => 2,
        300 => 5,
        1800 => 30,
        3600 => 60,
        _ => 360,
    }
}

/// 一条曲线的窗口点数：15 / 30 / 60 / 60 / 60 / 60。
pub(super) fn card_points(span_secs: u32) -> usize {
    (span_or_default(span_secs) / card_step(span_secs)) as usize
}

/// 远端采样基准：取三条曲线里最快的一档（与原来等价，不额外增加远端开销）。
pub(crate) fn base_step(cpu_span: u32, net_span: u32, mem_span: u32) -> u32 {
    card_step(cpu_span)
        .min(card_step(net_span))
        .min(card_step(mem_span))
}

pub(super) fn push_ring(buf: &mut Vec<f32>, val: f32, cap: usize) {
    if buf.len() != cap {
        // 用**首值**回填而不是 0：`smooth_path` 会把缓冲重采样成固定 60 点，
        // 若前半段是 0，真实波动只剩右端一小截、看起来像贴底直线。
        *buf = vec![val; cap];
    }
    // `remove(0)` would shrink and re-grow the Vec on every sample; shifting
    // in place keeps the length (and the allocation) untouched.
    buf.copy_within(1.., 0);
    if let Some(last) = buf.last_mut() {
        *last = val;
    }
}

/// 把样本画成「中点二次贝塞尔」的 SVG path，坐标系固定 0..100。
///
/// 节点取相邻采样点的中点、数据点只作控制点 —— 直连数据点的折线太生硬，这样
/// 曲率柔和又不会过冲。Slint 的 `Path` 不支持 `for`（旧分支因此展开写死 61 段），
/// 所以这里在 Rust 侧拼 `commands` 字符串，点数可调。
///
/// 输入按 0..1 处理（CPU 百分比先除 100）；末尾收口到基线形成面积填充。
pub(super) fn smooth_path(values: &[f32], npts: usize) -> String {
    if values.is_empty() {
        return String::new();
    }
    // 禁止升采样：历史点数少于目标分辨率时按实际点数画。硬拉到 60 点会让相邻 x
    // 重复同一个值，曲线上出现「平台 + 陡降」的台阶（15 秒档最明显）。
    let n = npts.min(values.len()).max(2);
    let take = values.len().min(n);
    let offset = values.len() - take;
    // 线性插值重采样：n == take 时取原值；降采样时按小数部分混合相邻两点。
    let sample = |k: usize| -> f32 {
        if take == 1 {
            return values[offset];
        }
        let t = k as f32 * (take - 1) as f32 / (n - 1) as f32;
        let i = (t.floor() as usize).min(take - 1);
        let f = t - i as f32;
        let a = values[offset + i];
        let b = values[offset + (i + 1).min(take - 1)];
        a + (b - a) * f
    };
    let vx = |k: usize| -> f32 { k as f32 * 100.0 / (n - 1) as f32 };
    let vy = |v: f32| -> f32 { 100.0 * (1.0 - v.clamp(0.0, 1.0)) };
    let mx = |k: usize| -> f32 { (vx(k) + vx(k + 1)) / 2.0 };
    let my = |k: usize| -> f32 { (vy(sample(k)) + vy(sample(k + 1))) / 2.0 };

    // 两个隐形锚点把 Path 的包围盒钉在 0..100 方框：`fit: fill` 会按包围盒
    // 等比/非等比缩放，没有锚点时「一直在 40..60 之间」的曲线会被放大成满屏。
    let mut out = String::from("M 0.00 0.00 L 0.01 0.00 ");
    out.push_str(&format!("M {:.2} {:.2}", mx(0), my(0)));
    for k in 0..n - 2 {
        out.push_str(&format!(
            " Q {:.2} {:.2} {:.2} {:.2}",
            vx(k + 1),
            vy(sample(k + 1)),
            mx(k + 1),
            my(k + 1)
        ));
    }
    let last = vy(sample(n - 1));
    out.push_str(&format!(" L 100.00 {last:.2} L 100.00 100.00 L 0.00 100.00 Z"));
    out
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

/// 挂载点过滤串（空格 / 逗号 / 分号分隔）→ 白名单；空串表示不过滤。
fn mount_filters(mount_filter: &str) -> Vec<&str> {
    if mount_filter.is_empty() {
        return Vec::new();
    }
    mount_filter
        .split([' ', ',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// 一行磁盘是否要显示：隐藏伪文件系统与用户指定挂载点。
fn disk_kept(mount: &str, filters: &[&str], hide_special: bool) -> bool {
    if hide_special && is_special_partition(mount) {
        return false;
    }
    filters.is_empty() || filters.contains(&mount)
}

pub(super) fn disk_rows(
    disks: &[(String, u64, u64)],
    mount_filter: &str,
    hide_special: bool,
) -> Vec<DiskInfo> {
    let filters = mount_filters(mount_filter);
    disks
        .iter()
        .filter(|(mount, _, _)| disk_kept(mount, &filters, hide_special))
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

/// 系统盘用量：优先挂载点 `/`，否则取过滤后的第一块盘。
/// 返回 `(percent, used_bytes, total_bytes)`；没有可用行时 `None`。
pub(super) fn system_disk_usage(
    disks: &[(String, u64, u64)],
    mount_filter: &str,
    hide_special: bool,
) -> Option<(f32, u64, u64)> {
    let filters = mount_filters(mount_filter);
    let kept = |mount: &str| disk_kept(mount, &filters, hide_special);
    let (_, avail, total) = disks
        .iter()
        .find(|(mount, _, _)| mount == "/" && kept(mount))
        .or_else(|| disks.iter().find(|(mount, _, _)| kept(mount)))?;
    let used = total.saturating_sub(*avail);
    let percent = if *total > 0 {
        used as f32 / *total as f32
    } else {
        0.0
    };
    Some((percent, used, *total))
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

pub(super) fn sync_proc_theme(main: &AppWindow, proc: &ProcWindow) {
    let t = main.global::<Theme>();
    proc.set_dark_mode(t.get_dark());
    proc.set_ui_scale(t.get_ui_scale());
    proc.global::<Theme>().set_ui_font_px(t.get_ui_font_px());
    proc.set_ui_font_family(t.get_ui_font_family());
    // Mirror the immersive wallpaper so the detached window shares the frosted
    // backdrop instead of a flat panel.
    proc.set_wallpaper_img(t.get_wallpaper());
    proc.set_wallpaper_active(t.get_wallpaper_active());
    proc.set_wp_accent(t.get_wp_accent());
    proc.set_wp_tint(t.get_wp_tint());
    proc.set_wp_base(t.get_wp_base());
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

pub(super) fn place_child_window(main: &AppWindow, target: &slint::Window) {
    use i_slint_backend_winit::winit::dpi::PhysicalPosition;

    let monitor = main
        .window()
        .with_winit_window(|ww| ww.current_monitor().or_else(|| ww.primary_monitor()))
        .flatten();
    let Some(monitor) = monitor else { return };
    let origin = monitor.position();
    let monitor_size = monitor.size();

    target.with_winit_window(|ww| {
        let window_size = ww.outer_size();
        let x = origin.x + monitor_size.width.saturating_sub(window_size.width) as i32 / 2;
        let y = origin.y + monitor_size.height.saturating_sub(window_size.height) as i32 / 2;
        ww.set_outer_position(PhysicalPosition::new(x, y));
    });
}

/// 日志窗只同步尺寸与等宽字体，配色固定（黑底浅字）。
pub(super) fn sync_log_metrics(main: &AppWindow, log: &crate::ui::DockerLogWindow) {
    let t = main.global::<Theme>();
    log.set_ui_scale(t.get_ui_scale());
    log.set_term_font(t.get_term_font_family());
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
