//! MCP 的 UI 线程桥 + 需要界面状态的只读操作。
//!
//! 为什么必须过桥：`AppWindow` 与 `AppContext` 都是 `Rc`，不满足 `Send`，而
//! `rmcp::ServerHandler` 要求 `Send + Sync + 'static`。因此 MCP 线程只持 `Send`
//! 数据，真正碰界面的闭包经 `invoke_from_event_loop` 回到 Slint 线程执行 ——
//! 与 `settings/mod.rs` 的 `UI_HANDLES` 是同一套范式（那里注释解释了为什么不能
//! 直接捕获 `Rc`）。
//!
//! 三条硬约束：
//! 1. 同线程调用直接拒绝：在 UI 线程上等自己 = 死锁。
//! 2. 闭包只捕获 `Send` 数据；句柄在 UI 线程从 `thread_local` 取。
//! 3. 闭包内**不得**再调 `invoke_from_event_loop`（`render_tickets.rs` 有血泪注释）。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;
use std::thread::ThreadId;
use std::time::Duration;

use serde_json::{Value, json};
use slint::ComponentHandle;

use std::sync::atomic::Ordering;

use super::AppContext;
use super::session_models::builtin_local_sessions;
use super::settings::{appearance, terminal as terminal_settings};
use super::tab_ops::{self, TabEnv};
use super::{settings_window, terminal_ui};
use crate::config::{AuthMethod, Secret, Session, SessionKind};
use crate::mcp::{AppHandles, McpError};
use crate::session::ConnectCtx;
use crate::ui::{AppWindow, Theme};

pub(crate) struct UiAccess {
    pub(crate) window: AppWindow,
    pub(crate) ctx: Rc<AppContext>,
}

thread_local! {
    static UI: RefCell<Option<UiAccess>> = const { RefCell::new(None) };
}

static UI_THREAD: OnceLock<ThreadId> = OnceLock::new();

const READ_TIMEOUT: Duration = Duration::from_secs(3);
/// 开 / 关会话要重建窗格树（`refresh_panes` 是全量重排），给足时间。
const TAB_TIMEOUT: Duration = Duration::from_secs(10);

impl UiAccess {
    /// 把 `AppContext` 的字段借成一束，交给标签页操作。
    fn tab_env(&self) -> TabEnv<'_> {
        TabEnv {
            window: &self.window,
            store: &self.ctx.store,
            handles: &self.ctx.handles,
            tab_titles: &self.ctx.tab_titles,
            tab_statuses: &self.ctx.tab_statuses,
            tabs_model: &self.ctx.tabs_model,
            terminals_model: &self.ctx.terminals_model,
            bufs: &self.ctx.bufs,
            render_gates: &self.ctx.render_gates,
            sftp_handles: &self.ctx.sftp_handles,
            sftp_last_cwd: &self.ctx.sftp_last_cwd,
            layout: &self.ctx.layout,
            panes_model: &self.ctx.panes_model,
            splitters_model: &self.ctx.splitters_model,
            content_size: &self.ctx.content_size,
        }
    }
}

/// 在 UI 线程注册句柄。事件循环结束前必须与 `uninstall` 成对出现。
pub(crate) fn install(window: &AppWindow, ctx: &Rc<AppContext>) {
    let _ = UI_THREAD.set(std::thread::current().id());
    UI.with(|slot| {
        *slot.borrow_mut() = Some(UiAccess {
            window: window.clone_strong(),
            ctx: ctx.clone(),
        });
    });
}

pub(crate) fn uninstall() {
    UI.with(|slot| *slot.borrow_mut() = None);
}

/// 把闭包送到 UI 线程执行并取回结果。超时 / 界面已销毁都映射成明确错误。
pub(crate) async fn call<T, F>(timeout: Duration, f: F) -> Result<T, McpError>
where
    T: Send + 'static,
    F: FnOnce(&UiAccess) -> Result<T, McpError> + Send + 'static,
{
    if UI_THREAD.get() == Some(&std::thread::current().id()) {
        return Err(McpError::Reentrant);
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    slint::invoke_from_event_loop(move || {
        let out = UI.with(|slot| match slot.borrow().as_ref() {
            Some(access) => f(access),
            None => Err(McpError::UiUnavailable),
        });
        let _ = tx.send(out);
    })
    .map_err(|_| McpError::UiUnavailable)?;
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(McpError::UiUnavailable),
        Err(_) => Err(McpError::UiTimeout),
    }
}

/// 取一份只读共享句柄给 MCP 服务（必须在 UI 线程调用一次）。
pub(crate) fn app_handles(ctx: &AppContext) -> AppHandles {
    AppHandles {
        bufs: ctx.bufs.clone(),
        tab_statuses: ctx.tab_statuses.clone(),
        local_snap: ctx.local_snap.clone(),
        sftp_handles: ctx.sftp_handles.clone(),
    }
}

/// 保存的主机清单。**永不返回**口令、私钥、备注（备注里常有人贴口令）。
pub(crate) async fn list_sessions(group: Option<String>) -> Result<Value, McpError> {
    call(READ_TIMEOUT, move |ui| {
        let store = ui.ctx.store.borrow();
        let mut rows = Vec::new();
        for session in store.sessions() {
            if let Some(group) = group.as_deref()
                && session.group != group
            {
                continue;
            }
            rows.push(session_row(session));
        }
        Ok(json!({ "count": rows.len(), "sessions": rows }))
    })
    .await
}

pub(crate) async fn get_session(session_id: String) -> Result<Value, McpError> {
    call(READ_TIMEOUT, move |ui| {
        let found = ui.ctx.store.borrow().get(&session_id).cloned();
        match found {
            Some(session) => Ok(json!({ "session": session_row(&session) })),
            None => Err(McpError::Invalid(format!(
                "unknown session_id: {session_id}"
            ))),
        }
    })
    .await
}

/// 侧栏是否折叠。折叠时 `start_session_in_tab` 会把资源采样停掉，
/// MCP 要临时采样就得先知道"原本该不该开"。
pub(crate) async fn sidebar_collapsed() -> Result<bool, McpError> {
    call(READ_TIMEOUT, |ui| Ok(ui.window.get_sidebar_collapsed())).await
}

/// 开 / 关某个标签页的远端资源采样。
pub(crate) async fn set_resource_monitoring(tab_id: String, enabled: bool) -> Result<(), McpError> {
    call(READ_TIMEOUT, move |ui| {
        let handles = ui.ctx.handles.borrow();
        let Some(handle) = handles.get(&tab_id) else {
            return Err(McpError::Invalid(format!("unknown tab_id: {tab_id}")));
        };
        handle.set_resource_monitoring(enabled);
        Ok(())
    })
    .await
}

// ── 设置注册表 ──────────────────────────────────────────────────────────
//
// 不为每个 ConfigStore 访问器写一个工具，而是用一张表描述"键 → 取值 / 写入 /
// 回显"。`set_settings` 事务式：只要有一个键非法就整体不写；全部合法才落一次盘，
// 再逐项把新值推到界面。

enum SettingKind {
    Bool,
    Int { min: u64, max: u64 },
    Choice(&'static [&'static str]),
    HexColor,
}

struct SettingDef {
    key: &'static str,
    kind: SettingKind,
    get: fn(&UiAccess) -> Value,
    set: fn(&UiAccess, &Value) -> Result<(), McpError>,
    apply: fn(&UiAccess),
}

/// 明确拒绝的键。给出原因而不是静默忽略，客户端才知道换个办法。
fn denied_reason(key: &str) -> Option<&'static str> {
    Some(match key {
        "sync.webdav_password" => "secret_not_exposed",
        "sync.webdav_url" => "secret_bearing_endpoint",
        "appearance.renderer_mode" => "restart_required_process",
        "appearance.language" => "requires_i18n_reload",
        "layout.window_width" | "layout.window_height" => "geometry_owned_by_ui",
        "sync.webdav_username" => "secret_bearing_endpoint",
        _ => return None,
    })
}

fn settings_table() -> &'static [SettingDef] {
    static TABLE: OnceLock<Vec<SettingDef>> = OnceLock::new();
    TABLE.get_or_init(|| {
        vec![
            SettingDef {
                key: "terminal.font_size",
                kind: SettingKind::Int { min: 8, max: 32 },
                get: |ui| json!(ui.ctx.store.borrow().font_size()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_font_size(value.as_u64().unwrap_or(13) as u32);
                    Ok(())
                },
                apply: |ui| {
                    ui.window
                        .global::<Theme>()
                        .set_term_font_size(ui.ctx.store.borrow().font_size() as f32);
                },
            },
            SettingDef {
                key: "terminal.bold",
                kind: SettingKind::Bool,
                get: |ui| json!(ui.ctx.store.borrow().terminal_bold()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_terminal_bold(value.as_bool().unwrap_or(false));
                    Ok(())
                },
                apply: |ui| {
                    ui.window
                        .global::<Theme>()
                        .set_term_font_bold(ui.ctx.store.borrow().terminal_bold());
                },
            },
            SettingDef {
                key: "terminal.cursor_style",
                kind: SettingKind::Choice(&["block", "bar", "underline"]),
                get: |ui| json!(ui.ctx.store.borrow().terminal_cursor_style()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_terminal_cursor_style(value.as_str().unwrap_or("bar").to_string());
                    Ok(())
                },
                apply: |ui| {
                    ui.window.set_term_cursor_style(
                        ui.ctx.store.borrow().terminal_cursor_style().into(),
                    );
                },
            },
            SettingDef {
                key: "terminal.cursor_color",
                kind: SettingKind::HexColor,
                get: |ui| json!(ui.ctx.store.borrow().terminal_cursor_color()),
                set: |ui, value| {
                    let color = value.as_str().unwrap_or_default().to_string();
                    if !ui.ctx.store.borrow_mut().set_terminal_cursor_color(&color) {
                        return Err(McpError::Invalid("invalid hex color".to_string()));
                    }
                    Ok(())
                },
                apply: |ui| {
                    terminal_settings::apply_cursor_color(
                        &ui.window,
                        ui.ctx.store.borrow().terminal_cursor_color(),
                    );
                },
            },
            SettingDef {
                key: "terminal.convert_eol",
                kind: SettingKind::Bool,
                get: |ui| json!(ui.ctx.store.borrow().convert_eol()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_convert_eol(value.as_bool().unwrap_or(true));
                    Ok(())
                },
                apply: |ui| {
                    ui.window.set_convert_eol(ui.ctx.store.borrow().convert_eol());
                },
            },
            SettingDef {
                key: "appearance.theme",
                kind: SettingKind::Choice(&["system", "dark", "light"]),
                get: |ui| json!(ui.ctx.store.borrow().theme_pref()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_theme_pref(value.as_str().unwrap_or("system").to_string());
                    Ok(())
                },
                apply: |ui| {
                    let dark = terminal_ui::theme_pref_is_dark(&ui.ctx.store.borrow());
                    terminal_ui::apply_dark_mode(&ui.window, &ui.ctx.bufs, dark);
                    let choice = ui.ctx.store.borrow().accent().to_string();
                    let follow = appearance::apply_accent(&ui.window, &choice);
                    let cursor = ui.ctx.store.borrow().terminal_cursor_color().to_string();
                    terminal_settings::apply_cursor_color_with(&ui.window, &cursor, &follow);
                    let mode = ui.ctx.store.borrow().theme_pref().to_string();
                    ui.window.set_accent_mode(mode.into());
                    settings_window::resync_if_open(&ui.window);
                },
            },
            SettingDef {
                key: "appearance.accent",
                kind: SettingKind::HexColor,
                get: |ui| json!(ui.ctx.store.borrow().accent()),
                set: |ui, value| {
                    let choice = value.as_str().unwrap_or_default().to_string();
                    if !choice.is_empty() && appearance::normalize_accent(&choice).is_none() {
                        return Err(McpError::Invalid(
                            "accent must be \"\", a preset id, or #RRGGBB".to_string(),
                        ));
                    }
                    ui.ctx.store.borrow_mut().set_accent(choice);
                    Ok(())
                },
                apply: |ui| {
                    let choice = ui.ctx.store.borrow().accent().to_string();
                    let follow = appearance::apply_accent(&ui.window, &choice);
                    let cursor = ui.ctx.store.borrow().terminal_cursor_color().to_string();
                    terminal_settings::apply_cursor_color_with(&ui.window, &cursor, &follow);
                    settings_window::resync_if_open(&ui.window);
                },
            },
            SettingDef {
                key: "appearance.ui_scale",
                kind: SettingKind::Int { min: 80, max: 200 },
                get: |ui| json!(ui.ctx.store.borrow().ui_scale()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_ui_scale(value.as_u64().unwrap_or(100) as u32);
                    Ok(())
                },
                apply: |ui| {
                    ui.window
                        .global::<Theme>()
                        .set_ui_scale(ui.ctx.store.borrow().ui_scale() as f32 / 100.0);
                },
            },
            SettingDef {
                key: "appearance.panel_font",
                kind: SettingKind::Int { min: 80, max: 160 },
                get: |ui| json!(ui.ctx.store.borrow().panel_font()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_panel_font(value.as_u64().unwrap_or(100) as u32);
                    Ok(())
                },
                apply: |ui| {
                    ui.window.global::<Theme>().set_panel_font(
                        ui.ctx.store.borrow().panel_font() as f32 / 100.0,
                    );
                },
            },
            SettingDef {
                key: "layout.collapse_sidebar_default",
                kind: SettingKind::Bool,
                get: |ui| json!(ui.ctx.store.borrow().collapse_sidebar_default()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_collapse_sidebar_default(value.as_bool().unwrap_or(false));
                    Ok(())
                },
                apply: |ui| super::settings::layout::apply_layout_prefs(&ui.window, &ui.ctx.store),
            },
            SettingDef {
                key: "layout.collapse_sftp_default",
                kind: SettingKind::Bool,
                get: |ui| json!(ui.ctx.store.borrow().collapse_sftp_default()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_collapse_sftp_default(value.as_bool().unwrap_or(false));
                    Ok(())
                },
                apply: |ui| super::settings::layout::apply_layout_prefs(&ui.window, &ui.ctx.store),
            },
            SettingDef {
                key: "layout.hide_cmd_bar",
                kind: SettingKind::Bool,
                get: |ui| json!(ui.ctx.store.borrow().cmd_bar_hidden()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_cmd_bar_hidden(value.as_bool().unwrap_or(false));
                    Ok(())
                },
                apply: |ui| {
                    ui.window
                        .set_show_cmd_bar(!ui.ctx.store.borrow().cmd_bar_hidden());
                },
            },
            SettingDef {
                key: "transfer.sftp_follow_cd",
                kind: SettingKind::Bool,
                get: |ui| json!(ui.ctx.store.borrow().sftp_follow_cd()),
                set: |ui, value| {
                    let follow = value.as_bool().unwrap_or(true);
                    ui.ctx.store.borrow_mut().set_sftp_follow_cd(follow);
                    ui.ctx.sftp_follow_cd.store(follow, Ordering::Relaxed);
                    Ok(())
                },
                apply: |ui| {
                    ui.window
                        .set_sftp_follow_cd(ui.ctx.store.borrow().sftp_follow_cd());
                },
            },
            SettingDef {
                key: "transfer.download_always_ask",
                kind: SettingKind::Bool,
                get: |ui| json!(ui.ctx.store.borrow().download_always_ask()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_download_always_ask(value.as_bool().unwrap_or(false));
                    Ok(())
                },
                apply: |ui| {
                    ui.window
                        .set_download_always_ask(ui.ctx.store.borrow().download_always_ask());
                },
            },
            SettingDef {
                key: "sync.upload_enabled",
                kind: SettingKind::Bool,
                get: |ui| json!(ui.ctx.store.borrow().sync_upload()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_sync_upload(value.as_bool().unwrap_or(false));
                    Ok(())
                },
                apply: |ui| {
                    ui.window
                        .set_sync_upload_enabled(ui.ctx.store.borrow().sync_upload());
                },
            },
            SettingDef {
                key: "update.check_enabled",
                kind: SettingKind::Bool,
                get: |ui| json!(ui.ctx.store.borrow().update_check_enabled()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_update_check_enabled(value.as_bool().unwrap_or(true));
                    Ok(())
                },
                apply: |ui| {
                    ui.window
                        .set_update_check_enabled(ui.ctx.store.borrow().update_check_enabled());
                },
            },
            SettingDef {
                key: "update.channel",
                kind: SettingKind::Choice(&["stable", "beta", "all"]),
                get: |ui| json!(ui.ctx.store.borrow().update_channel()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_update_channel(value.as_str().unwrap_or("stable").to_string());
                    Ok(())
                },
                apply: |ui| {
                    ui.window
                        .set_update_channel(ui.ctx.store.borrow().update_channel().into());
                },
            },
        ]
    })
}

fn validate(kind: &SettingKind, value: &Value) -> Result<(), &'static str> {
    match kind {
        SettingKind::Bool => value.as_bool().map(|_| ()).ok_or("expected_boolean"),
        SettingKind::Int { min, max } => match value.as_u64() {
            Some(number) if (*min..=*max).contains(&number) => Ok(()),
            _ => Err("out_of_range"),
        },
        SettingKind::Choice(options) => match value.as_str() {
            Some(value) if options.contains(&value) => Ok(()),
            _ => Err("unknown_value"),
        },
        SettingKind::HexColor => match value.as_str() {
            Some("") => Ok(()),
            Some(value)
                if value.len() == 7
                    && value.starts_with('#')
                    && value[1..].chars().all(|ch| ch.is_ascii_hexdigit()) =>
            {
                Ok(())
            }
            _ => Err("expected_hex_color"),
        },
    }
}

/// 读设置。`keys` 为空 = 返回整张白名单。
pub(crate) async fn get_settings(keys: Vec<String>) -> Result<Value, McpError> {
    call(READ_TIMEOUT, move |ui| {
        let table = settings_table();
        let mut values = serde_json::Map::new();
        let mut unknown = Vec::new();
        if keys.is_empty() {
            for def in table {
                values.insert(def.key.to_string(), (def.get)(ui));
            }
        } else {
            for key in &keys {
                match table.iter().find(|def| def.key == key) {
                    Some(def) => {
                        values.insert(key.clone(), (def.get)(ui));
                    }
                    None => unknown.push(key.clone()),
                }
            }
        }
        Ok(json!({ "values": Value::Object(values), "unknown": unknown }))
    })
    .await
}

/// 写设置。事务式：有一个键不合法就整体不写。
pub(crate) async fn set_settings(values: serde_json::Map<String, Value>) -> Result<Value, McpError> {
    call(Duration::from_secs(5), move |ui| {
        let table = settings_table();
        let mut rejected = Vec::new();
        for (key, value) in &values {
            match table.iter().find(|def| def.key == key) {
                Some(def) => {
                    if let Err(reason) = validate(&def.kind, value) {
                        rejected.push(json!({ "key": key, "reason": reason }));
                    }
                }
                None => rejected.push(json!({
                    "key": key,
                    "reason": denied_reason(key).unwrap_or("unknown_key"),
                })),
            }
        }
        if !rejected.is_empty() {
            return Ok(json!({ "applied": [], "rejected": rejected }));
        }

        let mut applied = Vec::new();
        let mut defs: Vec<&SettingDef> = Vec::new();
        for (key, value) in &values {
            if let Some(def) = table.iter().find(|def| def.key == key) {
                (def.set)(ui, value)?;
                defs.push(def);
                applied.push(key.clone());
            }
        }
        // 只触发一次防抖写盘：逐键 save() 会重写整个 sessions.json。
        super::settings::persist(&ui.ctx.store, |_| {});
        for def in defs {
            (def.apply)(ui);
        }
        Ok(json!({ "applied": applied, "rejected": [] }))
    })
    .await
}

// ── 主机增删改 ──────────────────────────────────────────────────────────

/// 新增或修改一个保存的主机。`session_id` 为空 = 新建。
pub(crate) async fn upsert_host(args: Value) -> Result<Value, McpError> {
    call(TAB_TIMEOUT, move |ui| {
        let mut store = ui.ctx.store.borrow_mut();
        let existing = args
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .and_then(|id| store.get(id).cloned());
        let created = existing.is_none();
        let mut session = existing.unwrap_or_else(Session::new_empty);

        if let Some(value) = args.get("name").and_then(Value::as_str) {
            session.name = value.trim().to_string();
        }
        if let Some(value) = args.get("host").and_then(Value::as_str) {
            session.host = value.trim().to_string();
        }
        if let Some(value) = args.get("user").and_then(Value::as_str) {
            session.user = value.trim().to_string();
        }
        if let Some(value) = args.get("port").and_then(Value::as_u64) {
            if !(1..=65535).contains(&value) {
                return Err(McpError::Invalid("`port` must be 1..65535".to_string()));
            }
            session.port = value as u16;
        }
        if let Some(value) = args.get("kind").and_then(Value::as_str) {
            session.kind = SessionKind::from_str(value);
        }
        // 新建且没给端口：按最终类型取出厂值（Telnet 是 23，其余 22）。
        if created && args.get("port").is_none() {
            session.port = if session.kind == SessionKind::Telnet { 23 } else { 22 };
        }
        if let Some(value) = args.get("auth").and_then(Value::as_str) {
            session.auth = AuthMethod::from_str(value);
        }
        if let Some(value) = args.get("password").and_then(Value::as_str)
            && !value.is_empty()
        {
            session.password = Secret::new(value);
        }
        if let Some(value) = args.get("private_key_path").and_then(Value::as_str) {
            session.private_key_path = value.replace('\\', "/");
        }
        // 密钥来源二选一：`key_mode=inline` 时用 private_key_inline，否则用路径。
        if let Some(mode) = args.get("key_mode").and_then(Value::as_str) {
            match mode {
                "inline" => session.private_key_path.clear(),
                "path" => session.private_key_inline = Secret::default(),
                other => {
                    return Err(McpError::Invalid(format!(
                        "`key_mode` must be `path` or `inline`, got `{other}`"
                    )));
                }
            }
        }
        if let Some(value) = args.get("private_key_inline").and_then(Value::as_str)
            && !value.is_empty()
        {
            session.private_key_inline = Secret::new(value);
            session.private_key_path.clear();
        }
        if let Some(value) = args.get("group").and_then(Value::as_str) {
            session.group = value.trim().to_string();
        }
        if let Some(value) = args.get("note").and_then(Value::as_str) {
            session.note = value.to_string();
        }
        if let Some(value) = args.get("jump_session_id").and_then(Value::as_str) {
            session.jump_session_id = value.trim().to_string();
        }
        if let Some(value) = args.get("proxy").and_then(Value::as_str) {
            let value = value.trim();
            if value.is_empty() {
                session.proxy = String::new();
                session.proxy_user = String::new();
                session.proxy_pass = Secret::default();
            } else {
                // 地址里可以带 user:pass：拆到独立字段，密码才能加密落盘。
                let (proxy_type, hostport) = crate::app::split_proxy(value);
                let (url, user, pass) =
                    crate::app::compose_proxy(&proxy_type, &hostport, "", "", None);
                if crate::ssh::proxy::parse(&url).is_none() {
                    return Err(McpError::Invalid(format!(
                        "`proxy` is not a valid proxy URL: {value}"
                    )));
                }
                session.proxy = url;
                if !user.is_empty() {
                    session.proxy_user = user;
                }
                if !pass.is_empty() {
                    session.proxy_pass = pass;
                }
            }
        }
        // 独立字段覆盖 URL 里的部分：给空串表示清除，省略表示保持原样。
        if let Some(value) = args.get("proxy_user").and_then(Value::as_str) {
            session.proxy_user = value.trim().to_string();
        }
        if let Some(value) = args.get("proxy_password").and_then(Value::as_str) {
            session.proxy_pass = Secret::new(value.to_string());
        }
        // ── 高级：编码 / 串口参数 / shell 集成 ──
        if let Some(value) = args.get("encoding").and_then(Value::as_str) {
            let value = value.trim();
            if value.len() > 32 {
                return Err(McpError::Invalid("`encoding` is too long".to_string()));
            }
            session.encoding = value.to_string();
        }
        if let Some(value) = args.get("disable_shell_integration").and_then(Value::as_bool) {
            session.disable_shell_integration = value;
        }
        if let Some(value) = args.get("serial_port").and_then(Value::as_str) {
            session.serial_port = value.trim().to_string();
        }
        if let Some(value) = args.get("baud_rate").and_then(Value::as_u64) {
            if !(300..=4_000_000).contains(&value) {
                return Err(McpError::Invalid("`baud_rate` must be 300..4000000".to_string()));
            }
            session.baud_rate = value as u32;
        }
        if let Some(value) = args.get("data_bits").and_then(Value::as_u64) {
            if value != 7 && value != 8 {
                return Err(McpError::Invalid("`data_bits` must be 7 or 8".to_string()));
            }
            session.data_bits = value as u8;
        }
        if let Some(value) = args.get("stop_bits").and_then(Value::as_u64) {
            if value != 1 && value != 2 {
                return Err(McpError::Invalid("`stop_bits` must be 1 or 2".to_string()));
            }
            session.stop_bits = value as u8;
        }
        if let Some(value) = args.get("parity").and_then(Value::as_str) {
            if !["none", "odd", "even"].contains(&value) {
                return Err(McpError::Invalid(
                    "`parity` must be none/odd/even".to_string(),
                ));
            }
            session.parity = value.to_string();
        }
        if let Some(value) = args.get("flow_control").and_then(Value::as_str) {
            if !["none", "hardware", "software"].contains(&value) {
                return Err(McpError::Invalid(
                    "`flow_control` must be none/hardware/software".to_string(),
                ));
            }
            session.flow_control = value.to_string();
        }

        // ── 高级：端口转发（-L / -R / -D） ──
        if let Some(list) = args.get("forwards").and_then(Value::as_array) {
            let mut forwards = Vec::with_capacity(list.len());
            for item in list {
                let kind = item.get("kind").and_then(Value::as_str).unwrap_or("local");
                if !["local", "remote", "dynamic"].contains(&kind) {
                    return Err(McpError::Invalid(format!(
                        "forward `kind` must be local/remote/dynamic, got `{kind}`"
                    )));
                }
                let bind_port = item.get("bind_port").and_then(Value::as_u64).unwrap_or(0);
                if !(1..=65535).contains(&bind_port) {
                    return Err(McpError::Invalid(
                        "forward `bind_port` must be 1..65535".to_string(),
                    ));
                }
                let host_port = item.get("host_port").and_then(Value::as_u64).unwrap_or(0);
                if host_port > 65535 {
                    return Err(McpError::Invalid(
                        "forward `host_port` must be 0..65535".to_string(),
                    ));
                }
                forwards.push(crate::config::PortForward {
                    kind: kind.to_string(),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                    bind_addr: item
                        .get("bind_addr")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                    bind_port: bind_port as u16,
                    host: item
                        .get("host")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                    host_port: host_port as u16,
                });
            }
            session.forwards = forwards;
        }

        // ── 高级：登录触发器（expect / send） ──
        if let Some(list) = args.get("triggers").and_then(Value::as_array) {
            let mut triggers = Vec::with_capacity(list.len());
            for item in list {
                let expect = item
                    .get("expect")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim();
                if expect.is_empty() {
                    // 空规则在界面上就是"未填的一行"，存下来只会在每次登录时白跑。
                    continue;
                }
                triggers.push(crate::config::SessionTrigger {
                    expect: expect.to_string(),
                    response: Secret::new(
                        item.get("response")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    ),
                    append_enter: item
                        .get("append_enter")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    repeat: item.get("repeat").and_then(Value::as_bool).unwrap_or(false),
                });
            }
            session.triggers = triggers;
        }
        // 按类型校验必填项：SSH/Telnet 要主机，串口要设备名，本地会话都不需要。
        match session.kind {
            SessionKind::Ssh | SessionKind::Telnet => {
                if session.host.is_empty() {
                    return Err(McpError::Invalid("`host` is required".to_string()));
                }
                if session.user.trim().is_empty() {
                    session.user = "root".to_string();
                }
                if session.port == 0 {
                    session.port = if session.kind == SessionKind::Telnet { 23 } else { 22 };
                }
            }
            SessionKind::Serial => {
                if session.serial_port.trim().is_empty() {
                    return Err(McpError::Invalid(
                        "`serial_port` is required for serial hosts".to_string(),
                    ));
                }
            }
            SessionKind::Local => {}
        }
        if session.name.is_empty() {
            session.name = match session.kind {
                SessionKind::Serial => format!("{} @{}", session.serial_port, session.baud_rate),
                SessionKind::Local => "local".to_string(),
                _ if session.user.trim().is_empty() => session.host.clone(),
                _ => format!("{}@{}", session.user, session.host),
            };
        }

        let session_id = session.id.clone();
        store.upsert(session);
        store.save_logging();
        drop(store);
        super::sync_sessions_for_window(
            &ui.window.as_weak(),
            &ui.ctx.store.borrow(),
            &ui.ctx.sessions_model,
        );
        Ok(json!({
            "session_id": session_id,
            "created": created,
            "name": ui.ctx.store.borrow().get(&session_id).map(|s| s.name.clone()),
        }))
    })
    .await
}

/// 删除一个保存的主机。`close_open_tabs=true` 时把指向它的标签页一并关掉。
pub(crate) async fn delete_host(session_id: String, close_open_tabs: bool) -> Result<Value, McpError> {
    call(TAB_TIMEOUT, move |ui| {
        {
            let mut store = ui.ctx.store.borrow_mut();
            if store.get(&session_id).is_none() {
                return Err(McpError::Invalid(format!(
                    "unknown session_id: {session_id}"
                )));
            }
            store.remove(&session_id);
            store.save_logging();
        }
        let open_tabs: Vec<String> = {
            let statuses = ui
                .ctx
                .tab_statuses
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            statuses
                .iter()
                .filter(|(_, status)| status.session_id == session_id)
                .map(|(tab_id, _)| tab_id.clone())
                .collect()
        };
        if close_open_tabs {
            for tab_id in &open_tabs {
                let env = ui.tab_env();
                tab_ops::close_tab(&env, tab_id);
            }
        }
        super::sync_sessions_for_window(
            &ui.window.as_weak(),
            &ui.ctx.store.borrow(),
            &ui.ctx.sessions_model,
        );
        Ok(json!({
            "deleted": true,
            "session_id": session_id,
            "open_tabs": open_tabs,
        }))
    })
    .await
}

/// 打开一个已保存（或内置 `system:`）会话，返回新标签页。
///
/// `trust_unknown_host_key` 只对**首次连接**的主机生效：直接记录密钥并放行。
/// 密钥与已保存值不一致时仍然弹窗（可能是中间人），MCP 无法越过这一步。
pub(crate) async fn open_session(
    session_id: String,
    trust_unknown_host_key: bool,
) -> Result<Value, McpError> {
    call(TAB_TIMEOUT, move |ui| {
        let session = {
            let store = ui.ctx.store.borrow();
            if session_id.starts_with("system:") {
                builtin_local_sessions(store.wsl_profiles())
                    .into_iter()
                    .find(|session| session.id == session_id)
            } else {
                store.get(&session_id).cloned()
            }
        }
        .ok_or_else(|| McpError::Invalid(format!("unknown session_id: {session_id}")))?;

        let mut connect_ctx = ConnectCtx::from_app(ui.window.as_weak(), &ui.ctx);
        connect_ctx.trust_unknown_host_key = trust_unknown_host_key;
        let env = ui.tab_env();
        let tab_id = tab_ops::open_session_tab(&env, &connect_ctx, session);
        Ok(json!({
            "tab_id": tab_id,
            "session_id": session_id,
            "state": "connecting",
            "host_key": if trust_unknown_host_key { "trust_unknown" } else { "prompt_if_unknown" },
        }))
    })
    .await
}

/// 关掉一个标签页。与用户点关闭按钮走同一条清理路径。
pub(crate) async fn close_session(tab_id: String) -> Result<Value, McpError> {
    call(TAB_TIMEOUT, move |ui| {
        let env = ui.tab_env();
        if tab_ops::close_tab(&env, &tab_id) {
            Ok(json!({ "closed": true, "tab_id": tab_id }))
        } else {
            Err(McpError::Invalid(format!("unknown tab_id: {tab_id}")))
        }
    })
    .await
}

/// 把文本写进会话的 PTY（等同用户打字，界面上可见）。
pub(crate) async fn send_input(
    tab_id: String,
    text: String,
    enter: bool,
) -> Result<Value, McpError> {
    call(READ_TIMEOUT, move |ui| {
        let mut bytes = text.into_bytes();
        // 终端里换行要发 CR；已经以换行结尾时不再补一个，避免多发一次空回车。
        if enter && !bytes.ends_with(b"\n") && !bytes.ends_with(b"\r") {
            bytes.push(b'\r');
        }
        let sent = bytes.len();
        let handles = ui.ctx.handles.borrow();
        let Some(handle) = handles.get(&tab_id) else {
            return Err(McpError::Invalid(format!("unknown tab_id: {tab_id}")));
        };
        handle.send_raw(bytes);
        Ok(json!({ "tab_id": tab_id, "sent_bytes": sent }))
    })
    .await
}

/// 在会话的连接上排一条 exec 命令，**不在这里等结果** —— 等结果会占住 UI 线程。
/// 返回的 `oneshot` 由调用方（tokio 线程）await。
pub(crate) async fn ssh_exec(
    tab_id: String,
    command: String,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<tokio::sync::oneshot::Receiver<crate::ssh::ExecResult>, McpError> {
    call(READ_TIMEOUT, move |ui| {
        let handles = ui.ctx.handles.borrow();
        let Some(handle) = handles.get(&tab_id) else {
            return Err(McpError::Invalid(format!("unknown tab_id: {tab_id}")));
        };
        Ok(handle.exec(command, timeout, max_output_bytes))
    })
    .await
}

fn session_row(session: &Session) -> Value {
    json!({
        "session_id": session.id,
        "name": session.name,
        "kind": session.kind.as_str(),
        "host": session.host,
        "port": session.port,
        "user": session.user,
        "group": session.group,
        "auth": session.auth.as_str(),
        "has_password": !session.password.is_empty(),
        "has_key": !session.private_key_path.is_empty() || !session.private_key_inline.is_empty(),
        // 代理 URL 里可能带口令，只报告有没有配；用户名可回读，密码不回。
        "has_proxy": !session.proxy.is_empty(),
        "proxy_user": session.proxy_user,
        "has_proxy_password": !session.proxy_pass.is_empty(),
        // 高级项：读回来是为了让 AI 在改主机前先看到现状（写会整体替换）。
        "encoding": session.encoding,
        "disable_shell_integration": session.disable_shell_integration,
        "serial": {
            "port": session.serial_port,
            "baud_rate": session.baud_rate,
            "data_bits": session.data_bits,
            "stop_bits": session.stop_bits,
            "parity": session.parity,
            "flow_control": session.flow_control,
        },
        "forwards": session
            .forwards
            .iter()
            .map(|forward| json!({
                "kind": forward.kind,
                "name": forward.name,
                "bind_addr": forward.bind_addr,
                "bind_port": forward.bind_port,
                "host": forward.host,
                "host_port": forward.host_port,
            }))
            .collect::<Vec<_>>(),
        // 触发器的 response 与口令同级，只报告有没有配。
        "triggers": session
            .triggers
            .iter()
            .map(|trigger| json!({
                "expect": trigger.expect,
                "append_enter": trigger.append_enter,
                "repeat": trigger.repeat,
                "has_response": !trigger.response.is_empty(),
            }))
            .collect::<Vec<_>>(),
        "jump_session_id": session.jump_session_id,
    })
}
