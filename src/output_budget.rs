//! Bounds terminal Output bytes buffered between session pumps and the UI.
//!
//! Output events go through `UnboundedSender<SessionEvent>` because SFTP shares
//! the same sender type for host-key and credential prompts. Instead of changing
//! that type, every Output chunk is accounted here and the pumps stop reading
//! from the PTY/socket once the budget is full, which propagates TCP/PTY flow
//! control back to the remote process.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use crate::ssh::SessionEvent;

/// One Output event is capped at 64 KiB, so 64 MiB is at most 1024 queued chunks.
/// The budget is process-wide: every session shares it.
pub(crate) const OUTPUT_QUEUE_BYTE_BUDGET: usize = 64 * 1024 * 1024;

/// Fallback poll interval: `notify_waiters` can miss a waiter that has not yet
/// been polled, so a bounded re-check guarantees progress.
const DRAIN_POLL: Duration = Duration::from_millis(10);

/// A window-close or a stuck render gate can stop the consumer. If no release
/// happens for this long, drop the stale accounting so pumps reach `send()` and
/// end the session instead of waiting forever on a dead queue.
const STALL_LIMIT: Duration = Duration::from_secs(2);

static PENDING: AtomicUsize = AtomicUsize::new(0);
static RELEASE_SEQ: AtomicU64 = AtomicU64::new(0);
static DRAINED: Notify = Notify::const_new();

/// Count bytes pushed into the event queue.
pub(crate) fn account(len: usize) {
    PENDING.fetch_add(len, Ordering::Relaxed);
}

/// Release bytes after the UI ingested them and wake the pumps.
/// Saturating: small Output events emitted without accounting (status lines,
/// tunnel notices) must not make the counter wrap or over-throttle.
pub(crate) fn release(len: usize) {
    let _ = PENDING.try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
        Some(n.saturating_sub(len))
    });
    RELEASE_SEQ.fetch_add(1, Ordering::Release);
    DRAINED.notify_waiters();
}

/// Pause a session pump until the UI has caught up.
pub(crate) async fn wait_for_drain() {
    let mut last_seq = RELEASE_SEQ.load(Ordering::Acquire);
    let mut last_progress = Instant::now();
    while PENDING.load(Ordering::Acquire) > OUTPUT_QUEUE_BYTE_BUDGET {
        tokio::select! {
            _ = DRAINED.notified() => {}
            _ = tokio::time::sleep(DRAIN_POLL) => {}
        }
        let seq = RELEASE_SEQ.load(Ordering::Acquire);
        if seq != last_seq {
            last_seq = seq;
            last_progress = Instant::now();
        } else if last_progress.elapsed() >= STALL_LIMIT {
            // No consumer made progress: clear the stale bytes. A live session
            // re-accounts its next chunk; a dead one ends on the next send().
            PENDING.store(0, Ordering::Release);
            return;
        }
    }
}

/// Blocking counterpart for the std::thread readers of local/serial sessions.
pub(crate) fn wait_for_drain_blocking(runtime: &tokio::runtime::Handle) {
    runtime.block_on(wait_for_drain());
}

/// Send a terminal Output event and account its bytes for backpressure.
pub(crate) fn emit_output(events: &UnboundedSender<SessionEvent>, text: String) -> bool {
    if events.is_closed() {
        return false;
    }
    let len = text.len();
    account(len);
    if events.send(SessionEvent::Output(text)).is_ok() {
        true
    } else {
        // send 失败时没有消费者会释放这批字节，就地撤销记账。
        release(len);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::SessionEvent;
    use std::sync::Mutex;

    // `PENDING` is process-wide; keep the accounting tests from overlapping.
    static LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn emit_output_balances_its_accounting() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        let before = PENDING.load(Ordering::Acquire);
        assert!(emit_output(&tx, "abcd".into()));
        assert_eq!(PENDING.load(Ordering::Acquire), before + 4);
        assert!(matches!(rx.try_recv(), Ok(SessionEvent::Output(_))));
        release(4);
        assert_eq!(PENDING.load(Ordering::Acquire), before);
    }

    #[test]
    fn closed_sender_does_not_account_bytes() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        drop(rx);
        let before = PENDING.load(Ordering::Acquire);
        assert!(!emit_output(&tx, "x".repeat(1024)));
        assert_eq!(PENDING.load(Ordering::Acquire), before);
    }
}
