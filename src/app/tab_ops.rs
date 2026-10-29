//! 标签页的开 / 关。
//!
//! 从 `session_callbacks::on_connect_session` 与 `tab_callbacks::on_pane_tab_closed`
//! 搬出来的共用实现：MCP 的 `open_session` / `close_session` 必须与用户点界面的
//! 路径**完全一致**（同样的模型、布局、SFTP 旁路、状态清理），否则两条路径会各自
//! 演进成不同的行为。
//!
//! 参数用 `TabEnv`（一组借用）而不是再造一个持有 `Rc` 的上下文：这些字段本来就
//! 活在 `AppContext` 里，复制一份指针只会给"漏同步"留下机会。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use slint::{Model, ModelRc, VecModel};

use super::pane_layout::refresh_panes;
use super::render_tickets::RENDER_MIN_INTERVAL;
use super::session_callbacks::{conn_label, session_has_sftp};
use super::session_runtime::start_session_in_tab;
use super::tab_title_len;
use crate::config::{ConfigStore, Session, SessionKind};
use crate::i18n::t;
use crate::layout::Layout;
use crate::resource::{TabStatus, TabStatuses};
use crate::sftp::{SftpHandles, SftpLastCwd};
use crate::session::ConnectCtx;
use crate::ssh::SessionHandle;
use crate::terminal::{
    OutputHighlightPreset, RenderGates, TabRenderGate, TermBuffer, TermBuffers,
    compile_output_rules,
};
use crate::ui::*;

/// 打开 / 关闭标签页需要的全部共享状态。
pub(crate) struct TabEnv<'a> {
    pub(crate) window: &'a AppWindow,
    pub(crate) store: &'a Rc<RefCell<ConfigStore>>,
    pub(crate) handles: &'a Rc<RefCell<HashMap<String, SessionHandle>>>,
    pub(crate) tab_titles: &'a Rc<RefCell<HashMap<String, String>>>,
    pub(crate) tab_statuses: &'a TabStatuses,
    pub(crate) tabs_model: &'a Rc<VecModel<TabInfo>>,
    pub(crate) terminals_model: &'a Rc<VecModel<TerminalState>>,
    pub(crate) bufs: &'a TermBuffers,
    pub(crate) render_gates: &'a RenderGates,
    pub(crate) sftp_handles: &'a SftpHandles,
    pub(crate) sftp_last_cwd: &'a SftpLastCwd,
    pub(crate) layout: &'a Rc<RefCell<Layout>>,
    pub(crate) panes_model: &'a Rc<VecModel<PaneInfo>>,
    pub(crate) splitters_model: &'a Rc<VecModel<SplitterInfo>>,
    pub(crate) content_size: &'a Rc<Cell<(f32, f32)>>,
}

/// 为已保存（或内置）会话开一个新标签页，返回新 `tab_id`。
pub(crate) fn open_session_tab(
    env: &TabEnv<'_>,
    connect_ctx: &ConnectCtx,
    session: Session,
) -> String {
    let session_id = session.id.clone();
    let tab_id = format!("term-{}", uuid::Uuid::new_v4());
    let tab_title = session.name.clone();

    // Connection label shown in the sidebar / status line, per transport.
    let conn_label = conn_label(&session);
    // Serial / Telnet have no SFTP side-channel.
    let has_sftp = session_has_sftp(&session.kind);

    // Seed the per-tab status so the sidebar shows "连接中 host" the moment this
    // tab becomes active (the `changed active-tab-id` handler fires
    // refresh-sidebar right after set_active_tab_id).
    env.tab_statuses
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .insert(
            tab_id.clone(),
            TabStatus {
                host: conn_label,
                user: session.user.clone(),
                session_id,
                kind: session.kind,
                state: 0,
                is_local: session.kind == SessionKind::Local,
                ..Default::default()
            },
        );

    // Register tab + terminal state (SFTP fields start empty/loading).
    env.tabs_model.push(TabInfo {
        id: tab_id.clone().into(),
        title_len: tab_title_len(&tab_title),
        title: tab_title.into(),
        kind: "terminal".into(),
        connected: false,
    });
    // Each session keeps its own SFTP collapse state + sizes, seeded from the
    // global defaults so they no longer bleed across panes (#v0.5).
    let (sftp_collapsed_default, sftp_h_default, sftp_w_default) = (
        env.window.get_collapse_sftp_default(),
        env.window.get_sftp_panel_height(),
        env.window.get_sftp_panel_width(),
    );
    env.terminals_model.push(TerminalState {
        id: tab_id.clone().into(),
        status: t("连接中...", "Connecting...").into(),
        spans: ModelRc::from(Rc::new(VecModel::<TermSpan>::default())),
        cursor_row: 0,
        mouse_tracked: false,
        cursor_col: 0,
        rows_used: 0,
        scroll_max: 0,
        scroll_offset: 0,
        is_alt_screen: false,
        find_matches: ModelRc::from(Rc::new(VecModel::<TermMatch>::default())),
        selection: ModelRc::from(Rc::new(VecModel::<TermMatch>::default())),
        font_size: 0.0,
        sftp_path: "/".into(),
        sftp_entries: ModelRc::from(Rc::new(VecModel::<SftpEntry>::default())),
        sftp_status: if has_sftp {
            t("SFTP 连接中...", "SFTP connecting...").into()
        } else {
            t(
                "此会话类型不支持 SFTP",
                "SFTP not available for this session",
            )
            .into()
        },
        sftp_loading: has_sftp,
        sftp_tree_nodes: ModelRc::from(Rc::new(VecModel::<SftpTreeNode>::default())),
        sftp_selected_count: 0,
        sftp_sort_key: "".into(),
        sftp_sort_dir: 0,
        sftp_available: has_sftp,
        tunnels: ModelRc::from(Rc::new(VecModel::<TunnelInfo>::default())),
        sftp_collapsed: !has_sftp || sftp_collapsed_default,
        sftp_panel_height: sftp_h_default,
        sftp_panel_width: sftp_w_default,
    });

    // Create the alacritty-backed terminal for this tab (default 24×80; resized
    // on the first terminal-resize callback). Scrollback depth comes from the
    // settings value (clamped in config.rs).
    let is_dark_now = env.window.global::<Theme>().get_dark();
    let (output_highlight, custom_highlight_rules) = {
        let settings = env.store.borrow();
        (
            OutputHighlightPreset::from_settings(
                settings.output_highlight_enabled(),
                settings.output_highlight_preset(),
            ),
            compile_output_rules(settings.output_highlight_rules()),
        )
    };
    let mut buffer = TermBuffer::new(24, 80, env.store.borrow().scrollback_lines());
    buffer.is_dark = is_dark_now;
    buffer.output_highlight = output_highlight;
    buffer.custom_highlight_rules = custom_highlight_rules;
    buffer.json_format_output = env.store.borrow().json_format_output();
    env.bufs
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .insert(tab_id.clone(), Arc::new(Mutex::new(buffer)));
    env.render_gates
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .insert(tab_id.clone(), Arc::new(TabRenderGate::new(RENDER_MIN_INTERVAL)));
    // No followed-cwd yet: the first OSC 7 always triggers a follow.
    env.sftp_last_cwd
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .remove(&tab_id);
    // Add the new tab to the focused pane and re-flatten (this also sets
    // active-tab-id to the new tab via refresh_panes).
    env.layout.borrow_mut().add_tab(tab_id.clone());
    refresh_panes(
        env.window,
        env.layout,
        env.content_size.get(),
        env.tabs_model,
        env.panes_model,
        env.splitters_model,
    );

    // Spawn the shell (+ SFTP) workers and their event-pump threads.
    start_session_in_tab(&tab_id, session, connect_ctx);
    tab_id
}

/// 关掉一个标签页。返回 `false` 表示这个 id 本来就不在标签列表里。
pub(crate) fn close_tab(env: &TabEnv<'_>, id: &str) -> bool {
    if id == "welcome" {
        return false;
    }
    // Drop any display-name override so a fresh connect starts clean.
    env.tab_titles.borrow_mut().remove(id);
    if let Some(handle) = env.handles.borrow_mut().remove(id) {
        handle.close();
    }
    // Closing a tab must never take the process down. Release builds use
    // panic = "abort", so a poisoned lock in this UI callback would kill the
    // whole client instead of just leaving one tab's resources behind — skip
    // the cleanup we cannot reach.
    if let Ok(mut handles) = env.sftp_handles.lock()
        && let Some(sftp) = handles.remove(id)
    {
        sftp.close();
    }
    if let Ok(mut cwd) = env.sftp_last_cwd.lock() {
        cwd.remove(id);
    }
    if let Ok(mut gates) = env.render_gates.lock()
        && let Some(gate) = gates.remove(id)
    {
        gate.close();
    }
    if let Ok(mut bufs) = env.bufs.lock()
        && let Some(handle) = bufs.remove(id)
    {
        // 用户**主动**关标签页 → 不保留回滚历史（网络断线是另一回事：那时要保留，
        // 方便回看断线前发生了什么）。这里显式释放一次：只把 Arc 从表里摘掉并不保证
        // 内存立刻回来 —— 会话线程还握着句柄，卡住的连接会让几十 MB 一直挂着。
        if let Ok(mut buffer) = handle.lock() {
            buffer.release_history_keep_screen();
        }
    }
    // The status entry carries the tab's process list, network history and disk
    // rows. Nothing else in the app removes one, so leaving it behind would
    // retain all of that for the rest of the process run.
    if let Ok(mut statuses) = env.tab_statuses.lock() {
        statuses.remove(id);
    }

    // Remove from tabs + terminals models.
    let tab_index = (0..env.tabs_model.row_count()).find(|index| {
        env.tabs_model
            .row_data(*index)
            .map(|row| row.id.as_str() == id)
            .unwrap_or(false)
    });
    if let Some(index) = tab_index {
        env.tabs_model.remove(index);
    }
    let terminal_index = (0..env.terminals_model.row_count()).find(|index| {
        env.terminals_model
            .row_data(*index)
            .map(|row| row.id.as_str() == id)
            .unwrap_or(false)
    });
    if let Some(index) = terminal_index {
        env.terminals_model.remove(index);
    }

    env.layout.borrow_mut().remove_tab(id);
    refresh_panes(
        env.window,
        env.layout,
        env.content_size.get(),
        env.tabs_model,
        env.panes_model,
        env.splitters_model,
    );
    tab_index.is_some()
}
