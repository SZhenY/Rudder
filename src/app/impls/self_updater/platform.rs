use super::*;

pub(crate) const REPO_OWNER: &str = "SZhenY";
pub(crate) const REPO_NAME: &str = "Rudder";

/// GitHub `/releases?per_page=30` 的整包上限。正常 300 KB~1 MB，2 MiB 只拦异常响应。
pub(crate) const MAX_RELEASES_BODY: u64 = 2 * 1024 * 1024;
/// `CHANGELOG.md` 整份上限（当前约 340 KB）。
pub(crate) const MAX_CHANGELOG_BODY: u64 = 1024 * 1024;

/// 读 HTTP body 时先卡上限：异常响应（或被劫持的代理）不能把几十 MB 拉进内存。
pub(crate) fn read_body_capped(
    resp: reqwest::blocking::Response,
    limit: u64,
    what: &str,
) -> Result<String> {
    let mut buf = Vec::new();
    resp.take(limit + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("read {what} response"))?;
    if buf.len() as u64 > limit {
        anyhow::bail!("{what} response exceeds {limit} bytes");
    }
    String::from_utf8(buf).with_context(|| format!("decode {what} response"))
}

/// 一个可用的更新：版本号 + 待下载资产。
#[derive(Debug, Clone)]
pub(crate) struct UpdateCandidate {
    pub version: String,
    pub asset_name: String,
    pub download_url: String,
    /// 发布说明（该 tag 的 CHANGELOG 段落，见 [`notes_for`]）。
    pub notes: String,
    /// 目标版本比本机**低**：这是"降级 / 切换"（见 [`is_channel_switch`]），不是升级。
    pub is_switch: bool,
    /// 选中的是 `.msi` 安装包而不是 `.zip` 便携包。
    pub is_msi: bool,
}

/// 当前这份 Rudder 是怎么装上去的。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum InstallKind {
    /// 便携版：解压即用。更新时替换 exe。
    Portable,
    /// Windows MSI 安装（装在 Program Files，由 Windows Installer 管理）。
    ///
    /// 这种安装不能走"替换 exe"：普通权限写不进 Program Files，而且替换后
    /// MSI 记录的组件哈希与磁盘文件不符，日后的"修复安装"会把旧 exe 写回来。
    Msi,
}

/// MSI 安装目录（`wix/main.wxs` 写入的 `HKLM\Software\Rudder\InstallDir`）。
///
/// 便携版不写这个值，所以"键存在且等于当前 exe 所在目录"就是 MSI 安装的判据。
/// 只看键存在不够：用户可能把便携版解压到别处运行。
#[cfg(windows)]
pub(crate) fn msi_install_dir() -> Option<PathBuf> {
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegGetValueW(
            hkey: usize,
            sub_key: *const u16,
            value: *const u16,
            flags: u32,
            ty: *mut u32,
            data: *mut core::ffi::c_void,
            size: *mut u32,
        ) -> i32;
    }
    const HKEY_LOCAL_MACHINE: usize = 0x8000_0002;
    /// 只接受 REG_SZ；KEY_WOW64_64KEY 避免 32 位进程被重定向到 WOW6432Node
    /// （64 位 MSI 写在 64 位视图里）。
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;
    const RRF_SUBKEY_WOW6464KEY: u32 = 0x0001_0000;
    const ERROR_SUCCESS: i32 = 0;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let sub_key = wide(r"SOFTWARE\Rudder");
    let value_name = wide("InstallDir");
    // 路径不可能超过 Windows 的 MAX_PATH 上限，定长缓冲够用，省掉两段式查询。
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: 两个字符串都以 NUL 结尾且在调用期间存活；`buf` 的大小与 `size`
    // 声明的字节数一致，函数只会写这么多。
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            sub_key.as_ptr(),
            value_name.as_ptr(),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    // size 含结尾 NUL；<= 2 说明是空串。
    if rc != ERROR_SUCCESS || size <= 2 {
        return None;
    }
    let len = (size as usize / 2).saturating_sub(1);
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

#[cfg(not(windows))]
pub(crate) fn msi_install_dir() -> Option<PathBuf> {
    None
}

/// 路径比较：Windows 上大小写不敏感，且注册表可能带结尾反斜杠。
pub(crate) fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase()
    };
    !norm(a).is_empty() && norm(a) == norm(b)
}

pub(crate) fn install_kind() -> InstallKind {
    if cfg!(windows)
        && let Some(dir) = msi_install_dir()
        && let Ok(exe) = current_exe()
        && let Some(parent) = exe.parent()
        && same_dir(&dir, parent)
    {
        return InstallKind::Msi;
    }
    InstallKind::Portable
}

/// 主力 Linux 构建的 glibc 底线：release.yml 的 `ubuntu-24.04` 作业。
///
/// 更老的发行版跑不了它，只能拿 `debian:10`（glibc 2.28）容器里编的 `-glibc228` 变体。
pub(crate) const MAIN_BUILD_GLIBC: (u32, u32) = (2, 39);

pub(crate) fn glibc_supports_main_build(glibc: Option<(u32, u32)>) -> bool {
    matches!(glibc, Some(v) if v >= MAIN_BUILD_GLIBC)
}

/// 平台 + 架构 + glibc → release 资产名里的**唯一标识片段**。
///
/// 片段必须带扩展名：Linux 的普通包与 `-glibc228` 兼容包共用 `linux-x86_64` 前缀，
/// 只写到架构的话两者都命中，取列表里靠前的那个 —— 并行上传时恰好是兼容包，等于
/// 让所有新系统都降级到按 glibc 2.28 编的构建。带上 `.tar.gz` 后
/// `...-linux-x86_64.tar.gz` 与 `...-linux-x86_64-glibc228.tar.gz` 互不包含。
///
/// 抽成纯函数是为了能测所有平台组合：CI 的测试作业只跑 Linux，用 `cfg!` 分支的话
/// macOS / Windows 的匹配规则永远测不到。
///
/// `glibc` 只在 Linux 上有意义，`None` 表示判定不出本机版本 —— 按兼容包处理，
/// 宁可多兼容（兼容包在新系统上照跑），也不能让老系统装上起不来的包。
///
/// 对应 `.github/workflows/release.yml` 的 `matrix.name`。
pub(crate) fn asset_keywords_for(
    os: &str,
    arch: &str,
    glibc: Option<(u32, u32)>,
    msi: bool,
) -> &'static [&'static str] {
    // MSI 只存在于 Windows 正式版，且只在检测到当前的 Rudder 本身就是 MSI 安装时
    // 才选它 —— 便携版去装 msi 等于凭空加一份系统安装记录。
    if msi {
        return match arch {
            "aarch64" => &["windows-aarch64.msi"],
            _ => &["windows-x86_64.msi"],
        };
    }
    match (os, arch) {
        // 两个 macOS 包都含 "macos" 字样，架构不同。必须按架构精确匹配，且**不放
        // 裸 "macos" 兜底**——兜底会命中先返回的那个包，Intel 与 Apple Silicon 互相
        // 拿到对方的二进制，替换后无法启动。宁可判定"无更新"，也不能装错架构。
        ("macos", "aarch64") => &["macos-aarch64.zip"],
        ("macos", _) => &["macos-x86_64.zip"],
        // Windows on ARM64 ships its own asset; the x86_64 keywords must not
        // match there or an ARM64 user would be handed the emulated build.
        ("windows", "aarch64") => &["windows-aarch64.zip"],
        ("windows", _) => &["windows-x86_64.zip"],
        // Linux：同一架构有两个包，差别只在 glibc 底线。主力包优先——兼容包虽然
        // 到处都能跑，但它按 glibc 2.28 编，没必要让新系统降级。
        (_, "aarch64") => {
            if glibc_supports_main_build(glibc) {
                &["linux-aarch64.tar.gz"]
            } else {
                &["linux-aarch64-glibc228.tar.gz"]
            }
        }
        _ => {
            if glibc_supports_main_build(glibc) {
                &["linux-x86_64.tar.gz"]
            } else {
                &["linux-x86_64-glibc228.tar.gz"]
            }
        }
    }
}

/// 本机 glibc 版本（仅 gnu 目标的 Linux 可判定）。
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn current_glibc() -> Option<(u32, u32)> {
    // SAFETY: `gnu_get_libc_version` 返回指向静态字符串的指针，进程生命周期内有效。
    let raw = unsafe { libc::gnu_get_libc_version() };
    if raw.is_null() {
        return None;
    }
    let text = unsafe { std::ffi::CStr::from_ptr(raw) }.to_str().ok()?;
    let mut parts = text.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// musl / 非 Linux：没有 glibc 可言，交给 `None` 的兼容分支。
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) fn current_glibc() -> Option<(u32, u32)> {
    None
}

/// 当前平台接受的资产扩展名。
///
/// `msi` 为真时只认 `.msi`（安装版交给系统安装程序）；否则排除安装器与
/// AppImage —— 便携版的自更新只换二进制。
pub(crate) fn asset_extension_for(os: &str, msi: bool) -> &'static str {
    if msi {
        return ".msi";
    }
    match os {
        "macos" | "windows" => ".zip",
        _ => ".tar.gz",
    }
}

/// 当前进程的可执行文件路径。
pub(crate) fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("resolve current executable")
}

/// 清理自更新留下的旧可执行文件。
///
/// Windows 上 `self_replace` 只能把**正在运行**的 exe 改名
/// （`.<名字>.<随机串>.__selfdelete__.exe`）—— 改名后仍然删不掉，句柄还开着。
/// crate 文档把这条列为已知限制，并说临时路径不固定、不建议自动清理；但我们的残留
/// 永远落在 exe 同目录，条件明确，启动时按标记名删掉即可。
pub(crate) fn cleanup_leftover_binaries() {
    let Ok(exe) = current_exe() else { return };
    let Some(dir) = exe.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // 只认 updater 生成的标记名；绝不删当前进程自己的 exe。
        let marked = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains("__selfdelete__"));
        if !marked || path == exe {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!("self-update: removed leftover binary {}", path.display()),
            Err(e) => tracing::warn!("self-update: cannot remove {}: {e}", path.display()),
        }
    }
}

/// macOS：进程是否跑在 .app 包内；返回包路径。
pub(crate) fn macos_app_bundle(exe: &Path) -> Option<PathBuf> {
    // …/rudder.app/Contents/MacOS/rudder
    let macos_dir = exe.parent()?; // …/Contents/MacOS
    let contents = macos_dir.parent()?; // …/Contents
    let app = contents.parent()?; // …/rudder.app
    if app.extension().and_then(|e| e.to_str()) == Some("app") {
        Some(app.to_path_buf())
    } else {
        None
    }
}

/// macOS App Translocation：从带隔离属性的 dmg/zip 首次运行时，系统把 app
/// 挂到只读的 `/private/var/folders/…/AppTranslocation/`，无法原地更新。
pub(crate) fn is_translocated(exe: &Path) -> bool {
    cfg!(target_os = "macos") && translocated_path(exe)
}

/// Pure path-shape check, split from `is_translocated` so the test can run on
/// every platform (the cfg! gate above would make the assert vacuous on CI).
pub(crate) fn translocated_path(exe: &Path) -> bool {
    exe.to_string_lossy().contains("/AppTranslocation/")
}
