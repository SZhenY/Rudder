//! 在线一键更新（GitHub Releases + `self_update`）。
//!
//! 流程：列 release → 按平台挑资产 → 下载（进度）→ 解压到临时目录 →
//! 平台替换 → 提示用户重启。
//!
//! 与 `updater.rs` 的分工：后者只负责"有新版本"的检查与横幅；本模块负责
//! 真正的下载与替换。
//!
//! 为什么不用 `self_update` 的自动资产匹配：Rudder 的 release 资产按
//! `matrix.name` 命名（`rudder-<ver>-macos-14.zip`），不含 target triple，
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
}

/// 当前平台在 release 资产名里的关键字（按优先级）。
///
/// 对应 `.github/workflows/release.yml` 的 `matrix.name`。
fn asset_keywords() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["macos-14", "macos"]
    } else if cfg!(target_os = "windows") {
        &["windows-x86_64", "windows"]
    } else if cfg!(target_arch = "aarch64") {
        &["linux-aarch64", "linux-arm64"]
    } else {
        &["linux-x86_64"]
    }
}

/// 当前平台接受的资产扩展名（排除安装器与 AppImage——自更新只换二进制）。
fn asset_extension() -> &'static str {
    if cfg!(target_os = "macos") || cfg!(target_os = "windows") {
        ".zip"
    } else {
        ".tar.gz"
    }
}

/// 当前进程的可执行文件路径。
pub(crate) fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("resolve current executable")
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
    cfg!(target_os = "macos")
        && exe
            .to_string_lossy()
            .contains("/AppTranslocation/")
}

/// 查询 GitHub Releases，返回比 `current` 新的最新版本（无则 None）。
///
/// 静默策略沿用既有检查逻辑：网络/解析失败返回 Err，但调用方只记录日志。
pub(crate) fn latest_update(current: (u32, u32, u32)) -> Result<Option<UpdateCandidate>> {
    let body = ureq::get(&format!(
        "https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest"
    ))
    .set("User-Agent", "rudder-self-update")
    .timeout(std::time::Duration::from_secs(15))
    .call()
    .context("query GitHub releases")?
    .into_string()
    .context("read releases response")?;

    let json: serde_json::Value =
        serde_json::from_str(&body).context("parse releases response")?;
    let tag = json["tag_name"].as_str().unwrap_or_default().to_string();
    let Some(latest) = super::parse_version(&tag) else {
        return Ok(None);
    };
    if latest <= current {
        return Ok(None);
    }

    // 挑资产：先按扩展名过滤，再按平台关键字（按优先级）匹配。
    let ext = asset_extension();
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

    for kw in asset_keywords() {
        if let Some((name, url)) = candidates.iter().find(|(n, _)| n.contains(kw)) {
            return Ok(Some(UpdateCandidate {
                version: tag.trim_start_matches('v').to_string(),
                asset_name: name.clone(),
                download_url: url.clone(),
            }));
        }
    }
    Ok(None)
}

/// 下载资产到临时目录并解压，返回解压目录。
///
/// `on_progress` 收到 0.0..=1.0 的进度（下载占 90%，解压占 10%）。
pub(crate) fn download_and_stage(
    cand: &UpdateCandidate,
    on_progress: impl Fn(f32),
) -> Result<PathBuf> {
    let stage_dir = std::env::temp_dir().join(format!("rudder-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage_dir);
    std::fs::create_dir_all(&stage_dir).context("create staging dir")?;

    let archive = stage_dir.join(&cand.asset_name);
    {
        let resp = ureq::get(&cand.download_url)
            .set("User-Agent", "rudder-self-update")
            .timeout(std::time::Duration::from_secs(60))
            .call()
            .context("download release asset")?;
        let total: u64 = resp
            .header("Content-Length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut reader = resp.into_reader();
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
                on_progress((done as f32 / total as f32) * 0.9);
            }
        }
    }
    on_progress(0.9);

    let extract_dir = stage_dir.join("extracted");
    std::fs::create_dir_all(&extract_dir).context("create extract dir")?;
    self_update::Extract::from_source(&archive)
        .extract_into(&extract_dir)
        .context("extract release archive")?;
    on_progress(1.0);
    Ok(extract_dir)
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

/// 把解压好的新版本安装到当前位置。
///
/// - macOS：整个 `.app` 包替换（保留 CI 的 ad-hoc 签名；只换二进制会让
///   Gatekeeper 判定"已损坏"）。
/// - Windows / Linux：用 `self_replace` 原地替换可执行文件（自动处理
///   Windows 的文件锁）。
///
/// 任一路径都先备份旧版，失败回滚。
pub(crate) fn install_staged(extract_dir: &Path) -> Result<()> {
    let exe = current_exe()?;
    if is_translocated(&exe) {
        return Err(anyhow!(
            "app is running from a read-only translocated location; move Rudder to /Applications first"
        ));
    }

    if cfg!(target_os = "macos")
        && let Some(app) = macos_app_bundle(&exe)
    {
        return replace_app_bundle(&app, extract_dir);
    }
    replace_binary(&exe, extract_dir)
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

/// 用新版本重启应用（Unix 用 exec 保留 PID，Windows 起新进程并退出当前进程）。
pub(crate) fn restart_app() -> Result<()> {
    // Returns `Infallible` on success: on Unix the process image is replaced by
    // exec, on Windows a new process is spawned and this one exits.
    let never = self_update::restart::restart().map_err(|e| anyhow!("restart: {e}"))?;
    Err(anyhow!("restart returned unexpectedly: {never:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_cover_current_platform() {
        // 资产关键字必须能命中 release.yml 里各平台的资产名。
        let kw = asset_keywords();
        let names = [
            "rudder-0.7.2-macos-14.zip",
            "rudder-0.7.2-windows-x86_64.zip",
            "rudder-0.7.2-linux-x86_64.tar.gz",
            "rudder-0.7.2-linux-aarch64-glibc228.tar.gz",
        ];
        let ext = asset_extension();
        let mine = names
            .iter()
            .filter(|n| n.ends_with(ext))
            .find(|n| kw.iter().any(|k| n.contains(k)));
        assert!(mine.is_some(), "no asset matches {kw:?} with ext {ext}");
    }

    #[test]
    fn translocated_paths_are_detected() {
        assert!(is_translocated(Path::new(
            "/private/var/folders/x/AppTranslocation/ABC/d/rudder.app/Contents/MacOS/rudder"
        )));
        assert!(!is_translocated(Path::new(
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
