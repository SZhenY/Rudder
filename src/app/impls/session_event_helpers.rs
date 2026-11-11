use super::*;

/// 传输行的进度文案（state：0 传输中 / 1 完成 / 2 失败 / 3 准备中 / 4 已取消）。
pub(crate) fn transfer_detail(state: u8, msg: &str, transferred: u64, total: u64) -> String {
    match state {
        // 失败时优先显示服务端给的原因，没有才用通用文案。
        2 => {
            if msg.is_empty() {
                t("失败", "Failed").to_string()
            } else {
                msg.to_string()
            }
        }
        1 => t("已完成", "Done").to_string(),
        // 远端准备阶段（如 tar 打包）还没开始传字节（#100）。
        3 => t("文件准备中", "Preparing...").to_string(),
        // 用户取消（#100）。
        4 => t("已取消", "Cancelled").to_string(),
        _ => {
            if total > 0 {
                format!("{}/{}", format_size(transferred), format_size(total))
            } else {
                format_size(transferred)
            }
        }
    }
}

/// 进度条比例：完成态直接 1.0（否则会永远停在 99%）；`total == 0` 时不能做除数。
pub(crate) fn transfer_percent(state: u8, transferred: u64, total: u64) -> f32 {
    if state == 1 {
        1.0
    } else if total > 0 {
        (transferred as f32 / total as f32).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// 写入/更新一行传输记录，返回**是否还有活跃任务**（驱动工具栏呼吸灯）。
///
/// 新行插到队首（最新在上）；超过上限时保留队首、淘汰队尾 —— 方向写反就会把刚完成的
/// 那条挤掉，用户再也看不到它。
pub(crate) fn upsert_transfer_row(model: &VecModel<TransferInfo>, rec: TransferInfo) -> bool {
    const MAX_TRANSFER_ROWS: usize = 200;
    let mut found = None;
    for i in 0..model.row_count() {
        if let Some(row) = model.row_data(i)
            && row.id.as_str() == rec.id.as_str()
        {
            found = Some(i);
            break;
        }
    }
    match found {
        Some(i) => model.set_row_data(i, rec),
        None => {
            model.insert(0, rec); // newest at top
            if model.row_count() > MAX_TRANSFER_ROWS {
                let keep: Vec<TransferInfo> = (0..MAX_TRANSFER_ROWS)
                    .filter_map(|i| model.row_data(i))
                    .collect();
                model.set_vec(keep);
            }
        }
    }
    // 呼吸灯：还有传输中(0)或准备中(3)的就算活跃。
    (0..model.row_count()).any(|i| {
        model
            .row_data(i)
            .map(|r| r.state == 0 || r.state == 3)
            .unwrap_or(false)
    })
}
