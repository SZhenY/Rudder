//! 资源文本格式化：网络速率（远端 overview 与面板共用）。

/// Human-readable network throughput (e.g. `"1.2 MB/s"`).
pub fn format_bytes_per_sec(bytes: u64) -> String {
    crate::ssh::format_bytes_units(bytes, &["B/s", "KB/s", "MB/s", "GB/s"])
}
