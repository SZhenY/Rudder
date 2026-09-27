//! CSI 骨架扫描 —— 字体化输出里唯一需要"在 SGR 处停顿"的那部分。
//!
//! `sgr_probe`（判断这一块要不要走慢路径）与 `scan_csi_sequences`（收集 SGR 区间）
//! 原先各自手写一份**逐字相同**的循环：memchr 跳到 ESC → 确认 `[` → 扫到 final byte
//! （0x40..=0x7e）→ 块尾半截留到下一块。两份并存时规则漂移过一次（`sgr_probe` 认
//! 块尾孤立 ESC、`scan_csi_sequences` 不认，于是 overline 区间永远闭合不了）——
//! 收在一处的意义就是让这类漂移在结构上不可能再发生。
//!
//! 接口刻意做成 **visitor + 零分配**：`sgr_probe` 在彩色刷屏的热路径上每块都会跑，
//! 返回 `Vec` 会平白多一次分配。

/// 逐个消费块内**完整**的 CSI 序列（`ESC [ … final`），返回块尾半截 CSI 的起点。
///
/// * `visit(start, final_idx)`：`start` 是 ESC 的下标，`final_idx` 是 final byte 的下标
///   （两者之间是参数段 `bytes[start + 2..final_idx]`）。
/// * 返回值 `Some(t)`：块尾从 `t` 起是不完整的 CSI —— 调用方必须缓存 `bytes[t..]`
///   并把它拼到下一块前面。否则 1 字节分块时把 `ESC` / `[` / `5` / `3` / `m` 当成
///   互不相干的块，那个 `53` 就漏了（分块等价测试抓的就是这个）。
/// * 块里没有 ESC 时立刻返回 `None`（一次 memchr）。
pub(crate) fn scan_csi_visit(bytes: &[u8], mut visit: impl FnMut(usize, usize)) -> Option<usize> {
    memchr::memchr(b'\x1b', bytes)?;
    let mut i = 0;
    // ⚠️ 循环条件**必须**带 `i < bytes.len()`：下面 `i += 2`（ESC + 非 '['，例如 OSC 引子
    // `ESC ]`）会一步跨过块尾，此时 `&bytes[i..]` 会 panic（0.7.9-beta2 在彩色刷屏下
    // 崩过一次，就是这里丢了上界）。
    while i < bytes.len() {
        let Some(offset) = memchr::memchr(b'\x1b', &bytes[i..]) else {
            break;
        };
        i += offset;
        if bytes.get(i + 1) != Some(&b'[') {
            // ESC 后面**没有下一个字节** = 序列刚开始 ⇒ 块尾留到下一块再判。
            if i + 1 >= bytes.len() {
                return Some(i);
            }
            // ESC + 非 '['：两字节转义（ESC 7 / ESC c …）或 OSC 引子（ESC ] …）。
            // OSC 的内容不在这里解析（各自的 OSC 抽取器负责），跳过引子继续找 ESC。
            i += 2;
            continue;
        }
        let mut j = i + 2;
        while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
            j += 1;
        }
        if j >= bytes.len() {
            return Some(i); // 块尾未完成的 CSI
        }
        visit(i, j);
        i = j + 1; // 跳过这个已完成的序列（无论是否为 SGR）
    }
    None
}

/// 这个参数是不是扩展色前缀（`38` 前景 / `48` 背景 / `58` 下划线色）。
pub(crate) fn is_extended_color_prefix(part: &[u8]) -> bool {
    matches!(part, b"38" | b"48" | b"58")
}

/// 扩展色前缀之后要跳过几个参数：`5` → 2（前缀 + 颜色下标），`2` → 4（前缀 + R/G/B）。
///
/// `apply_sgr` 的改写与 `sgr_probe` 的分类**必须用同一条规则**，否则 overline 区间
/// 会错位（`38;2;53;100;200m` 里的 53 是颜色分量、不是 overline）—— 原先这规则各写了
/// 一遍，现在收在这里。注意第二个前瞻参数两边都没用到，所以只收 `next1`。
pub(crate) fn extended_color_skip(next1: Option<&[u8]>) -> usize {
    match next1 {
        Some(b"5") => 2,
        Some(b"2") => 4,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visited(bytes: &[u8]) -> (Vec<(usize, usize)>, Option<usize>) {
        let mut seen = Vec::new();
        let tail = scan_csi_visit(bytes, |s, f| seen.push((s, f)));
        (seen, tail)
    }

    #[test]
    fn visits_complete_csi_and_reports_a_split_tail() {
        // "ESC[31m" 是 (0,4)；块尾的 "ESC[" 从下标 8 起是半截 → tail = Some(8)
        assert_eq!(
            visited(b"\x1b[31mred\x1b["),
            (vec![(0, 4)], Some(8)),
            "完整 CSI 要访问、块尾半截要报出来"
        );
        // 只有 ESC：序列刚开头，同样算块尾半截（否则 1 字节分块会漏掉 53）
        assert_eq!(visited(b"\x1b"), (vec![], Some(0)));
        // 没有 ESC：不访问、也没有尾部
        assert_eq!(visited(b"plain text"), (vec![], None));
    }

    #[test]
    fn skips_non_csi_escapes_and_keeps_scanning() {
        // OSC 引子（ESC ]）被跳过，后面的真 SGR 仍要访问
        let (seen, tail) = visited(b"\x1b]52;c;aGk=\x07\x1b[1m");
        assert_eq!(seen.len(), 1, "OSC 之后的 SGR 必须被访问");
        assert_eq!(tail, None);

        // ESC + 非 '[' 正好落在块尾：`i += 2` 跨过块尾也不能 panic（0.7.9-beta2 的崩点）
        assert_eq!(visited(b"\x1b\x1b"), (vec![], None));
        assert_eq!(visited(b"x\x1b]"), (vec![], None));
    }

    #[test]
    fn final_byte_is_any_byte_in_0x40_to_0x7e() {
        // 非 'm' 的 CSI 同样会被访问（过滤交给调用方：probe 只看 'm'）
        assert_eq!(
            visited(b"\x1b[2J\x1b[6n"),
            (vec![(0, 3), (4, 7)], None)
        );
    }
}
