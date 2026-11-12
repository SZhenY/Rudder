//! 关闭子窗口 / 标签页后的资源回收。wgpu 设备随窗口 `drop` 释放，这里只处理堆页与
//! 进程工作集。Windows 的 `SetProcessWorkingSetSize(-1, -1)` 会连主窗口的常驻页一起
//! 转到 standby，前台执行会让任务管理器读数低于真实基线、下次交互产生软缺页，因此
//! 只在整个应用持续隐藏、没有会话与传输时调用。

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use crate::app::WinActivity;

thread_local! {
    /// 每次活动 +1：定时器醒来发现变了就放弃，让最后一次关闭统一收尾。
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    /// 上次真正裁剪工作集的时间，用来限频。
    static LAST_TRIMMED: Cell<Option<Instant>> = const { Cell::new(None) };
    /// 上次观察到窗口可见 / 有会话 / 有传输的时间；None = 尚未观察到活动。
    static LAST_BUSY: Cell<Option<Instant>> = const { Cell::new(None) };
    /// 深度空闲观察器的定时器句柄，drop 会停掉定时器。
    static IDLE_OBSERVER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
}

/// 关完等这么久再回收堆页，避开「关一个标签马上又开一个」。
const RELEASE_DELAY: Duration = Duration::from_secs(3);
/// 整个应用持续隐藏、无活动多久才允许裁剪工作集。
pub(crate) const IDLE_TRIM_DELAY: Duration = Duration::from_secs(60);
/// 两次裁剪最短间隔。
const MIN_INTERVAL: Duration = Duration::from_secs(60);
/// 状态观察频率；低于空闲阈值即可，不需要更密。
const OBSERVE_INTERVAL: Duration = Duration::from_secs(30);

/// 触发回收的事件，只用于日志。关子窗和关标签都不允许裁剪进程工作集。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReclaimReason {
    SubWindowClosed,
    TabClosed,
}

/// UI 线程观察到的应用状态。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrimSnapshot {
    pub activity: WinActivity,
    /// 任一子窗口可见；主窗口的可见性由 `activity` 表达。
    pub has_visible_window: bool,
    pub live_sessions: bool,
    pub active_transfers: bool,
}

fn disabled() -> bool {
    std::env::var_os("RUDDER_NO_MEM_TRIM").is_some_and(|value| value != "0")
}

fn generation() -> u64 {
    GENERATION.with(|slot| slot.get())
}

/// 记一次活动：作废已排期的回收，并重置深度空闲计时。
pub(crate) fn note_activity() {
    GENERATION.with(|slot| slot.set(slot.get().wrapping_add(1)));
    LAST_BUSY.with(|slot| slot.set(Some(Instant::now())));
}

/// 裁剪策略：整个应用无可见窗口、无会话与传输、且已持续空闲够久。
pub(crate) fn ws_trim_allowed(snapshot: TrimSnapshot, idle_for: Duration) -> bool {
    !snapshot.has_visible_window
        && snapshot.activity == WinActivity::Hidden
        && !snapshot.live_sessions
        && !snapshot.active_transfers
        && idle_for >= IDLE_TRIM_DELAY
}

/// 关子窗 / 关标签后的真实回收：安静几秒后归还堆顶空闲页。进程级工作集裁剪不在
/// 这里执行 —— 关一个子窗不能清掉主窗口的常驻页。
pub(crate) fn reclaim_after_release(reason: ReclaimReason) {
    note_activity();
    let expected = generation();
    slint::Timer::single_shot(RELEASE_DELAY, move || {
        if generation() != expected {
            return;
        }
        trim_heap();
        tracing::debug!(
            sub_window = matches!(reason, ReclaimReason::SubWindowClosed),
            "release reclaimed free heap pages"
        );
    });
}

/// 注册 UI 线程的深度空闲观察器；闭包每 30s 取一次应用状态。
pub(crate) fn spawn_idle_observer<F>(snapshot: F)
where
    F: Fn() -> TrimSnapshot + 'static,
{
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, OBSERVE_INTERVAL, move || {
        observe(snapshot());
    });
    IDLE_OBSERVER.with(|slot| *slot.borrow_mut() = Some(timer));
}

/// UI 线程周期调用：维护空闲计时，条件持续成立才裁剪进程工作集。
fn observe(snapshot: TrimSnapshot) {
    let now = Instant::now();
    let busy = snapshot.has_visible_window
        || snapshot.activity != WinActivity::Hidden
        || snapshot.live_sessions
        || snapshot.active_transfers;
    if busy {
        LAST_BUSY.with(|slot| slot.set(Some(now)));
        return;
    }
    let idle_for = match LAST_BUSY.with(|slot| slot.get()) {
        Some(at) => now.duration_since(at),
        None => {
            // 首次进入空闲：从这里开始计时，否则 idle 永远算 0。
            LAST_BUSY.with(|slot| slot.set(Some(now)));
            return;
        }
    };
    if !ws_trim_allowed(snapshot, idle_for) || disabled() {
        return;
    }
    if LAST_TRIMMED.with(|slot| slot.get().is_some_and(|at| at.elapsed() < MIN_INTERVAL)) {
        return;
    }
    LAST_TRIMMED.with(|slot| slot.set(Some(now)));
    LAST_BUSY.with(|slot| slot.set(Some(now)));
    tracing::warn!(
        idle_secs = idle_for.as_secs(),
        hidden = snapshot.activity == WinActivity::Hidden,
        visible_window = snapshot.has_visible_window,
        "memory: trimmed process working set after sustained idle"
    );
    trim_working_set();
}

#[cfg(windows)]
fn trim_heap() {}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_heap() {
    // SAFETY: malloc_trim 只释放当前进程堆顶的空闲页。
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(all(target_os = "linux", not(target_env = "gnu")))]
fn trim_heap() {}

#[cfg(target_os = "macos")]
fn trim_heap() {}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn trim_heap() {}

#[cfg(windows)]
fn trim_working_set() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn SetProcessWorkingSetSize(
            process: *mut core::ffi::c_void,
            minimum: usize,
            maximum: usize,
        ) -> i32;
    }
    // SAFETY: 两个都是无状态的系统调用，只影响当前进程；-1 表示「尽量归还」。
    unsafe {
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn trim_working_set() {
    // SAFETY: malloc_trim 只释放当前进程堆顶的空闲页。
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(all(target_os = "linux", not(target_env = "gnu")))]
fn trim_working_set() {}

#[cfg(target_os = "macos")]
fn trim_working_set() {
    #[link(name = "System")]
    unsafe extern "C" {
        /// zone 传 null = 所有 zone；goal 0 = 尽量归还。
        fn malloc_zone_pressure_relief(zone: *mut core::ffi::c_void, goal: usize) -> usize;
    }
    // SAFETY: 只影响当前进程的 allocator 状态。
    unsafe {
        let _ = malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn trim_working_set() {}

#[cfg(test)]
mod trim_policy_tests {
    use super::{IDLE_TRIM_DELAY, TrimSnapshot, ws_trim_allowed};
    use crate::app::WinActivity;
    use std::time::Duration;

    fn snapshot(
        activity: WinActivity,
        has_visible_window: bool,
        live_sessions: bool,
        active_transfers: bool,
    ) -> TrimSnapshot {
        TrimSnapshot {
            activity,
            has_visible_window,
            live_sessions,
            active_transfers,
        }
    }

    #[test]
    fn trimming_needs_hidden_idle_and_no_activity() {
        let idle = snapshot(WinActivity::Hidden, false, false, false);
        assert!(ws_trim_allowed(idle, IDLE_TRIM_DELAY));
        assert!(ws_trim_allowed(idle, IDLE_TRIM_DELAY + Duration::from_secs(30)));
        assert!(!ws_trim_allowed(idle, IDLE_TRIM_DELAY - Duration::from_secs(1)));
        assert!(!ws_trim_allowed(idle, Duration::ZERO));
    }

    #[test]
    fn trimming_never_while_a_window_is_visible() {
        let visible = snapshot(WinActivity::Hidden, true, false, false);
        assert!(!ws_trim_allowed(visible, IDLE_TRIM_DELAY * 2));
    }

    #[test]
    fn trimming_never_while_sessions_or_transfers_are_active() {
        let sessions = snapshot(WinActivity::Hidden, false, true, false);
        let transfers = snapshot(WinActivity::Hidden, false, false, true);
        assert!(!ws_trim_allowed(sessions, IDLE_TRIM_DELAY * 2));
        assert!(!ws_trim_allowed(transfers, IDLE_TRIM_DELAY * 2));
    }

    #[test]
    fn trimming_never_in_active_or_background_state() {
        for activity in [WinActivity::Active, WinActivity::Background] {
            let state = snapshot(activity, false, false, false);
            assert!(!ws_trim_allowed(state, IDLE_TRIM_DELAY * 2));
        }
    }
}
