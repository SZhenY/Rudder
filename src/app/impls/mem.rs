//! 子窗口 / 标签页关闭后的资源回收，以及两档内存策略。
//!
//! 关闭档只做不影响性能的真回收：关窗后的堆合并与堆页归还，不碰进程工作集。
//! 激进档（设置 › Beta）额外在启动 3 秒后、关窗 1 秒后、最小化 20 秒后裁工作集。
//! Windows 的 `SetProcessWorkingSetSize(-1, -1)` 会把常驻页转到 standby，任务管理器
//! 读数随之下降，但提交内存不变，回访时会有一次软缺页 —— 所以它只在激进档使用。
//! `RUDDER_NO_MEM_TRIM` 存在且不为 `"0"` 时两条路径都不执行。

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::app::WinActivity;

thread_local! {
    /// 每次活动 +1：定时器醒来发现变了就放弃，让最后一次关闭统一收尾。
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    /// 上次真正裁剪工作集的时间，用来限频。
    static LAST_TRIMMED: Cell<Option<Instant>> = const { Cell::new(None) };
    /// 上次因系统内存压力回收的时间。
    static LAST_PRESSURE: Cell<Option<Instant>> = const { Cell::new(None) };
    /// 上次观察到窗口可见 / 有会话 / 有传输的时间；None = 尚未观察到活动。
    static LAST_BUSY: Cell<Option<Instant>> = const { Cell::new(None) };
    /// 深度空闲观察器的定时器句柄，drop 会停掉定时器。
    static IDLE_OBSERVER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
}

/// 系统内存压力的最短处理间隔：压力下清缓存是安全的，但没必要每次轮询都做。
const PRESSURE_INTERVAL: Duration = Duration::from_secs(60);
/// 启动后多久裁第一次工作集：留出首帧、设置播种与字体索引的时间。
const STARTUP_TRIM_DELAY: Duration = Duration::from_secs(3);
/// PSI `some avg10` 超过这个百分比即认为系统在换页压力下。
#[cfg(target_os = "linux")]
const PSI_PRESSURE_PERCENT: f32 = 10.0;

/// 回收策略。关闭档只做不影响性能的真回收（归还堆页、合并空闲块）；打开档在此之上
/// 缩短门槛、清可重建缓存、并在关窗/启动时裁整个进程工作集。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ReclaimPolicy {
    /// 关窗 / 关标签后等多久再归还堆页。
    pub release_delay: Duration,
    /// 整个应用持续隐藏多久才允许裁剪工作集。
    pub idle_trim_delay: Duration,
    /// 两次裁剪工作集的最短间隔。
    pub min_interval: Duration,
    /// 状态观察频率。
    pub observe_interval: Duration,
    /// 深度空闲时清空终端渲染模型（span / 查找 / 选区）。
    pub drop_render_caches: bool,
    /// 关窗后与深度空闲时调用 `compact_heap`。
    pub compact_on_release: bool,
    /// 系统内存压力下立即回收堆页与缓存（不裁工作集）。
    pub reclaim_on_pressure: bool,
    /// 关窗 / 关标签后裁整个进程工作集（读数优化）。
    pub trim_working_set_on_release: bool,
    /// 启动后裁一次整个进程工作集（读数优化）。
    pub trim_working_set_on_startup: bool,
}

impl ReclaimPolicy {
    pub(crate) const BALANCED: Self = Self {
        release_delay: Duration::from_secs(3),
        idle_trim_delay: Duration::from_secs(60),
        min_interval: Duration::from_secs(60),
        // 10s 而不是 30s：60s 门槛配 30s 采样，实际触发落在 60–90s，用户等 60 多秒
        // 会以为没生效。10s 一次只有四个信号读取，开销可忽略。
        observe_interval: Duration::from_secs(10),
        // 关闭档只做「关窗后归还堆页 + 合并空闲块」，不动工作集、不注册观察器。
        drop_render_caches: false,
        compact_on_release: true,
        reclaim_on_pressure: false,
        trim_working_set_on_release: false,
        trim_working_set_on_startup: false,
    };

    pub(crate) const AGGRESSIVE: Self = Self {
        release_delay: Duration::from_secs(1),
        idle_trim_delay: Duration::from_secs(20),
        min_interval: Duration::from_secs(30),
        observe_interval: Duration::from_secs(10),
        drop_render_caches: true,
        compact_on_release: true,
        reclaim_on_pressure: true,
        trim_working_set_on_release: true,
        trim_working_set_on_startup: true,
    };

    pub(crate) const fn from_aggressive(on: bool) -> Self {
        if on { Self::AGGRESSIVE } else { Self::BALANCED }
    }

    /// 是否需要周期性观察（裁工作集或响应压力）。关闭档不注册观察器。
    pub(crate) const fn observes_idle(&self) -> bool {
        self.trim_working_set_on_release || self.reclaim_on_pressure
    }
}

/// 启动期注入一次；运行期不再读配置（开关重启生效）。
static AGGRESSIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_aggressive(on: bool) {
    AGGRESSIVE.store(on, Ordering::Relaxed);
}

fn policy() -> ReclaimPolicy {
    ReclaimPolicy::from_aggressive(AGGRESSIVE.load(Ordering::Relaxed))
}

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
    /// winit 报告的最小化状态。与 `activity` 互为兜底：Slint 的 `window_event_filter`
    /// 能收到原始 `Resized(0,0)`（早于后端过滤），但事件顺序不是契约，Windows 也不发
    /// `Occluded`，所以裁剪判定同时看这个字段。
    pub minimized: bool,
    /// 任一子窗口可见；主窗口的可见性由 `activity` / `minimized` 表达。
    pub has_visible_window: bool,
    pub live_sessions: bool,
    pub active_transfers: bool,
}

impl TrimSnapshot {
    fn hidden(&self) -> bool {
        self.activity == WinActivity::Hidden || self.minimized
    }
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

/// 裁剪策略：整个应用不可见、无会话与传输、且已持续空闲够久。
pub(crate) fn ws_trim_allowed(
    policy: ReclaimPolicy,
    snapshot: TrimSnapshot,
    idle_for: Duration,
) -> bool {
    !snapshot.has_visible_window
        && snapshot.hidden()
        && !snapshot.live_sessions
        && !snapshot.active_transfers
        && idle_for >= policy.idle_trim_delay
}

/// 关子窗 / 关标签后的回收：安静几秒后归还堆页、合并空闲块；激进档再裁一次整个
/// 进程工作集（`SetProcessWorkingSetSize`，只改读数）。
pub(crate) fn reclaim_after_release(reason: ReclaimReason) {
    note_activity();
    if disabled() {
        return;
    }
    let p = policy();
    let expected = generation();
    slint::Timer::single_shot(p.release_delay, move || {
        if generation() != expected || disabled() {
            return;
        }
        if p.compact_on_release {
            compact_heap();
        }
        trim_heap();
        if p.trim_working_set_on_release {
            trim_working_set();
        }
        tracing::debug!(
            sub_window = matches!(reason, ReclaimReason::SubWindowClosed),
            working_set = p.trim_working_set_on_release,
            "release reclaimed memory"
        );
    });
}

/// 启动后把工作集转入待机。延后到首帧与设置播种完成之后，避免把正在初始化的页
/// 立刻换出；只有激进档执行。
pub(crate) fn trim_after_startup() {
    if disabled() || !policy().trim_working_set_on_startup {
        return;
    }
    slint::Timer::single_shot(STARTUP_TRIM_DELAY, || {
        if disabled() {
            return;
        }
        trim_working_set();
        tracing::warn!(
            delay_secs = STARTUP_TRIM_DELAY.as_secs(),
            "memory: trimmed process working set after startup"
        );
    });
}

/// 注册 UI 线程的深度空闲观察器；`on_deep_idle` 在激进档的深度空闲/压力回收时执行。
/// 关闭档不注册 —— 它只需要关窗路径上的堆回收，没有周期性动作。
pub(crate) fn spawn_idle_observer<S, C>(snapshot: S, on_deep_idle: C)
where
    S: Fn() -> TrimSnapshot + 'static,
    C: Fn() + 'static,
{
    let p = policy();
    if !p.observes_idle() {
        return;
    }
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, p.observe_interval, move || {
        observe(snapshot(), &on_deep_idle);
    });
    IDLE_OBSERVER.with(|slot| *slot.borrow_mut() = Some(timer));
}

/// UI 线程周期调用：先响应系统内存压力，再按空闲门槛裁剪工作集。
fn observe(snapshot: TrimSnapshot, on_deep_idle: &dyn Fn()) {
    if disabled() {
        return;
    }
    let now = Instant::now();
    let p = policy();
    let busy = snapshot.has_visible_window
        || !snapshot.hidden()
        || snapshot.live_sessions
        || snapshot.active_transfers;

    // 系统在换页压力下：归还堆页、清可重建缓存，但不裁工作集（那会让缺页更糟）。
    if p.reclaim_on_pressure && memory_pressure() && pressure_due(now) {
        LAST_PRESSURE.with(|slot| slot.set(Some(now)));
        if p.compact_on_release {
            compact_heap();
        }
        trim_heap();
        // 窗口还可见时不能清模型：清空后没人重建，前台会白屏。
        if !snapshot.has_visible_window && snapshot.hidden() {
            on_deep_idle();
        }
        tracing::warn!("memory: reclaimed under system memory pressure");
        return;
    }

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
    if !ws_trim_allowed(p, snapshot, idle_for) {
        return;
    }
    if LAST_TRIMMED.with(|slot| slot.get().is_some_and(|at| at.elapsed() < p.min_interval)) {
        return;
    }
    LAST_TRIMMED.with(|slot| slot.set(Some(now)));
    LAST_BUSY.with(|slot| slot.set(Some(now)));
    if p.compact_on_release {
        compact_heap();
    }
    if p.drop_render_caches {
        on_deep_idle();
    }
    tracing::warn!(
        idle_secs = idle_for.as_secs(),
        aggressive = p.drop_render_caches,
        minimized = snapshot.minimized,
        hidden = snapshot.activity == WinActivity::Hidden,
        visible_window = snapshot.has_visible_window,
        "memory: trimmed process working set after sustained idle"
    );
    trim_working_set();
}

fn pressure_due(now: Instant) -> bool {
    LAST_PRESSURE.with(|slot| {
        !slot
            .get()
            .is_some_and(|at| now.saturating_duration_since(at) < PRESSURE_INTERVAL)
    })
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

/// 合并本进程堆的空闲块。Windows 没有能把堆页 decommit 的公开 API，`HeapCompact`
/// 只做合并，段堆下可能是 no-op —— 按 best-effort 处理，只在激进档调用。
#[cfg(windows)]
fn compact_heap() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetProcessHeap() -> *mut core::ffi::c_void;
        fn HeapCompact(heap: *mut core::ffi::c_void, flags: u32) -> usize;
    }
    // SAFETY: 只影响当前进程的默认堆；flags 必须为 0。
    unsafe {
        let _ = HeapCompact(GetProcessHeap(), 0);
    }
}

#[cfg(not(windows))]
fn compact_heap() {}

/// Windows 低内存资源通知：句柄进程内复用，每次只查询状态。
#[cfg(windows)]
fn memory_pressure() -> bool {
    use std::sync::atomic::AtomicIsize;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateMemoryResourceNotification(kind: u32) -> *mut core::ffi::c_void;
        fn QueryMemoryResourceNotification(handle: *mut core::ffi::c_void, state: *mut i32) -> i32;
    }

    /// `LowMemoryResourceNotification`
    const LOW_MEMORY: u32 = 0;
    static HANDLE: AtomicIsize = AtomicIsize::new(0);

    let mut raw = HANDLE.load(Ordering::Relaxed);
    if raw == 0 {
        // SAFETY: 只创建通知对象，失败返回空指针。
        let created = unsafe { CreateMemoryResourceNotification(LOW_MEMORY) };
        if created.is_null() {
            return false;
        }
        raw = created as isize;
        HANDLE.store(raw, Ordering::Relaxed);
    }
    let mut state: i32 = 0;
    // SAFETY: 句柄由本函数创建并长期持有，查询不转移所有权。
    unsafe {
        QueryMemoryResourceNotification(raw as *mut core::ffi::c_void, &mut state) != 0 && state != 0
    }
}

/// Linux PSI：`some avg10` 超过阈值说明有任务在等内存。
#[cfg(target_os = "linux")]
fn memory_pressure() -> bool {
    std::fs::read_to_string("/proc/pressure/memory")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("some "))
                .and_then(|line| line.split("avg10=").nth(1))
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<f32>().ok())
        })
        .is_some_and(|avg10| avg10 >= PSI_PRESSURE_PERCENT)
}

/// macOS 的 memorystatus 压力等级：1 = normal，2 = warn，4 = critical。
#[cfg(target_os = "macos")]
fn memory_pressure() -> bool {
    #[link(name = "System")]
    unsafe extern "C" {
        fn sysctlbyname(
            name: *const core::ffi::c_char,
            oldp: *mut core::ffi::c_void,
            oldlenp: *mut usize,
            newp: *mut core::ffi::c_void,
            newlen: usize,
        ) -> i32;
    }

    let mut level: i32 = 1;
    let mut len = std::mem::size_of::<i32>();
    let name = b"kern.memorystatus_vm_pressure_level\0";
    // SAFETY: 只读 sysctl；缓冲区大小与声明类型一致。
    let ok = unsafe {
        sysctlbyname(
            name.as_ptr() as *const core::ffi::c_char,
            (&mut level as *mut i32).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    ok == 0 && level >= 2
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn memory_pressure() -> bool {
    false
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
    use super::{ReclaimPolicy, TrimSnapshot, ws_trim_allowed};
    use crate::app::WinActivity;
    use std::time::Duration;

    fn snapshot(
        activity: WinActivity,
        minimized: bool,
        has_visible_window: bool,
        live_sessions: bool,
        active_transfers: bool,
    ) -> TrimSnapshot {
        TrimSnapshot {
            activity,
            minimized,
            has_visible_window,
            live_sessions,
            active_transfers,
        }
    }

    fn hidden_idle() -> TrimSnapshot {
        snapshot(WinActivity::Hidden, false, false, false, false)
    }

    #[test]
    fn trimming_needs_hidden_idle_and_no_activity() {
        let p = ReclaimPolicy::BALANCED;
        assert!(ws_trim_allowed(p, hidden_idle(), p.idle_trim_delay));
        assert!(ws_trim_allowed(
            p,
            hidden_idle(),
            p.idle_trim_delay + Duration::from_secs(30)
        ));
        assert!(!ws_trim_allowed(
            p,
            hidden_idle(),
            p.idle_trim_delay - Duration::from_secs(1)
        ));
        assert!(!ws_trim_allowed(p, hidden_idle(), Duration::ZERO));
    }

    #[test]
    fn trimming_never_while_a_window_is_visible() {
        let p = ReclaimPolicy::BALANCED;
        let visible = snapshot(WinActivity::Hidden, false, true, false, false);
        assert!(!ws_trim_allowed(p, visible, p.idle_trim_delay * 2));
    }

    #[test]
    fn trimming_never_while_sessions_or_transfers_are_active() {
        let p = ReclaimPolicy::BALANCED;
        let sessions = snapshot(WinActivity::Hidden, false, false, true, false);
        let transfers = snapshot(WinActivity::Hidden, false, false, false, true);
        assert!(!ws_trim_allowed(p, sessions, p.idle_trim_delay * 2));
        assert!(!ws_trim_allowed(p, transfers, p.idle_trim_delay * 2));
    }

    #[test]
    fn trimming_never_in_active_or_background_state() {
        let p = ReclaimPolicy::BALANCED;
        for activity in [WinActivity::Active, WinActivity::Background] {
            let state = snapshot(activity, false, false, false, false);
            assert!(!ws_trim_allowed(p, state, p.idle_trim_delay * 2));
        }
    }

    /// Slint 的 winit 后端不转发 0 尺寸 `Resized`，Windows 也不发 `Occluded`，
    /// 所以最小化只能靠 `is_minimized()` 进快照。这条是那个 bug 的回归测试。
    #[test]
    fn minimized_window_counts_as_hidden() {
        let p = ReclaimPolicy::BALANCED;
        let state = snapshot(WinActivity::Background, true, false, false, false);
        assert!(ws_trim_allowed(p, state, p.idle_trim_delay));
    }

    #[test]
    fn aggressive_shortens_every_threshold() {
        let balanced = ReclaimPolicy::BALANCED;
        let aggressive = ReclaimPolicy::AGGRESSIVE;
        assert!(aggressive.release_delay < balanced.release_delay);
        assert!(aggressive.idle_trim_delay < balanced.idle_trim_delay);
        assert!(aggressive.min_interval < balanced.min_interval);
        // 观察周期两档相同：默认档也要让 60s 门槛准时触发。
        assert_eq!(aggressive.observe_interval, balanced.observe_interval);
        assert!(aggressive.drop_render_caches);
        assert!(aggressive.compact_on_release);
        assert!(aggressive.reclaim_on_pressure);
        assert!(aggressive.trim_working_set_on_release);
        assert!(aggressive.trim_working_set_on_startup);
        assert!(aggressive.observes_idle());
    }

    /// 关闭档只保留不影响性能的回收：关窗后合并空闲块 + 归还堆页，不动工作集。
    #[test]
    fn balanced_policy_reclaims_heap_only() {
        let p = ReclaimPolicy::BALANCED;
        assert!(p.compact_on_release, "关窗后仍应合并堆空闲块");
        assert!(!p.drop_render_caches);
        assert!(!p.reclaim_on_pressure);
        assert!(!p.trim_working_set_on_release);
        assert!(!p.trim_working_set_on_startup);
        assert!(!p.observes_idle(), "关闭档不应注册空闲观察器");
    }

    #[test]
    fn policy_thresholds_are_honoured() {
        let state = snapshot(WinActivity::Hidden, false, false, false, false);
        let idle = Duration::from_secs(25);
        assert!(ws_trim_allowed(ReclaimPolicy::AGGRESSIVE, state, idle));
        assert!(!ws_trim_allowed(ReclaimPolicy::BALANCED, state, idle));
    }

    #[test]
    fn policy_intervals_are_consistent() {
        for p in [ReclaimPolicy::BALANCED, ReclaimPolicy::AGGRESSIVE] {
            assert!(p.min_interval >= p.observe_interval);
            assert!(p.idle_trim_delay >= p.observe_interval);
            assert!(p.release_delay > Duration::ZERO);
        }
    }

    #[test]
    fn from_aggressive_maps_to_constants() {
        assert_eq!(
            ReclaimPolicy::from_aggressive(false),
            ReclaimPolicy::BALANCED
        );
        assert_eq!(
            ReclaimPolicy::from_aggressive(true),
            ReclaimPolicy::AGGRESSIVE
        );
    }
}
