//! 在线一键更新（GitHub Releases）。
//!
//! 流程：列 release → 按平台挑资产 → 下载（进度）→ 安装 → 重启。
//! 安装分两条路：
//! - 便携版（zip / tar.gz，以及 macOS 的 `.app`）：解压后原地替换，
//!   由用户点"重启"生效；
//! - Windows MSI 安装：不碰文件，把下载好的 `.msi` 交给 `msiexec`（UAC 提权），
//!   应用随即退出让出 exe 的文件锁。
//!
//! 与 `updater.rs` 的分工：后者只负责"有新版本"的检查与横幅；本模块负责
//! 真正的下载与替换。
//!
//! 为什么不用 `self_update` 的自动资产匹配：Rudder 的 release 资产按
//! `matrix.name` 命名（`rudder-<ver>-macos-aarch64.zip`），不含 target triple，
//! `asset_for(get_target())` 匹配不到——所以走手动流程。

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

#[path = "self_updater/platform.rs"] mod platform;
pub(crate) use platform::*;
#[path = "self_updater/discover.rs"] mod discover;
pub(crate) use discover::*;
#[path = "self_updater/install.rs"] mod install;
pub(crate) use install::*;
#[path = "self_updater/notes.rs"] mod notes;
pub(crate) use notes::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// 手写解压必须还原 Unix 权限位与符号链接 —— 漏了前者，macOS 更新后
    /// `Contents/MacOS/rudder` 会变成 0644，app 直接打不开（0.8.0-beta3 / 0.8.1 的真实故障）；
    /// 漏了后者，framework 的 `Versions/Current` 会被实体化，bundle 结构损坏。
    #[cfg(unix)]
    #[test]
    fn manual_zip_extract_restores_mode_and_symlinks() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        use zip::write::SimpleFileOptions;

        let dir = std::env::temp_dir().join(format!("rudder-zip-extract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("update.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            writer
                .start_file(
                    "rudder.app/Contents/MacOS/rudder",
                    SimpleFileOptions::default().unix_permissions(0o755),
                )
                .unwrap();
            writer.write_all(b"#!/bin/sh\n").unwrap();
            writer
                .add_symlink(
                    "rudder.app/Contents/Frameworks/Current",
                    "Versions/A",
                    SimpleFileOptions::default(),
                )
                .unwrap();
            writer.finish().unwrap();
        }

        let out = dir.join("out");
        extract_zip_manually(&zip_path, &out).expect("extract");

        let exe = out.join("rudder.app/Contents/MacOS/rudder");
        let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "可执行位必须还原，否则更新后 app 打不开");

        let link = out.join("rudder.app/Contents/Frameworks/Current");
        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "符号链接不能被实体化成普通文件"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// beta3 的 9 个资产，顺序取自 GitHub API（并行上传，glibc228 与 AppImage 混在中间）。
    const REAL_ASSETS: [&str; 9] = [
        "rudder-v0.8.0-beta3-linux-aarch64-glibc228.tar.gz",
        "rudder-v0.8.0-beta3-linux-aarch64.tar.gz",
        "rudder-v0.8.0-beta3-linux-x86_64-glibc228.tar.gz",
        "rudder-v0.8.0-beta3-linux-x86_64.AppImage",
        "rudder-v0.8.0-beta3-linux-x86_64.tar.gz",
        "rudder-v0.8.0-beta3-macos-aarch64.zip",
        "rudder-v0.8.0-beta3-macos-x86_64.zip",
        "rudder-v0.8.0-beta3-windows-aarch64.zip",
        "rudder-v0.8.0-beta3-windows-x86_64.zip",
    ];

    /// v0.8.0 正式版的 11 个资产 —— MSI 只在正式版产出，beta 版没有。
    const RELEASE_ASSETS: [&str; 11] = [
        "rudder-v0.8.0-linux-aarch64-glibc228.tar.gz",
        "rudder-v0.8.0-linux-aarch64.tar.gz",
        "rudder-v0.8.0-linux-x86_64-glibc228.tar.gz",
        "rudder-v0.8.0-linux-x86_64.AppImage",
        "rudder-v0.8.0-linux-x86_64.tar.gz",
        "rudder-v0.8.0-macos-aarch64.zip",
        "rudder-v0.8.0-macos-x86_64.zip",
        "rudder-v0.8.0-windows-aarch64.msi",
        "rudder-v0.8.0-windows-aarch64.zip",
        "rudder-v0.8.0-windows-x86_64.msi",
        "rudder-v0.8.0-windows-x86_64.zip",
    ];

    /// 按现有规则在给定资产表里挑一个。
    fn pick_from(
        assets: &[&'static str],
        os: &str,
        arch: &str,
        glibc: Option<(u32, u32)>,
        msi: bool,
    ) -> Option<&'static str> {
        let kw = asset_keywords_for(os, arch, glibc, msi);
        let ext = asset_extension_for(os, msi);
        assets
            .iter()
            .find(|n| n.ends_with(ext) && kw.iter().any(|k| n.contains(k)))
            .copied()
    }

    /// 便携版口径（zip / tar.gz）。
    fn pick(os: &str, arch: &str, glibc: Option<(u32, u32)>) -> Option<&'static str> {
        pick_from(&REAL_ASSETS, os, arch, glibc, false)
    }

    /// 三个平台 × 两个架构，各自落到自己那份构建上。
    #[test]
    fn every_platform_picks_its_own_asset() {
        assert_eq!(
            pick("macos", "aarch64", None),
            Some("rudder-v0.8.0-beta3-macos-aarch64.zip")
        );
        assert_eq!(
            pick("macos", "x86_64", None),
            Some("rudder-v0.8.0-beta3-macos-x86_64.zip")
        );
        assert_eq!(
            pick("windows", "aarch64", None),
            Some("rudder-v0.8.0-beta3-windows-aarch64.zip")
        );
        assert_eq!(
            pick("windows", "x86_64", None),
            Some("rudder-v0.8.0-beta3-windows-x86_64.zip")
        );
    }

    /// Linux 按本机 glibc 选构建：新系统用主力包，老系统与判定不出的用兼容包。
    ///
    /// 兼容包在新系统上照跑，所以判定失败时往兼容侧退化是安全的；反过来会让
    /// glibc < 2.39 的发行版装上起不来的二进制。
    #[test]
    fn linux_picks_build_matching_local_glibc() {
        assert_eq!(
            pick("linux", "x86_64", Some((2, 39))),
            Some("rudder-v0.8.0-beta3-linux-x86_64.tar.gz")
        );
        assert_eq!(
            pick("linux", "x86_64", Some((2, 31))),
            Some("rudder-v0.8.0-beta3-linux-x86_64-glibc228.tar.gz")
        );
        assert_eq!(
            pick("linux", "x86_64", None),
            Some("rudder-v0.8.0-beta3-linux-x86_64-glibc228.tar.gz")
        );
        assert_eq!(
            pick("linux", "aarch64", Some((2, 40))),
            Some("rudder-v0.8.0-beta3-linux-aarch64.tar.gz")
        );
        assert_eq!(
            pick("linux", "aarch64", Some((2, 28))),
            Some("rudder-v0.8.0-beta3-linux-aarch64-glibc228.tar.gz")
        );
    }

    /// 选择结果不能依赖资产排列顺序 —— API 返回顺序由并行上传决定，不可控。
    #[test]
    fn asset_order_does_not_matter() {
        let cases = [
            ("macos", "aarch64", None),
            ("macos", "x86_64", None),
            ("windows", "aarch64", None),
            ("windows", "x86_64", None),
            ("linux", "x86_64", Some((2, 39))),
            ("linux", "x86_64", Some((2, 28))),
            ("linux", "aarch64", Some((2, 39))),
            ("linux", "aarch64", Some((2, 28))),
        ];
        for (os, arch, glibc) in cases {
            let kw = asset_keywords_for(os, arch, glibc, false);
            let ext = asset_extension_for(os, false);
            let forward = REAL_ASSETS
                .iter()
                .find(|n| n.ends_with(ext) && kw.iter().any(|k| n.contains(k)));
            let backward = REAL_ASSETS
                .iter()
                .rev()
                .find(|n| n.ends_with(ext) && kw.iter().any(|k| n.contains(k)));
            assert_eq!(forward, backward, "{os}/{arch} 的选择依赖资产顺序");
        }
    }

    /// AppImage 是单文件安装器，自更新换的是压缩包里的二进制。
    #[test]
    fn appimage_is_never_selected() {
        let appimage = "rudder-v0.8.0-beta3-linux-x86_64.AppImage";
        let kw = asset_keywords_for("linux", "x86_64", Some((2, 39)), false);
        // 关键字带 `.tar.gz`，名字里没有它，天然排除；扩展名过滤是第二道防线。
        assert!(!kw.iter().any(|k| appimage.contains(k)));
        assert!(!appimage.ends_with(asset_extension_for("linux", false)));
    }

    /// MSI 安装必须拿 msi：拿 zip 会撞上 Program Files 的写权限，而且替换后
    /// MSI 记录的组件哈希与磁盘文件不符，"修复安装"会把旧 exe 写回来。
    #[test]
    fn msi_install_picks_msi_never_zip() {
        assert_eq!(
            pick_from(&RELEASE_ASSETS, "windows", "x86_64", None, true),
            Some("rudder-v0.8.0-windows-x86_64.msi")
        );
        assert_eq!(
            pick_from(&RELEASE_ASSETS, "windows", "aarch64", None, true),
            Some("rudder-v0.8.0-windows-aarch64.msi")
        );
    }

    /// 升级必须继续装在旧版目录：命令行要带 public 属性 `APPLICATIONFOLDER`，
    /// 否则 msiexec 会退回包里的默认值 `Program Files\Rudder`。
    #[test]
    fn msi_args_carry_the_previous_install_dir() {
        let msi = Path::new(
            r"C:\Users\me\AppData\Local\Temp\rudder-update-ab12\rudder-v0.8.1-windows-x86_64.msi",
        );
        let args = msi_args(msi, Some(Path::new(r"D:\Apps\Rudder")));
        assert!(
            args.starts_with(r#"/i "C:\Users\me\AppData\Local\Temp\rudder-update-ab12\"#),
            "{args}"
        );
        assert!(
            args.contains(r#"APPLICATIONFOLDER="D:\Apps\Rudder""#),
            "{args}"
        );
        assert!(args.ends_with(" /qb /norestart"), "{args}");
    }

    /// 取不到安装目录时省略该属性，让 wix/main.wxs 的 AppSearch 兜底。
    #[test]
    fn msi_args_omit_missing_install_dir() {
        let args = msi_args(Path::new(r"C:\Temp\rudder.msi"), None);
        assert!(!args.contains("APPLICATIONFOLDER"), "{args}");
        assert!(args.ends_with(" /qb /norestart"), "{args}");
    }

    /// 反向：便携版不能拿到 msi，否则会凭空多出一条系统安装记录。
    #[test]
    fn portable_install_picks_zip_never_msi() {
        assert_eq!(
            pick_from(&RELEASE_ASSETS, "windows", "x86_64", None, false),
            Some("rudder-v0.8.0-windows-x86_64.zip")
        );
        assert_eq!(
            pick_from(&RELEASE_ASSETS, "windows", "aarch64", None, false),
            Some("rudder-v0.8.0-windows-aarch64.zip")
        );
    }

    /// msi 关键字按架构分开，且都带 `.msi` 后缀（否则会命中 zip）。
    #[test]
    fn msi_keywords_are_arch_specific() {
        assert_eq!(
            asset_keywords_for("windows", "aarch64", None, true)
                .first()
                .copied(),
            Some("windows-aarch64.msi")
        );
        assert_eq!(
            asset_keywords_for("windows", "x86_64", None, true)
                .first()
                .copied(),
            Some("windows-x86_64.msi")
        );
        assert_eq!(asset_extension_for("windows", true), ".msi");
    }

    #[test]
    fn keywords_cover_current_platform() {
        let msi = install_kind() == InstallKind::Msi;
        let kw = asset_keywords_for(
            std::env::consts::OS,
            std::env::consts::ARCH,
            current_glibc(),
            msi,
        );
        let ext = asset_extension_for(std::env::consts::OS, msi);
        // MSI 安装只能对照正式版资产表（beta 不产 msi）。
        let pool: &[&str] = if msi {
            &RELEASE_ASSETS[..]
        } else {
            &REAL_ASSETS[..]
        };
        let mine = pool
            .iter()
            .find(|n| n.ends_with(ext) && kw.iter().any(|k| n.contains(k)));
        assert!(mine.is_some(), "no asset matches {kw:?} with ext {ext}");
    }

    #[test]
    fn translocated_paths_are_detected() {
        // Path-shape logic is platform-independent (the macOS gate lives in
        // is_translocated); testing the pure fn keeps this green on CI.
        assert!(translocated_path(Path::new(
            "/private/var/folders/x/AppTranslocation/ABC/d/rudder.app/Contents/MacOS/rudder"
        )));
        assert!(!translocated_path(Path::new(
            "/Applications/rudder.app/Contents/MacOS/rudder"
        )));
    }

    #[test]
    fn macos_bundle_is_resolved_from_exe() {
        let exe = Path::new("/Applications/rudder.app/Contents/MacOS/rudder");
        assert_eq!(
            macos_app_bundle(exe),
            Some(PathBuf::from("/Applications/rudder.app"))
        );
        // 非 bundle（例如 cargo 直接跑的裸二进制）不应误判。
        assert_eq!(
            macos_app_bundle(Path::new("/tmp/target/release/rudder")),
            None
        );
    }
}

#[cfg(test)]
mod channel_tests {
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
}

#[cfg(test)]
mod notes_tests {
    use super::{MAX_NOTES_CHARS, release_notes};

    /// Markdown 标记要清掉：普通 `Text` 不渲染它，留下来的只是符号噪声。
    #[test]
    fn strips_markdown_markup() {
        let body = "## Improvements\n\n- **HTTP transfer recovery** — better retries\n- `foo` and __bar__\n";
        assert_eq!(
            release_notes(body),
            "Improvements\n\n• HTTP transfer recovery — better retries\n• foo and bar"
        );
    }

    /// HTML 注释（含跨行）整段丢掉 —— 发布模板常留着它们。
    #[test]
    fn drops_html_comments_even_multiline() {
        let body = "<!-- release template\nnotes for maintainers -->\nReal notes\n<!-- inline -->tail";
        assert_eq!(release_notes(body), "Real notes\ntail");
    }

    /// 空行压缩 + 首尾清理：对话框里那块框不许开头结尾是一片空白。
    #[test]
    fn collapses_blank_runs_and_trims_ends() {
        assert_eq!(release_notes("\n\nA\n\n\n\nB\n\n"), "A\n\nB");
        assert_eq!(release_notes("   \n  \n"), "");
    }

    /// 超长截断，并按**字符**算（中文说明不会被截成半个字）。
    #[test]
    fn truncates_long_bodies_by_chars() {
        let long = "更".repeat(MAX_NOTES_CHARS + 200);
        let out = release_notes(&long);
        assert_eq!(out.chars().count(), MAX_NOTES_CHARS + 1, "截断后带一个省略号");
        assert!(out.ends_with('…'));

        let short = "短说明";
        assert_eq!(release_notes(short), "短说明", "没超长就不该动它");
    }

    /// 没有 body（很常见）→ 空串，对话框据此整块不显示。
    #[test]
    fn empty_body_is_empty() {
        assert_eq!(release_notes(""), "");
    }
}


#[cfg(test)]
mod changelog_tests {
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
}

/// 这个候选是不是"**降级 / 切换**"（目标版本比本机低）而不是升级？
///
/// 只在「正式版」通道上提供切换：本机跑的是 `0.7.9-beta1` 这类测试版、而正式版最新是
/// `0.7.9` 时，用户**已经明确选了正式版通道**，那就该告诉他"可切换到旧版本"，
/// 而不是一句"已是最新版本"（用户 2026-09-21 的要求）。
///
/// `all` / `beta` 通道永不提供降级 —— 它们的语义就是"要更新的"；而且本机跑着比线上
/// 一切都新的自编译版本时（很常见），在那两个通道上弹降级只会莫名其妙。
pub(crate) fn is_channel_switch(channel: &str, current: super::Version, latest: super::Version) -> bool {
    channel == "stable" && latest < current
}


#[cfg(test)]
mod switch_tests {
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
}
