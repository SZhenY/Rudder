use super::render_tickets::request_tab_render_from_ui;
use super::*;
use crate::ssh::{file_type_label, format_permissions};

pub(crate) struct SessionResources<'a> {
    pub(crate) bufs: &'a TermBuffers,
    pub(crate) gates: &'a RenderGates,
    pub(crate) statuses: &'a TabStatuses,
    pub(crate) local: &'a LocalSnap,
    pub(crate) local_net_hist: &'a NetHist,
}

#[path = "session_event_dispatch.rs"] mod session_event_dispatch;
pub(crate) use session_event_dispatch::*;
#[path = "session_event_helpers.rs"] mod session_event_helpers;
pub(crate) use session_event_helpers::*;
#[cfg(test)]
#[path = "session_event_tests.rs"] mod session_event_tests;
