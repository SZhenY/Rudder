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
/// 本机「系统信息」页的条目。
///
/// 之前本机走 `SystemDetails::default()`（全空），所以那一页对本地会话什么都没有；
/// 远端则由 `SYS_CMD` 解析 OS / KERNEL / HOSTNAME / UPTIME。这里用 sysinfo 的对应
/// 接口补齐，字段与远端一致。
///
/// 空值一律跳过（不塞空串）—— 面板会把每一对都显示成一行，塞空串只会得到一行空白。
fn local_system_details(snap: &SystemSnapshot) -> SystemDetails {
    let mut overview: Vec<(String, String)> = Vec::new();
    if !snap.os_version.is_empty() {
        overview.push((t("操作系统", "OS").to_string(), snap.os_version.clone()));
    }
    if !snap.host_name.is_empty() {
        overview.push((t("主机名", "Hostname").to_string(), snap.host_name.clone()));
    }
    if !snap.kernel_version.is_empty() {
        overview.push((
            t("内核", "Kernel").to_string(),
            snap.kernel_version.clone(),
        ));
    }
    overview.push((
        t("核心数", "Cores").to_string(),
        snap.core_usages.len().to_string(),
    ));
    if snap.uptime_secs > 0 {
        overview.push((
            t("开机时长", "Uptime").to_string(),
            format_uptime(snap.uptime_secs),
        ));
    }
    // CPU 明细页的 cpu_info 对本机保持空：品牌 / 频率在 sysinfo 0.38 里要额外引
    // trait 才能拿到，而这一页对本地不是关键信息，不值得为它加依赖面。
    SystemDetails {
        overview,
        ..Default::default()
    }
}

/// 秒 → "3 天 4 小时" / "5 小时 12 分" / "12 分"。
fn format_uptime(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{} {} {}", d, t("天", "d"), h)
    } else if h > 0 {
        format!("{} {} {} {}", h, t("小时", "h"), m, t("分", "m"))
    } else {
        format!("{} {}", m, t("分", "m"))
    }
}

fn write_disks(
    stats: &mut SidebarStats,
    win: &AppWindow,
    disks: &[(String, u64, u64)],
    picked: Option<&[String]>,
) {
    let filter = mount_filter();
    let hide_special = hide_special_partitions();
    // ① 弹层的**选择列表**：未置顶的完整集合 + 每行勾选态（`all-disks`）。必须与
    //    下面的展示列表分开 —— 否则"取消全选"之后弹层里一行都不剩，用户再也勾不
    //    回单个（死路）。
    write_model(
        stats,
        &win.get_all_disks(),
        &disk_rows(disks, &filter, hide_special, picked),
        || disk_model(disks, &filter, hide_special, picked),
        |m| win.set_all_disks(m),
    );
    // ② 展示列表：按勾选名单**筛选**（全选时是全部，取消全选时一行不剩）。
    let shown = filter_picked(disks, picked, |e| e.0.as_str());
    let rows = disk_rows(&shown, &filter, hide_special, picked);
    write_model(
        stats,
        &win.get_disks(),
        &rows,
        || disk_model(&shown, &filter, hide_special, picked),
        |m| win.set_disks(m),
    );
}

pub(super) fn refresh_sidebar(
    win: &AppWindow,
    statuses: &TabStatuses,
    local: &LocalSnap,
    local_net_hist: &NetHist,
) -> SidebarStats {
    let mut stats = SidebarStats::default();
    let pct = |used: u64, total: u64| usage_pct(used, total);
    // A poisoned lock must not take the client down — release builds use
    // panic = "abort". Skip this refresh pass instead; the sampler will
    // publish a fresh snapshot on the next tick.
    let Ok(snap) = local.lock().map(|s| s.clone()) else {
        return SidebarStats::default();
    };

    // --- Bottom network graph: always the local machine --------------------
    win.set_net_bot_up(format_bytes_per_sec(snap.net_tx_per_sec).into());
    win.set_net_bot_down(format_bytes_per_sec(snap.net_rx_per_sec).into());
    // 先取历史（锁内只做纯计算），**出锁之后**才碰 Slint 属性 —— 写属性可能重入 UI 代码。
    // 归一化只算一次：上下两个图共用同一个环形缓冲。
    let Ok(scaled) = local_net_hist.lock().map(|h| normalize(&h)) else {
        return SidebarStats::default();
    };
    let bot = win.get_net_bot_history();
    write_model(&mut stats, &bot, &scaled, || graph_model(&scaled), |m| {
        win.set_net_bot_history(m)
    });

    let set_top_local = |win: &AppWindow, stats: &mut SidebarStats, pd: Option<&[String]>| {
        win.set_net_top_up(format_bytes_per_sec(snap.net_tx_per_sec).into());
        win.set_net_top_down(format_bytes_per_sec(snap.net_rx_per_sec).into());
        // 本机 / 未连接 / 已断开：没有远端 CPU 样本，趋势图清空（只在非空时清一次，
        // 免得 1 Hz tick 反复换模型身份）。
        if win.get_cpu_history().row_count() > 0 {
            win.set_cpu_history(ModelRc::from(Rc::new(VecModel::<f32>::default())));
        }
        // 时间戳表必须**跟着一起清**：否则从远端切回本机时，曲线虽然空了，但气泡
        // 仍能读到上一台机器的时刻（两个模型各自独立，不会互相牵连）。
        if win.get_cpu_history_times().row_count() > 0 {
            win.set_cpu_history_times(ModelRc::from(Rc::new(VecModel::<i32>::default())));
        }
        let top = win.get_net_top_history();
        write_model(stats, &top, &scaled, || graph_model(&scaled), |m| {
            win.set_net_top_history(m)
        });
        win.set_net_show_selector(false);
        win.set_net_selected("".into());
        // 本机没有网卡下拉列表：只在"上一轮写的是远端列表"时清一次，不再每秒塞一个新模型。
        if win.get_net_ifaces().row_count() > 0 {
            win.set_net_ifaces(ModelRc::from(Rc::new(VecModel::<SharedString>::default())));
        }
        // 弹层那张网卡选择列表也要一起清，否则会留着上一台远端机器的网卡。
        if win.get_all_ifs().row_count() > 0 {
            win.set_all_ifs(ModelRc::from(Rc::new(VecModel::<SysNetRow>::default())));
        }
        // Non-connected tabs show the local machine's filesystems.
        write_disks(stats, win, &snap.disks, pd);
    };
    let show_local_res = |win: &AppWindow| {
        win.set_resource_title(t("本机资源", "Local resources").into());
        win.set_cpu_percent(snap.cpu_percent);
        win.set_mem_percent(snap.mem_percent);
        win.set_swap_percent(snap.swap_percent);
        win.set_mem_detail(format_mem(snap.mem_used_mib, snap.mem_total_mib).into());
        win.set_swap_detail(format_mem(snap.swap_used_mib, snap.swap_total_mib).into());
        win.set_mem_used(format_mib(snap.mem_used_mib).into());
        win.set_mem_total(format_mib(snap.mem_total_mib).into());
    };
    let clear_stats = |win: &AppWindow| {
        win.set_cpu_percent(0.0);
        win.set_mem_percent(0.0);
        win.set_swap_percent(0.0);
        win.set_mem_detail("".into());
        win.set_swap_detail("".into());
        win.set_mem_used("".into());
        win.set_mem_total("".into());
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
            apply_rows(vm, proc_rows(procs, current_user, tab_id));
        }
    };
    // 工具面板「进程 CPU 占用排行」：PROC_CMD 已 `--sort=-pcpu`，取前 20 行
    //（排行卡吸收页面剩余高度，行数超出可视范围时列表内滚轮滚动）。
    let set_proc_top = |win: &AppWindow, procs: &[ProcInfo], current_user: &str, tab_id: &str| {
        if let Some(vm) = win
            .get_proc_top()
            .as_any()
            .downcast_ref::<VecModel<ProcRow>>()
        {
            apply_rows(
                vm,
                proc_rows(procs, current_user, tab_id)
                    .into_iter()
                    .take(20)
                    .collect::<Vec<_>>(),
            );
        }
    };
    let set_system_models = |win: &AppWindow,
                             cpu: f32,
                             mem: f32,
                             swap: f32,
                             mem_detail: SharedString,
                             swap_detail: SharedString,
                             nets: Vec<SysNetRow>,
                             sys: SystemDetails,
                             host: &str| {
        // 工具面板头卡用的四个字段：**按标签**从 overview 里取 —— 本机 / 远端两条
        // 路径的字段顺序不一样，上一版按下标取，结果主机名的位置显示成了负载均值。
        let pick = |labels: &[String]| -> SharedString {
            sys.overview
                .iter()
                .find(|(k, _)| labels.iter().any(|label| k == label))
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
                .into()
        };
        win.set_panel_host(pick(&[t("主机名称", "Hostname").to_string()]));
        win.set_panel_ip(pick(&[t("IP", "IP").to_string()]));
        win.set_panel_os(pick(&[t("操作系统", "Operating system").to_string()]));
        win.set_panel_uptime(pick(&[t("运行", "Uptime").to_string()]));
        // 头卡 IP：**只显示本会话连接所用的地址**（用户要求；overview 里的 IPS 是
        // 全部网卡的列表，会撑爆徽标）。本机资源没有"会话地址"，回退到 overview 的 IP。
        let panel_ip = if host.trim().is_empty() {
            pick(&[t("IP", "IP").to_string()])
        } else {
            conn_ip(host).into()
        };
        win.set_panel_ip(panel_ip);
        // 负载均值（1 分钟）：overview 的「负载」是 "1m 5m 15m" 三个数，取第一个。
        let load_1m = pick(&[t("负载", "Load").to_string()])
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .to_string();
        win.set_panel_load_1m(load_1m.into());
        // 「等待监控数据」加载态：overview / CPU 信息一到就算就绪。远端要等异步
        // 采样回来（连接中 / 已断开传 SystemDetails::default() → 未就绪）；本机
        // 数据即时可用，由 show_local_system_models 置真。
        win.set_panel_ready(!sys.overview.is_empty() || !sys.cpu_info.is_empty());
        if let Some(vm) = win
            .get_sys_metrics()
            .as_any()
            .downcast_ref::<VecModel<SysMetricRow>>()
        {
            apply_rows(vm, metric_rows(cpu, mem, swap, mem_detail, swap_detail));
        }
        if let Some(vm) = win
            .get_sys_net_rows()
            .as_any()
            .downcast_ref::<VecModel<SysNetRow>>()
        {
            apply_rows(vm, nets);
        }
        if let Some(vm) = win
            .get_sys_overview_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            apply_rows(vm, pairs_to_overview_rows(&sys.overview));
        }
        if let Some(vm) = win
            .get_sys_cpu_info_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            apply_rows(vm, pairs_to_one_row(&sys.cpu_info));
        }
        if let Some(vm) = win
            .get_sys_gpu_info_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            apply_rows(vm, pairs_to_rows(&sys.gpu_info, 4));
        }
        if let Some(vm) = win
            .get_sys_cpu_usage_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            apply_rows(vm, cpu_usage_detail_rows(&sys.cpu_usage));
        }
        if let Some(vm) = win
            .get_sys_memory_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            apply_rows(vm, pairs_to_one_row(&sys.memory));
        }
        if let Some(vm) = win
            .get_sys_swap_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            apply_rows(vm, pairs_to_one_row(&sys.swap));
        }
        if let Some(vm) = win
            .get_sys_network_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            apply_rows(vm, tuple5_rows(&sys.networks));
        }
        if let Some(vm) = win
            .get_sys_filesystem_rows()
            .as_any()
            .downcast_ref::<VecModel<SysInfoRow>>()
        {
            apply_rows(vm, tuple5_rows(&sys.filesystems));
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
                // 本机只有一行总速率，没有"选哪块网卡"的余地。
                picked: false,
            }],
            local_system_details(&snap),
            "",
        );
        // 本机数据即时可用：不做「等待监控数据」加载态
        win.set_panel_ready(true);
    };
    win.set_proc_available(false);
    win.set_system_info_available(false);
    set_procs(win, &[], "", "");

    // 远端专属的面板数据先归位：本地 / 欢迎标签不吃上一个会话的残留
    //（远端连接分支会覆盖这些值）。
    win.set_cpu_user(0.0);
    win.set_cpu_system(0.0);
    win.set_cpu_iowait(0.0);
    win.set_cpu_temp("".into());
    if win.get_core_cpus().row_count() > 0 {
        win.set_core_cpus(ModelRc::from(Rc::new(VecModel::<f32>::default())));
    }
    if win.get_cores_left().row_count() > 0 {
        win.set_cores_left(ModelRc::from(Rc::new(VecModel::<CoreLoad>::default())));
    }
    if win.get_cores_right().row_count() > 0 {
        win.set_cores_right(ModelRc::from(Rc::new(VecModel::<CoreLoad>::default())));
    }
    if let Some(vm) = win
        .get_proc_top()
        .as_any()
        .downcast_ref::<VecModel<ProcRow>>()
    {
        apply_rows(vm, Vec::new());
    }

    let active = win.get_active_tab_id().to_string();
    let status = if active == "welcome" {
        None
    } else {
        statuses.lock().ok().and_then(|s| s.get(&active).cloned())
    };
    // 勾选集合镜像回界面：按钮上的「已选 N」要用长度。
    //（逐行的勾选态在行数据里 —— DiskInfo.picked / SysNetRow.picked）
    //
    // ⚠ `None`（全选）与 `Some(vec![])`（一个都不选）**名单都是空的**，光看数组
    // 分不出来，所以额外导出一个 `*-picked-all` bool 给「全部」那个复选框用。
    let (picked_ifs, ifs_all): (Vec<SharedString>, bool) = status
        .as_ref()
        .map(|st| match st.picked_ifaces.as_deref() {
            None => (Vec::new(), true),
            Some(v) => (v.iter().map(SharedString::from).collect(), false),
        })
        .unwrap_or((Vec::new(), true));
    let (picked_dks, disks_all): (Vec<SharedString>, bool) = status
        .as_ref()
        .map(|st| match st.picked_disks.as_deref() {
            None => (Vec::new(), true),
            Some(v) => (v.iter().map(SharedString::from).collect(), false),
        })
        .unwrap_or((Vec::new(), true));
    // 每次都写（不再比长度）：这两个模型只被按钮文案和「全部」复选框读，列表读的是
    // all-ifs / all-disks，换身份不会触发任何列表重建；而"换成另一个名字"（长度相同）
    // 这种旧逻辑会漏掉的情况必须能更新。
    win.set_net_picked(ModelRc::from(Rc::new(VecModel::from(picked_ifs))));
    win.set_disk_picked(ModelRc::from(Rc::new(VecModel::from(picked_dks))));
    win.set_net_picked_all(ifs_all);
    win.set_disk_picked_all(disks_all);

    // 「当前标签页是否已有会话」→ 右侧工具栏显不显示。
    //
    // ⚠ 不能拿"终端行数 > 0"当判据：终端页点「+」新建会话时会先建出一个空标签页
    // 并弹出「创建你的第一个终端会话」卡片，那一刻已经有行了，但一个会话都没开，
    // 工具栏却会弹出来显示本机 CPU / 内存 / 磁盘。
    //
    // 判据取"状态表里有没有该 tab 的条目"：条目只在**会话真正开始**时才插入
    // （session_callbacks：连接中 state=0 那次），所以 pending 的新建会话页没有
    // 条目 → 工具栏不显示；本机 / 远端会话一开始就绪条目就在 → 照常显示。
    win.set_has_session(status.is_some());

    match status {
        // A local-shell tab (WSL / cmd / PowerShell) also reaches the connected
        // state but never reports remote resources, so keep the connection line
        // and show this machine's own CPU / memory / swap instead of zeroes.
        Some(st) if st.is_local => {
            win.set_conn_state(conn_state_code(st.state));
            win.set_connection_state(connection_label(st.state, &st.host).into());
            win.set_conn_host(conn_ip(&st.host).into());
            show_local_res(win);
            set_top_local(win, &mut stats, st.picked_disks.as_deref());
            show_local_system_models(win);
            // 每核占用（面板 CPU 页「核心详情」）：与远端同一套 odd/even 分列。
            // 本机走 sysinfo 的 cpus()，之前这里恒空 → 显示"暂无数据"。
            let core_rows = |pick: fn(usize) -> bool| -> Vec<CoreLoad> {
                snap.core_usages
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| pick(*i))
                    .map(|(i, v)| CoreLoad {
                        idx: i as i32,
                        load: *v,
                    })
                    .collect()
            };
            let left = core_rows(|i| i % 2 == 0);
            let right = core_rows(|i| i % 2 == 1);
            write_model(
                &mut stats,
                &win.get_core_cpus(),
                &snap.core_usages,
                || {
                    ModelRc::from(Rc::new(VecModel::from(
                        snap.core_usages.to_vec(),
                    )))
                },
                |m| win.set_core_cpus(m),
            );
            write_model(
                &mut stats,
                &win.get_cores_left(),
                &left,
                || ModelRc::from(Rc::new(VecModel::from(core_rows(|i| i % 2 == 0)))),
                |m| win.set_cores_left(m),
            );
            write_model(
                &mut stats,
                &win.get_cores_right(),
                &right,
                || ModelRc::from(Rc::new(VecModel::from(core_rows(|i| i % 2 == 1)))),
                |m| win.set_cores_right(m),
            );
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
            win.set_mem_used(format_mib(st.mem_used_kib / 1024).into());
            win.set_mem_total(format_mib(st.mem_total_kib / 1024).into());
            // CPU 明细（用户态/内核态/IO 等待）+ 每核占用 + 温度（工具面板 CPU 页）
            win.set_cpu_user(st.cpu_user);
            win.set_cpu_system(st.cpu_system);
            win.set_cpu_iowait(st.cpu_iowait);
            win.set_cpu_temp(
                st.sys
                    .cpu_info
                    .iter()
                    .find(|(k, _)| *k == t("温度", "Temperature"))
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
                    .into(),
            );
            write_model(
                &mut stats,
                &win.get_core_cpus(),
                &st.core_cpus,
                || ModelRc::from(Rc::new(VecModel::from(st.core_cpus.clone()))),
                |m| win.set_core_cpus(m),
            );
            // 面板两列排布：偶数核左列、奇数核右列（行内带真实核心编号）。
            let core_rows = |pick: fn(usize) -> bool| -> Vec<CoreLoad> {
                st.core_cpus
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| pick(*i))
                    .map(|(i, v)| CoreLoad {
                        idx: i as i32,
                        load: *v,
                    })
                    .collect()
            };
            write_model(
                &mut stats,
                &win.get_cores_left(),
                &core_rows(|i| i % 2 == 0),
                || ModelRc::from(Rc::new(VecModel::from(core_rows(|i| i % 2 == 0)))),
                |m| win.set_cores_left(m),
            );
            write_model(
                &mut stats,
                &win.get_cores_right(),
                &core_rows(|i| i % 2 == 1),
                || ModelRc::from(Rc::new(VecModel::from(core_rows(|i| i % 2 == 1)))),
                |m| win.set_cores_right(m),
            );
            set_proc_top(win, &st.procs, &st.user, &active);
            let (name, rx, tx) = selected_iface(&st);
            win.set_net_top_up(format_bytes_per_sec(tx).into());
            win.set_net_top_down(format_bytes_per_sec(rx).into());
            let hist = normalize(&st.net_hist);
            let top = win.get_net_top_history();
            write_model(&mut stats, &top, &hist, || graph_model(&hist), |m| {
                win.set_net_top_history(m)
            });
            // CPU 负载趋势（工具面板「综合 / 处理器」用）：**不做峰值归一化**。
            //
            // `normalize()` 把整条曲线除以「窗口内的历史峰值」，对 CPU 这种本来就是
            // 0–100% 的量是错的：
            //   ① 一次尖峰会把随后整条曲线压到贴地 60 拍 —— 用户最早报的"突然显示
            //      一段异常负载，然后跳回正常"就是它（不是采样异常，是分母突变）；
            //   ② 同一个 20% 负载在空闲机 / 繁忙机上画出的高度完全不同，读数不可信；
            //   ③ 组件里的 25/50/75/100% 刻度线形同虚设 —— 数据根本不在那个量程上。
            //
            // 网络图（net_hist / net_top_history）**继续**归一化：那里单位是
            // 字节/秒、量程不确定，按窗口峰值缩放才是对的。
            let cpu_hist = st.cpu_hist.clone();
            let cpu_model = win.get_cpu_history();
            write_model(
                &mut stats,
                &cpu_model,
                &cpu_hist,
                || graph_model(&cpu_hist),
                |m| win.set_cpu_history(m),
            );
            // 采样时刻（与 cpu_hist 逐拍同步入环）。送整表而不是"当前时刻"，
            // 才能让悬停气泡显示**那个点**的绝对时刻 —— 而不只是"现在几点"。
            // 走同一条增量写路径：每拍只有末尾一行变化，等于零成本。
            let cpu_times: Vec<i32> = st.cpu_hist_t.clone();
            let times_model = win.get_cpu_history_times();
            write_model(
                &mut stats,
                &times_model,
                &cpu_times,
                || graph_model(&cpu_times),
                |m| win.set_cpu_history_times(m),
            );
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
            let if_sel = st.picked_ifaces.as_deref();
            // 弹层的**网卡选择列表**（未置顶、顺序稳定）+ 每行勾选态；下面 set_
            // system_models 里那个置顶列表只负责展示。分开是必须的：否则"取消全选"
            // 之后弹层里一行都不剩，用户没法再勾回单个（死路）。
            let if_choices = net_rows(&st.net, if_sel);
            write_model(
                &mut stats,
                &win.get_all_ifs(),
                &if_choices,
                || ModelRc::from(Rc::new(VecModel::from(if_choices.clone()))),
                |m| win.set_all_ifs(m),
            );
            write_disks(&mut stats, win, &st.disks, st.picked_disks.as_deref());
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
                net_rows(&filter_picked(&st.net, if_sel, |e| e.0.as_str()), if_sel),
                st.sys.clone(),
                &st.host,
            );
        }
        // Disconnected / timed-out session.
        Some(st) if st.state == 2 => {
            win.set_conn_state(2);
            win.set_connection_state(format!("{} {}", st.host, t("已断开", "disconnected")).into());
            win.set_conn_host(conn_ip(&st.host).into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            clear_stats(win);
            set_top_local(win, &mut stats, None);
            set_system_models(
                win,
                0.0,
                0.0,
                0.0,
                "".into(),
                "".into(),
                Vec::new(),
                SystemDetails::default(),
                &st.host,
            );
        }
        // Still connecting.
        Some(st) => {
            win.set_conn_state(0);
            win.set_connection_state(format!("{} {}", t("连接中", "Connecting"), st.host).into());
            win.set_conn_host(conn_ip(&st.host).into());
            win.set_resource_title(t("服务器资源", "Server resources").into());
            clear_stats(win);
            set_top_local(win, &mut stats, None);
            set_system_models(
                win,
                0.0,
                0.0,
                0.0,
                "".into(),
                "".into(),
                Vec::new(),
                SystemDetails::default(),
                &st.host,
            );
        }
        // Welcome tab (or unknown) → local machine top + bottom.
        None => {
            win.set_conn_state(0);
            win.set_connection_state(t("未连接", "Not connected").into());
            win.set_conn_host("".into());
            show_local_res(win);
            set_top_local(win, &mut stats, None);
            show_local_system_models(win);
        }
    }
    // per-tab 状态搬家①：这一趟也把活动标签的隧道 / SFTP 状态镜像到窗口级，
    // 新外壳右面板读的是窗口属性。放在末尾 —— 上面几个 helper 会写 Slint 属性，
    // 可能重入 UI 代码，镜像要在状态都落定之后再做。
    refresh_active_term(win);
    stats
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
