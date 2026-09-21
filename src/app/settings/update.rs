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

/// 「检查频率」下拉的取值 —— 直接用配置里的那张表（顺序即下拉顺序）。
const FREQ_VALUES: [&str; 6] = crate::config::UPDATE_FREQUENCIES;

/// 与 [`FREQ_VALUES`] **逐项对应**的标签；数量对不上就会在 `tests` 里红。
fn freq_labels() -> Vec<slint::SharedString> {
    vec![
        crate::i18n::t("每次启动", "On every launch").into(),
        crate::i18n::t("每天", "Every day").into(),
        crate::i18n::t("每周", "Every week").into(),
        crate::i18n::t("每月", "Every month").into(),
        crate::i18n::t("每半年", "Every 6 months").into(),
        crate::i18n::t("每年", "Every year").into(),
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
        let before = store_cb.borrow().update_channel().to_string();
        persist(&store_cb, |s| s.set_update_channel(channel.to_string()));
        let after = store_cb.borrow().update_channel().to_string();
        if let Some(w) = weak.upgrade() {
            w.set_update_channel(after.clone().into());
            // 通道**真的变了**就立刻按新通道查一次（复用设置里「立即检查」那条链路，
            // 结果同样就地显示 + 弹「自动更新」对话框）：
            // 选了「正式版」而本机是测试版时，这一步会给出"可切换到旧版本"。
            if before != after {
                w.invoke_check_update_now();
            }
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

#[cfg(test)]
mod tests {
    use super::{FREQ_VALUES, freq_labels};

    /// 下拉的标签数量必须与取值表一致，否则 `selected` 换算出来的下标会串位
    /// （用户选"每周"，存进去的却是"每月"）。
    #[test]
    fn labels_line_up_with_values() {
        assert_eq!(freq_labels().len(), FREQ_VALUES.len());
    }

    // 注：这里不需要再测"取值能否被配置层原样存下" —— 配置层的校验**直接用**
    // `UPDATE_FREQUENCIES`（同一张表），两边不可能漂移。
}
