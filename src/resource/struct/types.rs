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
