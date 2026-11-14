use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::ssh::{ProcInfo, SystemDetails};

#[derive(Clone, Default)]
pub(crate) struct TabStatus {
    pub(crate) host: String,
    pub(crate) user: String,
    pub(crate) session_id: String,
    /// 该标签页的传输类型：决定 MCP 的 `run_command` 走 exec 通道还是本地进程。
    pub(crate) kind: crate::config::SessionKind,
    pub(crate) state: u8,
    pub(crate) cpu: f32,
    pub(crate) mem_used_kib: u64,
    pub(crate) mem_total_kib: u64,
    pub(crate) swap_used_kib: u64,
    pub(crate) swap_total_kib: u64,
    pub(crate) net: Vec<(String, u64, u64)>,
    pub(crate) selected_iface: String,
    pub(crate) net_hist: Vec<f32>,
    /// CPU 使用率历史（0..100，环形缓冲）。综合页趋势卡用它；与 `net_hist` 同长度。
    pub(crate) cpu_hist: Vec<f32>,
    /// CPU 使用率的指数平滑值（α=0.25）：先平滑再入环，曲线不会随单拍抖动。
    pub(crate) cpu_ema: Option<f32>,
    pub(crate) disks: Vec<(String, u64, u64)>,
    pub(crate) procs: Vec<ProcInfo>,
    pub(crate) sys: SystemDetails,
    /// A local-shell tab (WSL / cmd / PowerShell). These reach the connected
    /// state but never produce remote resource stats, so the resource page falls
    /// back to the local machine's own figures instead of showing zeroes.
    pub(crate) is_local: bool,
    /// 最近一次拿到资源样本的时刻（不落盘）。MCP 的 `get_tab_status` 用它判断
    /// 数据是否已陈旧 —— 窗口隐藏时采样暂停，恢复后要等新样本。
    pub(crate) last_stats_at: Option<std::time::Instant>,
}

pub(crate) type TabStatuses = Arc<Mutex<HashMap<String, TabStatus>>>;
