//! 新版本提示页：启动检查开关。
//!
//! 该页**按规格没有「还原本页默认」**（只有一个开关，没有值得还原的内容），
//! 因此这里只有 `bind`。`SettingsPage` 枚举里也没有 `Update` 变体 ——
//! 若将来误加了按钮，`wiring_tests` 的契约测试会失败。

use super::{Store, persist};
use crate::ui::AppWindow;

/// 播种 + 注册持久化回调。
pub(crate) fn bind(w: &AppWindow, store: &Store) {
    // 存储字段是取反的 `update_check_disabled`；开关在下次启动才决定是否查询。
    w.set_update_check_enabled(store.borrow().update_check_enabled());
    let store = store.clone();
    w.on_set_update_check_enabled(move |v| {
        persist(&store, |s| s.set_update_check_enabled(v));
    });
}
