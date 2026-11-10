use super::changelog_section;

const MD: &str = "\
# Changelog\n\
\n\
## [Unreleased]\n\
\n\
### 新增 / Added\n\
\n\
- 还没发布的改动\n\
\n\
## [0.7.9] - 2026-09-19\n\
\n\
### 修复 / Fixed\n\
\n\
- **中英对照的一条** —— bilingual bullet\n\
\n\
## [0.7.8] - 2026-09-17\n\
\n\
- 上一个版本\n";

/// 取到的是该版本那一段，**不含**相邻版本的内容。
#[test]
fn extracts_only_the_requested_section() {
    let s = changelog_section(MD, "0.7.9").expect("应找到 0.7.9 段");
    assert!(s.contains("中英对照的一条 —— bilingual bullet"));
    assert!(s.contains("修复 / Fixed"), "小节标题也在段落里（清洗成纯文本后保留）");
    assert!(!s.contains("还没发布的改动"), "不该带 Unreleased 的内容");
    assert!(!s.contains("上一个版本"), "不该带下一个版本段的内容");
}

/// `[0.7.9]` 不能误配 `[0.7.9-beta1]`、`[0.7.90]` 这类。
#[test]
fn does_not_match_a_longer_version() {
    let md = "## [0.7.9-beta1] - 2026-09-21\n\n- 测试版改动\n\n## [0.7.90] - x\n\n- 别的\n";
    assert!(changelog_section(md, "0.7.9").is_none(), "前缀相同但不是它");
    assert!(changelog_section(md, "0.7.9-beta1").is_some(), "测试版段落要能取到");
}

/// 没有这个版本、或段落是空的 → `None`（调用方据此退回 release body）。
#[test]
fn missing_or_empty_section_is_none() {
    assert!(changelog_section(MD, "9.9.9").is_none());
    assert!(changelog_section("## [1.0.0] - x\n\n## [0.9.0] - y\n\n- z\n", "1.0.0").is_none());
    assert!(changelog_section("", "0.7.9").is_none());
}
