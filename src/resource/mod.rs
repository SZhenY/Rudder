#[path = "impls/system.rs"]
pub(crate) mod system;
#[path = "struct/types.rs"]
mod types;

pub(crate) use types::{
    LocalSnap, NetHist, SystemSampler, SystemSnapshot, TabStatus, TabStatuses,
};
