//! per-tab 状态搬家（旧外壳删除 ①）：把「活动标签」的隧道 / SFTP 状态从
//! `terminals` 模型的行（旧外壳 `for term` 里的 `term.*`）提升到 AppWindow 级。
//!
//! 数据源仍是 `terminals` 模型 —— 每个标签一行、非活动标签也照常更新（SFTP
//! 事件都带 tab_id）；这里只做「活动行 → 窗口级属性」的镜像，让窗口级的消费方
//! （新外壳右面板、后续的状态栏）不必待在旧外壳的 per-tab 循环里。套路与
//! `refresh_sidebar` 把活动标签的资源数据镜像到窗口级一致。
//!
//! Slint 侧拿不到「按 id 找行」的索引，所以镜像必须由 Rust 显式刷新，调用点：
//!   * `refresh_sidebar()`（活动标签变化 + 定时 tick）——覆盖切标签、建/关标签，
//!     以及启动后的首次填充；
//!   * `session_event` 里会改 SFTP / 隧道字段的事件分支 —— 覆盖实时数据；
//!   * SFTP 排序回调 —— 面板自己发起、只改行内顺序，没有事件回流。
//!
//! 尚未提升的字段（树节点 / 勾选数 / 面板几何）仍留在 per-tab 模型里：目前只有旧
//! SFTP 面板用，等旧外壳删除时一并处理。
//!
//! 另外顺带算出这些「Slint 算不了」的东西，给右侧工具面板用：
//!   * `active-sftp-dir-count` / `-file-count` / `-total` —— footer 的 DIR / FILE /
//!     TOTAL 徽标（Slint 没有 reduce/filter）；
//!   * `active-sftp-crumbs` —— 面包屑，按 `/` 切好路径（Slint 没有 split）；
//!   * `active-sftp-sort-key` / `-dir` —— 列头要显示排序方向；
//!   * 搜索过滤：面板把查询串写进窗口属性 `sftp-search-query`，这里按它过滤后再镜像
//!     （Slint 的字符串只有 starts-with / ends-with，没有 contains）。

use super::*;

/// 把 `active-tab-id` 那一行的 tunnels / SFTP 字段镜像到 AppWindow 级属性。
///
/// 找不到活动行（欢迎页、空布局）时清空 —— 否则右面板会显示上一个会话的残留。
/// 清空只在当前值非空时做，避免 1 Hz tick 每次都重新分配空模型。
pub(crate) fn refresh_active_term(win: &AppWindow) {
    let terminals_rc = win.get_terminals();
    let Some(terminals) = terminals_rc
        .as_any()
        .downcast_ref::<VecModel<TerminalState>>()
    else {
        return;
    };
    let active = win.get_active_tab_id().to_string();
    let row = (0..terminals.row_count()).find_map(|i| {
        let row = terminals.row_data(i)?;
        (row.id.as_str() == active.as_str()).then_some(row)
    });

    match row {
        Some(row) => {
            win.set_active_tunnels(row.tunnels.clone());
            win.set_active_sftp_path(row.sftp_path.clone());
            // 搜索过滤：面板把查询串写进窗口属性，这里按它过滤后再镜像。
            let query = win.get_sftp_search_query().to_lowercase();
            win.set_active_sftp_entries(filter_entries(&row, &query));
            win.set_active_sftp_status(row.sftp_status.clone());
            win.set_active_sftp_loading(row.sftp_loading);
            win.set_active_sftp_available(row.sftp_available);
            // 列头要显示排序方向（key + dir 都在行里）。
            win.set_active_sftp_sort_key(row.sftp_sort_key.clone());
            win.set_active_sftp_sort_dir(row.sftp_sort_dir);
            // 面包屑（按 `/` 切好；Slint 没有 split）。
            let crumbs = sftp_crumbs(row.sftp_path.as_str());
            win.set_active_sftp_crumbs(ModelRc::from(std::rc::Rc::new(VecModel::from(crumbs))));
            // 右面板 footer 的 PATH / DIR / FILE / TOTAL 统计（真数据，不靠 UI 数）。
            let (dirs, files, total) = sftp_stats(&row);
            win.set_active_sftp_dir_count(dirs);
            win.set_active_sftp_file_count(files);
            win.set_active_sftp_total(total.into());
        }
        None => {
            let stale = !win.get_active_sftp_path().is_empty()
                || win.get_active_tunnels().row_count() > 0
                || win.get_active_sftp_entries().row_count() > 0;
            if stale {
                win.set_active_tunnels(ModelRc::from(std::rc::Rc::new(
                    VecModel::<TunnelInfo>::default(),
                )));
                win.set_active_sftp_path("".into());
                win.set_active_sftp_entries(ModelRc::from(std::rc::Rc::new(
                    VecModel::<SftpEntry>::default(),
                )));
                win.set_active_sftp_status("".into());
                win.set_active_sftp_loading(false);
                win.set_active_sftp_available(false);
                win.set_active_sftp_sort_key("".into());
                win.set_active_sftp_sort_dir(0);
                win.set_active_sftp_crumbs(ModelRc::from(std::rc::Rc::new(
                    VecModel::<SftpCrumb>::default(),
                )));
                win.set_active_sftp_dir_count(0);
                win.set_active_sftp_file_count(0);
                win.set_active_sftp_total("".into());
            }
        }
    }
}

/// 按查询串过滤条目：大小写不敏感的子串匹配（Slint 的字符串只有 starts-with /
/// ends-with，没有 contains）。空串 = 原样返回那一行的模型（不换身份）。
fn filter_entries(row: &TerminalState, query: &str) -> ModelRc<SftpEntry> {
    if query.is_empty() {
        return row.sftp_entries.clone();
    }
    let Some(entries) = row
        .sftp_entries
        .as_any()
        .downcast_ref::<VecModel<SftpEntry>>()
    else {
        return ModelRc::from(std::rc::Rc::new(VecModel::<SftpEntry>::default()));
    };
    let kept: Vec<SftpEntry> = (0..entries.row_count())
        .filter_map(|i| entries.row_data(i))
        .filter(|e| e.name.to_lowercase().contains(query))
        .collect();
    ModelRc::from(std::rc::Rc::new(VecModel::from(kept)))
}

/// 面包屑：绝对路径先给一段根 `/`，然后逐段累积完整路径。空路径 = 空。
fn sftp_crumbs(path: &str) -> Vec<SftpCrumb> {
    let mut out: Vec<SftpCrumb> = Vec::new();
    if path.is_empty() {
        return out;
    }
    let absolute = path.starts_with('/');
    if absolute {
        out.push(SftpCrumb {
            name: "/".into(),
            path: "/".into(),
        });
    }
    let mut acc = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if acc.is_empty() && !absolute {
            acc = seg.to_string();
        } else {
            acc = format!("{acc}/{seg}");
        }
        out.push(SftpCrumb {
            name: seg.into(),
            path: acc.clone().into(),
        });
    }
    out
}

/// 当前目录的「目录数 / 文件数 / 文件总大小」——右面板 footer 那三个徽标的值。
/// 只看已到达的列表（`size_bytes` 由 Rust 侧格式化时就带上了），不做异步统计。
fn sftp_stats(row: &TerminalState) -> (i32, i32, String) {
    let (mut dirs, mut files, mut bytes) = (0i32, 0i32, 0u64);
    if let Some(entries) = row
        .sftp_entries
        .as_any()
        .downcast_ref::<VecModel<SftpEntry>>()
    {
        for i in 0..entries.row_count() {
            let Some(e) = entries.row_data(i) else { continue };
            if e.is_dir {
                dirs += 1;
            } else {
                files += 1;
                bytes += e.size_bytes.max(0.0) as u64;
            }
        }
    }
    (dirs, files, format_size(bytes))
}
