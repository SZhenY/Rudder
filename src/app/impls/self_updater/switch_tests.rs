use super::is_channel_switch;
use crate::app::{Version, parse_version};

fn v(s: &str) -> Version {
    parse_version(s).unwrap()
}

/// 用户 2026-09-21 的场景：本机 `0.7.9-beta1`、切到正式版、正式版最新是 `0.7.9`
/// → 要提示"可切换到旧版本"（因为按约定 beta 比同号正式版新）。
#[test]
fn beta_user_on_stable_channel_gets_a_switch_offer() {
    assert!(is_channel_switch("stable", v("0.7.9-beta1"), v("0.7.9")));
    // 正式版比测试版新（跨版本）：那是正常升级，不是切换。
    assert!(!is_channel_switch("stable", v("0.7.8-beta1"), v("0.7.9")));
    // 同为正式版（降级复现）：仍算切换，让用户能回到通道最新。
    assert!(is_channel_switch("stable", v("0.7.9"), v("0.7.8")));
}

/// `beta` / `all` 通道不给降级：要么升级，要么什么都不说。
#[test]
fn other_channels_never_downgrade() {
    assert!(!is_channel_switch("beta", v("0.7.9-beta2"), v("0.7.9-beta1")));
    assert!(!is_channel_switch("all", v("0.7.10-beta1"), v("0.7.9")));
    assert!(!is_channel_switch("nonsense", v("0.7.9-beta1"), v("0.7.9")));
}
