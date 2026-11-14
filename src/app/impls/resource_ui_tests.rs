#[cfg(test)]
mod tests {
    use super::super::*;

    fn pairs(xs: &[(&str, &str)]) -> Vec<(String, String)> {
        xs.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// 关键契约：**就地更新同一个 `VecModel` 实例**（Repeater 只在实例不变时复用已有的项），
    /// 并如实报出"内容是否变化"（调用方据此跳过一次整个窗口的重绘）。
    #[test]
    fn try_write_rows_changed_updates_in_place_and_reports_changes() {
        let vm = Rc::new(VecModel::from(vec!["a".to_string()]));
        let model = ModelRc::from(vm.clone());

        assert_eq!(
            try_write_rows_changed(&model, &["a".to_string(), "b".to_string()]),
            Some(true),
            "长度变了 → 有变化"
        );
        assert_eq!(vm.row_count(), 2, "长度变了要 push");
        assert_eq!(vm.row_data(1).unwrap(), "b");

        assert_eq!(
            try_write_rows_changed(&model, &["a".to_string(), "b".to_string()]),
            Some(false),
            "内容一模一样 → 一次通知都不发"
        );

        assert_eq!(
            try_write_rows_changed(&model, &["b".to_string()]),
            Some(true),
            "变短了 → 有变化"
        );
        assert_eq!(vm.row_count(), 1, "变短了要 remove");
        assert_eq!(vm.row_data(0).unwrap(), "b");

        // 同一个实例 —— 这就是"Repeater 不重建"的依据。
        assert!(std::ptr::eq(
            vm.as_ref(),
            model.as_any().downcast_ref::<VecModel<String>>().unwrap()
        ));
    }

    // ---------- apply_rows：模型增量写入 ----------

    /// 内容没变的行不写回 —— 这正是「增量」的意义（Repeater 不会重建这些行）。
    #[test]
    fn apply_rows_writes_only_changed_rows() {
        let vm = VecModel::from(vec!["a".to_string(), "b".to_string()]);
        apply_rows(&vm, vec!["a".to_string(), "B".to_string()]);
        assert_eq!(vm.row_count(), 2);
        assert_eq!(vm.row_data(0).unwrap(), "a");
        assert_eq!(vm.row_data(1).unwrap(), "B");
    }

    /// 行数变多 / 变少都在**尾部**增删，前面的行原地保留。
    #[test]
    fn apply_rows_grows_and_shrinks_at_the_tail() {
        let vm = VecModel::from(vec!["a".to_string()]);
        apply_rows(&vm, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        assert_eq!(vm.row_count(), 3);
        assert_eq!(vm.row_data(2).unwrap(), "c");

        apply_rows(&vm, vec!["c".to_string()]);
        assert_eq!(vm.row_count(), 1, "尾部多余的行走 remove");
        assert_eq!(vm.row_data(0).unwrap(), "c");
    }

    // ---------- push_ring：网络历史环形缓冲 ----------

    #[test]
    fn push_ring_shifts_left_and_appends() {
        let mut buf: Vec<f32> = (0..chart_points()).map(|i| i as f32).collect();
        push_ring(&mut buf, 42.0);
        assert_eq!(buf.len(), chart_points(), "长度恒定");
        assert_eq!(buf[0], 1.0, "最旧的一个（0.0）被挤掉");
        assert_eq!(*buf.last().unwrap(), 42.0, "新值进队尾");
    }

    /// 长度不对时**整体重建并用首值填满** —— 换网卡 / 新会话靠这条兜底；
    /// 填首值而不是 0，避免 `smooth_path` 重采样后曲线前半段是假零。
    #[test]
    fn push_ring_reinitializes_wrong_sized_input() {
        let mut buf = vec![9.0; 3];
        push_ring(&mut buf, 1.5);
        assert_eq!(buf.len(), chart_points());
        assert_eq!(buf[chart_points() - 1], 1.5);
        assert!(
            buf[..chart_points() - 1].iter().all(|v| *v == 1.5),
            "旧值不能残留，且用首值回填"
        );
    }

    // ---------- normalize / graph_model：折线图归一化 ----------

    #[test]
    fn normalize_scales_by_max() {
        let m = graph_model(&normalize(&[2.0, 4.0]));
        assert_eq!(m.row_count(), 2);
        assert_eq!(m.row_data(0).unwrap(), 0.5);
        assert_eq!(m.row_data(1).unwrap(), 1.0);
    }

    /// 峰值不超过 1.0 时**不放大**（`max` 从 1.0 起算）：低流量不该被拉满整个图。
    #[test]
    fn normalize_does_not_amplify_sub_unit_values() {
        let m = graph_model(&normalize(&[0.25, 1.0]));
        assert_eq!(m.row_data(0).unwrap(), 0.25, "小值保持原样");
    }

    #[test]
    fn normalize_clamps_negatives_and_tolerates_empty() {
        let m = graph_model(&normalize(&[-3.0, 0.0]));
        assert_eq!(m.row_data(0).unwrap(), 0.0, "负数压到 0");
        assert_eq!(
            graph_model(&normalize(&[])).row_count(),
            0,
            "空输入不 panic"
        );
    }

    // ---------- disk_rows ----------

    fn disks() -> Vec<(String, u64, u64)> {
        vec![
            ("/".to_string(), 50, 100),  // 用了一半
            ("/proc".to_string(), 0, 0), // 伪文件系统（total 为 0）
            ("/data".to_string(), 1, 4), // 已用 75%
        ]
    }

    #[test]
    fn disk_rows_computes_usage_and_detail() {
        let rows = disk_rows(&disks(), "", false);
        assert_eq!(rows.len(), 3, "空过滤 = 全部");
        assert_eq!(rows[0].path.as_str(), "/");
        assert_eq!(rows[0].percent, 0.5);
        assert_eq!(rows[2].percent, 0.75);
        assert_eq!(
            rows[0].detail.as_str(),
            format!("{}/{}", format_size(50), format_size(100)),
            "detail 是「可用/总量」"
        );
    }

    /// `total == 0` 不能做除数（伪文件系统就长这样）→ percent 取 0。
    #[test]
    fn disk_rows_handles_zero_total() {
        let rows = disk_rows(&[("/proc".to_string(), 0, 0)], "", false);
        assert_eq!(rows[0].percent, 0.0);
    }

    /// 过滤串是手输的：按空格 / 逗号 / 分号切，空段忽略。
    #[test]
    fn disk_rows_splits_the_mount_filter() {
        let rows = disk_rows(&disks(), " /data ,  ; ", false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path.as_str(), "/data");
    }

    #[test]
    fn disk_rows_hides_special_partitions_when_asked() {
        let rows = disk_rows(&disks(), "", true);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.path.as_str() != "/proc"));
    }

    // ---------- 进程列表 ----------

    #[test]
    fn process_needs_root_only_for_someone_elses_process() {
        assert!(!process_needs_root("root", "alice"), "root 不需要提权");
        assert!(!process_needs_root("alice", "alice"), "自己的进程");
        assert!(process_needs_root("alice", "root"), "别人的进程");
    }

    #[test]
    fn proc_rows_formats_and_clamps() {
        let procs = vec![ProcInfo {
            pid: 42,
            user: "alice".into(),
            cpu: 150.0, // 多核可以超 100%
            mem: 12.34,
            command: "vim".into(),
        }];
        let rows = proc_rows(&procs, "alice", "tab-1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tab_id.as_str(), "tab-1");
        assert_eq!(rows[0].pid.as_str(), "42");
        assert_eq!(rows[0].cpu.as_str(), "150.0");
        assert_eq!(rows[0].mem.as_str(), "12.3");
        assert_eq!(rows[0].cpu_frac, 1.0, "进度条比例必须夹在 0..1");
        assert!(rows[0].own_process, "自己的进程可直接操作");
    }

    // ---------- 系统信息窗口的行映射 ----------

    /// CPU / 内存 / 交换：固定三行、顺序固定，CPU 行不带 detail。
    #[test]
    fn metric_rows_keeps_three_fixed_rows() {
        let rows = metric_rows(12.5, 30.0, 1.0, "8/16 GB", "0/2 GB");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].label.as_str(), "CPU");
        assert_eq!(rows[0].percent, 12.5);
        assert!(rows[0].detail.is_empty());
        assert_eq!(rows[1].detail.as_str(), "8/16 GB");
        assert_eq!(rows[2].detail.as_str(), "0/2 GB");
    }

    /// **上下行不能接反**：`up` 取 tx、`down` 取 rx。
    #[test]
    fn net_rows_maps_tx_to_up_and_rx_to_down() {
        let rows = net_rows(&[("eth0".to_string(), 1024, 2048)]);
        assert_eq!(rows[0].name.as_str(), "eth0");
        assert_eq!(rows[0].up.as_str(), format_bytes_per_sec(2048));
        assert_eq!(rows[0].down.as_str(), format_bytes_per_sec(1024));
    }

    /// 概览：**两对一行**的四列；奇数个时末行的 c3/c4 留空（不是 "-"）。
    #[test]
    fn overview_rows_pair_two_pairs_per_row() {
        let rows = pairs_to_overview_rows(&pairs(&[("a", "1"), ("b", "2"), ("c", "3")]));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].c1.as_str(), "a");
        assert_eq!(rows[0].c4.as_str(), "2");
        assert_eq!(rows[0].c5.as_str(), "", "第五列不用");
        assert_eq!(rows[1].c1.as_str(), "c");
        assert_eq!(rows[1].c3.as_str(), "");
        assert_eq!(rows[1].c4.as_str(), "");
    }

    #[test]
    fn one_row_pads_missing_values_with_dash() {
        let rows = pairs_to_one_row(&pairs(&[("a", "1"), ("b", "2")]));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].c2.as_str(), "2");
        assert_eq!(rows[0].c3.as_str(), "-");
        assert_eq!(rows[0].c5.as_str(), "-");
    }

    /// 按 `width` 分块；**整块都是空值 / "-" 的直接丢掉**（否则界面上多出空白行）。
    /// ⚠️ `width` 必须 ≥ 1（`chunks(0)` 会 panic）；现有调用点传的是常量 4。
    #[test]
    fn rows_by_width_drops_blank_chunks_and_pads() {
        let data = pairs(&[
            ("1", "a"),
            ("2", "b"),
            ("3", "c"),
            ("4", "d"),
            ("5", ""),
            ("6", "-"),
            ("7", "   "),
            ("8", ""),
        ]);
        let rows = pairs_to_rows(&data, 4);
        assert_eq!(rows.len(), 1, "第二块全是空值 → 丢弃");
        assert_eq!(rows[0].c4.as_str(), "d");

        let short = pairs_to_rows(&pairs(&[("1", "a")]), 4);
        assert_eq!(short[0].c1.as_str(), "a");
        assert_eq!(short[0].c2.as_str(), "-", "块内缺位补 '-'");
        assert_eq!(short[0].c5.as_str(), "-");
    }

    /// CPU 详情行的**列序是刻意错位的**（c1←1、c2←3、c3←2、c4←4），第 5 列起拼 "k v / k v"。
    /// 这条测试就是为了钉死这个顺序 —— 调换即等于错标数据。
    #[test]
    fn cpu_detail_rows_reorder_columns_and_append_extras() {
        let rows = cpu_usage_detail_rows(&pairs(&[
            ("usr", "10%"),
            ("sys", "20%"),
            ("nice", "30%"),
            ("idle", "40%"),
            ("iowait", "5%"),
            ("irq", "1%"),
        ]));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].c1.as_str(), "10%");
        assert_eq!(rows[0].c2.as_str(), "30%", "第三项放第二列");
        assert_eq!(rows[0].c3.as_str(), "20%", "第二项放第三列");
        assert_eq!(rows[0].c4.as_str(), "40%");
        assert_eq!(rows[0].c5.as_str(), "iowait 5% / irq 1%");
    }

    #[test]
    fn cpu_detail_rows_default_missing_values_to_zero_percent() {
        let rows = cpu_usage_detail_rows(&pairs(&[("usr", "10%")]));
        assert_eq!(rows[0].c1.as_str(), "10%");
        assert_eq!(rows[0].c2.as_str(), "0.0%", "缺位用 0.0% 而不是 '-'");
        assert_eq!(rows[0].c5.as_str(), "", "没有额外项");
    }

    #[test]
    fn tuple5_rows_maps_straight_through() {
        let rows = tuple5_rows(&[("a".into(), "b".into(), "c".into(), "d".into(), "e".into())]);
        let r = &rows[0];
        assert_eq!(
            (
                r.c1.as_str(),
                r.c2.as_str(),
                r.c3.as_str(),
                r.c4.as_str(),
                r.c5.as_str()
            ),
            ("a", "b", "c", "d", "e")
        );
    }

    #[test]
    fn smooth_path_is_empty_for_empty_input() {
        assert!(smooth_path(&[], 60).is_empty());
    }

    #[test]
    fn smooth_path_uses_quadratic_segments_and_closes_on_the_baseline() {
        let p = smooth_path(&[0.5; 60], 60);
        assert!(p.starts_with("M "), "{p}");
        assert!(p.contains(" Q "), "应当有二次贝塞尔段：{p}");
        assert!(p.ends_with("L 100.00 100.00 L 0.00 100.00 Z"), "{p}");
        // 常量 0.5 → y = 50
        assert!(p.contains("50.00"), "{p}");
    }

    #[test]
    fn smooth_path_handles_a_single_sample() {
        let p = smooth_path(&[1.0], 60);
        assert!(p.starts_with("M "), "{p}");
        assert!(p.ends_with("Z"), "{p}");
    }
}
