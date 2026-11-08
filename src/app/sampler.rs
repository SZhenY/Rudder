//! 1 Hz system sampler for the resource status page (CPU / mem / swap / network graphs).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Mutex;

use crate::resource::{LocalSnap, NetHist, SystemSampler, TabStatuses};
use slint::ComponentHandle as _;

use crate::ui::AppWindow;

use super::{push_ring, refresh_sidebar, WinActivity};

thread_local! {
    /// 1 Hz 系统采样定时器，显式持有以保活。
    static SYSTEM_SAMPLER_TIMER: std::cell::RefCell<Option<slint::Timer>> =
        const { std::cell::RefCell::new(None) };
}

/// Spawn the 1 Hz system sampler: refreshes CPU/mem/swap and both network
/// graphs for the resource status page. Backs off to ~5 s when the window is
/// unfocused and pauses entirely while hidden.
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
            // Skip the (non-trivial) sysinfo refresh when no one is looking,
            // and back off to ~5 s when the window is in the background.
            // 运行状态已搬进 SFTP 面板，与导航条是否收起无关。
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
            let t_start = std::time::Instant::now();
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
            let sample_ms = t_start.elapsed().as_secs_f64() * 1000.0;
            // Append the raw local throughput to the bottom-graph ring buffer
            // (normalisation happens at display time so the graph auto-scales).
            if let Ok(mut net) = tick_net.lock() {
                push_ring(&mut net, snap.net_bytes_per_sec as f32);
            }
            // Stash the local sample; the status page shows it on the welcome
            // tab and in the bottom network graph.
            if let Ok(mut local) = tick_local.lock() {
                *local = snap.clone();
            }

            // Everything (status, CPU/mem/swap, both graphs) follows the active
            // tab; refresh_sidebar reads the stores we just updated.
            let stats = weak
                .upgrade()
                .map(|w| refresh_sidebar(&w, &tick_statuses, &tick_local, &tick_net))
                .unwrap_or_default();
            // 运行状态页的观测点：默认不打印，RUST_LOG=rudder::perf=debug 时每趟一行。
            // `replaced` = 模型身份被换掉的次数（目标 0）；`written` / `unchanged` 是就地写
            // 模型的结果 —— 磁盘那 9 个不刷新的 tick 应该落在 `unchanged` 上。
            tracing::debug!(
                target: "rudder::perf",
                sample_ms,
                total_ms = t_start.elapsed().as_secs_f64() * 1000.0,
                replaced = stats.replaced,
                written = stats.written,
                unchanged = stats.unchanged,
                "resource tick"
            );
        },
    );
    // 存入 thread_local 保活。
    SYSTEM_SAMPLER_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));

}
