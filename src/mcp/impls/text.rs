//! 截断工具输出的两种口径，单位写在函数名与参数名里。
//!
//! 同一批工具同时存在"按字符"和"按字节"的上限：终端画面按列宽排版，按字节切会把
//! 一个汉字劈成两半；而回给客户端的负载按字节算内存。两者混用时，一个中文串被算成
//! 1 还是 3 取决于调用点 —— 所以这里不提供"通用截断"，只提供两个各自标明单位的函数。

/// 按**字符数**截断，返回 (文本, 是否发生截断)。
///
/// 用于终端画面：`max_chars` 是"最多保留多少个字符"，调用方拿到的是列数语义的上限。
pub(super) fn truncate_chars(text: String, max_chars: usize) -> (String, bool) {
    // 先数字符再比：`text.len()` 是字节数，拿它跟字符上限比会把"没超"的文本
    // 也标成 truncated（一个汉字占 3 字节）。
    if text.chars().count() <= max_chars {
        return (text, false);
    }
    (text.chars().take(max_chars).collect(), true)
}

/// 取末尾不超过 `max_bytes` 字节的一段，起点对齐到字符边界。
///
/// 用于"回给客户端多少字节"的上限：从尾部取，命令最后几行才是结果。
pub(super) fn tail_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_counts_chars_not_bytes() {
        // 10 个汉字 = 30 字节：按字符上限 10 不该截断，也不该报 truncated。
        let text = "一二三四五六七八九十".to_string();
        let (kept, cut) = truncate_chars(text.clone(), 10);
        assert_eq!(kept, text);
        assert!(!cut, "字节数超过上限但字符数没超，不应标记为截断");

        let (kept, cut) = truncate_chars(text, 3);
        assert_eq!(kept, "一二三");
        assert!(cut);
    }

    #[test]
    fn tail_starts_at_a_char_boundary() {
        // "a一b" 共 5 字节，"一" 占 1..4；取 3 字节的起点落在它中间，
        // 必须往后挪到 4（下一个字符边界），结果只剩 "b"。
        assert_eq!(tail_bytes("a一b", 3), "b");
        assert_eq!(tail_bytes("short", 64), "short");
    }
}
