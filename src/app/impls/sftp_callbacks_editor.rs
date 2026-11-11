use super::*;

pub(super) fn wire_sftp_editor(window: &AppWindow, ctx: &AppContext) {
    let sftp_handles = ctx.sftp_handles.clone();

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
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_save_file(move || {
            let Some(w) = weak.upgrade() else { return };
            if w.get_editor_readonly() {
                return;
            }
            let path = w.get_editor_path().to_string();
            let content = w.get_editor_content().to_string();
            let tab_id = editor_tab_id(&w);
            if let Ok(handles) = sftp_handles.lock()
                && let Some(h) = handles.get(&tab_id)
            {
                h.write_text(path, content);
            }
            w.set_editor_dirty(false);
        });
    }
    // Close the editor; in edit mode upload first if there are unsaved edits.
    {
        let sftp_handles = sftp_handles.clone();
        let weak = window.as_weak();
        window.on_close_editor(move || {
            let Some(w) = weak.upgrade() else { return };
            if !w.get_editor_readonly() && w.get_editor_dirty() {
                let path = w.get_editor_path().to_string();
                let content = w.get_editor_content().to_string();
                let tab_id = editor_tab_id(&w);
                if let Ok(handles) = sftp_handles.lock()
                    && let Some(h) = handles.get(&tab_id)
                {
                    h.write_text(path, content);
                }
            }
            w.set_editor_open(false);
            w.set_editor_dirty(false);
            // Drop the find/replace bar state so the next open starts clean.
            w.set_editor_find_query("".into());
            w.set_editor_replace_text("".into());
            w.set_editor_match_count(0);
            w.set_editor_find_position(-1);
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
