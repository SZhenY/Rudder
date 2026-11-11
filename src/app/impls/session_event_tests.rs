use super::*;

fn rec(id: &str, state: i32) -> TransferInfo {
    TransferInfo {
        id: id.into(),
        name: format!("{id}.bin").into(),
        detail: String::new().into(),
        percent: 0.0,
        state,
        is_upload: true,
    }
}

#[test]
fn detail_uses_the_server_message_on_failure() {
    assert_eq!(transfer_detail(2, "", 0, 0), t("失败", "Failed"));
    assert_eq!(
        transfer_detail(2, "permission denied", 0, 0),
        "permission denied",
        "有服务端原因就用原因"
    );
    assert_eq!(transfer_detail(1, "", 0, 0), t("已完成", "Done"));
    assert_eq!(
        transfer_detail(3, "", 0, 0),
        t("文件准备中", "Preparing...")
    );
    assert_eq!(transfer_detail(4, "", 0, 0), t("已取消", "Cancelled"));
}

#[test]
fn detail_shows_bytes_while_transferring() {
    assert_eq!(
        transfer_detail(0, "", 1536, 4096),
        format!("{}/{}", format_size(1536), format_size(4096))
    );
    assert_eq!(
        transfer_detail(0, "", 1536, 0),
        format_size(1536),
        "不知道总量时只显示已传"
    );
}

/// 完成态必须直接给 1.0：靠 `transferred/total` 算的话，最后一段字节没到的任务会停在 99%。
#[test]
fn percent_is_one_when_done_and_zero_when_total_is_unknown() {
    assert_eq!(transfer_percent(1, 10, 100), 1.0);
    assert_eq!(transfer_percent(0, 0, 0), 0.0, "总量未知不能做除数");
    assert_eq!(transfer_percent(0, 50, 100), 0.5);
    assert_eq!(transfer_percent(0, 150, 100), 1.0, "超过 100% 要夹住");
}

/// 新任务插到队首，并按状态给出"还有活跃任务"（呼吸灯：只要有 0 或 3 就亮）。
#[test]
fn upsert_inserts_at_the_top_and_reports_activity() {
    let m = VecModel::from(vec![rec("old", 1)]);
    assert!(
        upsert_transfer_row(&m, rec("new", 0)),
        "传输中 -> 呼吸灯该亮"
    );
    assert_eq!(m.row_count(), 2);
    assert_eq!(m.row_data(0).unwrap().id.as_str(), "new", "最新的在最上面");

    // 全是终态（完成 1 / 失败 2 / 取消 4）时不该再亮。
    let settled = VecModel::from(vec![rec("a", 1), rec("b", 2), rec("c", 4)]);
    assert!(
        !upsert_transfer_row(&settled, rec("d", 1)),
        "只剩终态 -> 呼吸灯熄灭"
    );
    let preparing = VecModel::from(vec![rec("a", 1)]);
    assert!(
        upsert_transfer_row(&preparing, rec("b", 3)),
        "准备中也算活跃"
    );
}

/// 同一个 id 再来一次是原地替换（不新增行）—— 同一次传输的进度更新路径。
#[test]
fn upsert_replaces_the_same_id_in_place() {
    let m = VecModel::from(vec![rec("a", 0), rec("b", 1)]);
    assert!(upsert_transfer_row(&m, rec("a", 3)));
    assert_eq!(m.row_count(), 2, "不新增行");
    assert_eq!(m.row_data(0).unwrap().id.as_str(), "a", "位置不变");
    assert_eq!(m.row_data(0).unwrap().state, 3, "状态已更新");
}

/// 超过 200 行时保留队首、淘汰队尾 —— 方向写反就会把刚完成的挤掉。
#[test]
fn upsert_evicts_oldest_rows_past_the_cap() {
    let m = VecModel::from(Vec::new());
    for i in 0..201 {
        upsert_transfer_row(&m, rec(&format!("t{i}"), 1));
    }
    assert_eq!(m.row_count(), 200, "上限 200 行");
    let ids: Vec<String> = (0..m.row_count())
        .filter_map(|i| m.row_data(i))
        .map(|r| r.id.to_string())
        .collect();
    assert_eq!(ids[0], "t200", "最新的还在最上面");
    assert!(!ids.contains(&"t0".to_string()), "最旧的被淘汰");
}
