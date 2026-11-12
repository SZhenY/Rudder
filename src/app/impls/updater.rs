//! In-app update check (#48).

use std::rc::Rc;

use slint::{ComponentHandle as _, ModelRc, SharedString, VecModel};

use crate::ui::{AppWindow, TransferInfo};

use super::settings::{Store, persist};
use super::{AppContext, DEP_VERSIONS};

/// 「检查频率」对应的最小间隔；`None` = 每次启动都查（`startup`）。
///
/// 月/半年/年按月与年的常用近似（30 / 182 / 365 天）—— 目的只是"别太频繁"，
/// 不需要日历级精确。
pub(crate) fn check_interval_secs(frequency: &str) -> Option<i64> {
    const DAY: i64 = 24 * 60 * 60;
    match frequency {
        "daily" => Some(DAY),
        "weekly" => Some(7 * DAY),
        "monthly" => Some(30 * DAY),
        "semiannual" => Some(182 * DAY),
        "yearly" => Some(365 * DAY),
        // `startup` 以及任何未知值：每次启动都查。
        _ => None,
    }
}

/// 「自动更新」对话框上那个通道徽章：被提示的版本是**测试版**（`-betaN` 等）就写「测试版」，
/// 否则「正式版」。按 tag 名判定，与更新通道的筛选同一套规则（`is_test_build`）。
fn version_badge(tag: &str) -> String {
    let is_test = crate::app::parse_version(tag).is_some_and(|v| crate::app::is_test_build(&v));
    if is_test {
        crate::i18n::t("测试版", "Beta").to_string()
    } else {
        crate::i18n::t("正式版", "Stable").to_string()
    }
}

/// 当前 Unix 秒；系统时钟异常时退化为 0（调用方按"还没查过"处理）。
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 「上次检查时间」那一行显示的本机时间（`2026/09/21 10:14:58`）；0 → 空串。
pub(crate) fn format_last_check(unix: i64) -> String {
    if unix <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y/%m/%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

/// 记下「这次检查发生的时间」：写配置（带去抖）+ 刷新界面那一行。
///
/// 在**发起**检查之前、UI 线程上调用 —— 时间戳即"上次检查时间"，界面显示的就是它；
/// `daily` 的节流也据此判断。放在这里而不是等结果回来，是因为配置句柄是
/// `Rc<RefCell<..>>`（只能待在 UI 线程），而检查跑在后台线程上。
fn mark_check_started(window: &AppWindow, store: &Store) {
    let now = now_unix();
    persist(store, |s| s.set_update_last_check(now));
    window.set_update_last_check(format_last_check(now).into());
}

#[path = "updater_links.rs"] mod updater_links;
#[path = "updater_check.rs"] mod updater_check;
#[path = "updater_transfers.rs"] mod updater_transfers;

pub(crate) fn wire_update_check(window: &AppWindow, ctx: &AppContext) {
    updater_links::wire_update_links(window);
    updater_check::wire_update_checks(window, ctx);
    updater_transfers::wire_update_transfers(window, ctx);
}
/// 查编译期烘焙的依赖版本（`build.rs` -> `$OUT_DIR/deps.rs`，见文件顶部的 `DEP_VERSIONS`）。
///
/// 未命中返回 `"-"`：About 面板按名字查表，拼错会静默显示成 `-` ——
/// 用户贴 bug 报告时给出的库版本就是错的。
fn dep_version(name: &str) -> &str {
    DEP_VERSIONS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, v)| *v)
        .unwrap_or("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// About 面板真的会去查的名字必须命中（否则那一行显示成 `-`）。
    #[test]
    fn dep_version_finds_the_about_panel_entries() {
        for name in ["slint", "russh", "tokio"] {
            let v = dep_version(name);
            assert_ne!(v, "-", "{name} 未命中 DEP_VERSIONS");
            assert!(!v.is_empty(), "{name} 的版本号为空");
        }
    }

    #[test]
    fn dep_version_falls_back_to_dash() {
        assert_eq!(dep_version("no-such-crate"), "-");
        assert_eq!(dep_version(""), "-");
    }
}

#[cfg(test)]
mod interval_tests {
    use super::check_interval_secs;

    /// 「每次启动」不节流；其余各档必须**严格递增** —— 顺序写错会让"每周"比"每天"更频繁。
    #[test]
    fn intervals_are_ordered_and_startup_is_unthrottled() {
        assert_eq!(check_interval_secs("startup"), None);
        assert_eq!(check_interval_secs("nonsense"), None, "未知值按每次启动处理");
        let secs: Vec<i64> = ["daily", "weekly", "monthly", "semiannual", "yearly"]
            .iter()
            .map(|f| check_interval_secs(f).expect("必须有区间"))
            .collect();
        for pair in secs.windows(2) {
            assert!(pair[1] > pair[0], "区间必须递增：{secs:?}");
        }
    }
}
