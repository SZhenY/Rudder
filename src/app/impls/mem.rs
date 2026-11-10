//! 关闭标签 / 子窗口后把工作集归还给系统：关闭路径已把对象摘干净，但 Windows 的堆
//! 只 `free` 不 decommit，页仍计入「专用工作集」。这里在安静几秒后调
//! `SetProcessWorkingSetSize(-1, -1)` 把它们转 standby。
//!
//! 只改读数，不减少提交大小；之后首次访问会有一次软缺页。

use std::cell::Cell;
use std::time::{Duration, Instant};

thread_local! {
    /// 每次活动 +1：定时器醒来发现变了就放弃，让最后一次关闭统一收尾。
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    /// 上次真正执行的时间，用来限频。
    static LAST_TRIMMED: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// 关完等这么久再动手，避开"关一个标签马上又开一个"。
const DELAY: Duration = Duration::from_secs(3);

/// 最短间隔：60 秒内最多归还一次，避免连续开关时反复缺页。
const MIN_INTERVAL: Duration = Duration::from_secs(60);

fn disabled() -> bool {
    std::env::var_os("RUDDER_NO_MEM_TRIM").is_some_and(|value| value != "0")
}

/// 记一次活动：开标签、开设置窗、关标签都算；已排期的到点后发现代际变了就放弃。
pub(crate) fn note_activity() {
    GENERATION.with(|slot| slot.set(slot.get().wrapping_add(1)));
}

/// 排一次「安静下来后归还工作集」。必须从 UI 线程调用（Slint 定时器）。
pub(crate) fn trim_after_close() {
    if disabled() {
        return;
    }
    note_activity();
    let generation = GENERATION.with(|slot| slot.get());
    slint::Timer::single_shot(DELAY, move || {
        if GENERATION.with(|slot| slot.get()) != generation {
            return;
        }
        if LAST_TRIMMED.with(|slot| slot.get().is_some_and(|at| at.elapsed() < MIN_INTERVAL)) {
            return;
        }
        LAST_TRIMMED.with(|slot| slot.set(Some(Instant::now())));
        trim_working_set();
    });
}

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
    // SAFETY: 两个都是无状态的系统调用，只影响当前进程；-1 表示"尽量归还"。
    unsafe {
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

#[cfg(not(windows))]
fn trim_working_set() {}
