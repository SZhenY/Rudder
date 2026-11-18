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

/// One parsed monitor block. Splitting collection from rate calculation
/// keeps the echo-marker sections testable without fabricating jiffy history.
#[derive(Default)]
struct MonitorSample {
    cpu_total: u64,
    cpu_idle: u64,
    /// user + nice 的累计 jiffies，与 `cpu_total` 同一时刻，用于 User 分项差分。
    cpu_user: u64,
    /// system + irq + softirq 的累计 jiffies，用于 Sys 分项差分。
    cpu_sys: u64,
    have_cpu: bool,
    mem_total: u64,
    mem_avail: u64,
    mem_buffers: u64,
    mem_cached: u64,
    cpu_nums: Vec<u64>,
    net_now: Vec<(String, u64, u64)>,
    /// 默认网关（`__GW__` 段，5 拍采一次；其余拍为空）。
    gw: String,
    disks: Vec<(String, u64, u64)>,
    /// `lsblk -P` 原始行：用来把「/ 所在分区」上溯到整块物理盘并汇总用量。
    lsblk: Vec<LsblkRow>,
    seen_fs: std::collections::HashSet<(u64, u64)>,
    sys_kv: std::collections::HashMap<String, String>,
}

/// `lsblk -b -P -o NAME,PKNAME,TYPE,SIZE,MOUNTPOINT` 的一行。
#[derive(Default)]
pub(crate) struct LsblkRow {
    name: String,
    pkname: String,
    kind: String,
    size: u64,
    /// 可能是逗号分隔的多个挂载点（btrfs 子卷）。
    mounts: String,
}

/// 上一次 `/proc/stat` 的累计 jiffies：总、空闲、user、system 四路。
///
/// 只存总量算不出 User/Sys 分项，所以四路一起留。
#[derive(Clone, Copy, Default)]
pub(crate) struct CpuPrev {
    total: u64,
    idle: u64,
    user: u64,
    sys: u64,
}

/// Cap interfaces / filesystems per sample so a hostile server cannot flood
/// the parser and sidebar with fabricated rows (#27).
const MAX_MON_ENTRIES: usize = 64;

fn collect_monitor_sample(block: &str) -> MonitorSample {
    let mut s = MonitorSample::default();
    // Sections are split by `echo` markers; lines before the first marker
    // belong to the cpu/mem/net block.
    enum Section {
        Top,
        Df,
        Gw,
        Lsblk,
        Sys,
    }
    let mut section = Section::Top;
    for line in block.lines() {
        if line == "__DF__" {
            section = Section::Df;
            continue;
        }
        if line == "__GW__" {
            section = Section::Gw;
            continue;
        }
        if line == "__LSBLK__" {
            section = Section::Lsblk;
            continue;
        }
        if line == "__SYS__" {
            section = Section::Sys;
            continue;
        }
        match section {
            Section::Df => {
                if s.disks.len() < MAX_MON_ENTRIES
                    && let Some((mount, avail, total)) = parse_df_line(line)
                {
                    // Set-style dedup: skip a filesystem whose (total, available)
                    // we've already added — collapses the dozens of identical
                    // Docker overlay mounts a NAS reports down to one row (#38).
                    if s.seen_fs.insert((total, avail)) {
                        s.disks.push((mount, avail, total));
                    }
                }
                continue;
            }
            Section::Gw => {
                if let Some(v) = line.trim().strip_prefix("GW=") {
                    s.gw = v.trim().to_string();
                }
                continue;
            }
            Section::Lsblk => {
                if s.lsblk.len() < MAX_MON_ENTRIES
                    && let Some(row) = parse_lsblk_line(line)
                {
                    s.lsblk.push(row);
                }
                continue;
            }
            Section::Sys => {
                if let Some((k, v)) = line.split_once('=') {
                    s.sys_kv.insert(k.trim().to_string(), v.trim().to_string());
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
                s.cpu_total = nums.iter().copied().fold(0u64, u64::saturating_add);
                s.cpu_idle = nums[3].saturating_add(nums.get(4).copied().unwrap_or(0)); // idle + iowait
                // top 的 us 含 nice、sy 含 irq/softirq；缺列按 0 处理。
                s.cpu_user = nums[0].saturating_add(nums.get(1).copied().unwrap_or(0));
                s.cpu_sys = nums[2]
                    .saturating_add(nums.get(5).copied().unwrap_or(0))
                    .saturating_add(nums.get(6).copied().unwrap_or(0));
                s.have_cpu = true;
                s.cpu_nums = nums;
            }
        } else if let Some(v) = line.strip_prefix("MemTotal:") {
            s.mem_total = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            s.mem_avail = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("Buffers:") {
            s.mem_buffers = parse_meminfo_kib(v);
        } else if let Some(v) = line.strip_prefix("Cached:") {
            s.mem_cached = parse_meminfo_kib(v);
        } else if s.net_now.len() < MAX_MON_ENTRIES
            && let Some((iface, counters)) = parse_net_dev_line(line)
        {
            s.net_now.push((iface, counters.0, counters.1));
        }
    }

    s
}

/// Parse one monitor sample (a block of `/proc/stat` cpu line + `/proc/meminfo`
/// fields) into a [`SessionEvent::ResourceStats`].
///
/// CPU usage needs two consecutive `/proc/stat` snapshots; `prev` carries the
/// previous jiffies split (total / idle / user / system) across calls.  The
/// first sample therefore reports 0% (no baseline yet).
pub(crate) fn parse_monitor_block(
    block: &str,
    prev: &mut Option<CpuPrev>,
    prev_net: &mut std::collections::HashMap<String, (u64, u64)>,
    prev_net_at: &mut std::time::Instant,
) -> Option<SessionEvent> {
    let MonitorSample {
        cpu_total,
        cpu_idle,
        cpu_user,
        cpu_sys,
        have_cpu,
        mem_total,
        mem_avail,
        mem_buffers,
        mem_cached,
        cpu_nums,
        net_now,
        gw,
        disks,
        lsblk,
        seen_fs: _,
        sys_kv,
    } = collect_monitor_sample(block);

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

    // 三路百分比共用同一个总 jiffies 增量：User / Sys 是总占用里的分项，
    // 分母必须是 CPU 的全部时间片，不能各自归一化。
    let (cpu_percent, cpu_user_percent, cpu_sys_percent) = if have_cpu {
        let result = match *prev {
            Some(p) => {
                let dt = cpu_total.saturating_sub(p.total);
                if dt > 0 {
                    let dt = dt as f32;
                    (
                        (1.0 - cpu_idle.saturating_sub(p.idle) as f32 / dt).clamp(0.0, 1.0),
                        (cpu_user.saturating_sub(p.user) as f32 / dt).clamp(0.0, 1.0),
                        (cpu_sys.saturating_sub(p.sys) as f32 / dt).clamp(0.0, 1.0),
                    )
                } else {
                    (0.0, 0.0, 0.0)
                }
            }
            None => (0.0, 0.0, 0.0),
        };
        *prev = Some(CpuPrev {
            total: cpu_total,
            idle: cpu_idle,
            user: cpu_user,
            sys: cpu_sys,
        });
        result
    } else {
        (0.0, 0.0, 0.0)
    };

    // Need at least memory numbers to be a useful sample.
    if mem_total == 0 {
        return None;
    }

    // 整块系统盘口径：`/` 所在分区上溯到物理盘，总量取整盘容量，
    // 已用按该盘上每个已挂载分区的用量汇总（同一设备的多个挂载点只计一次，
    // 否则 btrfs 子卷会把同一份占用重复累加）。
    let disk_bytes = system_disk_bytes(&disks, &lsblk);

    let sys = (!sys_kv.is_empty()).then(|| {
        build_system_details(SystemDetailsInput {
            sys: &sys_kv,
            cpu_nums: &cpu_nums,
            mem_total,
            mem_avail,
            mem_buffers,
            mem_cached,
            net_counters: &net_counters,
            disks: &disks,
        })
    });

    Some(SessionEvent::ResourceStats {
        cpu_percent,
        cpu_user_percent,
        cpu_sys_percent,
        mem_used_kib: mem_total.saturating_sub(mem_avail),
        mem_total_kib: mem_total,
        net,
        // 网关 5 拍才采一次：其余拍留 None，由消费端沿用上一次的值。
        gateway: (!gw.is_empty()).then_some(gw),
        // 整块系统盘口径（0 = 本轮没采到，沿用上一次）。
        disk_used_bytes: disk_bytes.map(|d| d.0).unwrap_or(0),
        disk_total_bytes: disk_bytes.map(|d| d.1).unwrap_or(0),
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
        net_counters,
        disks,
    } = input;
    let mem_used = mem_total.saturating_sub(mem_avail);
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
        host: HostCard {
            name: sys_value(sys, "HOSTNAME"),
            os: sys_value(sys, "OS"),
            kernel: sys_value(sys, "KERNEL_RELEASE"),
            uptime: sys_value(sys, "UPTIME"),
            load: sys_value(sys, "LOAD"),
        },
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

/// Parse one `lsblk -b -P` line (`NAME="sda" PKNAME="" TYPE="disk" SIZE="…"`).
/// 这些列都不含空格；`MOUNTPOINT` 为逗号分隔的多挂载点。
fn parse_lsblk_line(line: &str) -> Option<LsblkRow> {
    let mut row = LsblkRow::default();
    for kv in line.split_whitespace() {
        let Some((key, value)) = kv.split_once('=') else {
            continue;
        };
        let value = value.trim_matches('"');
        match key {
            "NAME" => row.name = value.to_string(),
            "PKNAME" => row.pkname = value.to_string(),
            "TYPE" => row.kind = value.to_string(),
            "SIZE" => row.size = value.parse().unwrap_or(0),
            "MOUNTPOINT" | "MOUNTPOINTS" => row.mounts = value.to_string(),
            _ => {}
        }
    }
    if row.name.is_empty() { None } else { Some(row) }
}

/// 把一行上溯到它所属的物理盘（`TYPE="disk"`）。LVM / dm 可能有多层。
fn disk_of<'a>(row: &'a LsblkRow, rows: &'a [LsblkRow]) -> Option<&'a LsblkRow> {
    if row.kind == "disk" {
        return Some(row);
    }
    let mut name = row.pkname.clone();
    for _ in 0..8 {
        let cur = rows.iter().find(|r| r.name == name)?;
        if cur.kind == "disk" {
            return Some(cur);
        }
        if cur.pkname.is_empty() {
            return None;
        }
        name = cur.pkname.clone();
    }
    None
}

/// 系统盘用量 `(used_bytes, total_bytes)`：`/` 所在分区所属的整块物理盘。
///
/// `used` 按该盘上**每个已挂载分区**的已用量求和；同一分区的多个挂载点
/// （btrfs 子卷、bind mount）取最大值，不重复累加。
/// 任一环节缺数据（无 `lsblk`、`/` 在 overlay/tmpfs 上、md/RAID 无父盘）→ `None`，
/// 调用方回退到 `/` 分区口径。
fn system_disk_bytes(
    disks: &[(String, u64, u64)],
    rows: &[LsblkRow],
) -> Option<(u64, u64)> {
    let root = rows
        .iter()
        .find(|r| r.mounts.split(',').any(|m| m.trim() == "/"))?;
    let disk = disk_of(root, rows)?;
    if disk.size == 0 {
        return None;
    }
    let mut per_device: std::collections::HashMap<&str, u64> =
        std::collections::HashMap::new();
    for (mount, avail, total) in disks {
        let Some(row) = rows
            .iter()
            .find(|r| r.mounts.split(',').any(|m| m.trim() == mount))
        else {
            continue;
        };
        if disk_of(row, rows).map(|d| d.name.as_str()) != Some(disk.name.as_str()) {
            continue;
        }
        let used = total.saturating_sub(*avail);
        let slot = per_device.entry(row.name.as_str()).or_insert(0);
        *slot = (*slot).max(used);
    }
    let used = per_device
        .values()
        .copied()
        .fold(0u64, u64::saturating_add);
    Some((used, disk.size))
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

#[cfg(test)]
#[path = "monitor_sample_tests.rs"]
mod monitor_sample_tests;
