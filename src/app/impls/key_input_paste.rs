use super::*;

pub(super) fn wire_key_paste(window: &AppWindow, app: &AppContext) {
    let handles = app.handles.clone();
    let bufs = app.bufs.clone();

    // 打开确认框时，**完整内容只留在 `pending_paste`**，不写进 Slint 字符串属性：
    // 软件渲染器给大段 `Text` 排版时会因 i16 坐标溢出而 panic（#434 / slint#12985）。
    let pending_paste: Arc<PendingPaste> = Arc::new(Mutex::new(None));
    {
        let handles = handles.clone();
        let bufs = bufs.clone();
        let weak = window.as_weak();
        let pending_paste = pending_paste.clone();
        window.on_paste_from_clipboard(move |tab_id: SharedString| {
            // Clone the (Send) command sender for this tab so the clipboard read
            // can run off the UI thread.  Reading arboard on the event-loop
            // thread is what froze the app on middle-click / paste — see the
            // copy handler above for the deadlock explanation.
            let sender = handles
                .borrow()
                .get(tab_id.as_str())
                .map(|h| h.commands.clone());
            let Some(sender) = sender else { return };
            let bracketed = terminal_uses_bracketed_paste(&bufs, tab_id.as_str());
            let weak = weak.clone();
            let pending_paste = pending_paste.clone();
            let tab_id = tab_id.to_string();
            std::thread::spawn(move || {
                match arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
                    Ok(text) => {
                        // Review gate: multi-line pastes AND single-line pastes
                        // longer than the compact limit (a 100 KB base64 line
                        // used to stream straight into the PTY — past
                        // #single-line-paste-review).
                        if text.contains(['\r', '\n']) || paste_requires_large_review(&text) {
                            let large =
                                text.contains(['\r', '\n']) && paste_requires_large_review(&text);
                            // 只有这段有上限的预览进 UI 树；完整内容留在 Rust 里，
                            // 确认时由 take_pending_paste 取走（#434）。
                            let preview = store_pending_paste(&pending_paste, tab_id.clone(), text);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_paste_confirm_tab(tab_id.into());
                                    w.set_paste_confirm_preview(preview.into());
                                    w.set_paste_confirm_large(large);
                                    w.set_paste_confirm_open(true);
                                }
                            });
                        } else {
                            let bytes = encode_pasted_text(&text, bracketed, convert_eol());
                            let _ = sender.send(SessionCommand::RawInput(bytes));
                        }
                    }
                    Err(e) => tracing::warn!("paste_from_clipboard: clipboard error: {}", e),
                }
            });
        });
    }

    // Accept a previously reviewed multi-line paste (#262).
    {
        let handles_paste = handles.clone();
        let bufs_paste = bufs.clone();
        let pending_paste = pending_paste.clone();
        let weak = window.as_weak();
        window.on_paste_confirmed(move |tab_id: SharedString| {
            let Some(sender) = handles_paste
                .borrow()
                .get(tab_id.as_str())
                .map(|h| h.commands.clone())
            else {
                return;
            };
            // 完整内容在 Rust 侧；对话框关掉之后才谈拿不拿得到。
            let text = take_pending_paste(&pending_paste, tab_id.as_str());
            if let Some(w) = weak.upgrade() {
                w.set_paste_confirm_open(false);
            }
            let Some(text) = text else {
                tracing::warn!("paste_confirmed: no pending paste payload for tab {tab_id}");
                return;
            };
            let bracketed = terminal_uses_bracketed_paste(&bufs_paste, tab_id.as_str());
            let _ = sender.send(SessionCommand::RawInput(encode_pasted_text(
                &text,
                bracketed,
                convert_eol(),
            )));
        });
    }

    {
        let pending_paste = pending_paste.clone();
        window.on_paste_confirm_cancelled(move || {
            clear_pending_paste(&pending_paste);
        });
    }
}
