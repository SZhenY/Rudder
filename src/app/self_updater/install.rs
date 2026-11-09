use super::*;

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
        let client = crate::app::update_net::current()
            .client()
            .map_err(|e| anyhow!("{e}"))?;
        let resp = client
            .get(&cand.download_url)
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .map_err(|e| anyhow!("{}", crate::app::update_net::describe(&e)))?
            .error_for_status()
            .map_err(|e| anyhow!("{}", crate::app::update_net::describe(&e)))?;
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
pub(crate) fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
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
        repair_bundle_after_install(&app);
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
pub(crate) fn msi_args(msi: &Path, install_dir: Option<&Path>) -> String {
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
pub(crate) fn launch_msi_installer(msi: &Path) -> Result<()> {
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
pub(crate) fn launch_msi_installer(_msi: &Path) -> Result<()> {
    Err(anyhow!("MSI 更新只适用于 Windows"))
}

/// macOS：stash 旧 bundle → 移入新 bundle → 失败回滚。
pub(crate) fn replace_app_bundle(app: &Path, extract_dir: &Path) -> Result<()> {
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

/// 替换完 bundle 之后的兜底修复（macOS）。
///
/// 两步都是"上游环节出错时唯一还能救回来"的：
/// 1. 强制恢复 `Contents/MacOS/*` 的执行位；
/// 2. ad-hoc 重签 —— CI 产出的就是 ad-hoc 签名，替换后内容只要有一点偏差，
///    Gatekeeper 会直接报"已损坏"，比"未识别开发者"更难排查。
#[cfg(target_os = "macos")]
pub(crate) fn repair_bundle_after_install(app: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    if let Ok(entries) = std::fs::read_dir(app.join("Contents/MacOS")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
            }
        }
    }

    match std::process::Command::new("/usr/bin/codesign")
        .args(["--force", "--deep", "--sign", "-"])
        .arg(app)
        .output()
    {
        Ok(out) if out.status.success() => {}
        Ok(out) => tracing::warn!(
            "ad-hoc re-sign after update failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => tracing::warn!("codesign unavailable after update: {e}"),
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn repair_bundle_after_install(_app: &Path) {}

pub(crate) fn find_bundle(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
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

pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src).context("read source dir")? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        // 用 symlink_metadata：`is_dir()` 会跟随链接，把 `.app` 里 framework 的
        // `Versions/Current` 之类实体化成真实目录，bundle 结构就坏了。
        let meta = std::fs::symlink_metadata(&from).context("stat source entry")?;
        if meta.file_type().is_symlink() {
            #[cfg(unix)]
            {
                let target = std::fs::read_link(&from).context("read source symlink")?;
                let _ = std::fs::remove_file(&to);
                std::os::unix::fs::symlink(&target, &to).context("copy source symlink")?;
            }
            #[cfg(not(unix))]
            std::fs::copy(&from, &to).context("copy source entry")?;
            continue;
        }
        if meta.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).context("copy source file")?;
        }
    }
    Ok(())
}

/// Windows / Linux：用 self_replace 换掉当前可执行文件。
pub(crate) fn replace_binary(_exe: &Path, extract_dir: &Path) -> Result<()> {
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
pub(crate) fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    let name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if name.ends_with(".zip") {
        // macOS 的发布包是 CI 用 `ditto -c -k` 打的，这里用 `ditto -x -k` 解是闭环：
        // 权限位、符号链接、扩展属性全由系统保留。自己解压则必须逐项还原，漏一项就坏了
        // （0.8.0-beta3 / 0.8.1 就是把 `Contents/MacOS/rudder` 解成 0644，更新后打不开）。
        #[cfg(target_os = "macos")]
        if ditto_extract(archive, dest) {
            return Ok(());
        }
        extract_zip_manually(archive, dest)?;
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

/// macOS：用系统 `ditto` 解 zip。成功返回 true，失败或不可用时返回 false 让调用方回退。
#[cfg(target_os = "macos")]
pub(crate) fn ditto_extract(archive: &Path, dest: &Path) -> bool {
    match std::process::Command::new("/usr/bin/ditto")
        .arg("-x")
        .arg("-k")
        .arg(archive)
        .arg(dest)
        .status()
    {
        Ok(status) if status.success() => true,
        Ok(status) => {
            tracing::warn!("ditto extract exited with {status}; falling back to built-in unzip");
            false
        }
        Err(e) => {
            tracing::warn!("ditto extract unavailable ({e}); falling back to built-in unzip");
            false
        }
    }
}

/// 内置 zip 解压（非 macOS，或 ditto 失败时的回退）。
///
/// 有两样东西必须还原，否则 bundle 会被破坏：
/// 1. Unix 权限位 —— `File::create` 一律建 0644，可执行文件就此丢掉执行位；
/// 2. 符号链接 —— zip 里链接的"内容"是目标路径字符串，当普通文件写出来会让
///    macOS framework 的 `Versions/Current` 这类链接实体化。
pub(crate) fn extract_zip_manually(archive: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive).context("open update zip")?;
    let mut zip = zip::ZipArchive::new(file).context("read update zip")?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).context("read zip entry")?;
        // `enclosed_name` 拒绝 `../` 之类越界路径：压缩包来自网络，必须防穿越。
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        let out = dest.join(rel);
        let mode = entry.unix_mode();

        if entry.is_dir() {
            std::fs::create_dir_all(&out).context("create dir from zip")?;
            apply_zip_mode(&out, mode);
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).context("create parent from zip")?;
        }

        #[cfg(unix)]
        if mode.is_some_and(|m| m & 0o170000 == 0o120000) {
            let mut target = String::new();
            std::io::Read::read_to_string(&mut entry, &mut target)
                .context("read zip symlink target")?;
            let _ = std::fs::remove_file(&out);
            std::os::unix::fs::symlink(&target, &out).context("create symlink from zip")?;
            continue;
        }

        let mut f = std::fs::File::create(&out).context("create file from zip")?;
        std::io::copy(&mut entry, &mut f).context("write file from zip")?;
        drop(f);
        apply_zip_mode(&out, mode);
    }
    Ok(())
}

/// 把 zip 条目记录的 Unix 权限位落到磁盘上。
#[cfg(unix)]
pub(crate) fn apply_zip_mode(path: &Path, mode: Option<u32>) {
    use std::os::unix::fs::PermissionsExt as _;
    if let Some(mode) = mode {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777));
    }
}

#[cfg(not(unix))]
pub(crate) fn apply_zip_mode(_path: &Path, _mode: Option<u32>) {}

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
