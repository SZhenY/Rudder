use super::*;

pub(crate) fn parse_process_block(block: &str) -> (String, Vec<ProcInfo>) {
    const MAX_PROCESS_ENTRIES: usize = 64;
    enum Section {
        None,
        User,
        Processes,
    }
    let mut section = Section::None;
    let mut current_user = String::new();
    let mut procs = Vec::new();
    for line in block.lines().map(str::trim).filter(|line| !line.is_empty()) {
        match line {
            "__ME__" => section = Section::User,
            "__PS__" => section = Section::Processes,
            _ => match section {
                Section::User if current_user.is_empty() => current_user = line.to_string(),
                Section::Processes if procs.len() < MAX_PROCESS_ENTRIES => {
                    if let Some(process) = parse_ps_line(line) {
                        procs.push(process);
                    }
                }
                _ => {}
            },
        }
    }
    (current_user, procs)
}

/// Parse one monitor sample (a block of `/proc/stat` cpu line + `/proc/meminfo`
/// fields) into a [`SessionEvent::ResourceStats`].
///
/// CPU usage needs two consecutive `/proc/stat` snapshots; `prev` carries the
/// previous (total, idle) jiffies across calls.  The first sample therefore
/// reports 0% (no baseline yet).
pub(crate) fn parse_monitor_block(
    block: &str,
    prev: &mut Option<(u64, u64)>,
    prev_net: &mut std::collections::HashMap<String, (u64, u64)>,
    prev_net_at: &mut std::time::Instant,
) -> Option<SessionEvent> {
    let mut cpu_total = 0u64;
    let mut cpu_idle = 0u64;
    let mut have_cpu = false;
    let mut mem_total = 0u64;
    let mut mem_avail = 0u64;
    let mut mem_buffers = 0u64;
    let mut mem_cached = 0u64;
    let mut swap_total = 0u64;
    let mut swap_free = 0u64;
    let mut cpu_nums: Vec<u64> = Vec::new();
    // Raw /proc/net/dev counters this sample: iface -> (rx_bytes, tx_bytes).
    let mut net_now: Vec<(String, u64, u64)> = Vec::new();
    // Filesystems from `df -kP`: (mount, available_bytes, total_bytes).
    let mut disks: Vec<(String, u64, u64)> = Vec::new();
    // Dedup duplicate filesystems before they reach the panel (#38): NAS boxes
    // (FNOS …) report the same underlying volume dozens of times — one Docker
    // overlay mount per container layer, all with identical size. Like dropping rows
    // into a Set: skip a (total, available) we've already shown. `df` lists the real
    // mount first, so that's the one kept.
    let mut seen_fs: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
    let mut sys_kv: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    // The sample is split into sections by `echo` markers; everything before the
    // first marker is the cpu/mem/net block.
    enum Section {
        Top,
        Df,
        Sys,
    }
    let mut section = Section::Top;

    // Cap how many interfaces / filesystems we accept from one sample
    // so a hostile server can't flood the parser and sidebar with fabricated rows
    // (#27). No real machine has anywhere near this many.
    const MAX_MON_ENTRIES: usize = 64;

    for line in block.lines() {
        if line == "__DF__" {
            section = Section::Df;
            continue;
        }
        if line == "__SYS__" {
            section = Section::Sys;
            continue;
        }
        match section {
            Section::Df => {
                if disks.len() < MAX_MON_ENTRIES
                    && let Some((mount, avail, total)) = parse_df_line(line)
                {
                    // Set-style dedup: skip a filesystem whose (total, available)
                    // we've already added — collapses the dozens of identical
                    // Docker overlay mounts a NAS reports down to one row (#38).
                    if seen_fs.insert((total, avail)) {
                        disks.push((mount, avail, total));
                    }
                }
                continue;
            }
            Section::Sys => {
                if let Some((k, v)) = line.split_once('=') {
                    sys_kv.insert(k.trim().to_string(), v.trim().to_string());
                }
                continue;
            }
            Section::Top => {}
        }
        if let Some(rest) = line.strip_prefix("cpu ") {
            let nums: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|x| x.parse().ok())
                .collect();
            // user nice system idle iowait irq softirq steal ...
            if nums.len() >= 4 {
                // Saturating arithmetic: a server can send arbitrary jiffy
                // values, and a plain sum/add would panic on overflow in debug.
                cpu_total = nums.iter().copied().fold(0u64, u64::saturating_add);
                cpu_idle = nums[3].saturating_add(nums.get(4).copied().unwrap_or(0)); // idle + iowait
                have_cpu = true;
                cpu_nums = nums;
            }
        } else if let Some(v) = line.strip_prefix("MemTotal:") {
            mem_total = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            mem_avail = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("Buffers:") {
            mem_buffers = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("Cached:") {
            mem_cached = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("SwapTotal:") {
            swap_total = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("SwapFree:") {
            swap_free = parse_meminfo_kib(v);
        } else if net_now.len() < MAX_MON_ENTRIES
            && let Some((iface, counters)) = parse_net_dev_line(line)
        {
            net_now.push((iface, counters.0, counters.1));
        }
    }

    // Convert raw byte counters into per-second rates using the previous sample.
    let now = std::time::Instant::now();
    let elapsed = now.duration_since(*prev_net_at).as_secs_f64().max(0.001);
    let mut net: Vec<(String, u64, u64)> = Vec::new();
    let net_counters = net_now.clone();
    if !net_now.is_empty() {
        for (iface, rx, tx) in &net_now {
            if let Some((prx, ptx)) = prev_net.get(iface) {
                let rx_bps = (rx.saturating_sub(*prx) as f64 / elapsed) as u64;
                let tx_bps = (tx.saturating_sub(*ptx) as f64 / elapsed) as u64;
                net.push((iface.clone(), rx_bps, tx_bps));
            }
        }
        prev_net.clear();
        for (iface, rx, tx) in net_now {
            prev_net.insert(iface, (rx, tx));
        }
        *prev_net_at = now;
        // Show busiest first so the default-selected NIC is the active one.
        net.sort_by_key(|a| std::cmp::Reverse(a.1 + a.2));
    }

    let cpu_percent = if have_cpu {
        let result = match *prev {
            Some((ptotal, pidle)) => {
                let dt = cpu_total.saturating_sub(ptotal);
                let di = cpu_idle.saturating_sub(pidle);
                if dt > 0 {
                    (1.0 - di as f32 / dt as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            }
            None => 0.0,
        };
        *prev = Some((cpu_total, cpu_idle));
        result
    } else {
        0.0
    };

    // Need at least memory numbers to be a useful sample.
    if mem_total == 0 {
        return None;
    }

    let sys = (!sys_kv.is_empty()).then(|| {
        build_system_details(SystemDetailsInput {
            sys: &sys_kv,
            cpu_nums: &cpu_nums,
            mem_total,
            mem_avail,
            mem_buffers,
            mem_cached,
            swap_total,
            swap_free,
            net_counters: &net_counters,
            disks: &disks,
        })
    });

    Some(SessionEvent::ResourceStats {
        cpu_percent,
        mem_used_kib: mem_total.saturating_sub(mem_avail),
        mem_total_kib: mem_total,
        swap_used_kib: swap_total.saturating_sub(swap_free),
        swap_total_kib: swap_total,
        net,
        disks,
        sys: Box::new(sys),
    })
}

pub(crate) fn sys_value(sys: &std::collections::HashMap<String, String>, key: &str) -> String {
    sys.get(key)
        .filter(|v| !v.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| "-".to_string())
}

pub(crate) fn kib_size(kib: u64) -> String {
    format_size(kib.saturating_mul(1024))
}

pub(crate) fn percent_text(used: u64, total: u64) -> String {
    if total == 0 {
        "-".to_string()
    } else {
        format!("{:.1}%", used as f64 * 100.0 / total as f64)
    }
}

pub(crate) fn cpu_usage_rows(nums: &[u64]) -> Vec<(String, String)> {
    let labels = [
        ("用户", "User"),
        ("Nice", "Nice"),
        ("系统", "System"),
        ("空闲", "Idle"),
        ("IO", "IO"),
        ("硬件中断", "IRQ"),
        ("软件中断", "SoftIRQ"),
        ("实时", "Steal"),
    ];
    let total = nums.iter().copied().fold(0u64, u64::saturating_add);
    labels
        .iter()
        .enumerate()
        .map(|(idx, (zh, en))| {
            let value = nums.get(idx).copied().unwrap_or(0);
            let pct = if total == 0 {
                "0.0%".to_string()
            } else {
                format!("{:.1}%", value as f64 * 100.0 / total as f64)
            };
            (t(zh, en).to_string(), pct)
        })
        .collect()
}

pub(crate) struct SystemDetailsInput<'a> {
    pub(crate) sys: &'a std::collections::HashMap<String, String>,
    pub(crate) cpu_nums: &'a [u64],
    pub(crate) mem_total: u64,
    pub(crate) mem_avail: u64,
    pub(crate) mem_buffers: u64,
    pub(crate) mem_cached: u64,
    pub(crate) swap_total: u64,
    pub(crate) swap_free: u64,
    pub(crate) net_counters: &'a [(String, u64, u64)],
    pub(crate) disks: &'a [(String, u64, u64)],
}

pub(crate) fn build_system_details(input: SystemDetailsInput) -> SystemDetails {
    let SystemDetailsInput {
        sys,
        cpu_nums,
        mem_total,
        mem_avail,
        mem_buffers,
        mem_cached,
        swap_total,
        swap_free,
        net_counters,
        disks,
    } = input;
    let mem_used = mem_total.saturating_sub(mem_avail);
    let swap_used = swap_total.saturating_sub(swap_free);
    let cpu_model = sys_value(sys, "CPU_MODEL");
    let gpu = sys.get("GPU").cloned().unwrap_or_default();
    let gpu_info = if gpu.trim().is_empty() {
        Vec::new()
    } else {
        vec![
            (t("名称", "Name").to_string(), gpu),
            (t("厂商", "Vendor").to_string(), "-".to_string()),
            (t("驱动", "Driver").to_string(), "-".to_string()),
            (t("内存", "Memory").to_string(), "-".to_string()),
        ]
    };

    SystemDetails {
        overview: vec![
            (
                t("操作系统", "Operating system").to_string(),
                sys_value(sys, "OS"),
            ),
            (
                t("内核版本", "Kernel version").to_string(),
                sys_value(sys, "KERNEL_RELEASE"),
            ),
            (
                t("主机名称", "Hostname").to_string(),
                sys_value(sys, "HOSTNAME"),
            ),
            (t("IP", "IP").to_string(), sys_value(sys, "IPS")),
            (t("负载", "Load").to_string(), sys_value(sys, "LOAD")),
            (t("内核", "Kernel").to_string(), sys_value(sys, "KERNEL")),
            (
                t("硬件架构", "Architecture").to_string(),
                sys_value(sys, "ARCH"),
            ),
            (t("连接", "Connection").to_string(), sys_value(sys, "IPS")),
            (t("运行", "Uptime").to_string(), sys_value(sys, "UPTIME")),
        ],
        cpu_info: vec![
            (t("名称", "Name").to_string(), cpu_model),
            (
                t("核心数", "Cores").to_string(),
                sys_value(sys, "CPU_CORES"),
            ),
            (t("频率", "Frequency").to_string(), "-".to_string()),
            (t("缓存", "Cache").to_string(), sys_value(sys, "CPU_CACHE")),
            ("BogoMips".to_string(), sys_value(sys, "CPU_BOGO")),
        ],
        gpu_info,
        cpu_usage: cpu_usage_rows(cpu_nums),
        memory: vec![
            (t("总计", "Total").to_string(), kib_size(mem_total)),
            (t("已使用", "Used").to_string(), kib_size(mem_used)),
            (t("剩余", "Free").to_string(), kib_size(mem_avail)),
            (
                t("已用", "Usage").to_string(),
                percent_text(mem_used, mem_total),
            ),
            (t("缓冲", "Buffers").to_string(), kib_size(mem_buffers)),
            (t("缓存", "Cached").to_string(), kib_size(mem_cached)),
        ],
        swap: vec![
            (t("总计", "Total").to_string(), kib_size(swap_total)),
            (t("已使用", "Used").to_string(), kib_size(swap_used)),
            (t("剩余", "Free").to_string(), kib_size(swap_free)),
            (
                t("已用", "Usage").to_string(),
                percent_text(swap_used, swap_total),
            ),
        ],
        networks: net_counters
            .iter()
            .map(|(name, rx, tx)| {
                (
                    name.clone(),
                    format_size(*tx),
                    format_size(*rx),
                    "-".to_string(),
                    "-".to_string(),
                )
            })
            .collect(),
        filesystems: disks
            .iter()
            .map(|(mount, avail, total)| {
                let used = total.saturating_sub(*avail);
                (
                    mount.clone(),
                    format_size(*total),
                    percent_text(used, *total),
                    format_size(*avail),
                    mount.clone(),
                )
            })
            .collect(),
    }
}

/// Parse one `ps -eo pid,user,pcpu,pmem,args` line into a [`ProcInfo`]. The
/// header row (`PID` is not numeric) and any malformed line yield `None`.
/// `args` (everything past the four fixed columns) keeps internal spacing
/// collapsed — fine for a display-only command column.
pub(crate) fn parse_ps_line(line: &str) -> Option<ProcInfo> {
    let mut it = line.split_whitespace();
    let pid: u32 = it.next()?.parse().ok()?;
    let user = it.next()?.to_string();
    let cpu: f32 = it.next()?.parse().ok()?;
    let mem: f32 = it.next()?.parse().ok()?;
    let command = it.collect::<Vec<_>>().join(" ");
    if command.is_empty() {
        return None;
    }
    Some(ProcInfo {
        pid,
        user,
        cpu,
        mem,
        command,
    })
}

/// Parse one `df -kP` data line into `(mount, available_bytes, total_bytes)`.
/// Columns: `Filesystem 1024-blocks Used Available Capacity Mounted-on`.
pub(crate) fn parse_df_line(line: &str) -> Option<(String, u64, u64)> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 6 || f[0] == "Filesystem" {
        return None;
    }
    let total_kb: u64 = f[1].parse().ok()?;
    let avail_kb: u64 = f[3].parse().ok()?;
    if total_kb == 0 {
        return None;
    }
    // Mount point is the last column (joined in case it contains spaces).
    let mount = f[5..].join(" ");
    // Saturating: a server can report arbitrary block counts; KiB→bytes must
    // not overflow-panic in debug (#27).
    Some((
        mount,
        avail_kb.saturating_mul(1024),
        total_kb.saturating_mul(1024),
    ))
}

/// Extract the leading integer (KiB) from a `/proc/meminfo` value like
/// `"  3288560 kB"`.
pub(crate) fn parse_meminfo_kib(s: &str) -> u64 {
    s.split_whitespace()
        .next()
        .and_then(|x| x.parse().ok())
        .unwrap_or(0)
}

/// Parse one `/proc/net/dev` data line into `(iface, (rx_bytes, tx_bytes))`.
/// Format: `  eth0: <rx_bytes> <rx_pkts> ... <tx_bytes> <tx_pkts> ...`
/// (16 numeric columns; rx_bytes is col 0, tx_bytes is col 8).  The `lo`
/// loopback interface is skipped — it never reflects real traffic.
pub(crate) fn parse_net_dev_line(line: &str) -> Option<(String, (u64, u64))> {
    let (name, rest) = line.split_once(':')?;
    let iface = name.trim();
    if iface.is_empty() || iface == "lo" || iface.contains(' ') {
        return None;
    }
    let nums: Vec<u64> = rest
        .split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect();
    if nums.len() < 9 {
        return None;
    }
    Some((iface.to_string(), (nums[0], nums[8])))
}
