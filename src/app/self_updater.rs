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
    /// 发布说明（release 的 `body`，已由 [`release_notes`] 清洗 + 截断）。
    pub notes: String,
}

/// 当前平台在 release 资产名里的关键字（按优先级）。
///
/// 对应 `.github/workflows/release.yml` 的 `matrix.name`。
fn asset_keywords() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["macos-14", "macos"]
    } else if cfg!(target_os = "windows") {
        // Windows on ARM64 ships its own asset; the x86_64 keywords must not
        // match there or an ARM64 user would be handed the emulated build.
        if cfg!(target_arch = "aarch64") {
            &["windows-aarch64", "windows-arm64"]
        } else {
            &["windows-x86_64", "windows"]
        }
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
    let body = ureq::get(&url)
        .set("User-Agent", "rudder-self-update")
        .timeout(std::time::Duration::from_secs(15))
        .call()
        .context("query GitHub releases")?
        .into_string()
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

    let notes = notes_for(&tag, json["body"].as_str().unwrap_or_default());
    for kw in asset_keywords() {
        if let Some((name, url)) = candidates.iter().find(|(n, _)| n.contains(kw)) {
            return Ok(Some(UpdateCandidate {
                version: tag.trim_start_matches('v').to_string(),
                asset_name: name.clone(),
                download_url: url.clone(),
                notes: notes.clone(),
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
    // 随机名 + 0700 + 创建即独占。老写法是 `/tmp/rudder-update-<pid>`：名字可预测，
    // 而且先 `remove_dir_all` 再 `create_dir_all`（会跟随符号链接）—— 共享机器上别人
    // 可以先摆一个同名符号链接，把下载与解压都引到他指定的目录去。
    let stage_dir = crate::config::create_private_temp_dir("rudder-update")?;

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
    // 解压完就把压缩包删掉：不再需要它，也少留一份可执行文件在临时目录里。
    let _ = std::fs::remove_file(&archive);
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
            "rudder-0.7.3-windows-aarch64.zip",
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
fn fetch_changelog(tag: &str) -> Option<String> {
    let url = format!("https://raw.githubusercontent.com/{REPO_OWNER}/{REPO_NAME}/{tag}/CHANGELOG.md");
    ureq::get(&url)
        .set("User-Agent", "rudder-self-update")
        .timeout(std::time::Duration::from_secs(15))
        .call()
        .ok()?
        .into_string()
        .ok()
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
