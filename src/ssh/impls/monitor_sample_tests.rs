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
    // user+nice / system(+irq/softirq)：top 的 us / sy 两列。
    assert_eq!(s.cpu_user, 10);
    assert_eq!(s.cpu_sys, 5);
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
fn gateway_section_is_parsed() {
    let mut block = top_block();
    block.push_str("__GW__\n");
    block.push_str("GW=192.168.100.1\n");
    let s = collect_monitor_sample(&block);
    assert_eq!(s.gw, "192.168.100.1");
}

#[test]
fn missing_gateway_leaves_empty() {
    let s = collect_monitor_sample("__GW__\n");
    assert!(s.gw.is_empty());
}

#[test]
fn lsblk_section_is_parsed() {
    let mut block = top_block();
    block.push_str("__LSBLK__\n");
    block.push_str("NAME=\"sda\" PKNAME=\"\" TYPE=\"disk\" SIZE=\"1000\" MOUNTPOINT=\"\"\n");
    let s = collect_monitor_sample(&block);
    assert_eq!(s.lsblk.len(), 1);
    assert_eq!(s.lsblk[0].name, "sda");
    assert_eq!(s.lsblk[0].kind, "disk");
    assert_eq!(s.lsblk[0].size, 1000);
    assert!(s.lsblk[0].pkname.is_empty());
}

fn lsblk_rows() -> Vec<LsblkRow> {
    [
        "NAME=\"sda\" PKNAME=\"\" TYPE=\"disk\" SIZE=\"32000000000\" MOUNTPOINT=\"\"",
        "NAME=\"sda1\" PKNAME=\"sda\" TYPE=\"part\" SIZE=\"8000000000\" MOUNTPOINT=\"/\"",
        "NAME=\"sda2\" PKNAME=\"sda\" TYPE=\"part\" SIZE=\"24000000000\" MOUNTPOINT=\"/data\"",
        "NAME=\"sdz\" PKNAME=\"\" TYPE=\"disk\" SIZE=\"64000000000\" MOUNTPOINT=\"\"",
        "NAME=\"sdz1\" PKNAME=\"sdz\" TYPE=\"part\" SIZE=\"64000000000\" MOUNTPOINT=\"/mnt/other\"",
    ]
    .iter()
    .filter_map(|line| parse_lsblk_line(line))
    .collect()
}

#[test]
fn system_disk_uses_whole_device_and_sums_its_partitions() {
    let rows = lsblk_rows();
    let disks = vec![
        ("/".to_string(), 1_000_000_000, 8_000_000_000),
        ("/data".to_string(), 4_000_000_000, 24_000_000_000),
        // 另一块盘：不能计入系统盘。
        ("/mnt/other".to_string(), 0, 64_000_000_000),
    ];
    let (used, total) = system_disk_bytes(&disks, &rows).expect("/ 在 sda1 上");
    assert_eq!(total, 32_000_000_000, "总量取整盘");
    assert_eq!(used, 27_000_000_000, "7G + 20G");
}

#[test]
fn system_disk_dedups_multiple_mounts_of_one_device() {
    // btrfs 子卷：同一分区两个挂载点各报一次用量，只算一次（取最大值）。
    let rows: Vec<LsblkRow> = [
        "NAME=\"sda\" PKNAME=\"\" TYPE=\"disk\" SIZE=\"1000\" MOUNTPOINT=\"\"",
        "NAME=\"sda1\" PKNAME=\"sda\" TYPE=\"part\" SIZE=\"1000\" MOUNTPOINT=\"/,/home\"",
    ]
    .iter()
    .filter_map(|line| parse_lsblk_line(line))
    .collect();
    let disks = vec![
        ("/".to_string(), 400, 1000),
        ("/home".to_string(), 400, 1000),
    ];
    let (used, total) = system_disk_bytes(&disks, &rows).unwrap();
    // 同一分区报两次 600：取最大值 600，而不是相加成 1200。
    assert_eq!((used, total), (600, 1000));
}

#[test]
fn system_disk_returns_none_when_root_is_not_on_a_physical_disk() {
    // overlay / tmpfs 上的 `/`，或 lsblk 缺失 → 回退 `/` 分区口径。
    let rows: Vec<LsblkRow> = ["NAME=\"sda\" PKNAME=\"\" TYPE=\"disk\" SIZE=\"1000\" MOUNTPOINT=\"\""]
        .iter()
        .filter_map(|line| parse_lsblk_line(line))
        .collect();
    assert!(system_disk_bytes(&[("/".to_string(), 1, 2)], &rows).is_none());
    assert!(system_disk_bytes(&[], &[]).is_none());
}

#[test]
fn marker_only_sections_produce_no_rows() {
    let s = collect_monitor_sample("__DF__\n__SYS__\n");
    assert!(s.disks.is_empty());
    assert!(s.sys_kv.is_empty());
    assert!(!s.have_cpu);
    assert_eq!(s.mem_total, 0);
}
