//! 资源文本格式化：内存 used/total 与网络速率，远端 overview 与面板共用。



/// Format a used/total memory pair (both in MiB) for the narrow sidebar.
/// Below 1 GiB it stays in megabytes (`512/2048M`); at or above, it switches to
/// gigabytes and drops the decimal for whole or large values to stay compact
/// (`1.5G/16G`, `120G/256G`).
pub fn format_mem(used_mib: u64, total_mib: u64) -> String {
    if total_mib < 1024 {
        return format!("{used_mib}/{total_mib}M");
    }
    // MiB → GiB, with a tidy width: integer when round or ≥100, else one decimal.
    fn gib(mib: u64) -> String {
        let g = mib as f64 / 1024.0;
        if g.fract() == 0.0 || g >= 100.0 {
            (g as u64).to_string()
        } else {
            format!("{g:.1}")
        }
    }
    format!("{}G/{}G", gib(used_mib), gib(total_mib))
}

/// Human-readable network throughput (e.g. `"1.2 MB/s"`).
pub fn format_bytes_per_sec(bytes: u64) -> String {
    crate::ssh::format_bytes_units(bytes, &["B/s", "KB/s", "MB/s", "GB/s"])
}
