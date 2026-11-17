use super::*;

/// 这一趟刷新在**模型**上做了哪些动作 —— 只用于给 `RUST_LOG=rudder::perf=debug` 记账。
///
/// `replaced` = "模型身份被换掉"的次数：那会让 Slint 把对应 Repeater 的每一项都当新项
/// 重建（行内状态、悬停、动画全丢），所以目标是**常年为 0**；`written` / `unchanged` 是
/// 就地写的结果 —— 就地写不换身份，Slint 只收到真正变化的行。
#[derive(Default, Clone, Copy)]
pub(crate) struct SidebarStats {
    pub(crate) replaced: u32,
    pub(crate) written: u32,
    pub(crate) unchanged: u32,
}

/// 就地写模型并记账；拿不到 `VecModel`（首次 / 被别人换过）时才用 `fresh` 新建一个 set 一次。
fn write_model<T: Clone + PartialEq + 'static>(
    stats: &mut SidebarStats,
    model: &ModelRc<T>,
    next: &[T],
    fresh: impl FnOnce() -> ModelRc<T>,
    set: impl FnOnce(ModelRc<T>),
) {
    match try_write_rows_changed(model, next) {
        Some(true) => stats.written += 1,
        Some(false) => stats.unchanged += 1,
        None => {
            set(fresh());
            stats.replaced += 1;
        }
    }
}

/// 磁盘列表：能就地写就就地写 —— 磁盘 10 秒才真刷新一次，中间那些 tick 内容没变，
/// 一次通知都不该发（原来每秒整张重建 + 换模型，列表里每一行都跟着重建）。
fn write_disks(stats: &mut SidebarStats, win: &AppWindow, disks: &[(String, u64, u64)]) {
    let rows = disk_rows(disks, &mount_filter(), hide_special_partitions());
    write_model(
        stats,
        &win.get_disks(),
        &rows,
        || disk_model(disks, &mount_filter(), hide_special_partitions()),
        |m| win.set_disks(m),
    );
}



/// 清空综合页头卡与 CPU 趋势。幂等：值已经是空的时候不再写属性，避免每帧
/// 重建 `VecModel`（换模型会让 Repeater 重建整条曲线）。
fn clear_host_card(win: &AppWindow) {
    if !win.get_host_name().is_empty()
        || !win.get_host_ip().is_empty()
        || !win.get_host_os().is_empty()
        || !win.get_host_kernel().is_empty()
        || !win.get_host_uptime().is_empty()
        || !win.get_host_load().is_empty()
    {
        win.set_host_name("".into());
        win.set_host_ip("".into());
        win.set_host_os("".into());
        win.set_host_kernel("".into());
        win.set_host_uptime("".into());
        win.set_host_load("".into());
    }
    if !win.get_cpu_path().is_empty() {
        win.set_cpu_path("".into());
    }
    if !win.get_mem_path().is_empty() {
        win.set_mem_path("".into());
    }
    if !win.get_net_top_path().is_empty() {
        win.set_net_top_path("".into());
    }
    if !win.get_net_up_path().is_empty() {
        win.set_net_up_path("".into());
    }
}

fn clear_stats(win: &AppWindow) {
    clear_host_card(win);
    win.set_cpu_percent(0.0);
    win.set_mem_percent(0.0);
    win.set_swap_percent(0.0);
    win.set_mem_detail("".into());
    win.set_swap_detail("".into());
}

/// Process monitor (#23) lives in a shared model (the AppWindow and the
/// detachable ProcWindow point at the same VecModel), so mutate it in place
/// instead of replacing it — replacing would break the sharing.
fn set_procs(win: &AppWindow, procs: &[ProcInfo], current_user: &str, tab_id: &str) {
    if let Some(vm) = win
        .get_proc_list()
        .as_any()
        .downcast_ref::<VecModel<ProcRow>>()
    {
        apply_rows(vm, proc_rows(procs, current_user, tab_id));
    }
}

/// 综合页能否撤掉加载态：只有「连接中」返回 false，其余一律 true。
/// 宁可显示 "—" 卡片，也不无限转圈。
fn overview_ready(st: &TabStatus) -> bool {
    if st.is_local {
        return true; // 本地 shell 不上报远端资源
    }
    match st.state {
        1 => {
            let sys_ok = !st.sys.host.name.is_empty() && st.mem_total_kib > 0;
            let timed_out = st
                .first_stats_at
                .is_some_and(|t| t.elapsed() > std::time::Duration::from_secs(5));
            sys_ok || timed_out
        }
        2 => true,
        _ => false,
    }
}

thread_local! {
    // 每个标签最近一次已应用到 UI 的 `stats_seq`。
    static APPLIED_SEQ: std::cell::RefCell<std::collections::HashMap<String, u64>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// 活动标签变化时清掉 seq 缓存，强制下一次刷新按全量走。
pub(super) fn invalidate_sidebar_cache() {
    APPLIED_SEQ.with(|m| m.borrow_mut().clear());
}

pub(super) fn refresh_sidebar(win: &AppWindow, statuses: &TabStatuses) -> SidebarStats {
    let mut stats = SidebarStats::default();

    let active = win.get_active_tab_id().to_string();
    let status = if active == "welcome" {
        None
    } else {
        statuses.lock().ok().and_then(|s| s.get(&active).cloned())
    };

    // 快照没变就只刷新连接状态行：空闲时不再重新栅格化三条曲线、不再重建进程列表。
    if let Some(st) = status.as_ref() {
        let unchanged = APPLIED_SEQ.with(|m| m.borrow().get(&active).copied() == Some(st.stats_seq));
        if unchanged {
            win.set_conn_state(conn_state_code(st.state));
            win.set_connection_state(connection_label(st.state, &st.host).into());
            win.set_conn_host(conn_ip(&st.host).into());
            win.set_stats_ready(overview_ready(st));
            return stats;
        }
        APPLIED_SEQ.with(|m| {
            m.borrow_mut().insert(active.clone(), st.stats_seq);
        });
    }

    // 非已连接态先清掉进程入口（`apply_rows` 在列表已空时是 no-op，不重复标脏）。
    if status.as_ref().is_none_or(|st| st.state != 1) {
        win.set_proc_available(false);
        set_procs(win, &[], "", "");
    }

    match status {
        // A local-shell tab (WSL / cmd / PowerShell) also reaches the connected
        // state but never reports remote resources, so keep the connection line
        // and show this machine's own CPU / memory / swap instead of zeroes.
        // 本地 shell 会话不采样本机数据，也不显示右侧 overview 面板。
        Some(st) if st.is_local => {
            win.set_conn_state(conn_state_code(st.state));
            win.set_connection_state(connection_label(st.state, &st.host).into());
            win.set_conn_host(conn_ip(&st.host).into());
            clear_stats(win);
            win.set_stats_ready(overview_ready(&st));
        }
        // A live session tab → remote resources + remote NIC on top.
        Some(st) if st.state == 1 => {
            win.set_conn_state(1);
            win.set_connection_state(st.host.clone().into());
            win.set_conn_host(conn_ip(&st.host).into());
            // 就绪判据统一走 overview_ready()，见本分支末尾；此处不再提前写入，
            // 避免 Slint 的 changed 处理读到尚未更新的 host-name。
            win.set_resource_title(t("服务器资源", "Server resources").into());
            // 综合页头卡：字段由 `monitor.rs` 从 /etc/os-release、uname、hostname、
            // uptime、loadavg 采集，结构化存放，不按本地化标签反查。
            win.set_host_name(st.sys.host.name.clone().into());
            win.set_host_ip(st.sys.host.ip.clone().into());
            win.set_host_os(st.sys.host.os.clone().into());
            win.set_host_kernel(st.sys.host.kernel.clone().into());
            win.set_host_uptime(st.sys.host.uptime.clone().into());
            win.set_host_load(st.sys.host.load.clone().into());
            // CPU 趋势按固定 0..100 刻度换算，不按当前最大值归一化 —— 否则 5% 的
            // 空闲机器也会画出满格曲线。
            // 固定 0..100 刻度生成平滑曲线；字符串在 Rust 侧拼好，Slint 只画 Path。
            let cpu_scaled: Vec<f32> = st.cpu_hist.iter().map(|v| v / 100.0).collect();
            win.set_cpu_span_index(crate::app::resource_ui::span_index(st.cpu_span_secs));
            win.set_net_span_index(crate::app::resource_ui::span_index(st.net_span_secs));
            win.set_mem_span_index(crate::app::resource_ui::span_index(st.mem_span_secs));
            win.set_cpu_path(smooth_path(&cpu_scaled, 60).into());
            win.set_mem_path(smooth_path(&st.mem_hist, 60).into());
            win.set_cpu_percent(st.cpu);
            win.set_mem_percent(usage_pct(st.mem_used_kib, st.mem_total_kib));
            win.set_swap_percent(usage_pct(st.swap_used_kib, st.swap_total_kib));
            win.set_mem_detail(format_mem(st.mem_used_kib / 1024, st.mem_total_kib / 1024).into());
            win.set_swap_detail(
                format_mem(st.swap_used_kib / 1024, st.swap_total_kib / 1024).into(),
            );
            let (name, rx, tx) = selected_iface(&st);
            win.set_net_top_up(format_bytes_per_sec(tx).into());
            win.set_net_top_down(format_bytes_per_sec(rx).into());
            // 上下行共用同一个峰值：两条线画在同一刻度上才可比。
            let peak = st
                .net_hist
                .iter()
                .chain(st.net_up_hist.iter())
                .fold(1.0_f32, |m, v| m.max(*v));
            let down: Vec<f32> = st
                .net_hist
                .iter()
                .map(|v| (v / peak).clamp(0.0, 1.0))
                .collect();
            let up: Vec<f32> = st
                .net_up_hist
                .iter()
                .map(|v| (v / peak).clamp(0.0, 1.0))
                .collect();
            win.set_net_top_path(smooth_path(&down, 60).into());
            win.set_net_up_path(smooth_path(&up, 60).into());
            win.set_net_show_selector(!st.net.is_empty());
            win.set_net_selected(name.into());
            let ifaces: Vec<SharedString> = st.net.iter().map(|e| e.0.clone().into()).collect();
            // 网卡列表很少变：内容一样就别换模型（换身份 = Repeater 重建整个列表）。
            let iface_model = win.get_net_ifaces();
            write_model(
                &mut stats,
                &iface_model,
                &ifaces,
                || ModelRc::from(Rc::new(VecModel::from(ifaces.clone()))),
                |m| win.set_net_ifaces(m),
            );
            write_disks(&mut stats, win, &st.disks);
            win.set_proc_available(true);
            set_procs(win, &st.procs, &st.user, &active);
            // 字段全部落地后再判就绪：Slint 侧 changed 会读 host-name。
            win.set_stats_ready(overview_ready(&st));
        }
        // Disconnected / timed-out session.
        Some(st) if st.state == 2 => {
            // 从未连上就断开 = 连接失败（红）；连上后断开 = 掉线（灰）。
            win.set_conn_state(if st.failed { 3 } else { 2 });
            win.set_connection_state(
                if st.failed {
                    format!("{} {}", st.host, t("连接失败", "Connection failed"))
                } else {
                    connection_label(2, &st.host)
                }
                .into(),
            );
            win.set_conn_host(conn_ip(&st.host).into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            clear_stats(win);
            win.set_stats_ready(overview_ready(&st));
        }
        // Still connecting.
        Some(st) => {
            win.set_conn_state(0);
            win.set_connection_state(format!("{} {}", t("连接中", "Connecting"), st.host).into());
            win.set_conn_host(conn_ip(&st.host).into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            clear_stats(win);
            win.set_stats_ready(overview_ready(&st));
        }
        // 欢迎页 / 未知标签：不展示任何资源。
        None => {
            win.set_conn_state(0);
            win.set_connection_state(t("未连接", "Not connected").into());
            win.set_conn_host("".into());
            clear_stats(win);
            win.set_stats_ready(false);
        }
    }
    stats
}
/// 使用率：`total == 0` 时不能用 0 除（还没采到样本的本地快照就是这样）。
pub(super) fn usage_pct(used: u64, total: u64) -> f32 {
    if total > 0 {
        used as f32 / total as f32
    } else {
        0.0
    }
}

/// 连接状态码：1 = 已连接，2 = 已断开，其余（含 0 = 连接中）= 0。
fn conn_state_code(state: u8) -> i32 {
    if state == 1 {
        1
    } else if state == 2 {
        2
    } else {
        0
    }
}

/// 运行状态页的连接文案：已连接只显示主机；已断开加后缀；其余显示「连接中 <主机>」。
fn connection_label(state: u8, host: &str) -> String {
    if state == 1 {
        host.to_string()
    } else if state == 2 {
        format!("{} {}", host, t("已断开", "disconnected"))
    } else {
        format!("{} {}", t("连接中", "Connecting"), host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_pct_never_divides_by_zero() {
        assert_eq!(usage_pct(0, 0), 0.0);
        assert_eq!(usage_pct(1, 2), 0.5);
        assert_eq!(usage_pct(0, 100), 0.0);
        assert!(usage_pct(u64::MAX, 1) > 1.0, "超出 100% 也不 panic");
    }

    /// 状态码映射：**只有 1/2 是明确的，其余一律当"连接中"** ——
    /// "断开后还亮绿灯"是最容易骗到用户的错法，这条把它钉住。
    #[test]
    fn conn_state_code_maps_only_connected_and_closed() {
        assert_eq!(conn_state_code(1), 1);
        assert_eq!(conn_state_code(2), 2);
        assert_eq!(conn_state_code(0), 0, "连接中");
        assert_eq!(conn_state_code(3), 0, "未知状态也当连接中");
        assert_eq!(conn_state_code(255), 0);
    }

    #[test]
    fn connection_label_wording_per_state() {
        assert_eq!(connection_label(1, "nas"), "nas", "已连接只显示主机");
        assert_eq!(
            connection_label(2, "nas"),
            format!("nas {}", t("已断开", "disconnected"))
        );
        assert_eq!(
            connection_label(0, "nas"),
            format!("{} nas", t("连接中", "Connecting"))
        );
    }

    /// 就绪判据：只有"连接中"返回 false；其余宁可显示 "—" 也不无限转圈。
    #[test]
    fn overview_ready_only_blocks_while_collecting() {
        let mut st = TabStatus::default();
        assert!(!overview_ready(&st), "state=0 连接中");

        st.state = 1;
        assert!(!overview_ready(&st), "已连接但主机信息未到");

        st.mem_total_kib = 1024;
        st.sys.host.name = "nas".into();
        assert!(overview_ready(&st), "主机名 + 内存总量到齐");

        st.sys.host.name.clear();
        st.first_stats_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(6));
        assert!(overview_ready(&st), "5 秒兜底放行");

        st.state = 2;
        assert!(overview_ready(&st), "已断开显示 '—' 卡片");

        st.is_local = true;
        st.state = 0;
        assert!(overview_ready(&st), "本地 shell 不上报远端资源");
    }

}
