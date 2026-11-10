use super::pick_channel_release;
use serde_json::json;

fn rel(tag: &str) -> serde_json::Value {
    json!({ "tag_name": tag, "draft": false, "assets": [] })
}

fn pick(list: &[serde_json::Value], channel: &str) -> String {
    pick_channel_release(list, channel)
        .and_then(|v| v["tag_name"].as_str().map(str::to_string))
        .unwrap_or_default()
}

/// 用户 2026/09/21 的约定：`X.Y.Z-betaN` 是正式版之后继续做出来的构建，**比同号正式版新**。
/// 所以「全通道最新版」和「测试版」两个通道都要挑它 —— 挑不出来就等于测试版白发了。
#[test]
fn test_builds_beat_the_stable_of_the_same_version() {
    let list = vec![
        rel("v0.7.8"),
        rel("v0.7.8-fix5"),
        rel("v0.7.9"),
        rel("v0.7.9-beta1"),
    ];
    assert_eq!(pick(&list, "all"), "v0.7.9-beta1", "全通道：beta 比同号正式版新");
    assert_eq!(pick(&list, "beta"), "v0.7.9-beta1", "测试版通道：只认测试版");
}

/// 测试版通道在**没有**测试版时必须返回 None，而不是退回正式版 ——
/// 否则选了"测试版"的用户会在正式版发布时收到一次提示，与其选择不符。
#[test]
fn beta_channel_ignores_plain_stables() {
    let list = vec![rel("v0.7.8"), rel("v0.7.9")];
    assert_eq!(pick(&list, "beta"), "", "没有测试版就是不提示");
    assert_eq!(pick(&list, "all"), "v0.7.9");
}

/// 版本号本身仍然优先：下一个号的测试版 > 上一个号的正式版/测试版。
#[test]
fn version_numbers_outrank_stages() {
    let list = vec![rel("v0.7.9"), rel("v0.7.9-beta9"), rel("v0.8.0-beta1")];
    assert_eq!(pick(&list, "all"), "v0.8.0-beta1");
    assert_eq!(pick(&list, "beta"), "v0.8.0-beta1");
}

/// 草稿与认不出的 tag 一律跳过（否则一个手滑的 tag 会把通道整体带偏）。
#[test]
fn skips_drafts_and_unparsable_tags() {
    let list = vec![
        json!({ "tag_name": "vNEXT", "draft": false }),
        json!({ "tag_name": "v0.9.0", "draft": true }),
        rel("v0.7.9"),
    ];
    assert_eq!(pick(&list, "all"), "v0.7.9");
    assert_eq!(pick(&[json!({ "tag_name": "vNEXT", "draft": false })], "all"), "");
}
