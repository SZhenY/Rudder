use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sysinfo::{Disks, Networks, System};

use crate::ssh::{ProcInfo, SystemDetails};

/// Snapshot passed to the UI each tick.
#[derive(Debug, Clone, Default)]
pub(crate) struct SystemSnapshot {
    pub cpu_percent: f32,
    pub mem_percent: f32,
    pub swap_percent: f32,
    pub mem_used_mib: u64,
    pub mem_total_mib: u64,
    pub swap_used_mib: u64,
    pub swap_total_mib: u64,
    pub net_bytes_per_sec: u64,
    pub net_rx_per_sec: u64,
    pub net_tx_per_sec: u64,
    /// Per-filesystem (mount, available_bytes, total_bytes)。
    ///
    /// 用 `Arc<[_]>` 而不是 `Vec<_>`：这份快照每秒被克隆两次（采样线程存一份、界面读一份），
    /// `Vec` 版本每次都要把每个挂载点的字符串深拷贝一遍 —— 而磁盘数据本来就只在
    /// `DISK_REFRESH_EVERY` 那一轮才变，那份深拷贝纯属白做。
    pub disks: Arc<[(String, u64, u64)]>,
    /// 每核占用（0..1）。远端走 `/proc/stat` 的 `cpuN` 行，不经过这里；本机只有
    /// sysinfo 这一条路 —— 之前本机没有这个字段，面板 CPU 页的「核心详情」恒为空
    /// （显示"暂无数据"）。
    pub core_usages: Arc<[f32]>,
    /// 操作系统名（`long_os_version` → `os_version` → `name` 依次兜底）。
    pub os_version: String,
    /// 本机主机名（Windows 上通常是 `DESKTOP-XXXX`，Linux 是短主机名）。
    pub host_name: String,
    /// 内核版本（macOS 是 Darwin 版本号，Windows 是 NT 版本号）。
    pub kernel_version: String,
    /// 开机时长（秒）。远端来自 `uptime -p`，本机自己算。
    pub uptime_secs: u64,
}

/// Stateful sampler. Construct once per process and poll via [`Self::sample`].
///
/// Fields are `pub(crate)` so the sampling logic in `impls/system.rs` can reach
/// them across modules (struct def lives here, impl lives in the impls module).
pub(crate) struct SystemSampler {
    pub(crate) sys: System,
    pub(crate) nets: Networks,
    pub(crate) disks: Disks,
    /// Counts down to the next disk re-enumeration; see `DISK_REFRESH_EVERY`.
    pub(crate) disk_tick: u32,
    /// 上一次磁盘 refresh 算出来的挂载点行（`Arc`：每秒分发一份只是加引用计数）。
    pub(crate) cached_disks: Arc<[(String, u64, u64)]>,
    pub(crate) last_rx_total: u64,
    pub(crate) last_tx_total: u64,
    pub(crate) last_instant: std::time::Instant,
}

#[derive(Clone, Default)]
pub(crate) struct TabStatus {
    pub(crate) host: String,
    pub(crate) user: String,
    pub(crate) session_id: String,
    pub(crate) state: u8,
    pub(crate) cpu: f32,
    pub(crate) mem_used_kib: u64,
    pub(crate) mem_total_kib: u64,
    pub(crate) swap_used_kib: u64,
    pub(crate) swap_total_kib: u64,
    pub(crate) net: Vec<(String, u64, u64)>,
    pub(crate) selected_iface: String,
    /// 勾选的网卡集合（工具面板「网络端口」里的复选框，可多选）。
    ///
    /// 存在**状态表**而不是只在界面上：采样线程也要用（`session_event` 把选中网卡
    /// 的速率喂进顶部 sparkline），那里拿不到界面属性。
    ///
    /// 语义 = **筛选**：只显示勾选的行（全选 = 全部，取消全选 = 一行不剩）。
    /// 过滤在 Rust 侧做（不把这些行放进模型）—— 早先在界面层用 `visible` 隐藏
    /// 未勾选的行，而 Slint 里布局子项 `visible: false` **仍然占位**，列表留下一堆
    /// 空格（用户反馈"太难看"）；改成"只置顶不隐藏"，复选框又勾了等于没勾
    /// （"要不然无法筛选"）。弹层另有一份**未筛选**的完整列表供选择，两者别混用。
    ///
    /// **三态**用 `Option` 表达 —— 早先只有"空集合 = 全部"两态，于是弹层里
    /// 「全部网口」/「全部磁盘」那个复选框点了等于清空、勾选态毫无变化，用户
    /// 没法"取消全选"来筛出空集（反馈：全部选项要求能够复选框取消选择）：
    /// * `None`         = **全选**（默认）
    /// * `Some(vec![])` = **一个都不选**（"筛空"，用户主动取消「全部」）
    /// * `Some(names)`  = 只勾这些
    pub(crate) picked_ifaces: Option<Vec<String>>,
    /// 勾选的挂载点集合（同上，磁盘分区用）。
    pub(crate) picked_disks: Option<Vec<String>>,
    pub(crate) net_hist: Vec<f32>,
    /// CPU 使用率环形缓冲（0..1），供工具面板的「CPU 负载趋势」用 —— 与 `net_hist`
    /// 同一套 push_ring/normalize。只有远端会话有；本机标签走本地快照。
    pub(crate) cpu_hist: Vec<f32>,
    /// 与 `cpu_hist` **逐拍严格同步**的采样时刻：**本地时间**的「当天秒数」(0..86399)。
    ///
    /// 存秒-of-day 而非 Unix 秒 —— Slint 侧没有时区支持，格式化 Unix 秒只能显示
    /// UTC。Rust 侧（chrono）把时区解析完，Slint 侧只需除法取模。跨天时数值会
    /// 回绕，但气泡只按索引取值、不做时间运算，所以单调性无关紧要。
    pub(crate) cpu_hist_t: Vec<i32>,
    /// 「CPU 负载趋势」曲线的指数移动平均状态（`None` = 尚未采到第一拍）。
    ///
    /// 只有**曲线**用平滑值，`st.cpu` 那个数字仍显示原始读数 —— 趋势图要协调、
    /// 实时读数要诚实，这是任务管理器那类系统监视器的通行做法。
    pub(crate) cpu_ema: Option<f32>,
    pub(crate) disks: Vec<(String, u64, u64)>,
    pub(crate) procs: Vec<ProcInfo>,
    /// CPU 明细（0..1，与 cpu 同一差分间隔）：用户态(+nice)/内核态(+irq+softirq)/IO 等待
    pub(crate) cpu_user: f32,
    pub(crate) cpu_system: f32,
    pub(crate) cpu_iowait: f32,
    /// 每核占用（0..1，核心顺序）
    pub(crate) core_cpus: Vec<f32>,
    pub(crate) sys: SystemDetails,
    /// A local-shell tab (WSL / cmd / PowerShell). These reach the connected
    /// state but never produce remote resource stats, so the sidebar must fall
    /// back to the local machine's own figures instead of showing zeroes.
    pub(crate) is_local: bool,
}

pub(crate) type TabStatuses = Arc<Mutex<HashMap<String, TabStatus>>>;
pub(crate) type LocalSnap = Arc<Mutex<SystemSnapshot>>;
pub(crate) type NetHist = Arc<Mutex<Vec<f32>>>;
