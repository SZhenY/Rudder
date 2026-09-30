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
    // **内容相同** = 重复上传同一张图 / 同一款字体：直接复用已落盘的那份，
    // 不再生成 `<名字> 2` 副本 —— 否则壁纸/字体下拉里同一项重复显示
    // （用户"只上传了三张壁纸"却出现七个条目就是这么来的）。
    let src_len = std::fs::metadata(src).ok()?.len();
    if let (true, Ok(entries)) = (src_len > 0, std::fs::read_dir(dest_dir)) {
        for entry in entries.flatten() {
            let candidate = entry.path();
            if !candidate.is_file() {
                continue;
            }
            if std::fs::metadata(&candidate).map(|m| m.len()).unwrap_or(0) != src_len {
                continue;
            }
            if same_content(src, &candidate) {
                return Some(candidate);
            }
        }
    }
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

/// 两个文件内容是否逐字节一致（分块读，不整读进内存）。
fn same_content(a: &Path, b: &Path) -> bool {
    use std::io::Read;
    let (mut fa, mut fb) = match (std::fs::File::open(a), std::fs::File::open(b)) {
        (Ok(x), Ok(y)) => (x, y),
        _ => return false,
    };
    let mut buf_a = [0u8; 8192];
    let mut buf_b = [0u8; 8192];
    loop {
        let na = fa.read(&mut buf_a).unwrap_or(0);
        let nb = fb.read(&mut buf_b).unwrap_or(0);
        if na != nb {
            return false;
        }
        if na == 0 {
            return true;
        }
        if buf_a[..na] != buf_b[..nb] {
            return false;
        }
    }
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
        // 同一内容再导入：**复用**已落盘的那份（不再生成编号副本 —— 副本会让
        // 壁纸/字体下拉重复显示同一项）。
        let second = import_file(&src, &dst_dir).expect("第二次导入");
        assert_eq!(second, first);
        // 内容**不同**但同名：不覆盖，走 `<名字> 2` 后缀。
        std::fs::write(&src, b"two").unwrap();
        let third = import_file(&src, &dst_dir).expect("第三次导入");
        assert_eq!(third.file_name().unwrap(), "logo 2.png");
        assert_eq!(std::fs::read(&third).unwrap(), b"two");
        assert_eq!(std::fs::read(&first).unwrap(), b"one");

        // 原文件仍在原处 ✓（first 仍是 one、third 是 two 已在上面断言）
        assert!(src.exists());

        // 没有扩展名的文件也能导入（不会多出一个点）
        let bare = src_dir.join("LICENSE");
        std::fs::write(&bare, b"t").unwrap();
        let imported = import_file(&bare, &dst_dir).expect("无扩展名导入");
        assert_eq!(imported.file_name().unwrap(), "LICENSE");
        let _ = std::fs::remove_dir_all(&base);
    }
}
