//! 1 Hz system sampler for the sidebar (CPU / mem / swap / network graphs).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Mutex;

use crate::resource::{LocalSnap, NetHist, SystemSampler, TabStatuses};
use slint::ComponentHandle as _;

use crate::ui::AppWindow;

use super::{push_ring, refresh_sidebar, WinActivity};

/// Spawn the 1 Hz system sampler: refreshes CPU/mem/swap and both network
/// graphs for the sidebar. Backs off to ~5 s when the window is unfocused and
/// pauses entirely while hidden or when the sidebar is collapsed.
pub(crate) fn spawn_system_sampler(
    window: &AppWindow,
    tab_statuses: &TabStatuses,
    local_snap: &LocalSnap,
    local_net_hist: &NetHist,
    activity: &Rc<Cell<WinActivity>>,
) {
    // --- System sampler (1 Hz) ------------------------------------------
    let sampler = Rc::new(Mutex::new(SystemSampler::new()));
    let weak = window.as_weak();
    let tick_sampler = sampler.clone();
    let tick_statuses = tab_statuses.clone();
    let tick_local = local_snap.clone();
    let tick_net = local_net_hist.clone();
    let tick_activity = activity.clone();
    let mut bg_tick = 0u32;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        SystemSampler::recommended_interval(),
        move || {
            // Skip the (non-trivial) sysinfo refresh + sidebar repaint when no one
            // is looking, and back off to ~5 s when the window is in the background.
            // A collapsed sidebar hides the graphs entirely, so skip too
            // (upstream b17da25).
            if weak.upgrade().map(|w| w.get_sidebar_collapsed()).unwrap_or(false) {
                return;
            }
            match tick_activity.get() {
                WinActivity::Hidden => return,
                WinActivity::Background => {
                    bg_tick = bg_tick.wrapping_add(1);
                    if !bg_tick.is_multiple_of(5) {
                        return;
                    }
                }
                WinActivity::Active => {}
            }
            let snap = {
                // A poisoned sampler mutex must not take the client down:
                // release builds use panic = "abort", so the old expect() here
                // turned any panic in the sampler thread into a dead client.
                // Skip this tick instead; the next one retries.
                let Ok(mut s) = tick_sampler.lock() else {
                    return;
                };
                s.sample()
            };
            // Append the raw local throughput to the bottom-graph ring buffer
            // (normalisation happens at display time so the graph auto-scales).
            if let Ok(mut net) = tick_net.lock() {
                push_ring(&mut net, snap.net_bytes_per_sec as f32);
            }
            // Stash the local sample; the sidebar shows it on the welcome tab
            // and in the bottom network graph.
            if let Ok(mut local) = tick_local.lock() {
                *local = snap.clone();
            }

            if let Some(w) = weak.upgrade() {
                // Everything (status, CPU/mem/swap, both graphs) follows the
                // active tab; refresh_sidebar reads the stores we just updated.
                refresh_sidebar(&w, &tick_statuses, &tick_local, &tick_net);
            }
        },
    );
    // Keep the timer alive for the entire event loop by parking it on a
    // leaked Box. Slint timers drop themselves on Drop, and we don't want
    // that here.
    Box::leak(Box::new(timer));

}
