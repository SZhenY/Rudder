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
    let interval = if interactive {
        INTERACTIVE_RENDER_MIN_INTERVAL
    } else {
        RENDER_MIN_INTERVAL
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

