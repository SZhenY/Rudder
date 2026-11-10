/// Format a Unix timestamp as `YYYY-MM-DD HH:MM`.
pub fn format_mtime(ts: u32) -> String {
    // SFTP mtime is a Unix timestamp (UTC seconds). Render it in the machine's
    // *local* timezone so the displayed time matches the user's wall clock
    // (e.g. UTC+8) instead of showing UTC — which read 8 h early (#168).
    use chrono::{Local, TimeZone};
    let dt = Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .unwrap_or_else(Local::now);
    dt.format("%Y-%m-%d %H:%M").to_string()
}

/// 文件类型位的可读标签（`S_IFMT` → 单词），供 SFTP 列表的「类型」列使用
/// （上游 `ed7f231`）。
pub fn file_type_label(mode: u32) -> &'static str {
    match mode & 0o170_000 {
        0o040_000 => "dir",
        0o120_000 => "link",
        0o140_000 => "socket",
        0o010_000 => "fifo",
        0o020_000 => "char",
        0o060_000 => "block",
        0o100_000 => "file",
        _ => "?",
    }
}

/// 权限位 → `ls -l` 风格的 9 字符串（`rwxr-xr-x`），第 3/6/9 位按 setuid/setgid/sticky
/// 显示成 `s`/`S`/`t`/`T`（上游 `ed7f231` 的「权限」列）。
///
/// 不含类型首字母 —— 那是「类型」列的事（[`file_type_label`]）。
pub fn format_permissions(mode: u32) -> String {
    let mut out = String::with_capacity(9);
    // (读, 写, 执行, 特殊位) —— 三组依次是 user / group / other。
    let groups = [
        (0o400, 0o200, 0o100, 0o4000),
        (0o040, 0o020, 0o010, 0o2000),
        (0o004, 0o002, 0o001, 0o1000),
    ];
    for (i, (read, write, exec, special)) in groups.into_iter().enumerate() {
        out.push(if mode & read != 0 { 'r' } else { '-' });
        out.push(if mode & write != 0 { 'w' } else { '-' });
        let has_exec = mode & exec != 0;
        let has_special = mode & special != 0;
        out.push(match (has_special, has_exec) {
            (false, true) => 'x',
            (false, false) => '-',
            (true, true) => {
                if i == 2 {
                    't'
                } else {
                    's'
                }
            }
            (true, false) => {
                if i == 2 {
                    'T'
                } else {
                    'S'
                }
            }
        });
    }
    out
}
