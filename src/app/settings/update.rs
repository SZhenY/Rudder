//! 新版本提示页：启动检查开关 + 检查频率 + 更新通道 + 上次检查时间。
//!
//! 该页**按规格没有「还原本页默认」**（这里都是"行为开关"，没有值得还原的内容），
//! 因此这里只有 `bind`。`SettingsPage` 枚举里也没有 `Update` 变体 ——
//! 若将来误加了按钮，`wiring_tests` 的契约测试会失败。

use std::rc::Rc;

use slint::{ComponentHandle as _, ModelRc, VecModel};

use super::{Store, persist};
use crate::app::updater::format_last_check;
use crate::ui::AppWindow;

/// 「检查频率」下拉的选项顺序 —— 下标即 `update-freq-index`，与
/// `ConfigStore::{update_frequency, set_update_frequency}` 的取值一一对应。
const FREQ_VALUES: [&str; 2] = ["startup", "daily"];

fn freq_labels() -> Vec<slint::SharedString> {
    vec![
        crate::i18n::t("每次启动", "On every launch").into(),
        crate::i18n::t("每天最多一次", "At most once a day").into(),
    ]
}

/// 播种 + 注册持久化回调。
pub(crate) fn bind(w: &AppWindow, store: &Store) {
    // 存储字段是取反的 `update_check_disabled`；开关在下次启动才决定是否查询。
    w.set_update_check_enabled(store.borrow().update_check_enabled());
    let store = store.clone();

    // 更新通道（stable / beta / all）—— 字符串原样交给界面，界面用 `==` 选中。
    w.set_update_channel(store.borrow().update_channel().into());

    // 检查频率下拉：值 ↔ 下标的换算走 `FREQ_VALUES` 这一张表，避免两处各写一份映射。
    w.set_update_freq_labels(ModelRc::from(Rc::new(VecModel::from(freq_labels()))));
    let freq = store.borrow().update_frequency().to_string();
    let freq_index = FREQ_VALUES
        .iter()
        .position(|v| *v == freq)
        .unwrap_or(0) as i32;
    w.set_update_freq_index(freq_index);

    // 上次检查时间（配置里存 Unix 秒，0 = 还没查过 → 空串）。
    w.set_update_last_check(format_last_check(store.borrow().update_last_check()).into());

    let store_cb = store.clone();
    w.on_set_update_check_enabled(move |v| {
        persist(&store_cb, |s| s.set_update_check_enabled(v));
    });

    let store_cb = store.clone();
    let weak = w.as_weak();
    w.on_set_update_channel(move |channel| {
        persist(&store_cb, |s| s.set_update_channel(channel.to_string()));
        if let Some(w) = weak.upgrade() {
            w.set_update_channel(store_cb.borrow().update_channel().into());
        }
    });

    let weak = w.as_weak();
    w.on_set_update_freq(move |index| {
        let value = FREQ_VALUES
            .get(index.max(0) as usize)
            .copied()
            .unwrap_or(FREQ_VALUES[0]);
        persist(&store, |s| s.set_update_frequency(value.to_string()));
        if let Some(w) = weak.upgrade() {
            w.set_update_freq_index(index);
        }
    });
}
