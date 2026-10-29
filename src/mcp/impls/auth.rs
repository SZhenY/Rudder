//! 客户端令牌：生成、常量时间比对。
//!
//! 令牌等价于"能操作这台机器的登录凭据"，所以比较必须走常量时间（`ring` 的
//! `verify_slices_are_equal`），并且只以 hash 形式比较，避免把明文放在比较函数里。

use std::fmt::Write;

use ring::digest::{SHA256, digest};

/// 出厂长度 32 字节，前缀 `rk_` 便于在日志 / 配置里一眼认出这是 Rudder 令牌。
pub(crate) fn new_token() -> String {
    let mut bytes = [0u8; 32];
    if getrandom::fill(&mut bytes).is_err() {
        // 系统熵源不可用时不降级成弱随机：宁可让服务起不来。
        return String::new();
    }
    let mut out = String::with_capacity(3 + bytes.len() * 2);
    out.push_str("rk_");
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 常量时间比对。空令牌恒为不匹配 —— 未配置令牌时必须拒绝所有请求。
///
/// 先各自求 SHA-256 再逐字节比较：两边长度固定，比较耗时不随令牌长度暴露，
/// 也不依赖 `ring` 里已标记废弃的 `verify_slices_are_equal`。
pub(super) fn token_matches(expected: &str, provided: &str) -> bool {
    if expected.is_empty() || provided.is_empty() {
        return false;
    }
    let expected = digest(&SHA256, expected.as_bytes());
    let provided = digest(&SHA256, provided.as_bytes());
    let mut diff = 0u8;
    for (left, right) in expected.as_ref().iter().zip(provided.as_ref()) {
        diff |= left ^ right;
    }
    diff == 0
}
