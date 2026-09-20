//! Coalesced render ticketing for terminal tabs.
//!
//! Every tab owns a `TabRenderGate` that throttles repaints to a minimum
//! interval; a ticket records "a flush was requested at generation N" so the
//! caller can wait (bounded) for the UI thread to catch up before writing the
//! next PTY chunk into the model.

use std::sync::Arc;

use crate::terminal::{RenderGates, TabRenderGate, TermBuffers};
use crate::ui::AppWindow;

use super::terminal_ui::rebuild_tab_display;
use super::{visible_tab_ids, with_term_buf};

/// A busy or closing UI must never block a session pump indefinitely.
pub(crate) const UI_FLUSH_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

/// Max UI renders per second for a tab under sustained output (#209).
use i_slint_backend_winit::WinitWindowAccessor as _;
use slint::ComponentHandle as _;

/// 最慢一档（30Hz）—— 现在只作为节流的**上界**（见 `MAX_FRAME_INTERVAL`），
/// 实际间隔按显示器刷新率算（`frame_interval`）。
pub(crate) const RENDER_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);

/// Echo produced shortly after a physical keypress should feel immediate. This
/// temporary 120 Hz ceiling is still coalesced, then falls back to 30 Hz once
/// the user stops typing so firehose output keeps its existing CPU protection.
pub(crate) const INTERACTIVE_RENDER_MIN_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(8);

pub(crate) struct TabRenderTicket {
    gate: Arc<TabRenderGate>,
    generation: u64,
}

pub(crate) fn register_tab_render_request(
    tab_id: &str,
    gates: &RenderGates,
) -> Option<(Arc<TabRenderGate>, TabRenderTicket, bool)> {
    let gate = {
        let map = gates.lock().unwrap_or_else(|e| e.into_inner());
        map.get(tab_id).cloned()
    }?;
    let (generation, should_schedule) = gate.request()?;
    let ticket = TabRenderTicket {
        gate: gate.clone(),
        generation,
    };
    Some((gate, ticket, should_schedule))
}

pub(crate) fn request_tab_render(
    weak: slint::Weak<AppWindow>,
    tab_id: &str,
    bufs: &TermBuffers,
    gates: &RenderGates,
) -> Option<TabRenderTicket> {
    let (gate, ticket, should_schedule) = register_tab_render_request(tab_id, gates)?;
    if !should_schedule {
        return Some(ticket);
    }

    let weak2 = weak.clone();
    let tid = tab_id.to_string();
    let bufs2 = bufs.clone();
    let gate2 = gate.clone();
    // Always bounce through the event loop from pump / worker threads.
    // Never call invoke_from_event_loop from inside a UI callback — that
    // deadlocks Slint (opening a second tab then froze the whole app).
    if slint::invoke_from_event_loop(move || {
        run_coalesced_tab_render(&weak2, &tid, &bufs2, gate2);
    })
    .is_err()
    {
        // The event loop is gone. Wake any pump waiting on this ticket and
        // reject future requests instead of leaving the gate scheduled forever.
        gate.close();
    }
    Some(ticket)
}

/// UI-thread variant for synthetic Output events. It shares the same gate but
/// enters the throttle directly because invoking Slint from its own callback
/// can deadlock.
pub(crate) fn request_tab_render_from_ui(
    weak: slint::Weak<AppWindow>,
    tab_id: &str,
    bufs: &TermBuffers,
    gates: &RenderGates,
) {
    let Some((gate, _, should_schedule)) = register_tab_render_request(tab_id, gates) else {
        return;
    };
    if should_schedule {
        run_coalesced_tab_render(&weak, tab_id, bufs, gate);
    }
}

pub(crate) fn wait_for_ui_flush(ticket: Option<TabRenderTicket>) {
    if let Some(ticket) = ticket {
        let _ = ticket
            .gate
            .wait_for(ticket.generation, UI_FLUSH_ACK_TIMEOUT);
    }
}

/// 目标帧间隔 = 当前显示器的**一个刷新周期**（60Hz → 16.6ms，120Hz → 8.3ms，144Hz → 6.9ms）。
///
/// 为什么不再用写死的两档：33ms（≈30Hz）与 120Hz 屏不是整数倍关系，画面会出现"有时快、
/// 有时慢"的节奏差；反过来在 144Hz 屏上按 30Hz 渲染也白白浪费了刷新率。渲染**快过一帧没有
/// 意义** —— 多出来的帧只会被显示器丢掉（还可能因为 GPU 资源反复申请而抖）。
///
/// 显示器可能在运行期间变化（换屏 / 拖到另一块屏 / 合盖接外显），所以带 TTL 缓存：
/// 既跟得上变化，又不必每次 flush 都去问 winit。
fn frame_interval(win: &AppWindow) -> std::time::Duration {
    FRAME_INTERVAL_CACHE.with(|cell| {
        let (cached, at) = cell.get();
        if at.elapsed() < FRAME_INTERVAL_TTL {
            return cached;
        }
        let fresh = query_frame_interval(win).unwrap_or(FALLBACK_FRAME_INTERVAL);
        cell.set((fresh, std::time::Instant::now()));
        fresh
    })
}

fn query_frame_interval(win: &AppWindow) -> Option<std::time::Duration> {
    // 两层 Option：外层是"窗口是否还活着"，内层是 winit 的"显示器是否报了刷新率"。
    let mhz = win.window().with_winit_window(|ww| {
        ww.current_monitor()
            .or_else(|| ww.primary_monitor())
            .and_then(|m| m.refresh_rate_millihertz())
    })??;
    if mhz == 0 {
        return None;
    }
    // millihertz → 每帧纳秒；再夹到 [240Hz, 30Hz]，避免异常报告把节流搞坏。
    let interval = std::time::Duration::from_nanos(1_000_000_000_000 / mhz as u64);
    Some(interval.clamp(MIN_FRAME_INTERVAL, MAX_FRAME_INTERVAL))
}

/// 拿不到显示器信息时按 60Hz 兜底。
const FALLBACK_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_micros(16_667);
/// 上界：240Hz（再快没有意义）。下界沿用旧的"firehose 保护" 30Hz。
const MIN_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_micros(4_167);
const MAX_FRAME_INTERVAL: std::time::Duration = RENDER_MIN_INTERVAL;
/// 提交链路的提前量（见 `run_coalesced_tab_render` 里的注释）。
const FRAME_SLACK: std::time::Duration = std::time::Duration::from_micros(1_500);
/// 缓存有效期：显示器变化后最多 2 秒跟上来。
const FRAME_INTERVAL_TTL: std::time::Duration = std::time::Duration::from_secs(2);

thread_local! {
    /// UI 线程独占（`run_coalesced_tab_render` 只在事件循环线程跑），所以 thread_local 足够。
    static FRAME_INTERVAL_CACHE: std::cell::Cell<(std::time::Duration, std::time::Instant)> =
        std::cell::Cell::new((
            FALLBACK_FRAME_INTERVAL,
            std::time::Instant::now() - FRAME_INTERVAL_TTL,
        ));
}

/// UI-thread entry: honour the throttle, then render. Timer must be created
/// here — not on pump threads (#209).
pub(crate) fn run_coalesced_tab_render(
    weak: &slint::Weak<AppWindow>,
    tab_id: &str,
    bufs: &TermBuffers,
    gate: Arc<TabRenderGate>,
) {
    // Interactive typing short-circuits the firehose throttle: while the echo
    // window is open, render at 120 Hz so keystrokes feel immediate.
    let interactive = with_term_buf(bufs, tab_id, |b| {
        std::time::Instant::now() < b.interactive_echo_until
    })
    .unwrap_or(false);
    // 目标帧间隔 = 显示器的一个刷新周期（见 `frame_interval`）。以前写死 33ms ≈ 30Hz，
    // 与 120Hz 屏不是整数倍关系 —— 那正是"有时快、有时慢"的来源；渲染比屏幕刷新更快也没有意义。
    let interval = weak
        .upgrade()
        .map(|win| frame_interval(&win))
        .unwrap_or(FALLBACK_FRAME_INTERVAL);
    let interval = if interactive {
        interval.min(INTERACTIVE_RENDER_MIN_INTERVAL)
    } else {
        // 留出一点提前量：提交（模型冲洗 → Slint 重建 → wgpu 提交 → 合成）本身要花时间，
        // 正好卡在目标间隔上很容易"差一点点没赶上"这一帧，于是退到再下一个刷新周期 ——
        // 视觉上就是同一段输出里有的帧快、有的帧慢。往前挪一档能让绝大多数帧落在窗口内。
        interval.saturating_sub(FRAME_SLACK)
    };
    let delay = gate.flush_delay(interval);

    let weak2 = weak.clone();
    let tid = tab_id.to_string();
    let bufs2 = bufs.clone();

    if delay.is_zero() {
        do_tab_render_flush(&weak2, &tid, &bufs2, gate);
    } else {
        slint::Timer::single_shot(delay, move || {
            do_tab_render_flush(&weak2, &tid, &bufs2, gate);
        });
    }
}

/// UI-thread only: commit the vt100 snapshot to Slint's model, then reschedule
/// if output arrived after this snapshot began. `request_redraw` is asynchronous,
/// so completion acknowledges a model flush rather than GPU presentation.
pub(crate) fn do_tab_render_flush(
    weak: &slint::Weak<AppWindow>,
    tab_id: &str,
    bufs: &TermBuffers,
    gate: Arc<TabRenderGate>,
) {
    let Some(through) = gate.begin_flush() else {
        return;
    };

    let visible = if let Some(win) = weak.upgrade() {
        if visible_tab_ids(&win).contains(tab_id) {
            rebuild_tab_display(&win, bufs, tab_id);
            true
        } else {
            false
        }
    } else {
        false
    };

    if gate.finish_flush(through, visible) {
        let weak2 = weak.clone();
        let tid = tab_id.to_string();
        let bufs2 = bufs.clone();
        // Defer the continuation to avoid recursive flushes for hidden tabs,
        // whose last-visible timestamp intentionally does not throttle them.
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            run_coalesced_tab_render(&weak2, &tid, &bufs2, gate);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn gates_with(tab_ids: &[&str]) -> RenderGates {
        let mut map = HashMap::new();
        for id in tab_ids {
            map.insert(
                (*id).to_string(),
                Arc::new(TabRenderGate::new(RENDER_MIN_INTERVAL)),
            );
        }
        Arc::new(Mutex::new(map))
    }

    /// 已关闭 / 尚未建 gate 的标签页不再进入渲染流程。
    #[test]
    fn register_returns_none_for_unknown_tab() {
        assert!(register_tab_render_request("ghost", &gates_with(&["t1"])).is_none());
    }

    /// 节流的全部意义：一串请求里**只有第一个**真去调度，其余合并进同一帧。
    #[test]
    fn register_coalesces_a_burst_into_one_schedule() {
        let gates = gates_with(&["t1"]);
        let (_, first, schedule_first) = register_tab_render_request("t1", &gates).unwrap();
        let (_, second, schedule_second) = register_tab_render_request("t1", &gates).unwrap();
        assert!(schedule_first, "首个请求要调度");
        assert!(!schedule_second, "紧随其后的请求被合并进同一帧");
        assert_ne!(
            first.generation, second.generation,
            "世代必须递增 —— 等待者靠它判断自己等的是哪一帧"
        );
    }

    /// 等待是有界的：没人 settle 时必须在 ~50ms 内返回，绝不能把 pump 线程挂死。
    #[test]
    fn wait_for_ui_flush_is_bounded() {
        let gates = gates_with(&["t1"]);
        let (_, ticket, _) = register_tab_render_request("t1", &gates).unwrap();
        let started = std::time::Instant::now();
        wait_for_ui_flush(Some(ticket)); // 没有任何人 settle
        let elapsed = started.elapsed();
        assert!(
            elapsed >= UI_FLUSH_ACK_TIMEOUT,
            "至少要等满一轮超时：{elapsed:?}"
        );
        assert!(
            elapsed < UI_FLUSH_ACK_TIMEOUT * 4,
            "但不能无界等待：{elapsed:?}"
        );
        wait_for_ui_flush(None); // 没有票据时立即返回、不 panic
    }

    /// 节流策略常量之间的关系（防策略漂移）：打字比常态快，且节流间隔必须小于应答超时。
    #[test]
    fn throttle_constants_stay_consistent() {
        assert_eq!(
            INTERACTIVE_RENDER_MIN_INTERVAL,
            std::time::Duration::from_millis(8),
            "交互式 ≈120Hz"
        );
        assert_eq!(
            RENDER_MIN_INTERVAL,
            std::time::Duration::from_millis(33),
            "常态 ≈30Hz"
        );
        assert!(
            INTERACTIVE_RENDER_MIN_INTERVAL < RENDER_MIN_INTERVAL,
            "打字时的回显必须更快"
        );
        assert!(
            RENDER_MIN_INTERVAL < UI_FLUSH_ACK_TIMEOUT,
            "节流间隔不能超过应答超时，否则票据还没过期就先超时了"
        );
    }
}

