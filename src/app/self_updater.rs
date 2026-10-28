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

const REPO_OWNER: &str = "SZhenY";
const REPO_NAME: &str = "Rudder";

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
fn msi_install_dir() -> Option<PathBuf> {
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
fn msi_install_dir() -> Option<PathBuf> {
    None
}

/// 路径比较：Windows 上大小写不敏感，且注册表可能带结尾反斜杠。
fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_ascii_lowercase()
    };
    !norm(a).is_empty() && norm(a) == norm(b)
}

fn install_kind() -> InstallKind {
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
const MAIN_BUILD_GLIBC: (u32, u32) = (2, 39);

fn glibc_supports_main_build(glibc: Option<(u32, u32)>) -> bool {
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
fn asset_keywords_for(
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
fn current_glibc() -> Option<(u32, u32)> {
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
fn current_glibc() -> Option<(u32, u32)> {
    None
}

/// 当前平台接受的资产扩展名。
///
/// `msi` 为真时只认 `.msi`（安装版交给系统安装程序）；否则排除安装器与
/// AppImage —— 便携版的自更新只换二进制。
fn asset_extension_for(os: &str, msi: bool) -> &'static str {
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
    let Ok(entries) = std::fs::read_dir(dir) else { return };
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
fn macos_app_bundle(exe: &Path) -> Option<PathBuf> {
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
fn is_translocated(exe: &Path) -> bool {
    cfg!(target_os = "macos") && translocated_path(exe)
}

/// Pure path-shape check, split from `is_translocated` so the test can run on
/// every platform (the cfg! gate above would make the assert vacuous on CI).
fn translocated_path(exe: &Path) -> bool {
    exe.to_string_lossy().contains("/AppTranslocation/")
}

/// 按**更新通道**挑出最新的一个 release（返回它的 JSON，调用方再读 `tag_name` / `assets`）。
///
/// * `stable`：走 `/releases/latest` —— GitHub 保证它不含预发布，响应也最小；
/// * `beta` / `all`：拉一页 `/releases` 自己挑 —— GitHub **没有**"最新测试版"这种端点。
///   `beta` 只认测试版（`-alphaN` / `-betaN` / `-rcN`，即 `is_test_build`），
///   `all`（全通道最新版）认版本最高的那个，不挑类型 —— 于是它同时包含测试版与正式版。
///   排序按 `parse_version` 的约定：**同号的测试版比正式版新**（`0.7.9-beta1 > 0.7.9`），
///   所以正式版之后发出去的测试版能被提示到（见下面的 `channel_tests`）。
///
/// 预发布与否按 **tag 名**判断，而不是 API 的 `prerelease` 字段：tag 是权威
/// （`-betaN` 的语义写在我们自己的版本解析里），API 字段只是发布时的标记。
pub(crate) fn fetch_channel_release(channel: &str) -> Result<Option<serde_json::Value>> {
    let url = if channel == "stable" {
        format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest")
    } else {
        format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases?per_page=30")
    };
    let client = super::update_net::current()
        .client()
        .map_err(|e| anyhow!("{e}"))?;
    let body = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .map_err(|e| anyhow!("{}", super::update_net::describe(&e)))?
        .error_for_status()
        .map_err(|e| anyhow!("{}", super::update_net::describe(&e)))?
        .text()
        .context("read releases response")?;
    let json: serde_json::Value =
        serde_json::from_str(&body).context("parse releases response")?;
    if channel == "stable" {
        return Ok(Some(json));
    }

    let list = json.as_array().cloned().unwrap_or_default();
    Ok(pick_channel_release(&list, channel))
}

/// 从一页 `/releases` 的结果里挑出该通道要提示的那一个（`stable` 不走这里，它直接读
/// `/releases/latest`）。
///
/// 跳过草稿、以及 tag 认不出格式的项；`beta` 通道只要测试版（`is_test_build`），
/// `all` 什么都认 —— 取版本最高的那个。
///
/// 抽成纯函数是为了**能测**：这段挑选逻辑原先和网络请求缠在一起，唯一"验证"它的是线上的
/// 真实 release，而 `-betaN` 的排序约定（同号比正式版新）恰好是错的 —— 没有任何测试盯着。
fn pick_channel_release(list: &[serde_json::Value], channel: &str) -> Option<serde_json::Value> {
    let mut best: Option<(&serde_json::Value, super::Version)> = None;
    for release in list {
        if release["draft"].as_bool().unwrap_or(false) {
            continue;
        }
        let Some(v) = super::parse_version(release["tag_name"].as_str().unwrap_or_default()) else {
            continue;
        };
        if channel == "beta" && !super::is_test_build(&v) {
            continue;
        }
        if best.as_ref().is_none_or(|(_, best_v)| v > *best_v) {
            best = Some((release, v));
        }
    }
    best.map(|(release, _)| release.clone())
}

/// 查询 GitHub Releases，返回比 `current` 新的最新版本（无则 None）。
///
/// `channel` 决定查哪一类 release（见 [`fetch_channel_release`]）。
/// 静默策略沿用既有检查逻辑：网络/解析失败返回 Err，但调用方只记录日志。
pub(crate) fn latest_update(current: super::Version, channel: &str) -> Result<Option<UpdateCandidate>> {
    let Some(json) = fetch_channel_release(channel)? else {
        return Ok(None);
    };
    let tag = json["tag_name"].as_str().unwrap_or_default().to_string();
    let Some(latest) = super::parse_version(&tag) else {
        return Ok(None);
    };
    if latest == current {
        return Ok(None);
    }
    let is_switch = latest < current;
    if is_switch && !is_channel_switch(channel, current, latest) {
        return Ok(None);
    }

    // 挑资产：先按扩展名过滤，再按平台关键字（按优先级）匹配。
    // MSI 安装要 msi（交给系统安装程序去覆盖 Program Files 里的文件），便携版要
    // zip / tar.gz（替换 exe）。
    let msi = install_kind() == InstallKind::Msi;
    // 安装版不做降级：`wix/main.wxs` 的 MajorUpgrade 明确禁止降级
    // （DowngradeErrorMessage），把旧包交给 msiexec 只会弹一句"已安装更新的版本"
    // 然后退出。宁可判"无更新"，也不要让用户看到安装程序报错。
    if msi && is_switch {
        tracing::info!("self-update: MSI 安装不支持降级到 {tag}，已跳过");
        return Ok(None);
    }
    let ext = asset_extension_for(std::env::consts::OS, msi);
    let empty = Vec::new();
    let assets = json["assets"].as_array().unwrap_or(&empty);
    let candidates: Vec<(String, String)> = assets
        .iter()
        .filter_map(|a| {
            let name = a["name"].as_str()?;
            let url = a["browser_download_url"].as_str()?;
            if !name.ends_with(ext) {
                return None;
            }
            Some((name.to_string(), url.to_string()))
        })
        .collect();

    let notes = notes_for(&tag, json["body"].as_str().unwrap_or_default());
    for kw in asset_keywords_for(
        std::env::consts::OS,
        std::env::consts::ARCH,
        current_glibc(),
        msi,
    ) {
        if let Some((name, url)) = candidates.iter().find(|(n, _)| n.contains(kw)) {
            return Ok(Some(UpdateCandidate {
                version: tag.trim_start_matches('v').to_string(),
                asset_name: name.clone(),
                download_url: url.clone(),
                notes: notes.clone(),
                is_switch,
                is_msi: msi,
            }));
        }
    }
    // 安装版遇到不带 msi 的版本（预发布版按设计不产 msi）。这里只能判"无更新"，
    // 但必须留一条日志：否则用户切到测试版通道后看到"已是最新版本"，无从判断
    // 是自己已最新还是这个版本根本没提供安装包。
    if msi {
        tracing::info!("self-update: {tag} 未提供 MSI 安装包，安装版无法更新到该版本");
    }
    Ok(None)
}

/// 下载完成后的中间产物。
pub(crate) enum StagedUpdate {
    /// 压缩包已解压到目录（便携版 zip / tar.gz，或 macOS 的 .app）。
    Extracted(PathBuf),
    /// 安装包原样落盘（Windows MSI）。不解压 —— 交给系统安装程序，
    /// 它会按自己的记录覆盖 Program Files 里的文件并更新卸载项。
    Installer(PathBuf),
}

/// 下载资产到临时目录；压缩包顺带解压，MSI 原样保留。
///
/// `on_progress` 收到 0.0..=1.0 的进度（下载占 90%，解压占 10%）。
pub(crate) fn download_and_stage(
    cand: &UpdateCandidate,
    on_progress: impl Fn(f32),
) -> Result<StagedUpdate> {
    // 随机名 + 0700 + 创建即独占。老写法是 `/tmp/rudder-update-<pid>`：名字可预测，
    // 而且先 `remove_dir_all` 再 `create_dir_all`（会跟随符号链接）—— 共享机器上别人
    // 可以先摆一个同名符号链接，把下载与解压都引到他指定的目录去。
    let stage_dir = crate::config::create_private_temp_dir("rudder-update")?;

    let archive = stage_dir.join(&cand.asset_name);
    {
        let client = super::update_net::current()
            .client()
            .map_err(|e| anyhow!("{e}"))?;
        let resp = client
            .get(&cand.download_url)
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .map_err(|e| anyhow!("{}", super::update_net::describe(&e)))?
            .error_for_status()
            .map_err(|e| anyhow!("{}", super::update_net::describe(&e)))?;
        let total: u64 = resp.content_length().unwrap_or(0);
        // blocking 的 Response 直接实现 io::Read。
        let mut reader = resp;
        let mut file = std::fs::File::create(&archive).context("create archive file")?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut done: u64 = 0;
        loop {
            let n = reader.read(&mut buf).context("read download stream")?;
            if n == 0 {
                break;
            }
            use std::io::Write;
            file.write_all(&buf[..n]).context("write archive")?;
            done += n as u64;
            if total > 0 {
                // reqwest 开了 gzip：若代理转码，`done` 是解压后字节数，会大于
                // content-length 声明的压缩长度，进度要封顶。
                on_progress(((done as f32 / total as f32) * 0.9).min(0.9));
            }
        }
        // 连接中途被切断时 `read` 未必报错，可能只是提前返回 0。不核对长度的话，
        // 截断的 msi 会一路交给 msiexec，报出来的错误与网络毫无关系。
        // 只查 `done < total`：gzip 转码时 `done` 会大于声明值，那一条不能算失败。
        if total > 0 && done < total {
            return Err(anyhow!("下载不完整（{done}/{total} 字节），请重试"));
        }
    }
    on_progress(0.9);

    // MSI 不解压、不删包：安装程序要读它，清理交给系统。
    if cand.is_msi {
        on_progress(1.0);
        return Ok(StagedUpdate::Installer(archive));
    }

    let extract_dir = stage_dir.join("extracted");
    std::fs::create_dir_all(&extract_dir).context("create extract dir")?;
    extract_archive(&archive, &extract_dir)?;
    // 解压完就把压缩包删掉：不再需要它，也少留一份可执行文件在临时目录里。
    let _ = std::fs::remove_file(&archive);
    on_progress(1.0);
    Ok(StagedUpdate::Extracted(extract_dir))
}

/// 在解压目录里递归查找指定文件名的路径。
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().and_then(|n| n.to_str()) == Some(name) {
                return Some(p);
            }
        }
    }
    None
}

/// 安装结果：调用方据此决定提示什么。
pub(crate) enum InstallOutcome {
    /// 二进制已原地替换，重启后生效。
    Replaced,
    /// 已拉起系统安装程序（MSI）。它会自己覆盖文件并更新卸载记录，
    /// 应用应尽快退出把文件锁让出来。
    InstallerLaunched,
}

/// 把下载好的更新安装上去。
///
/// - Windows MSI 安装：拉起 `msiexec`，不碰文件（见 [`launch_msi_installer`]）。
/// - macOS：整个 `.app` 包替换（保留 CI 的 ad-hoc 签名；只换二进制会让
///   Gatekeeper 判定"已损坏"）。
/// - 便携版 Windows / Linux：用 `self_replace` 原地替换可执行文件。
pub(crate) fn install_staged(staged: &StagedUpdate) -> Result<InstallOutcome> {
    let extract_dir = match staged {
        StagedUpdate::Installer(msi) => {
            launch_msi_installer(msi)?;
            return Ok(InstallOutcome::InstallerLaunched);
        }
        StagedUpdate::Extracted(dir) => dir,
    };

    let exe = current_exe()?;
    if is_translocated(&exe) {
        return Err(anyhow!(
            "app is running from a read-only translocated location; move Rudder to /Applications first"
        ));
    }

    if cfg!(target_os = "macos")
        && let Some(app) = macos_app_bundle(&exe)
    {
        replace_app_bundle(&app, extract_dir)?;
        return Ok(InstallOutcome::Replaced);
    }
    replace_binary(&exe, extract_dir)?;
    Ok(InstallOutcome::Replaced)
}

/// `msiexec` 参数。
///
/// `install_dir` 有值时作为 `APPLICATIONFOLDER` 传入：包里的同名属性是 public 且
/// `Secure="yes"`，命令行赋值优先于 AppSearch 的搜索结果，升级于是落在旧版目录，
/// 而不是回到默认的 `Program Files\Rudder`。取不到安装目录时省略，退回包内搜索。
#[cfg(any(windows, test))]
fn msi_args(msi: &Path, install_dir: Option<&Path>) -> String {
    // 两个值都可能含空格（"C:\Program Files\..."），各自加引号。
    let mut args = format!("/i \"{}\"", msi.display());
    if let Some(dir) = install_dir {
        args.push_str(&format!(" APPLICATIONFOLDER=\"{}\"", dir.display()));
    }
    args.push_str(" /qb /norestart");
    args
}

/// 拉起 `msiexec` 安装刚下载的 msi（Windows，走 UAC 提权）。
///
/// 四条取舍：
/// - `runas` 动词：Program Files 要管理员权限，不提权必然失败。
/// - `/qb` 而不是 `/qn`：用户得看到进度条，否则双击之后几分钟毫无反应像是卡死。
/// - `/norestart`：更新不该顺手重启整台机器。
/// - 命令行显式传 `APPLICATIONFOLDER=<当前 exe 所在目录>`：它是包里的 public +
///   Secure 属性，命令行的值优先于 AppSearch 的任何搜索结果，所以升级一定落在
///   旧版目录，不会退回默认的 `Program Files\Rudder`。提权后该属性仍会传给
///   server 端（这正是 `Secure="yes"` 的作用）。
///
/// 调用返回时安装程序已经在跑，调用方随后退出进程，让 msiexec 拿到文件锁。
/// 若 msiexec 抢先进入卸载旧版阶段，Windows Installer 的 Restart Manager 会自己
/// 结束本进程 —— 这正是我们要的结果。
#[cfg(windows)]
fn launch_msi_installer(msi: &Path) -> Result<()> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: isize,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_cmd: i32,
        ) -> isize;
    }

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    let operation = wide(OsStr::new("runas"));
    let file = wide(OsStr::new("msiexec.exe"));
    // 能跑到这里说明 install_kind 已确认当前副本是 MSI 装的，exe 父目录就是
    // 注册表里的安装目录；取不到时留空，退回 wix/main.wxs 里的 AppSearch。
    let install_dir = current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let params = wide(OsStr::new(&msi_args(msi, install_dir.as_deref())));
    // SAFETY: 四个字符串都以 NUL 结尾且在调用期间存活；hwnd/环境/目录传空。
    let rc = unsafe {
        ShellExecuteW(
            0,
            operation.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            1,
        )
    };
    // ShellExecute 的约定：返回值 > 32 才算成功；1223 是用户在 UAC 点了"否"。
    if rc as usize <= 32 {
        return Err(anyhow!(
            "无法启动安装程序（ShellExecute={}）：安装版更新需要管理员权限，\
             请在弹出的 UAC 提示里选择“是”，或手动运行已下载的安装包：{}",
            rc as usize,
            msi.display()
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn launch_msi_installer(_msi: &Path) -> Result<()> {
    Err(anyhow!("MSI 更新只适用于 Windows"))
}

/// macOS：stash 旧 bundle → 移入新 bundle → 失败回滚。
fn replace_app_bundle(app: &Path, extract_dir: &Path) -> Result<()> {
    let new_app = find_bundle(extract_dir).ok_or_else(|| anyhow!("no .app bundle in update"))?;
    let parent = app
        .parent()
        .ok_or_else(|| anyhow!("app has no parent directory"))?;
    let backup = parent.join(format!(
        "{}.old",
        app.file_name().unwrap_or_default().to_string_lossy()
    ));
    // 清掉上一轮遗留的备份，保证 stash 是干净的 rename。
    if backup.exists() {
        let _ = std::fs::remove_dir_all(&backup);
    }
    let staged = parent.join(format!(
        "{}.new",
        app.file_name().unwrap_or_default().to_string_lossy()
    ));
    if staged.exists() {
        let _ = std::fs::remove_dir_all(&staged);
    }
    // 先把新包放到目标目录（同一文件系统，rename 才原子）。
    std::fs::rename(&new_app, &staged)
        .or_else(|_| copy_dir_recursive(&new_app, &staged))
        .context("stage new bundle next to the current one")?;

    if let Err(e) = std::fs::rename(app, &backup) {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(anyhow!("stash current bundle: {e}"));
    }
    if let Err(e) = std::fs::rename(&staged, app) {
        // 回滚：把旧包放回去。
        let _ = std::fs::rename(&backup, app);
        let _ = std::fs::remove_dir_all(&staged);
        return Err(anyhow!("install new bundle: {e}"));
    }
    Ok(())
}

fn find_bundle(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if p.extension().and_then(|e| e.to_str()) == Some("app") {
                    return Some(p);
                }
                stack.push(p);
            }
        }
    }
    None
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src).context("read source dir")? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir_recursive(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Windows / Linux：用 self_replace 换掉当前可执行文件。
fn replace_binary(_exe: &Path, extract_dir: &Path) -> Result<()> {
    let name = if cfg!(target_os = "windows") {
        "rudder.exe"
    } else {
        "rudder"
    };
    let new_bin = find_file(extract_dir, name)
        .ok_or_else(|| anyhow!("update archive does not contain {name}"))?;
    self_replace::self_replace(&new_bin).context("replace executable in place")
}

/// 解压发布包（.zip 或 .tar.gz）。自己实现是为了不再依赖 `self_update` ——
/// 它强制要求启用一个 HTTP 客户端特性，会把第二份 reqwest 与 aws-lc-sys 拉回依赖图。
fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    let name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if name.ends_with(".zip") {
        let file = std::fs::File::open(archive).context("open update zip")?;
        let mut zip = zip::ZipArchive::new(file).context("read update zip")?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).context("read zip entry")?;
            // `enclosed_name` 拒绝 `../` 之类越界路径：压缩包来自网络，必须防穿越。
            let Some(rel) = entry.enclosed_name() else {
                continue;
            };
            let out = dest.join(rel);
            if entry.is_dir() {
                std::fs::create_dir_all(&out).context("create dir from zip")?;
                continue;
            }
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent).context("create parent from zip")?;
            }
            let mut f = std::fs::File::create(&out).context("create file from zip")?;
            std::io::copy(&mut entry, &mut f).context("write file from zip")?;
        }
        return Ok(());
    }

    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        let file = std::fs::File::open(archive).context("open update tar.gz")?;
        let gz = flate2::read::GzDecoder::new(file);
        tar::Archive::new(gz)
            .unpack(dest)
            .context("unpack update tar.gz")?;
        return Ok(());
    }

    anyhow::bail!("unsupported update archive: {name}")
}

/// 用新版本重启应用（Unix 用 exec 保留 PID，Windows 起新进程并退出当前进程）。
pub(crate) fn restart_app() -> Result<()> {
    let exe = current_exe()?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(std::env::args_os().skip(1));

    #[cfg(windows)]
    {
        cmd.spawn().context("spawn new instance")?;
        // 新进程已经起来，当前进程直接退出；调用方不会拿到返回值。
        std::process::exit(0);
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(anyhow!("exec new instance: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

/// 从 `CHANGELOG.md` 里取出某个版本的段落（`## [0.7.9] - …` 到下一个 `## [` 之间）。
///
/// 为什么发布说明不只用 release 的 `body`：它是 GitHub 自动生成的（我们线上那几个 release
/// 实际上常常是空的），而 `CHANGELOG.md` 是**中英对照**手写的 —— 用户要看的就是那份。
/// 段落里带 Markdown，交给 [`release_notes`] 清洗。
///
/// `version` 不带 `v`（tag 去掉前缀后的形式）。
pub(crate) fn changelog_section(md: &str, version: &str) -> Option<String> {
    let want = format!("[{version}]");
    let mut started = false;
    let mut out: Vec<&str> = Vec::new();
    for line in md.lines() {
        if let Some(head) = line.strip_prefix("## ") {
            if started {
                break; // 下一个版本段开始 → 本段结束
            }
            // `## [0.7.9] - 2026-09-19`；`[0.7.9]` 不会误配 `[0.7.9-beta1]`（差在中括号）。
            started = head.trim().starts_with(&want);
            continue;
        }
        if started {
            out.push(line);
        }
    }
    if !started || out.is_empty() {
        return None;
    }
    let text = release_notes(&out.join("\n"));
    if text.is_empty() { None } else { Some(text) }
}

/// 拉某个 tag 的 `CHANGELOG.md`（raw.githubusercontent）。失败返回 `None` ——
/// 调用方会退回 release 的 `body`，网络不好时不至于没说明可看。
///
/// 走与检查 / 下载同一个 Agent：这个域同样可能只有代理能通、同样可能被 TLS 中间人拦，
/// 用裸 `ureq::get` 会让「检查能出结果、发布说明却是空的」这种半失败很难解释。
fn fetch_changelog(tag: &str) -> Option<String> {
    let url = format!("https://raw.githubusercontent.com/{REPO_OWNER}/{REPO_NAME}/{tag}/CHANGELOG.md");
    let agent = match super::update_net::current().client() {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!("update notes: {e}");
            return None;
        }
    };
    match agent
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
    {
        Ok(resp) => resp.text().ok(),
        Err(e) => {
            tracing::debug!(
                "update notes: fetching CHANGELOG failed: {}",
                super::update_net::describe(&e)
            );
            None
        }
    }
}

/// 「自动更新」对话框里那块发布说明的内容：优先取 **CHANGELOG.md 里该版本的段落**
/// （中英对照、手写），取不到再退回 release 的 `body`。
pub(crate) fn notes_for(tag: &str, body: &str) -> String {
    let version = tag.trim().trim_start_matches('v');
    let notes = if let Some(md) = fetch_changelog(tag)
        && let Some(section) = changelog_section(&md, version)
    {
        section
    } else {
        release_notes(body)
    };
    // 给排查留一句话（debug 级，正式版默认 info 不打印）：这段说明来自哪儿、多长。
    tracing::debug!("update notes for {tag}: {} chars", notes.chars().count());
    notes
}
/// 发布说明的上限。对话框里那块是**可滚动**的，所以标准是"别把窗口撑坏"，而不是"少放点"：
/// 一个版本的 CHANGELOG 段落一般 2~3 千字符，都放得下；这里只拦异常离谱的输入。
/// 按**字符**（不是字节）算，中文说明不会被截成半个字。
pub(crate) const MAX_NOTES_CHARS: usize = 4000;

/// 把 release 的 `body` 清洗成能直接塞进普通 `Text` 的纯文本。
///
/// 为什么要洗：Slint 的 `Text` **不渲染 Markdown**，原样显示就是一堆 `##`、`**`、`- `
/// 符号。这里只做最保守的几步，不追求完整解析：
/// * HTML 注释（发布模板常留着 `<!-- ... -->`，可能跨行）整段丢掉；
/// * 行首 `#` / `>` 去掉，`- ` / `* ` 列表项换成 `• `；
/// * 行内 `**` / `__` / 反引号去掉；
/// * 连续空行压成一个，首尾空行去掉；
/// * 超长截断并加省略号。
pub(crate) fn release_notes(body: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_comment = false;
    let mut blanks = 0usize;

    for raw in body.lines() {
        let mut line = raw.trim().to_string();

        if in_comment {
            match line.find("-->") {
                Some(i) => {
                    line = line[i + 3..].trim().to_string();
                    in_comment = false;
                }
                None => continue,
            }
        }
        while let Some(start) = line.find("<!--") {
            match line[start + 4..].find("-->") {
                Some(rel) => {
                    let end = start + 4 + rel + 3;
                    line = format!("{}{}", &line[..start], &line[end..]);
                }
                None => {
                    line.truncate(start);
                    in_comment = true;
                    break;
                }
            }
        }

        let mut text = line.trim();
        text = text.trim_start_matches('#').trim();
        text = text.trim_start_matches('>').trim();
        let text = match text.strip_prefix("- ").or_else(|| text.strip_prefix("* ")) {
            Some(rest) => format!("• {}", rest.trim()),
            None => text.to_string(),
        };
        let text = text.replace("**", "").replace("__", "").replace('`', "");
        let text = text.trim_end().to_string();

        if text.is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue; // 连续空行只留一个
            }
        } else {
            blanks = 0;
        }
        out.push(text);
    }

    // 首尾空行都清掉：对话框里那块框不该以空白开头或结尾（注释被整段删掉时，
    // 开头很容易留下一串空行）。
    while out.first().is_some_and(|l| l.is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    let mut s = out.join("\n");
    if s.chars().count() > MAX_NOTES_CHARS {
        s = s.chars().take(MAX_NOTES_CHARS).collect::<String>();
        s.push('…');
    }
    s
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
