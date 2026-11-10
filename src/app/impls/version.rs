//! 版本号解析与"测试版"判定（原在 `app.rs` 里，与启动接线混在一起）。
//!
//! 这些是纯函数：`updater` / `self_updater` 与更新检查都靠它们排序版本、判断
//! 当前是不是 alpha/beta/rc 构建（测试版不自动更新，见 updater 的策略）。
//! 搬出 app.rs 后由 app.rs 重新导出（`pub(crate) use version::{…}`），
//! 调用点（`crate::app::parse_version` 等）路径不变。

/// Split a stored proxy URL into `(type, host:port)` for the session dialog.
///
/// `""` → `("none", "")`. Recognises `socks5`/`socks5h`/`socks` and
/// `http`/`https` scheme prefixes. A value without a (recognised) scheme is
/// treated as SOCKS5, matching proxy.rs's parse default, so older configs that
/// stored a bare `host:port` keep working.
/// Parse a `"vX.Y.Z"` / `"X.Y.Z"` / `"X.Y.Z-betaN"` / `"X.Y.Z-fixN"` tag into a
/// comparable `(major, minor, patch, stage, num)` tuple, or `None` if it isn't a
/// three-part numeric version.
///
/// `stage` 决定**同版本号之内**的先后（数字越大越新），`num` 是它的序号：
///
/// | stage | 含义 | 例 |
/// |---|---|---|
/// | 1 | 正式版（无后缀；认不出的后缀也按此容忍） | `0.7.9` |
/// | 2 | 补丁版 `-fixN`（旧写法，`-betaN` 的前身） | `0.7.9` **<** `0.7.9-fix1` |
/// | 3 | 测试版 `-alphaN` | `0.7.9` **<** `0.7.9-alpha1` |
/// | 4 | 测试版 `-betaN`（现在的写法） | `0.7.9` **<** `0.7.9-beta1` |
/// | 5 | `-rcN`（成熟度最高） | `0.7.9` **<** `0.7.9-rc1` |
///
/// 即带后缀的版本一律**排在基准正式版之后**：`-fixN` / `-betaN` 都是"正式版发出去以后，
/// 在它之上继续做出来的构建"——线上时间线可以印证（`v0.7.8` 09-17 → `v0.7.8-fix1..5`
/// 09-18 → `v0.7.9` 09-20），用户 2026/09/21 也明确过这条约定："带有 beta 的比正式版要新"。
/// 少了 `stage` 这一位，`0.7.9-beta1` 会被算成**等于** `0.7.9`，用户就永远收不到
/// 测试版的更新提示（与当年 `-fixN` 那个坑同源，见 #48）。
///
/// 全序示例：`0.7.9 < 0.7.9-fix1 < 0.7.9-beta1 < 0.7.9-rc1 < 0.7.10-beta1`。
/// `alpha` / `beta` / `rc` 的先后只是"成熟度"约定（同一个号的几种测试版不会并存）。
/// 序号解析不出来时记 0。
/// 解析后的版本号：`(major, minor, patch, stage, num)`（语义见 [`parse_version`]）。
/// 抽成别名是为了让 `Option<(Value, Version)>` 这类签名过得了 clippy 的 `type_complexity`。
pub(crate) type Version = (u32, u32, u32, u8, u32);

/// 测试版（`-alphaN` / `-betaN` / `-rcN`）？「测试版」更新通道就只认它们。
///
/// 注意**不是** SemVer 的 `prerelease`：按本项目的约定，测试版排在**同号正式版之后**
/// （见 `parse_version` 的 stage 表），所以它和"版本更小"无关，纯粹是写法判定。
pub(crate) fn is_test_build(v: &Version) -> bool {
    v.3 >= 3
}

pub(crate) fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim().trim_start_matches('v');
    let (core, suffix) = match s.split_once('-') {
        Some((core, rest)) => (core, rest),
        None => (s, ""),
    };
    let (stage, num) = if suffix.is_empty() {
        (1u8, 0u32)
    } else if let Some(n) = suffix.strip_prefix("fix") {
        (2, n.parse().unwrap_or(0))
    } else if let Some(n) = suffix.strip_prefix("alpha") {
        (3, n.parse().unwrap_or(0))
    } else if let Some(n) = suffix.strip_prefix("beta") {
        (4, n.parse().unwrap_or(0))
    } else if let Some(n) = suffix.strip_prefix("rc") {
        (5, n.parse().unwrap_or(0))
    } else {
        // 认不出来的后缀仍按"正式版"容忍（沿用旧行为）。
        (1, 0)
    };
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor, patch, stage, num))
}

#[cfg(test)]
mod version_tests {
    use super::{is_test_build, parse_version};

    #[test]
    fn parses_plain_and_prefixed_versions() {
        assert_eq!(parse_version("0.7.7"), Some((0, 7, 7, 1, 0)));
        assert_eq!(parse_version("v0.7.7"), Some((0, 7, 7, 1, 0)));
        assert_eq!(parse_version(" v1.20.3 "), Some((1, 20, 3, 1, 0)));
        // 测试版后缀：序号进第 5 位（旧行为是并进 patch，见 #48），stage 分成熟度。
        assert_eq!(parse_version("0.7.3-rc1"), Some((0, 7, 3, 5, 1)));
        assert_eq!(parse_version("0.7.9-beta2"), Some((0, 7, 9, 4, 2)));
        assert_eq!(parse_version("v0.7.9-alpha3"), Some((0, 7, 9, 3, 3)));
        assert_eq!(parse_version("nonsense"), None);
        assert_eq!(parse_version("1.2"), None);
    }

    /// 补丁版必须能**排在对应正式版之后** —— 少了这一位，`0.7.7-fix1` 会与
    /// `0.7.7` 相等，更新检查就永远不会提示用户（release 流程上的静默失败）。
    #[test]
    fn fix_releases_sort_after_their_base_version() {
        assert_eq!(parse_version("0.7.7-fix1"), Some((0, 7, 7, 2, 1)));
        assert_eq!(parse_version("v0.7.7-fix12"), Some((0, 7, 7, 2, 12)));

        let v = |s: &str| parse_version(s).unwrap();
        assert!(v("0.7.7-fix1") > v("0.7.7"), "补丁版必须新于它的基准版");
        assert!(v("0.7.7-fix2") > v("0.7.7-fix1"), "fix 号递增");
        assert!(v("0.7.8") > v("0.7.7-fix9"), "下一个小版本仍然更大");
        assert!(v("0.8.0") > v("0.7.7-fix1"));
        assert_eq!(v("0.7.7-fix1"), v("v0.7.7-fix1"), "v 前缀不影响比较");
    }

    /// **测试版必须排在它的基准正式版之后**（`0.7.9-beta1 > 0.7.9`）—— 用户 2026/09/21
    /// 明确过这条约定：`-betaN` 是"正式版发出去之后、在它之上继续做出来的构建"
    /// （`-fixN` 的替代写法，线上时间线 `v0.7.8` → `v0.7.8-fix1..5` → `v0.7.9` 可印证）。
    ///
    /// 反过来写的话，「全通道最新版」与「测试版」两个通道**永远提示不出测试版**：
    /// `latest <= current` 会把它判成"更旧"。
    #[test]
    fn test_builds_sort_after_their_base_version() {
        let v = |s: &str| parse_version(s).unwrap();
        assert!(v("0.7.9-beta1") > v("0.7.9"), "测试版新于同号正式版");
        assert!(v("0.7.9-beta2") > v("0.7.9-beta1"), "beta 序号递增");
        assert!(v("0.7.9-fix1") > v("0.7.9"), "补丁版同样在基准版之后（老写法）");
        assert!(v("0.7.9-beta1") > v("0.7.9-alpha1"), "beta 比 alpha 成熟");
        assert!(v("0.7.9-rc1") > v("0.7.9-beta9"), "rc 比 beta 成熟");
        assert!(v("0.7.10-beta1") > v("0.7.9-rc9"), "下一个版本的测试版仍然更新");
        assert!(v("0.7.9-beta1") > v("0.7.8"), "上一个正式版之后才轮到它");
    }

    /// 「测试版」通道用它筛 release：只有 alpha/beta/rc 算，`-fixN` 与认不出的后缀都不算。
    #[test]
    fn is_test_build_covers_alpha_beta_rc_only() {
        for t in ["0.7.9-beta1", "0.7.9-alpha2", "0.7.9-rc3"] {
            assert!(is_test_build(&parse_version(t).unwrap()), "{t} 应算测试版");
        }
        for t in ["0.7.9", "0.7.9-fix1", "0.7.9-whatever"] {
            assert!(!is_test_build(&parse_version(t).unwrap()), "{t} 不该算测试版");
        }
    }
}
