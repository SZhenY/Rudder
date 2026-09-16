use super::*;
use crate::ui::{ Theme };

pub(super) fn push_ring(buf: &mut Vec<f32>, val: f32) {
    if buf.len() != NET_HISTORY_LEN {
        *buf = vec![0.0; NET_HISTORY_LEN];
    }
    buf.remove(0);
    buf.push(val);
}

pub(super) fn normalized_model(buf: &[f32]) -> ModelRc<f32> {
    let max = buf.iter().cloned().fold(1.0_f32, f32::max);
    let scaled: Vec<f32> = buf.iter().map(|v| (v / max).clamp(0.0, 1.0)).collect();
    ModelRc::from(Rc::new(VecModel::from(scaled)))
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
    proc.set_dark_mode(main.global::<Theme>().get_dark());
    proc.set_ui_scale(main.global::<Theme>().get_ui_scale());
    proc.set_ui_font_family(main.global::<Theme>().get_ui_font_family());
    // Mirror the immersive wallpaper so the detached window shares the frosted
    // backdrop instead of a flat panel.
    proc.set_wallpaper_img(main.global::<Theme>().get_wallpaper());
    proc.set_wallpaper_active(main.global::<Theme>().get_wallpaper_active());
    proc.set_wp_accent(main.global::<Theme>().get_wp_accent());
    proc.set_wp_tint(main.global::<Theme>().get_wp_tint());
}

pub(super) fn sync_system_info_theme(main: &AppWindow, sys: &SystemInfoWindow) {
    sys.set_dark_mode(main.global::<Theme>().get_dark());
    sys.set_ui_scale(main.global::<Theme>().get_ui_scale());
    sys.set_ui_font_family(main.global::<Theme>().get_ui_font_family());
    sys.set_wallpaper_img(main.global::<Theme>().get_wallpaper());
    sys.set_wallpaper_active(main.global::<Theme>().get_wallpaper_active());
    sys.set_wp_accent(main.global::<Theme>().get_wp_accent());
    sys.set_wp_tint(main.global::<Theme>().get_wp_tint());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(xs: &[(&str, &str)]) -> Vec<(String, String)> {
        xs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    // ---------- push_ring：网络历史环形缓冲 ----------

    #[test]
    fn push_ring_shifts_left_and_appends() {
        let mut buf: Vec<f32> = (0..NET_HISTORY_LEN).map(|i| i as f32).collect();
        push_ring(&mut buf, 42.0);
        assert_eq!(buf.len(), NET_HISTORY_LEN, "长度恒定");
        assert_eq!(buf[0], 1.0, "最旧的一个（0.0）被挤掉");
        assert_eq!(*buf.last().unwrap(), 42.0, "新值进队尾");
    }

    /// 长度不对时**整体重建为 0**（而不是补零到能放下）—— 换网卡 / 新会话都靠这条兜底。
    #[test]
    fn push_ring_reinitializes_wrong_sized_input() {
        let mut buf = vec![9.0; 3];
        push_ring(&mut buf, 1.5);
        assert_eq!(buf.len(), NET_HISTORY_LEN);
        assert_eq!(buf[NET_HISTORY_LEN - 1], 1.5);
        assert!(
            buf[..NET_HISTORY_LEN - 1].iter().all(|v| *v == 0.0),
            "旧值不能残留"
        );
    }

    // ---------- normalized_model：折线图归一化 ----------

    #[test]
    fn normalized_model_scales_by_max() {
        let m = normalized_model(&[2.0, 4.0]);
        assert_eq!(m.row_count(), 2);
        assert_eq!(m.row_data(0).unwrap(), 0.5);
        assert_eq!(m.row_data(1).unwrap(), 1.0);
    }

    /// 峰值不超过 1.0 时**不放大**（`max` 从 1.0 起算）：低流量不该被拉满整个图。
    #[test]
    fn normalized_model_does_not_amplify_sub_unit_values() {
        let m = normalized_model(&[0.25, 1.0]);
        assert_eq!(m.row_data(0).unwrap(), 0.25, "小值保持原样");
    }

    #[test]
    fn normalized_model_clamps_negatives_and_tolerates_empty() {
        let m = normalized_model(&[-3.0, 0.0]);
        assert_eq!(m.row_data(0).unwrap(), 0.0, "负数压到 0");
        assert_eq!(normalized_model(&[]).row_count(), 0, "空输入不 panic");
    }

    // ---------- disk_rows ----------

    fn disks() -> Vec<(String, u64, u64)> {
        vec![
            ("/".to_string(), 50, 100),   // 用了一半
            ("/proc".to_string(), 0, 0),  // 伪文件系统（total 为 0）
            ("/data".to_string(), 1, 4),  // 已用 75%
        ]
    }

    #[test]
    fn disk_rows_computes_usage_and_detail() {
        let rows = disk_rows(&disks(), "", false);
        assert_eq!(rows.len(), 3, "空过滤 = 全部");
        assert_eq!(rows[0].path.as_str(), "/");
        assert_eq!(rows[0].percent, 0.5);
        assert_eq!(rows[2].percent, 0.75);
        assert_eq!(
            rows[0].detail.as_str(),
            format!("{}/{}", format_size(50), format_size(100)),
            "detail 是「可用/总量」"
        );
    }

    /// `total == 0` 不能做除数（伪文件系统就长这样）→ percent 取 0。
    #[test]
    fn disk_rows_handles_zero_total() {
        let rows = disk_rows(&[("/proc".to_string(), 0, 0)], "", false);
        assert_eq!(rows[0].percent, 0.0);
    }

    /// 过滤串是手输的：按空格 / 逗号 / 分号切，空段忽略。
    #[test]
    fn disk_rows_splits_the_mount_filter() {
        let rows = disk_rows(&disks(), " /data ,  ; ", false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path.as_str(), "/data");
    }

    #[test]
    fn disk_rows_hides_special_partitions_when_asked() {
        let rows = disk_rows(&disks(), "", true);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.path.as_str() != "/proc"));
    }

    // ---------- 进程列表 ----------

    #[test]
    fn process_needs_root_only_for_someone_elses_process() {
        assert!(!process_needs_root("root", "alice"), "root 不需要提权");
        assert!(!process_needs_root("alice", "alice"), "自己的进程");
        assert!(process_needs_root("alice", "root"), "别人的进程");
    }

    #[test]
    fn proc_rows_formats_and_clamps() {
        let procs = vec![ProcInfo {
            pid: 42,
            user: "alice".into(),
            cpu: 150.0, // 多核可以超 100%
            mem: 12.34,
            command: "vim".into(),
        }];
        let rows = proc_rows(&procs, "alice", "tab-1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tab_id.as_str(), "tab-1");
        assert_eq!(rows[0].pid.as_str(), "42");
        assert_eq!(rows[0].cpu.as_str(), "150.0");
        assert_eq!(rows[0].mem.as_str(), "12.3");
        assert_eq!(rows[0].cpu_frac, 1.0, "进度条比例必须夹在 0..1");
        assert!(rows[0].own_process, "自己的进程可直接操作");
    }

    // ---------- 系统信息窗口的行映射 ----------

    /// CPU / 内存 / 交换：固定三行、顺序固定，CPU 行不带 detail。
    #[test]
    fn metric_rows_keeps_three_fixed_rows() {
        let rows = metric_rows(12.5, 30.0, 1.0, "8/16 GB", "0/2 GB");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].label.as_str(), "CPU");
        assert_eq!(rows[0].percent, 12.5);
        assert!(rows[0].detail.is_empty());
        assert_eq!(rows[1].detail.as_str(), "8/16 GB");
        assert_eq!(rows[2].detail.as_str(), "0/2 GB");
    }

    /// **上下行不能接反**：`up` 取 tx、`down` 取 rx。
    #[test]
    fn net_rows_maps_tx_to_up_and_rx_to_down() {
        let rows = net_rows(&[("eth0".to_string(), 1024, 2048)]);
        assert_eq!(rows[0].name.as_str(), "eth0");
        assert_eq!(rows[0].up.as_str(), format_bytes_per_sec(2048));
        assert_eq!(rows[0].down.as_str(), format_bytes_per_sec(1024));
    }

    /// 概览：**两对一行**的四列；奇数个时末行的 c3/c4 留空（不是 "-"）。
    #[test]
    fn overview_rows_pair_two_pairs_per_row() {
        let rows = pairs_to_overview_rows(&pairs(&[("a", "1"), ("b", "2"), ("c", "3")]));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].c1.as_str(), "a");
        assert_eq!(rows[0].c4.as_str(), "2");
        assert_eq!(rows[0].c5.as_str(), "", "第五列不用");
        assert_eq!(rows[1].c1.as_str(), "c");
        assert_eq!(rows[1].c3.as_str(), "");
        assert_eq!(rows[1].c4.as_str(), "");
    }

    #[test]
    fn one_row_pads_missing_values_with_dash() {
        let rows = pairs_to_one_row(&pairs(&[("a", "1"), ("b", "2")]));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].c2.as_str(), "2");
        assert_eq!(rows[0].c3.as_str(), "-");
        assert_eq!(rows[0].c5.as_str(), "-");
    }

    /// 按 `width` 分块；**整块都是空值 / "-" 的直接丢掉**（否则界面上多出空白行）。
    /// ⚠️ `width` 必须 ≥ 1（`chunks(0)` 会 panic）；现有调用点传的是常量 4。
    #[test]
    fn rows_by_width_drops_blank_chunks_and_pads() {
        let data = pairs(&[
            ("1", "a"),
            ("2", "b"),
            ("3", "c"),
            ("4", "d"),
            ("5", ""),
            ("6", "-"),
            ("7", "   "),
            ("8", ""),
        ]);
        let rows = pairs_to_rows(&data, 4);
        assert_eq!(rows.len(), 1, "第二块全是空值 → 丢弃");
        assert_eq!(rows[0].c4.as_str(), "d");

        let short = pairs_to_rows(&pairs(&[("1", "a")]), 4);
        assert_eq!(short[0].c1.as_str(), "a");
        assert_eq!(short[0].c2.as_str(), "-", "块内缺位补 '-'");
        assert_eq!(short[0].c5.as_str(), "-");
    }

    /// CPU 详情行的**列序是刻意错位的**（c1←1、c2←3、c3←2、c4←4），第 5 列起拼 "k v / k v"。
    /// 这条测试就是为了钉死这个顺序 —— 调换即等于错标数据。
    #[test]
    fn cpu_detail_rows_reorder_columns_and_append_extras() {
        let rows = cpu_usage_detail_rows(&pairs(&[
            ("usr", "10%"),
            ("sys", "20%"),
            ("nice", "30%"),
            ("idle", "40%"),
            ("iowait", "5%"),
            ("irq", "1%"),
        ]));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].c1.as_str(), "10%");
        assert_eq!(rows[0].c2.as_str(), "30%", "第三项放第二列");
        assert_eq!(rows[0].c3.as_str(), "20%", "第二项放第三列");
        assert_eq!(rows[0].c4.as_str(), "40%");
        assert_eq!(rows[0].c5.as_str(), "iowait 5% / irq 1%");
    }

    #[test]
    fn cpu_detail_rows_default_missing_values_to_zero_percent() {
        let rows = cpu_usage_detail_rows(&pairs(&[("usr", "10%")]));
        assert_eq!(rows[0].c1.as_str(), "10%");
        assert_eq!(rows[0].c2.as_str(), "0.0%", "缺位用 0.0% 而不是 '-'");
        assert_eq!(rows[0].c5.as_str(), "", "没有额外项");
    }

    #[test]
    fn tuple5_rows_maps_straight_through() {
        let rows = tuple5_rows(&[(
            "a".into(),
            "b".into(),
            "c".into(),
            "d".into(),
            "e".into(),
        )]);
        let r = &rows[0];
        assert_eq!(
            (
                r.c1.as_str(),
                r.c2.as_str(),
                r.c3.as_str(),
                r.c4.as_str(),
                r.c5.as_str()
            ),
            ("a", "b", "c", "d", "e")
        );
    }
}
