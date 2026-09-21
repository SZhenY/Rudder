//! 设置页的按页装配层：**每页一个模块**，各自负责该页的三件事。
//!
//! ```text
//! bind(w, store, deps)   播种 + 注册持久化回调 —— 该页"全部落点"集中在此
//! reset(w, store, deps)  还原 = 替换该域为出厂默认 + 走同一条 apply 路径
//! ```
//!
//! 为什么要按页拆
//! --------------
//! 此前每页的代码被拆在三处：启动播种在 `seed_settings` 的前半段、持久化回调在
//! 中段、还原在另一组函数里。一项设置因此有 5 类落点（配置字段 / 控件属性 /
//! 派生索引 / 运行时镜像 / 跨组件副作用）散落在三个位置，还原时必须手工把它们
//! 再写一遍 —— 漏掉任意一类就表现为"UI 显示已还原、实际没生效"。
//!
//! 拆成模块后，新增一项设置只改一个文件；`settings_ui.rs` 的页-属性契约测试
//! （`wiring_tests`）再从外部钉住"还原不能漏"。

pub(super) mod appearance;
pub(super) mod layout;
pub(super) mod sync;
pub(super) mod terminal;
pub(super) mod transfer;
pub(super) mod update;

use std::cell::RefCell;
use std::rc::Rc;

use slint::VecModel;

use crate::app::FontEntry;
use crate::config::ConfigStore;
use crate::ui::SessionInfo;

/// 配置存储句柄（UI 线程独占）。
pub(super) type Store = Rc<RefCell<ConfigStore>>;

/// 写盘防抖窗口。
///
/// 缩放 / 面板字号这类滑条是 `changed(v)` 逐帧触发的 —— 不防抖的话一次拖动会
/// 产生几十次全量写盘（每次都要重新加密口令、重写整个文件）。250 ms 足够让
/// 一次连续拖动只落一次盘，又在手感上察觉不到延迟。
const SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

thread_local! {
    /// 只保留**最后一个**定时器：替换 `Option` 里的旧定时器即取消它（trailing 防抖，
    /// 连续改动只会顺延，不会积累成一串定时写盘）。
    static PENDING_FLUSH: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };

    /// UI 线程上的 (配置存储, 会话列表模型) —— 供**后台线程回填结果**时取用。
    ///
    /// 为什么需要：`slint::invoke_from_event_loop` 的闭包必须是 `Send`，而这两个都是
    /// `Rc`，捕获不进去。于是后台线程只回传 `Send` 的数据（JSON、计数），闭包在 UI 线程
    /// 上执行时再从这里取句柄写回去（WebDAV 上传/下载即如此）。
    static UI_HANDLES: RefCell<Option<(Store, Rc<VecModel<SessionInfo>>)>> =
        const { RefCell::new(None) };
}

/// 注册 UI 线程的句柄（设置面板绑定的时候调用一次即可）。
pub(super) fn register_ui_handles(store: &Store, sessions: &Rc<VecModel<SessionInfo>>) {
    UI_HANDLES.with(|slot| *slot.borrow_mut() = Some((store.clone(), sessions.clone())));
}

/// 在 UI 线程上取出句柄（供 `invoke_from_event_loop` 回调使用）。
pub(super) fn with_ui_handles<R>(
    f: impl FnOnce(&Store, &Rc<VecModel<SessionInfo>>) -> R,
) -> Option<R> {
    UI_HANDLES.with(|slot| slot.borrow().as_ref().map(|(s, m)| f(s, m)))
}

/// 改一个设置并写盘 —— **全项目统一的持久化出口**。
///
/// 集中在这里的原因：此前 40 多个回调各写一遍 `borrow_mut() + save()`，错误处理
/// 还分成两种（少数 `tracing::warn!`、多数 `let _ =` 静默吞掉），写盘失败既没有
/// 日志也没有 UI 反馈。现在只有这一处策略。
///
/// 这里走**防抖**写盘：设置是"丢了最多退回上一次的值"的数据，不值得为它每帧
/// 写一次。退出路径负责 `flush`（见 `app.rs` 的关闭与 `run()` 返回处）。
pub(crate) fn persist(store: &Store, set: impl FnOnce(&mut ConfigStore)) {
    {
        let mut s = store.borrow_mut();
        set(&mut s);
    }
    debounced_save(store);
}

/// 标记挂起并重置防抖定时器。窗口内再次调用只会把写盘时间往后推。
pub(super) fn debounced_save(store: &Store) {
    store.borrow_mut().save_debounced();

    let weak = Rc::downgrade(store);
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::SingleShot, SAVE_DEBOUNCE, move || {
        let Some(store) = weak.upgrade() else {
            return;
        };
        if let Err(error) = store.borrow_mut().flush() {
            tracing::warn!("failed to save config: {error:#}");
        }
    });
    PENDING_FLUSH.with(|slot| *slot.borrow_mut() = Some(timer));
}

/// 字体选择器的条目表。枚举系统字体（fontdb）代价不低，所以启动时算一次，
/// 「还原本页默认」需要把 family 反查回选择器索引时复用它 —— 不能每次重枚举。
#[derive(Clone)]
pub(super) struct FontCatalog {
    pub(super) term: Rc<Vec<FontEntry>>,
    pub(super) ui: Rc<Vec<FontEntry>>,
}

impl FontCatalog {
    /// 终端等宽字体列表中该 family 的索引（找不到时回退到第一个可选家族）。
    pub(super) fn term_index(&self, family: &str) -> i32 {
        self.term
            .iter()
            .position(|e| matches!(e, FontEntry::Family(f) if f == family))
            .or_else(|| {
                self.term
                    .iter()
                    .position(|e| matches!(e, FontEntry::Family(_)))
            })
            .unwrap_or(0) as i32
    }

    /// 界面字体列表中该**存储值**的索引。
    ///
    /// * 空串 = auto（出厂默认）→ 指向「跟随系统（自动）」条目；
    /// * 其余是单个家族名（`resolve_ui_font_family` 不再产生逗号栈，见那里的 ⚠️），
    ///   精确匹配即可。
    ///
    /// **找不到时不假定下标 0 可选** —— 曾经的 0 是分组标题（`▍内嵌字体`），把标题
    /// 当成当前选中项显示出来，就是"界面字体一栏显示内嵌字体"那个 bug；现在 0 是
    /// `Auto`，回退依然只回退到真正可选的条目。
    pub(super) fn ui_index(&self, family: &str) -> i32 {
        if family.trim().is_empty()
            && let Some(i) = self.ui.iter().position(|e| matches!(e, FontEntry::Auto))
        {
            return i as i32;
        }
        // 回退：第一个**可选**条目 —— `Auto` 与普通家族都算可选，分组标题不算。
        self.ui
            .iter()
            .position(|e| matches!(e, FontEntry::Family(f) if f == family))
            .or_else(|| {
                self.ui
                    .iter()
                    .position(|e| matches!(e, FontEntry::Auto | FontEntry::Family(_)))
            })
            .unwrap_or(0) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat(term: Vec<FontEntry>, ui: Vec<FontEntry>) -> FontCatalog {
        FontCatalog {
            term: Rc::new(term),
            ui: Rc::new(ui),
        }
    }

    #[test]
    fn term_index_matches_family_then_falls_back_to_first_family() {
        let c = cat(
            vec![FontEntry::Family("A".into()), FontEntry::Family("B".into())],
            vec![],
        );
        assert_eq!(c.term_index("B"), 1, "精确命中");
        assert_eq!(c.term_index("ghost"), 0, "未命中 → 第一个可选家族");
    }

    /// 回退**不能落在分组标题上**（标题不可选），空列表也要安全退回 0。
    #[test]
    fn term_index_skips_headers_and_survives_empty_list() {
        let c = cat(
            vec![
                FontEntry::Header("▍内嵌字体"),
                FontEntry::Family("A".into()),
            ],
            vec![],
        );
        assert_eq!(c.term_index("ghost"), 1, "回退到第一个家族，而不是 0 号标题");
        assert_eq!(cat(vec![], vec![]).term_index("any"), 0, "空列表退回 0");
    }

    /// 空串 = auto → 指向「跟随系统（自动）」条目（历史上回退落在分组标题上，
    /// 界面字体那一栏就显示成了「▍内嵌字体」）。
    #[test]
    fn ui_index_maps_empty_to_the_auto_entry() {
        let c = cat(
            vec![],
            vec![
                FontEntry::Auto,
                FontEntry::Header("▍内嵌字体"),
                FontEntry::Family("X".into()),
            ],
        );
        assert_eq!(c.ui_index(""), 0, "空串 = auto");
        assert_eq!(c.ui_index("   "), 0, "纯空白也算 auto");
        assert_eq!(c.ui_index("X"), 2, "精确命中家族");
    }

    #[test]
    fn ui_index_fallback_never_lands_on_a_header() {
        let c = cat(
            vec![],
            vec![
                FontEntry::Header("▍内嵌字体"),
                FontEntry::Family("X".into()),
            ],
        );
        assert_eq!(c.ui_index("ghost"), 1, "回退到第一个可选条目，不是 0 号标题");
        assert_eq!(c.ui_index(""), 1, "连 Auto 都没有时也不能落到标题上");
    }
}
