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
