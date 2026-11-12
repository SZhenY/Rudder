use super::*;

pub(super) fn wire_sftp_editor(window: &AppWindow, ctx: &AppContext) {
    let sftp_handles = ctx.sftp_handles.clone();
    let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let close_pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Built-in editor: save (Ctrl+S / button) writes the text back to the
    // remote file (#70). Read-only (view) sessions never save.
    //
    // 写到哪个会话由 `editor_tab_id` 决定（编辑器**打开时**记下的那个），不是"当前活动
    // 标签页" —— 否则开着编辑器切标签再 Ctrl+S 会把内容写到别的会话（上游 eafd513）。
    fn editor_tab_id(w: &AppWindow) -> String {
        let id = w.get_editor_tab_id().to_string();
        if id.is_empty() {
            w.get_active_tab_id().to_string()
        } else {
            id
        }
    }

    /// 保存一次，成功才清 dirty / 关窗。失败时保留内容并写 `editor-status`。
    ///
    /// 回执由 SFTP worker 发：`WriteText` 现在带 oneshot，UI 线程不能 `block_on`，
    /// 所以放在 std 线程里 `blocking_recv`，再回到事件循环更新属性。
    fn save_then(
        w: &AppWindow,
        sftp_handles: &crate::sftp::SftpHandles,
        busy: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        close_pending: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        close_after: bool,
    ) {
        use std::sync::atomic::Ordering;
        if busy.swap(true, Ordering::AcqRel) {
            // 已在保存：把「关窗」意图记下来，成功后一并处理。
            if close_after {
                close_pending.store(true, Ordering::Release);
            }
            return;
        }
        let path = w.get_editor_path().to_string();
        let content = w.get_editor_content().to_string();
        let tab_id = editor_tab_id(w);
        let rx = match sftp_handles.lock() {
            Ok(handles) => handles
                .get(&tab_id)
                .and_then(|h| h.write_text(path, content)),
            Err(_) => None,
        };
        let Some(rx) = rx else {
            busy.store(false, Ordering::Release);
            w.set_editor_status(
                crate::i18n::t("保存失败：会话已断开", "Save failed: session closed").into(),
            );
            return;
        };
        w.set_editor_status(crate::i18n::t("正在保存…", "Saving…").into());
        let weak = w.as_weak();
        let busy = busy.clone();
        let close_pending = close_pending.clone();
        std::thread::spawn(move || {
            let result = rx
                .blocking_recv()
                .unwrap_or_else(|_| Err(crate::i18n::t("会话已关闭", "Session closed").to_string()));
            let _ = weak.upgrade_in_event_loop(move |w| {
                busy.store(false, Ordering::Release);
                match result {
                    Ok(()) => {
                        w.set_editor_dirty(false);
                        w.set_editor_status("".into());
                        if close_after || close_pending.swap(false, Ordering::AcqRel) {
                            close_editor_now(&w);
                        }
                    }
                    Err(msg) => w.set_editor_status(
                        format!(
                            "{}: {msg}",
                            crate::i18n::t("保存失败，内容未丢失", "Save failed — edits kept")
                        )
                        .into(),
                    ),
                }
            });
        });
    }

    /// 关闭编辑器并清掉内容：`editor-content` 最大 512 KB，关闭后没有任何读者。
    fn close_editor_now(w: &AppWindow) {
        w.set_editor_open(false);
        w.set_editor_dirty(false);
        w.set_editor_find_query("".into());
        w.set_editor_replace_text("".into());
        w.set_editor_match_count(0);
        w.set_editor_find_position(-1);
        w.set_editor_content("".into());
        w.set_editor_line_numbers("".into());
        w.set_editor_path("".into());
        w.set_editor_name("".into());
        w.set_editor_tab_id("".into());
        w.set_editor_status("".into());
    }
    {
        let sftp_handles = sftp_handles.clone();
        let busy = busy.clone();
        let close_pending = close_pending.clone();
        let weak = window.as_weak();
        window.on_save_file(move || {
            let Some(w) = weak.upgrade() else { return };
            if w.get_editor_readonly() {
                return;
            }
            save_then(&w, &sftp_handles, &busy, &close_pending, false);
        });
    }
    // Close the editor; in edit mode upload first if there are unsaved edits.
    {
        let sftp_handles = sftp_handles.clone();
        let busy = busy.clone();
        let close_pending = close_pending.clone();
        let weak = window.as_weak();
        window.on_close_editor(move || {
            let Some(w) = weak.upgrade() else { return };
            if !w.get_editor_readonly() && w.get_editor_dirty() {
                // 保存成功后才关窗；失败时窗口保持打开，内容不丢。
                save_then(&w, &sftp_handles, &busy, &close_pending, true);
            } else {
                close_editor_now(&w);
            }
        });
    }

    // Built-in editor find/replace (#287). Matching is literal and
    // case-sensitive, which is predictable for configuration/source files.
    window.on_editor_count_matches(|content: SharedString, query: SharedString| {
        if query.is_empty() {
            0
        } else {
            content
                .matches(query.as_str())
                .count()
                .min(i32::MAX as usize) as i32
        }
    });
    window.on_editor_find_step(
        |content: SharedString, query: SharedString, current: i32, reverse: bool| {
            editor_find_next(content.as_str(), query.as_str(), current, reverse)
        },
    );
    {
        let weak = window.as_weak();
        window.on_editor_replace_all(move |query: SharedString, replacement: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            if w.get_editor_readonly() || query.is_empty() {
                return;
            }
            let replaced = w
                .get_editor_content()
                .replace(query.as_str(), replacement.as_str());
            w.set_editor_content(replaced.clone().into());
            w.set_editor_dirty(true);
            w.set_editor_line_numbers(line_numbers_for(&replaced).into());
            w.set_editor_match_count(0);
        });
    }
}
