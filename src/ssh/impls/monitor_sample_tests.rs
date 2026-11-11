//! `collect_monitor_sample` 的分节、去重与上限。`parse_monitor_block` 的
//! 速率/CPU 行为仍由 `ssh_tests.rs::monitor_hardening_tests` 覆盖。

use super::*;

fn top_block() -> String {
    concat!(
        "cpu 10 0 5 80 5\n",
        "MemTotal: 1000 kB\n",
        "MemAvailable: 400 kB\n",
        "eth0: 100 0 0 0 0 0 0 0 200 0 0 0 0 0 0 0\n",
    )
    .to_string()
}

#[test]
fn echo_markers_route_lines_into_their_sections() {
    let mut block = top_block();
    block.push_str("__DF__\n");
    block.push_str("/dev/sda1 1000 100 900 10% /\n");
    block.push_str("/dev/sda2 1000 100 900 10% /mnt/same\n");
    block.push_str("__SYS__\nOS=Debian\nKERNEL=Linux\nno equals here\n");
    let s = collect_monitor_sample(&block);
    assert!(s.have_cpu);
    assert_eq!(s.cpu_nums.len(), 5);
    assert_eq!(s.cpu_total, 100);
    assert_eq!(s.cpu_idle, 85);
    assert_eq!(s.mem_total, 1000);
    assert_eq!(s.mem_avail, 400);
    assert_eq!(s.net_now.len(), 1);
    assert_eq!(s.net_now[0].0, "eth0");
    assert_eq!(s.disks.len(), 1, "identical (total, avail) should collapse");
    assert_eq!(s.disks[0].0, "/");
    assert_eq!(s.sys_kv.get("OS").map(String::as_str), Some("Debian"));
    assert_eq!(s.sys_kv.len(), 2);
}

#[test]
fn hostile_entry_counts_are_capped_per_section() {
    let mut block = String::from("MemTotal: 1000 kB\nMemAvailable: 500 kB\n");
    for i in 0..200 {
        block.push_str(&format!("eth{i}: 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16\n"));
    }
    block.push_str("__DF__\n");
    for i in 0..200 {
        let n = 1000 + i;
        block.push_str(&format!("/dev/sd{i} {n} 0 {n} 10% /mnt/{i}\n"));
    }
    let s = collect_monitor_sample(&block);
    assert_eq!(s.net_now.len(), MAX_MON_ENTRIES);
    assert_eq!(s.disks.len(), MAX_MON_ENTRIES);
}

#[test]
fn marker_only_sections_produce_no_rows() {
    let s = collect_monitor_sample("__DF__\n__SYS__\n");
    assert!(s.disks.is_empty());
    assert!(s.sys_kv.is_empty());
    assert!(!s.have_cpu);
    assert_eq!(s.mem_total, 0);
}
