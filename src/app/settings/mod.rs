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
pub(super) mod transfer;
pub(super) mod update;

use std::cell::RefCell;
use std::rc::Rc;

use crate::config::ConfigStore;

/// 配置存储句柄（UI 线程独占）。
pub(super) type Store = Rc<RefCell<ConfigStore>>;

/// 改一个设置并写盘 —— **全项目统一的持久化出口**。
///
/// 集中在这里的原因：此前 40 多个回调各写一遍 `borrow_mut() + save()`，错误处理
/// 还分成两种（少数 `tracing::warn!`、多数 `let _ =` 静默吞掉），写盘失败既没有
/// 日志也没有 UI 反馈。现在只有这一处策略。
pub(super) fn persist(store: &Store, set: impl FnOnce(&mut ConfigStore)) {
    let mut s = store.borrow_mut();
    set(&mut s);
    if let Err(error) = s.save() {
        tracing::warn!("failed to save config: {error:#}");
    }
}
