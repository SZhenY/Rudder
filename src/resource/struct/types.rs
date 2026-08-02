use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::resource::system::SystemSnapshot;
use crate::ssh::{ProcInfo, SystemDetails};

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
    pub(crate) disks: Vec<(String, u64, u64)>,
    pub(crate) procs: Vec<ProcInfo>,
    pub(crate) sys: SystemDetails,
}

pub(crate) type TabStatuses = Arc<Mutex<HashMap<String, TabStatus>>>;
pub(crate) type LocalSnap = Arc<Mutex<SystemSnapshot>>;
pub(crate) type NetHist = Arc<Mutex<Vec<f32>>>;
