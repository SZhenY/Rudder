//! 用户文件的扫描与导入 —— 字体与壁纸共用。
//!
//! 这两个动作原先在 `fonts.rs` 与 `wallpaper/impls/wallpaper.rs` 里各写了一遍：
//! 逐字相同，只差扩展名表与目标目录。收在一处，免得两边各自演化出差异
//! （比如"重名加 2/3"的规则只改了一边）。

use std::path::{Path, PathBuf};

/// 扫描 `dir` 下扩展名命中的文件：大小写不敏感，按路径排序（顺序稳定，
/// 选择器里的排列不会随文件系统抖动）；目录不存在或读不到就给空表。
pub(crate) fn scan_files(dir: &Path, exts: &[&str]) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| exts.iter().any(|ext| x.eq_ignore_ascii_case(ext)))
        })
        .collect();
    files.sort();
    files
}

/// 把用户挑的文件**复制**进 `dest_dir`，返回落点；重名**不覆盖**，改成
/// `<名字> 2.ext`、`<名字> 3.ext`……
///
/// 复制而不是记住原路径：原文件挪走 / 删掉之后字体与壁纸不会失效，重装也还在。
pub(crate) fn import_file(src: &Path, dest_dir: &Path) -> Option<PathBuf> {
    std::fs::create_dir_all(dest_dir).ok()?;
    let file_name = src.file_name()?.to_string_lossy().into_owned();
    let (stem, ext) = match file_name.rsplit_once('.') {
        Some((stem, ext)) => (stem.to_string(), format!(".{ext}")),
        None => (file_name.clone(), String::new()),
    };
    let mut dst = dest_dir.join(&file_name);
    let mut n = 2;
    while dst.exists() {
        dst = dest_dir.join(format!("{stem} {n}{ext}"));
        n += 1;
    }
    std::fs::copy(src, &dst).ok()?;
    Some(dst)
}


// ── Size-limited reads ──────────────────────────────────────────────────────

/// Session / settings JSON must stay small enough that a corrupt or misplaced
/// file cannot pull an arbitrary amount of data into memory at startup.
pub(crate) const MAX_CONFIG_BYTES: u64 = 64 * 1024 * 1024;
/// User-selected import JSON gets the same budget.
pub(crate) const MAX_IMPORT_BYTES: u64 = 64 * 1024 * 1024;
/// Private keys are small text/binary blobs; 8 MiB covers every real key type
/// (including PPK v3 comments and large-integer serialisation).
pub(crate) const MAX_KEY_BYTES: u64 = 8 * 1024 * 1024;

/// Format a byte count with the given 1024-based unit suffixes. The first
/// unit is rendered as an integer; >=TB sizes (when more than 4 units exist)
/// get two decimals, everything else one.
pub(crate) fn format_bytes_units(bytes: u64, units: &[&str]) -> String {
    let mut value = bytes as f64;
    let mut idx = 0;
    while value >= 1024.0 && idx < units.len() - 1 {
        value /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{} {}", bytes, units[0])
    } else if units.len() > 4 && idx >= 4 {
        format!("{:.2} {}", value, units[idx])
    } else {
        format!("{:.1} {}", value, units[idx])
    }
}

fn human_bytes(bytes: u64) -> String {
    format_bytes_units(bytes, &["B", "KB", "MB", "GB", "TB"])
}

/// Reject a file whose size exceeds `limit`, logging a message that names the
/// file, its real size and the limit. `== limit` is allowed.
pub(crate) fn check_file_size(path: &Path, limit: u64, what: &str) -> std::io::Result<()> {
    let actual = std::fs::metadata(path)?.len();
    if actual > limit {
        let msg = format!(
            "refusing to read {what} {}: size {} ({} bytes) exceeds the {} ({} bytes) limit",
            path.display(),
            human_bytes(actual),
            actual,
            human_bytes(limit),
            limit
        );
        tracing::warn!("{msg}");
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, msg));
    }
    Ok(())
}

/// Read a UTF-8 text file after checking its size.
pub(crate) fn read_string_capped(
    path: &Path,
    limit: u64,
    what: &str,
) -> std::io::Result<String> {
    check_file_size(path, limit, what)?;
    std::fs::read_to_string(path)
}

/// Read a binary file after checking its size.
pub(crate) fn read_file_capped(path: &Path, limit: u64, what: &str) -> std::io::Result<Vec<u8>> {
    check_file_size(path, limit, what)?;
    std::fs::read(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rudder-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn scan_filters_by_extension_ignoring_case_and_sorts() {
        let dir = scratch("scan");
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["b.PNG", "a.png", "skip.txt"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }

        let names: Vec<String> = scan_files(&dir, &["png"])
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.png", "b.PNG"]);
        // 目录读不到 → 空表（而不是 panic）
        assert!(scan_files(&dir.join("missing"), &["png"]).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_never_overwrites_and_keeps_the_extension() {
        let base = scratch("import");
        let src_dir = base.join("src");
        let dst_dir = base.join("dst");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join("logo.png");
        std::fs::write(&src, b"one").unwrap();

        let first = import_file(&src, &dst_dir).expect("第一次导入");
        assert_eq!(first.file_name().unwrap(), "logo.png");
        let second = import_file(&src, &dst_dir).expect("第二次导入");
        assert_eq!(second.file_name().unwrap(), "logo 2.png");
        let third = import_file(&src, &dst_dir).expect("第三次导入");
        assert_eq!(third.file_name().unwrap(), "logo 3.png");

        // 原文件仍在原处 ✓，副本内容与源一致 ✓
        assert!(src.exists());
        assert_eq!(std::fs::read(&third).unwrap(), b"one");

        // 没有扩展名的文件也能导入（不会多出一个点）
        let bare = src_dir.join("LICENSE");
        std::fs::write(&bare, b"t").unwrap();
        let imported = import_file(&bare, &dst_dir).expect("无扩展名导入");
        assert_eq!(imported.file_name().unwrap(), "LICENSE");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn size_limit_allows_exact_limit() {
        let base = std::env::temp_dir().join(format!("rudder-cap-exact-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        let path = base.join("exact.json");
        std::fs::write(&path, "x".repeat(16)).unwrap();
        assert!(check_file_size(&path, 16, "test file").is_ok());
        assert_eq!(read_string_capped(&path, 16, "test file").unwrap().len(), 16);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn size_limit_rejects_and_names_real_size_and_limit() {
        let base = std::env::temp_dir().join(format!("rudder-cap-over-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        let path = base.join("over.json");
        std::fs::write(&path, "x".repeat(17)).unwrap();
        let err = read_string_capped(&path, 16, "test file").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("over.json"), "missing path: {msg}");
        assert!(msg.contains("17 bytes"), "missing real size: {msg}");
        assert!(msg.contains("16 bytes"), "missing limit: {msg}");
        assert!(msg.contains("test file"), "missing what: {msg}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn size_limit_reports_missing_file_as_io_error() {
        let path = std::env::temp_dir().join("rudder-cap-does-not-exist.json");
        let _ = std::fs::remove_file(&path);
        assert!(read_string_capped(&path, 16, "test file").is_err());
    }
}
