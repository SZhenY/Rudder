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
//! 尚未提升的字段（树节点 / 勾选数 / 排序键 / 面板几何）仍留在 per-tab 模型里：
//! 目前只有旧 SFTP 面板用，等旧外壳删除时一并处理。

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
            win.set_active_sftp_entries(row.sftp_entries.clone());
            win.set_active_sftp_status(row.sftp_status.clone());
            win.set_active_sftp_loading(row.sftp_loading);
            win.set_active_sftp_available(row.sftp_available);
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
            }
        }
    }
}
