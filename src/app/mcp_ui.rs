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
    bind_consent(window);
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
    let started = std::time::Instant::now();
    let (tx, rx) = tokio::sync::oneshot::channel();
    slint::invoke_from_event_loop(move || {
        let out = UI.with(|slot| match slot.borrow().as_ref() {
            Some(access) => f(access),
            None => Err(McpError::UiUnavailable),
        });
        let _ = tx.send(out);
    })
    .map_err(|_| McpError::UiUnavailable)?;
    let out = match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(McpError::UiUnavailable),
        Err(_) => Err(McpError::UiTimeout),
    };
    // 记进本次调用的"UI 等待"累计（审计里的 `ui_ms`）。
    crate::mcp::add_ui_wait(started.elapsed().as_millis() as u64);
    out
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

/// 传输管理配置的下载目录。MCP 的 `sftp_download` 固定写这里。
pub(crate) async fn download_dir() -> Result<std::path::PathBuf, McpError> {
    call(READ_TIMEOUT, |ui| {
        let dir = ui.ctx.store.borrow().download_dir().to_string();
        if dir.trim().is_empty() {
            return Err(McpError::Invalid(
                "download directory is not configured".to_string(),
            ));
        }
        Ok(std::path::PathBuf::from(dir))
    })
    .await
}

/// 把一次授权变更写回"按客户端记住"的配置，并刷新 MCP 配置快照。
pub(crate) async fn remember_client_grant(update: crate::mcp::GrantUpdate) -> bool {
    let Some(client_id) = update.client_id else {
        return false;
    };
    let granted = update.granted;
    call(READ_TIMEOUT, move |ui| {
        super::settings::persist(&ui.ctx.store, move |store| {
            store.upsert_mcp_client_grant(&client_id, granted);
        });
        super::settings::ai::apply(&ui.ctx.store);
        Ok(true)
    })
    .await
    .unwrap_or(false)
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
    /// 自由文本。长度与格式约束放在 `extra_check` 里 —— 有些键的上界要读配置
    /// （例如回滚行数取决于"大回滚"开关），静态枚举表达不了。
    Text,
    /// 对象 / 数组：规则表、WSL 配置这类**整表替换**的键。形状校验同样在
    /// `extra_check`，这里只要求是 JSON 对象或数组。
    Structured,
}

struct SettingDef {
    key: &'static str,
    kind: SettingKind,
    get: fn(&UiAccess) -> Value,
    set: fn(&UiAccess, &Value) -> Result<(), McpError>,
    /// 部分键的界面是"动作"而不是"属性"（例如壁纸项要同时改深浅档、换肤、
    /// 重置主题色与光标），所以 apply 也拿得到刚写入的值。
    apply: fn(&UiAccess, &Value),
}

/// 明确拒绝的键。给出原因而不是静默忽略，客户端才知道换个办法。
fn denied_reason(key: &str) -> Option<&'static str> {
    Some(match key {
        "sync.webdav_password" => "secret_not_exposed",
        "appearance.renderer_mode" => "restart_required_process",
        "appearance.language" => "requires_i18n_reload",
        "layout.window_width" | "layout.window_height" => "geometry_owned_by_ui",
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
                apply: |ui, _| {
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
                apply: |ui, _| {
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
                apply: |ui, _| {
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
                apply: |ui, _| {
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
                apply: |ui, _| {
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
                apply: |ui, _| {
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
                apply: |ui, _| {
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
                apply: |ui, _| {
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
                apply: |ui, _| {
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
                apply: |ui, _| super::settings::layout::apply_layout_prefs(&ui.window, &ui.ctx.store),
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
                apply: |ui, _| super::settings::layout::apply_layout_prefs(&ui.window, &ui.ctx.store),
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
                apply: |ui, _| {
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
                apply: |ui, _| {
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
                apply: |ui, _| {
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
                apply: |ui, _| {
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
                apply: |ui, _| {
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
                apply: |ui, _| {
                    ui.window
                        .set_update_channel(ui.ctx.store.borrow().update_channel().into());
                },
            },
            // ── 终端页其余项 ────────────────────────────────────────────
            SettingDef {
                key: "terminal.font_family",
                kind: SettingKind::Text,
                get: |ui| json!(ui.ctx.store.borrow().font_family()),
                set: |ui, value| {
                    ui.ctx.store.borrow_mut().set_font_family(
                        value.as_str().unwrap_or_default().trim().to_string(),
                    );
                    Ok(())
                },
                apply: |ui, _| {
                    let family = ui.ctx.store.borrow().font_family().to_string();
                    ui.window
                        .global::<Theme>()
                        .set_term_font_family(family.clone().into());
                    ui.window
                        .global::<Theme>()
                        .set_term_font_cjk(super::fonts_ui::term_font_covers_cjk(&family));
                    // 选中项跟着走：设置窗 1 Hz 会被主窗口的索引覆盖，不同步的话
                    // 界面显示与实际字体不一致（看着像没改成功）。
                    let catalog = font_catalog();
                    ui.window.set_term_font_index(catalog.term_index(&family));
                },
            },
            SettingDef {
                key: "terminal.scrollback_lines",
                kind: SettingKind::Int { min: 100, max: 1_000_000 },
                get: |ui| json!(ui.ctx.store.borrow().scrollback_lines()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_scrollback_lines(value.as_u64().unwrap_or(5_000) as usize);
                    Ok(())
                },
                apply: |ui, _| {
                    let lines = ui.ctx.store.borrow().scrollback_lines();
                    ui.window.set_scrollback_lines(lines.to_string().into());
                },
            },
            SettingDef {
                key: "terminal.osc52",
                kind: SettingKind::Bool,
                get: |ui| json!(ui.ctx.store.borrow().osc52_clipboard()),
                set: |ui, value| {
                    let enabled = value.as_bool().unwrap_or(false);
                    ui.ctx.store.borrow_mut().set_osc52_clipboard(enabled);
                    // 剪贴板那侧读的是原子量，不只读配置。
                    crate::terminal::vt_adapter::OSC52_ENABLED.store(enabled, Ordering::Relaxed);
                    Ok(())
                },
                apply: |ui, _| {
                    ui.window
                        .set_osc52_clipboard(ui.ctx.store.borrow().osc52_clipboard());
                },
            },
            SettingDef {
                key: "terminal.json_format",
                kind: SettingKind::Bool,
                get: |ui| json!(ui.ctx.store.borrow().json_format_output()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_json_format_output(value.as_bool().unwrap_or(true));
                    Ok(())
                },
                apply: |ui, _| {
                    let enabled = ui.ctx.store.borrow().json_format_output();
                    // 已在跑的标签页也要立刻换行为，否则要重连才生效。
                    for buffer in ui
                        .ctx
                        .bufs
                        .lock()
                        .unwrap_or_else(|err| err.into_inner())
                        .values()
                    {
                        buffer
                            .lock()
                            .unwrap_or_else(|err| err.into_inner())
                            .json_format_output = enabled;
                    }
                    ui.window.set_json_format_output(enabled);
                },
            },
            // ── 布局页其余项 ────────────────────────────────────────────
            SettingDef {
                key: "layout.quick_commands_sidebar",
                kind: SettingKind::Bool,
                get: |ui| json!(ui.ctx.store.borrow().quick_commands_as_sidebar()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_quick_commands_as_sidebar(value.as_bool().unwrap_or(false));
                    Ok(())
                },
                apply: |ui, _| {
                    // 复用界面那条：它还要把快捷面板的开关一起同步过去。
                    let enabled = ui.ctx.store.borrow().quick_commands_as_sidebar();
                    super::settings::layout::apply_quick_commands_as_sidebar(
                        &ui.window,
                        &ui.ctx.store,
                        enabled,
                    );
                },
            },
            SettingDef {
                key: "layout.welcome_sidebar",
                kind: SettingKind::Bool,
                get: |ui| json!(ui.ctx.store.borrow().welcome_as_sidebar()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_welcome_as_sidebar(value.as_bool().unwrap_or(false));
                    Ok(())
                },
                apply: |ui, _| {
                    let enabled = ui.ctx.store.borrow().welcome_as_sidebar();
                    // 走界面回调：它内部把 Welcome 子树延迟一帧重建（#323），
                    // 自己只改属性会漏掉这一步。
                    ui.window.invoke_set_welcome_as_sidebar(enabled);
                },
            },
            // ── 更新页其余项 ────────────────────────────────────────────
            SettingDef {
                key: "update.frequency",
                kind: SettingKind::Choice(&crate::config::UPDATE_FREQUENCIES),
                get: |ui| json!(ui.ctx.store.borrow().update_frequency()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_update_frequency(value.as_str().unwrap_or("daily").to_string());
                    Ok(())
                },
                apply: |ui, _| {
                    let current = ui.ctx.store.borrow().update_frequency().to_string();
                    let index = crate::config::UPDATE_FREQUENCIES
                        .iter()
                        .position(|value| *value == current)
                        .unwrap_or(0) as i32;
                    ui.window.set_update_freq_index(index);
                },
            },
            SettingDef {
                key: "update.proxy_enabled",
                kind: SettingKind::Bool,
                get: |ui| json!(ui.ctx.store.borrow().update_proxy_enabled()),
                set: |ui, value| {
                    ui.ctx
                        .store
                        .borrow_mut()
                        .set_update_proxy_enabled(value.as_bool().unwrap_or(false));
                    Ok(())
                },
                apply: |ui, _| {
                    // 更新器读的是全局快照，改完配置得推一次。
                    super::settings::update::apply_net(&ui.window, &ui.ctx.store, None);
                },
            },
            SettingDef {
                key: "update.proxy",
                kind: SettingKind::Text,
                get: |ui| json!(ui.ctx.store.borrow().update_proxy()),
                set: |ui, value| {
                    ui.ctx.store.borrow_mut().set_update_proxy(
                        value.as_str().unwrap_or_default().trim().to_string(),
                    );
                    Ok(())
                },
                apply: |ui, _| {
                    let proxy = ui.ctx.store.borrow().update_proxy().to_string();
                    super::settings::update::apply_net(&ui.window, &ui.ctx.store, Some(&proxy));
                },
            },
            // ── 同步页（WebDAV）：口令仍不开放，其余三项可写 ─────────────
            SettingDef {
                key: "sync.webdav_url",
                kind: SettingKind::Text,
                get: |ui| json!(ui.ctx.store.borrow().webdav_url()),
                set: |ui, value| {
                    webdav_update(
                        ui,
                        Some(value.as_str().unwrap_or_default().to_string()),
                        None,
                        None,
                    )
                },
                apply: |ui, _| {
                    ui.window
                        .set_webdav_url(ui.ctx.store.borrow().webdav_url().into());
                },
            },
            SettingDef {
                key: "sync.webdav_username",
                kind: SettingKind::Text,
                get: |ui| json!(ui.ctx.store.borrow().webdav_username()),
                set: |ui, value| {
                    webdav_update(
                        ui,
                        None,
                        Some(value.as_str().unwrap_or_default().to_string()),
                        None,
                    )
                },
                apply: |ui, _| {
                    ui.window
                        .set_webdav_username(ui.ctx.store.borrow().webdav_username().into());
                },
            },
            SettingDef {
                key: "sync.webdav_remote_path",
                kind: SettingKind::Text,
                get: |ui| json!(ui.ctx.store.borrow().webdav_remote_path()),
                set: |ui, value| {
                    webdav_update(
                        ui,
                        None,
                        None,
                        Some(value.as_str().unwrap_or_default().to_string()),
                    )
                },
                apply: |ui, _| {
                    ui.window.set_webdav_remote_path(
                        ui.ctx.store.borrow().webdav_remote_path().into(),
                    );
                },
            },
            // ── 外观：壁纸与界面字体 ──────────────────────────────────
            SettingDef {
                key: "appearance.wallpaper",
                kind: SettingKind::Text,
                get: |ui| {
                    let store = ui.ctx.store.borrow();
                    match store.wallpaper().strip_prefix("builtin:") {
                        // 三档内置背景图直接报档位；上传的图片报当前深浅档。
                        Some(mode) => json!(mode),
                        None => json!(store.theme_pref()),
                    }
                },
                // 写入交给 apply：换壁纸是"动作"，要同时改深浅档、换肤、重算主题色
                // 与光标色（见 on_set_wallpaper），这里重复一遍必然分叉。
                set: |_, _| Ok(()),
                apply: |ui, value| {
                    let mode = value.as_str().unwrap_or("system");
                    let label = super::settings::appearance::wallpaper_choices()
                        .into_iter()
                        .find(|choice| {
                            if matches!(mode, "system" | "dark" | "light") {
                                choice.theme == mode && choice.wallpaper.is_empty()
                            } else {
                                choice.theme.is_empty() && choice.wallpaper == mode
                            }
                        })
                        .map(|choice| choice.label);
                    if let Some(label) = label {
                        ui.window.invoke_set_wallpaper(label.into());
                    }
                },
            },
            SettingDef {
                key: "appearance.ui_font",
                kind: SettingKind::Text,
                get: |ui| json!(ui.ctx.store.borrow().ui_font_family()),
                set: |ui, value| {
                    ui.ctx.store.borrow_mut().set_ui_font_family(
                        value.as_str().unwrap_or_default().trim().to_string(),
                    );
                    Ok(())
                },
                apply: |ui, _| {
                    ui.window
                        .global::<Theme>()
                        .set_ui_font_family(super::fonts_ui::resolve_ui_font_family());
                    let stored = ui.ctx.store.borrow().ui_font_family().to_string();
                    let catalog = font_catalog();
                    ui.window.set_ui_font_index(catalog.ui_index(&stored));
                },
            },
            // ── 输出高亮：开关 / 预设 / 自定义规则（整表替换）────────────
            SettingDef {
                key: "terminal.output_highlight",
                kind: SettingKind::Structured,
                get: |ui| {
                    let store = ui.ctx.store.borrow();
                    json!({
                        "enabled": store.output_highlight_enabled(),
                        "preset": store.output_highlight_preset(),
                        "rules": store.output_highlight_rules(),
                    })
                },
                set: |ui, value| {
                    let object = value.as_object().ok_or_else(|| {
                        McpError::Invalid("`terminal.output_highlight` must be an object".into())
                    })?;
                    let enabled = object
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    let preset = object
                        .get("preset")
                        .and_then(Value::as_str)
                        .unwrap_or("builtin")
                        .to_string();
                    let rules = match object.get("rules") {
                        Some(rules) => Some(
                            serde_json::from_value::<Vec<crate::config::OutputHighlightRule>>(
                                rules.clone(),
                            )
                            .map_err(|err| {
                                McpError::Invalid(format!("invalid output highlight rules: {err}"))
                            })?,
                        ),
                        None => None,
                    };
                    let mut store = ui.ctx.store.borrow_mut();
                    store.set_output_highlight_enabled(enabled);
                    store.set_output_highlight_preset(preset);
                    if let Some(rules) = rules {
                        store.set_output_highlight_rules(rules);
                    }
                    Ok(())
                },
                apply: |ui, _| {
                    let (enabled, preset, rules) = {
                        let store = ui.ctx.store.borrow();
                        (
                            store.output_highlight_enabled(),
                            store.output_highlight_preset().to_string(),
                            store.output_highlight_rules().to_vec(),
                        )
                    };
                    super::terminal_ui::apply_output_highlight(
                        &ui.window,
                        &ui.ctx.bufs,
                        enabled,
                        &preset,
                    );
                    super::terminal_ui::apply_custom_output_rules(
                        &ui.window,
                        &ui.ctx.bufs,
                        &rules,
                    );
                },
            },
            // ── WSL 启动项（整表替换）──────────────────────────────────
            SettingDef {
                key: "wsl.profiles",
                kind: SettingKind::Structured,
                get: |ui| json!(ui.ctx.store.borrow().wsl_profiles()),
                set: |ui, value| {
                    let items = value.as_array().ok_or_else(|| {
                        McpError::Invalid("`wsl.profiles` must be an array".into())
                    })?;
                    let existing = ui.ctx.store.borrow().wsl_profiles().to_vec();
                    let mut profiles = Vec::with_capacity(items.len());
                    for item in items {
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        if name.is_empty() {
                            return Err(McpError::Invalid(
                                "wsl profile `name` is required".to_string(),
                            ));
                        }
                        // 带 id 就沿用（更新），否则按同名匹配旧条目，最后才新建。
                        let id = item
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|id| !id.is_empty())
                            .map(str::to_string)
                            .or_else(|| {
                                existing
                                    .iter()
                                    .find(|profile| profile.name == name)
                                    .map(|profile| profile.id.clone())
                            })
                            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                        profiles.push(crate::config::WslProfile {
                            id,
                            name,
                            distribution: item
                                .get("distribution")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .trim()
                                .to_string(),
                            directory: item
                                .get("directory")
                                .and_then(Value::as_str)
                                .unwrap_or("~")
                                .to_string(),
                        });
                    }
                    ui.ctx.store.borrow_mut().set_wsl_profiles(profiles);
                    Ok(())
                },
                apply: |ui, _| {
                    let model = super::session_models::wsl_profile_model(&ui.ctx.store.borrow());
                    ui.window.set_wsl_profiles(model);
                    // 设置窗口拿到的是独立副本，不同步就会显示旧列表。
                    super::settings_window::resync_if_open(&ui.window);
                },
            },
        ]
    })
}

/// WebDAV 没有单项 setter：读齐其余字段后整组写回，只替换传进来的那一项。
fn webdav_update(
    ui: &UiAccess,
    url: Option<String>,
    username: Option<String>,
    remote_path: Option<String>,
) -> Result<(), McpError> {
    let mut store = ui.ctx.store.borrow_mut();
    let enabled = store.webdav_enabled();
    let url = url.unwrap_or_else(|| store.webdav_url().to_string());
    let username = username.unwrap_or_else(|| store.webdav_username().to_string());
    // 口令不在白名单里，但整组写回时必须带上，否则会被空串覆盖掉。
    let password = store.webdav_password().to_string();
    let remote_path = remote_path.unwrap_or_else(|| store.webdav_remote_path().to_string());
    let certs = store.webdav_accept_invalid_certs();
    store.set_webdav_settings(enabled, url, username, password, remote_path, certs);
    Ok(())
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
        SettingKind::Text => value.as_str().map(|_| ()).ok_or("expected_string"),
        SettingKind::Structured => {
            if value.is_object() || value.is_array() {
                Ok(())
            } else {
                Err("expected_object_or_array")
            }
        }
    }
}

/// 依赖当前配置的上界 / 禁止携带凭据的键，放这里统一前置校验。
///
/// 必须在写入前判断：`set_settings` 是"全部合法才写"，等执行 `set` 时才发现
/// 不合法，前面几个键已经落库了。
fn extra_check(ui: &UiAccess, key: &str, value: &Value) -> Result<(), &'static str> {
    match key {
        "terminal.font_family" => {
            let family = value.as_str().unwrap_or_default().trim();
            if family.is_empty() || family.len() > 64 {
                return Err("invalid_font_family");
            }
        }
        "terminal.scrollback_lines" => {
            let max = if ui.ctx.store.borrow().large_scrollback() {
                crate::config::SCROLLBACK_MAX_LARGE
            } else {
                crate::config::SCROLLBACK_MAX
            };
            match value.as_u64() {
                Some(lines) if (100..=max as u64).contains(&lines) => {}
                _ => return Err("out_of_range"),
            }
        }
        // URL 里嵌口令的话会明文躺进 settings.json：只允许在用户名 / 密码字段里给。
        "sync.webdav_url" if url_has_userinfo(value.as_str().unwrap_or_default()) => {
            return Err("credentials_not_allowed_in_url");
        }
        "appearance.wallpaper" => {
            let value = value.as_str().unwrap_or_default();
            if !matches!(value, "system" | "dark" | "light") {
                // 其余只可能是已上传壁纸的 id（列表里 theme 为空的那几项）。
                let known = super::settings::appearance::wallpaper_choices()
                    .iter()
                    .any(|choice| choice.theme.is_empty() && choice.wallpaper == value);
                if !known {
                    return Err("unknown_wallpaper");
                }
            }
        }
        "appearance.ui_font" => {
            let family = value.as_str().unwrap_or_default().trim();
            if family.len() > 64 {
                return Err("invalid_font_family");
            }
            if !family.is_empty() && !ui_font_exists(family) {
                return Err("unknown_font_family");
            }
        }
        "terminal.output_highlight" => {
            let Some(object) = value.as_object() else {
                return Err("expected_object");
            };
            if let Some(preset) = object.get("preset").and_then(Value::as_str)
                && !matches!(preset, "builtin" | "devops" | "log")
            {
                return Err("unknown_value");
            }
            if let Some(rules) = object.get("rules") {
                let Some(items) = rules.as_array() else {
                    return Err("expected_array");
                };
                if items.len() > 200 {
                    return Err("too_many_rules");
                }
                for item in items {
                    let pattern = item.get("pattern").and_then(Value::as_str).unwrap_or("");
                    if pattern.trim().is_empty() {
                        return Err("invalid_rule_pattern");
                    }
                }
            }
        }
        "wsl.profiles" => {
            let Some(items) = value.as_array() else {
                return Err("expected_array");
            };
            if items.len() > 64 {
                return Err("too_many_profiles");
            }
            for item in items {
                let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                if name.trim().is_empty() {
                    return Err("invalid_profile_name");
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// 字体选择器用的目录：与 `settings_ui` 播种时同一套 `FontCatalog` 规则，
/// 保证 MCP 写入后的选中项与界面自己算出来的一致。
fn font_catalog() -> super::settings::FontCatalog {
    let external = crate::fonts::load_external_fonts(&crate::fonts::external_fonts_dirs());
    let (_, term) = super::fonts_ui::font_choices(&external, true);
    let (_, mut ui) = super::fonts_ui::font_choices(&external, false);
    // 界面字体列表最前面是「跟随系统（自动）」= 空串。
    ui.insert(0, super::fonts_ui::FontEntry::Auto);
    super::settings::FontCatalog {
        term: Rc::new(term),
        ui: Rc::new(ui),
    }
}

/// 界面字体是否存在于可选列表（内嵌 + 外置 + 系统字体）。
fn ui_font_exists(family: &str) -> bool {
    let external = crate::fonts::load_external_fonts(&crate::fonts::external_fonts_dirs());
    let (_, entries) = super::fonts_ui::font_choices(&external, false);
    entries.iter().any(|entry| match entry {
        super::fonts_ui::FontEntry::Family(name) => name == family,
        _ => false,
    })
}

/// `scheme://user:pass@host/...` 里的 authority 是否带 userinfo。
fn url_has_userinfo(url: &str) -> bool {
    url.split_once("://")
        .and_then(|(_, rest)| rest.split('/').next())
        .is_some_and(|authority| authority.contains('@'))
}

/// 读设置。`keys` 为空 = 返回整张白名单。
///
/// `allow_high == false` 时，9 个高风险键不进 `values`，改列在 `hidden` 里。
pub(crate) async fn get_settings(
    keys: Vec<String>,
    allow_high: bool,
) -> Result<Value, McpError> {
    call(READ_TIMEOUT, move |ui| {
        let table = settings_table();
        let mut values = serde_json::Map::new();
        let mut unknown = Vec::new();
        let mut hidden = Vec::new();
        if keys.is_empty() {
            for def in table {
                if !allow_high && crate::mcp::is_high_risk_setting(def.key) {
                    hidden.push(def.key.to_string());
                } else {
                    values.insert(def.key.to_string(), (def.get)(ui));
                }
            }
        } else {
            for key in &keys {
                match table.iter().find(|def| def.key == key) {
                    Some(def) => {
                        if !allow_high && crate::mcp::is_high_risk_setting(def.key) {
                            hidden.push(key.clone());
                        } else {
                            values.insert(key.clone(), (def.get)(ui));
                        }
                    }
                    None => unknown.push(key.clone()),
                }
            }
        }
        Ok(json!({
            "values": Value::Object(values),
            "unknown": unknown,
            "hidden": hidden,
        }))
    })
    .await
}

/// 写设置。事务式：有一个键不合法就整体不写。
pub(crate) async fn set_settings(
    values: serde_json::Map<String, Value>,
    allow_high: bool,
) -> Result<Value, McpError> {
    call(Duration::from_secs(5), move |ui| {
        let table = settings_table();
        let mut rejected = Vec::new();
        for (key, value) in &values {
            // 高风险键额外要 settings_high；工具层再挡一次，防绕过授权门。
            if !allow_high && crate::mcp::is_high_risk_setting(key) {
                rejected.push(json!({
                    "key": key,
                    "reason": "permission_denied",
                    "hint": "settings_high is required for this key",
                }));
                continue;
            }
            match table.iter().find(|def| def.key == key) {
                Some(def) => {
                    if let Err(reason) = validate(&def.kind, value)
                        .and_then(|_| extra_check(ui, key, value))
                    {
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
        let mut defs: Vec<(&SettingDef, &Value)> = Vec::new();
        for (key, value) in &values {
            if let Some(def) = table.iter().find(|def| def.key == key) {
                (def.set)(ui, value)?;
                defs.push((def, value));
                applied.push(key.clone());
            }
        }
        // 只触发一次防抖写盘：逐键 save() 会重写整个 sessions.json。
        super::settings::persist(&ui.ctx.store, |_| {});
        for (def, value) in defs {
            (def.apply)(ui, value);
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
                // 只认这四种 scheme：`ftp://x:1` 这种会被 `parse` 当成主机名放过去，
                // 存下来只会到连接时才报一个莫名其妙的错。
                let lower = value.to_ascii_lowercase();
                if let Some((scheme, _)) = lower.split_once("://")
                    && !matches!(scheme, "socks5" | "socks5h" | "socks" | "http")
                {
                    return Err(McpError::Invalid(format!(
                        "`proxy` scheme is not supported: {scheme}:// (use socks5:// or http://)"
                    )));
                }
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

/// MCP 的动作类调用：`check_update` / `webdav_upload` / `webdav_download`。
/// 这三件事在界面上就是回调，这里只负责触发并把"已开始"回给客户端 ——
/// 进度和结果落在状态栏 / 设置页，不在这份返回值里。
pub(crate) async fn run_action(action: String) -> Result<Value, McpError> {
    call(READ_TIMEOUT, move |ui| {
        match action.as_str() {
            "check_update" => ui.window.invoke_check_update_now(),
            "webdav_upload" => ui.window.invoke_webdav_upload(),
            "webdav_download" => ui.window.invoke_webdav_download(),
            other => {
                return Err(McpError::Invalid(format!(
                    "unknown action: {other} (expected check_update / webdav_upload / webdav_download)"
                )));
            }
        }
        Ok(json!({ "started": true, "action": action }))
    })
    .await
}


// ── 会话授权弹窗 ────────────────────────────────────────────────────────
//
// 两段式桥接：MCP 线程登记一个 oneshot → 请 UI 线程开窗 → 等用户点。
// 与 `call()` 的区别是"闭包不同步执行"，所以不能用同一个 helper。

use std::sync::atomic::AtomicU64;
use std::sync::Mutex;

use slint::{Model as _, ModelRc, SharedString, VecModel};

use crate::mcp::Permissions;
use crate::ui::McpConsentRow;

/// 授权弹窗要展示的内容（全部为 `Send` 数据，可跨线程）。
pub(crate) struct ConsentRequest {
    pub(crate) client: String,
    /// 该客户端上次记住的授权，用来预勾。
    pub(crate) base: Permissions,
}

type ConsentSender = tokio::sync::oneshot::Sender<Result<Permissions, McpError>>;

static CONSENT_REPLY: Mutex<Option<ConsentSender>> = Mutex::new(None);
static CONSENT_SEQ: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// 弹窗里的模型句柄：只在本线程用（`Rc`）。
    static CONSENT_ROWS: RefCell<Option<Rc<VecModel<McpConsentRow>>>> = const { RefCell::new(None) };
}

fn bind_consent(window: &AppWindow) {
    {
        let weak = window.as_weak();
        window.on_mcp_consent_toggle(move |key| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let model = window.get_mcp_consent_rows();
            for index in 0..model.row_count() {
                let Some(row) = model.row_data(index) else {
                    continue;
                };
                if row.key == key {
                    let mut updated = row.clone();
                    updated.checked = !updated.checked;
                    model.set_row_data(index, updated);
                    break;
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_mcp_consent_accept(move || {
            if let Some(window) = weak.upgrade() {
                finish_consent(&window, true);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_mcp_consent_reject(move || {
            if let Some(window) = weak.upgrade() {
                finish_consent(&window, false);
            }
        });
    }
}

/// 用户点了按钮：把勾选收成权限位，关窗，唤醒等待方。
fn finish_consent(window: &AppWindow, accept: bool) {
    let mut granted = Permissions::default();
    if accept {
        let model = window.get_mcp_consent_rows();
        for row in model.iter() {
            if row.allowed && row.checked {
                let _ = granted.set(row.key.as_str(), true);
            }
        }
    }
    window.set_mcp_consent_open(false);
    CONSENT_ROWS.with(|slot| *slot.borrow_mut() = None);
    let sender = CONSENT_REPLY
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .take();
    if let Some(sender) = sender {
        let _ = sender.send(if accept {
            Ok(granted)
        } else {
            Err(McpError::Revoked)
        });
    }
}

/// 请用户批准本次接入。返回勾选出来的权限位。
pub(crate) async fn ask_consent(
    request: ConsentRequest,
    timeout: Duration,
) -> Result<Permissions, McpError> {
    if UI_THREAD.get() == Some(&std::thread::current().id()) {
        return Err(McpError::Reentrant);
    }
    // 上一轮若还有残留（理论上不该有），先丢弃，避免错唤醒。
    let _ = CONSENT_REPLY
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .take();

    let (tx, rx) = tokio::sync::oneshot::channel();
    *CONSENT_REPLY
        .lock()
        .unwrap_or_else(|err| err.into_inner()) = Some(tx);
    CONSENT_SEQ.fetch_add(1, Ordering::Relaxed);

    let base = request.base;
    let client = request.client;

    slint::invoke_from_event_loop(move || {
        UI.with(|slot| {
            let borrowed = slot.borrow();
            if let Some(access) = borrowed.as_ref() {
                // 文案语言在 UI 线程取（MCP 线程读不到 `lang-en`）。
                let en = access.window.get_lang_en();
                let pick = |zh: &'static str, en_text: &'static str| if en { en_text } else { zh };
                let rows: Vec<McpConsentRow> = crate::mcp::bits_with_heads()
                    .into_iter()
                    .map(|(bit, risk, group)| McpConsentRow {
                        key: bit.key.into(),
                        label: pick(bit.label_zh, bit.label_en).into(),
                        desc: pick(bit.desc_zh, bit.desc_en).into(),
                        // 预勾该客户端上次记住的授权；用户可以改。
                        checked: base.allows(bit.key),
                        allowed: true,
                        risk: risk
                            .map(|risk| pick(risk.label_zh(), risk.label_en()))
                            .unwrap_or_default()
                            .into(),
                        risk_note: risk
                            .map(|risk| pick(risk.note_zh(), risk.note_en()))
                            .unwrap_or_default()
                            .into(),
                        group: group
                            .map(|group| pick(group.label_zh(), group.label_en()))
                            .unwrap_or_default()
                            .into(),
                    })
                    .collect();
                let model = Rc::new(VecModel::from(rows));
                access.window.set_mcp_consent_client(client.into());
                access.window.set_mcp_consent_tool(SharedString::default());
                access.window.set_mcp_consent_summary(SharedString::default());
                access.window.set_mcp_consent_rows(ModelRc::from(model.clone()));
                access.window.set_mcp_consent_open(true);
                CONSENT_ROWS.with(|slot| *slot.borrow_mut() = Some(model));
            }
        });
    })
    .map_err(|_| McpError::UiUnavailable)?;

    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(McpError::Revoked),
        Err(_) => {
            // 超时：关窗、清掉句柄，按拒绝处理。
            let _ = CONSENT_REPLY
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .take();
            let _ = slint::invoke_from_event_loop(|| {
                UI.with(|slot| {
                    if let Some(access) = slot.borrow().as_ref() {
                        access.window.set_mcp_consent_open(false);
                    }
                });
                CONSENT_ROWS.with(|slot| *slot.borrow_mut() = None);
            });
            Err(McpError::ConsentTimeout)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 9 个高风险键必须都在白名单里；33 - 9 = 24 对应 UI 文案里的常规项数量。
    #[test]
    fn high_risk_settings_match_the_table() {
        let table = settings_table();
        assert_eq!(table.len(), 33);
        for key in crate::mcp::HIGH_RISK_SETTINGS {
            assert!(
                table.iter().any(|def| def.key == *key),
                "高风险键 {key} 不在 settings_table 里"
            );
        }
        assert_eq!(table.len() - crate::mcp::HIGH_RISK_SETTINGS.len(), 24);
    }

    #[test]
    fn webdav_url_rejects_embedded_credentials() {
        assert!(url_has_userinfo("https://bob:pw@dav.example.com/remote.php"));
        assert!(url_has_userinfo("https://bob@dav.example.com"));
        assert!(!url_has_userinfo("https://dav.example.com/remote.php"));
        // `@` 出现在路径里不算 userinfo。
        assert!(!url_has_userinfo("https://dav.example.com/dav/user@example.com"));
        assert!(!url_has_userinfo(""));
        assert!(!url_has_userinfo("localhost:8080"));
    }

    #[test]
    fn text_kind_requires_a_string() {
        assert!(validate(&SettingKind::Text, &json!("x")).is_ok());
        assert!(validate(&SettingKind::Text, &json!("")).is_ok());
        assert_eq!(validate(&SettingKind::Text, &json!(1)), Err("expected_string"));
        assert_eq!(validate(&SettingKind::Text, &json!(true)), Err("expected_string"));
    }
}
