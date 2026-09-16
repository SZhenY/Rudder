use super::*;

pub(super) fn refresh_sidebar(
    win: &AppWindow,
    statuses: &TabStatuses,
    local: &LocalSnap,
    local_net_hist: &NetHist,
) {
    let pct = |used: u64, total: u64| usage_pct(used, total);
    // A poisoned lock must not take the client down — release builds use
    // panic = "abort". Skip this refresh pass instead; the sampler will
    // publish a fresh snapshot on the next tick.
    let Ok(snap) = local.lock().map(|s| s.clone()) else {
        return;
    };

    // --- Bottom network graph: always the local machine --------------------
    win.set_net_bot_up(format_bytes_per_sec(snap.net_tx_per_sec).into());
    win.set_net_bot_down(format_bytes_per_sec(snap.net_rx_per_sec).into());
    // Snapshot the history *before* writing the Slint property: writing can
    // re-enter UI code, which must not happen while the mutex is held.
    let bot_hist = local_net_hist
        .lock()
        .map(|h| normalized_model(&h))
        .unwrap_or_default();
    win.set_net_bot_history(bot_hist);

    let set_top_local = |win: &AppWindow| {
        win.set_net_top_up(format_bytes_per_sec(snap.net_tx_per_sec).into());
        win.set_net_top_down(format_bytes_per_sec(snap.net_rx_per_sec).into());
        let top_hist = local_net_hist
            .lock()
            .map(|h| normalized_model(&h))
            .unwrap_or_default();
        win.set_net_top_history(top_hist);
        win.set_net_show_selector(false);
        win.set_net_selected("".into());
        win.set_net_ifaces(ModelRc::from(Rc::new(VecModel::<SharedString>::default())));
        // Non-connected tabs show the local machine's filesystems.
        win.set_disks(disk_model(
            &snap.disks,
            &mount_filter(),
            hide_special_partitions(),
        ));
    };
    let show_local_res = |win: &AppWindow| {
        win.set_resource_title(t("本机资源", "Local resources").into());
        win.set_cpu_percent(snap.cpu_percent);
        win.set_mem_percent(snap.mem_percent);
        win.set_swap_percent(snap.swap_percent);
        win.set_mem_detail(format_mem(snap.mem_used_mib, snap.mem_total_mib).into());
        win.set_swap_detail(format_mem(snap.swap_used_mib, snap.swap_total_mib).into());
    };
    let clear_stats = |win: &AppWindow| {
        win.set_cpu_percent(0.0);
        win.set_mem_percent(0.0);
        win.set_swap_percent(0.0);
        win.set_mem_detail("".into());
        win.set_swap_detail("".into());
    };

    // Process monitor (#23) lives in a shared model (the AppWindow and the
    // detachable ProcWindow point at the same VecModel), so mutate it in place
    // instead of replacing it — replacing would break the sharing. Only a live
    // remote session has process data; default to empty and let the connected
    // branch below fill it in.
    let set_procs = |win: &AppWindow, procs: &[ProcInfo], current_user: &str, tab_id: &str| {
        if let Some(vm) = win
            .get_proc_list()
            .as_any()
            .downcast_ref::<VecModel<ProcRow>>()
        {
            vm.set_vec(proc_rows(procs, current_user, tab_id));
        }
    };
    let set_system_models = |win: &AppWindow,
                             cpu: f32,
                             mem: f32,
                             swap: f32,
                             mem_detail: SharedString,
                             swap_detail: SharedString,
                             nets: Vec<SysNetRow>,
                             sys: SystemDetails| {
        if let Some(vm) = win
            .get_sys_metrics()
            .as_any()
            .downcast_ref::<VecModel<SysMetricRow>>()
        {
            vm.set_vec(metric_rows(cpu, mem, swap, mem_detail, swap_detail));
        }
        if let Some(vm) = win
            .get_sys_net_rows()
            .as_any()
            .downcast_ref::<VecModel<SysNetRow>>()
        {
            vm.set_vec(nets);
        }
        if let Some(vm) = win
            .get_sys_overview_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            vm.set_vec(pairs_to_overview_rows(&sys.overview));
        }
        if let Some(vm) = win
            .get_sys_cpu_info_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            vm.set_vec(pairs_to_one_row(&sys.cpu_info));
        }
        if let Some(vm) = win
            .get_sys_gpu_info_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            vm.set_vec(pairs_to_rows(&sys.gpu_info, 4));
        }
        if let Some(vm) = win
            .get_sys_cpu_usage_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            vm.set_vec(cpu_usage_detail_rows(&sys.cpu_usage));
        }
        if let Some(vm) = win
            .get_sys_memory_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            vm.set_vec(pairs_to_one_row(&sys.memory));
        }
        if let Some(vm) = win
            .get_sys_swap_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            vm.set_vec(pairs_to_one_row(&sys.swap));
        }
        if let Some(vm) = win
            .get_sys_network_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            vm.set_vec(tuple5_rows(&sys.networks));
        }
        if let Some(vm) = win
            .get_sys_filesystem_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            vm.set_vec(tuple5_rows(&sys.filesystems));
        }
    };
    // The local machine's own figures: used both by a local-shell tab and by
    // the welcome tab, neither of which has remote stats to show.
    let show_local_system_models = |win: &AppWindow| {
        set_system_models(
            win,
            snap.cpu_percent,
            snap.mem_percent,
            snap.swap_percent,
            format_mem(snap.mem_used_mib, snap.mem_total_mib).into(),
            format_mem(snap.swap_used_mib, snap.swap_total_mib).into(),
            vec![SysNetRow {
                name: t("本机", "Local").into(),
                up: format_bytes_per_sec(snap.net_tx_per_sec).into(),
                down: format_bytes_per_sec(snap.net_rx_per_sec).into(),
            }],
            SystemDetails::default(),
        );
    };
    win.set_proc_available(false);
    win.set_system_info_available(false);
    set_procs(win, &[], "", "");

    let active = win.get_active_tab_id().to_string();
    let status = if active == "welcome" {
        None
    } else {
        statuses.lock().ok().and_then(|s| s.get(&active).cloned())
    };

    match status {
        // A local-shell tab (WSL / cmd / PowerShell) also reaches the connected
        // state but never reports remote resources, so keep the connection line
        // and show this machine's own CPU / memory / swap instead of zeroes.
        Some(st) if st.is_local => {
            win.set_conn_state(conn_state_code(st.state));
            win.set_connection_state(connection_label(st.state, &st.host).into());
            win.set_conn_host(conn_ip(&st.host).into());
            show_local_res(win);
            set_top_local(win);
            show_local_system_models(win);
        }
        // A live session tab → remote resources + remote NIC on top.
        Some(st) if st.state == 1 => {
            win.set_conn_state(1);
            win.set_connection_state(st.host.clone().into());
            win.set_conn_host(conn_ip(&st.host).into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            win.set_cpu_percent(st.cpu);
            win.set_mem_percent(pct(st.mem_used_kib, st.mem_total_kib));
            win.set_swap_percent(pct(st.swap_used_kib, st.swap_total_kib));
            win.set_mem_detail(format_mem(st.mem_used_kib / 1024, st.mem_total_kib / 1024).into());
            win.set_swap_detail(
                format_mem(st.swap_used_kib / 1024, st.swap_total_kib / 1024).into(),
            );
            let (name, rx, tx) = selected_iface(&st);
            win.set_net_top_up(format_bytes_per_sec(tx).into());
            win.set_net_top_down(format_bytes_per_sec(rx).into());
            win.set_net_top_history(normalized_model(&st.net_hist));
            win.set_net_show_selector(!st.net.is_empty());
            win.set_net_selected(name.into());
            let ifaces: Vec<SharedString> = st.net.iter().map(|e| e.0.clone().into()).collect();
            win.set_net_ifaces(ModelRc::from(Rc::new(VecModel::from(ifaces))));
            win.set_disks(disk_model(
                &st.disks,
                &mount_filter(),
                hide_special_partitions(),
            ));
            win.set_proc_available(true);
            win.set_system_info_available(true);
            set_procs(win, &st.procs, &st.user, &active);
            set_system_models(
                win,
                st.cpu,
                pct(st.mem_used_kib, st.mem_total_kib),
                pct(st.swap_used_kib, st.swap_total_kib),
                format_mem(st.mem_used_kib / 1024, st.mem_total_kib / 1024).into(),
                format_mem(st.swap_used_kib / 1024, st.swap_total_kib / 1024).into(),
                net_rows(&st.net),
                st.sys.clone(),
            );
        }
        // Disconnected / timed-out session.
        Some(st) if st.state == 2 => {
            win.set_conn_state(2);
            win.set_connection_state(format!("{} {}", st.host, t("已断开", "disconnected")).into());
            win.set_conn_host(conn_ip(&st.host).into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            clear_stats(win);
            set_top_local(win);
            set_system_models(
                win,
                0.0,
                0.0,
                0.0,
                "".into(),
                "".into(),
                Vec::new(),
                SystemDetails::default(),
            );
        }
        // Still connecting.
        Some(st) => {
            win.set_conn_state(0);
            win.set_connection_state(format!("{} {}", t("连接中", "Connecting"), st.host).into());
            win.set_conn_host(conn_ip(&st.host).into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            clear_stats(win);
            set_top_local(win);
            set_system_models(
                win,
                0.0,
                0.0,
                0.0,
                "".into(),
                "".into(),
                Vec::new(),
                SystemDetails::default(),
            );
        }
        // Welcome tab (or unknown) → local machine top + bottom.
        None => {
            win.set_conn_state(0);
            win.set_connection_state(t("未连接", "Not connected").into());
            win.set_conn_host("".into());
            show_local_res(win);
            set_top_local(win);
            show_local_system_models(win);
        }
    }
}
/// 使用率：`total == 0` 时不能用 0 除（还没采到样本的本地快照就是这样）。
fn usage_pct(used: u64, total: u64) -> f32 {
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

/// 状态行文案：已连接只显示主机；已断开加后缀；其余显示「连接中 <主机>」。
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
}
