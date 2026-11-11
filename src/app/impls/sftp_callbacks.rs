use super::window_geometry::active_sftp_path;
use super::*;
use crate::sftp::DownloadConflict;

thread_local! {
    /// 同 tab 的刷新去重表：`(path, 时刻)`，50ms 内合并。
    static LAST_SFTP_REFRESH: std::cell::RefCell<
        std::collections::HashMap<String, (String, std::time::Instant)>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Ask what to do when a download would overwrite an existing local file.
/// Returns `None` when the user cancels the download. Resolved here, in the UI
/// thread, so the transfer task never has to block on a modal prompt.
fn choose_download_conflict(remote: &str, local_dir: &str) -> Option<DownloadConflict> {
    let target = crate::sftp::download_target_path(remote, local_dir);
    if !target.is_file() {
        return Some(DownloadConflict::Replace);
    }
    let replace = t("替换", "Replace");
    let keep_both = t("共存", "Keep both");
    let cancel = t("取消", "Cancel");
    let result = rfd::MessageDialog::new()
        .set_title(t("文件已存在", "File already exists"))
        .set_description(format!(
            "{}\n{}",
            target.display(),
            t(
                "请选择替换现有文件，或保留两份文件。",
                "Choose whether to replace the existing file or keep both files."
            )
        ))
        .set_level(rfd::MessageLevel::Warning)
        .set_buttons(rfd::MessageButtons::YesNoCancelCustom(
            replace.to_string(),
            keep_both.to_string(),
            cancel.to_string(),
        ))
        .show();
    match result {
        rfd::MessageDialogResult::Yes => Some(DownloadConflict::Replace),
        rfd::MessageDialogResult::No => Some(DownloadConflict::KeepBoth),
        rfd::MessageDialogResult::Custom(value) if value == replace => {
            Some(DownloadConflict::Replace)
        }
        rfd::MessageDialogResult::Custom(value) if value == keep_both => {
            Some(DownloadConflict::KeepBoth)
        }
        _ => None,
    }
}

/// Locate the next (or previous, when `reverse`) match of a literal query in
/// the editor text, relative to `current` (-1 = no selection yet). Wraps around
/// at the ends. The count is always the total, so the bar can show "3" even
/// when the current selection is at the last match.
fn editor_find_next(
    content: &str,
    query: &str,
    current: i32,
    reverse: bool,
) -> EditorFindResult {
    if query.is_empty() {
        return EditorFindResult {
            start: 0,
            end: 0,
            count: 0,
        };
    }
    let positions: Vec<usize> = content
        .match_indices(query)
        .map(|(index, _)| index)
        .collect();
    let selected = if reverse {
        positions
            .iter()
            .rev()
            .copied()
            .find(|position| current < 0 || *position < current as usize)
            .or_else(|| positions.last().copied())
    } else {
        positions
            .iter()
            .copied()
            .find(|position| current < 0 || *position > current as usize)
            .or_else(|| positions.first().copied())
    };
    let start = selected.unwrap_or(0).min(i32::MAX as usize) as i32;
    EditorFindResult {
        start,
        end: start.saturating_add(query.len().min(i32::MAX as usize) as i32),
        count: positions.len().min(i32::MAX as usize) as i32,
    }
}

#[path = "sftp_callbacks_nav.rs"] mod sftp_callbacks_nav;
#[path = "sftp_callbacks_listing.rs"] mod sftp_callbacks_listing;
#[path = "sftp_callbacks_actions.rs"] mod sftp_callbacks_actions;
#[path = "sftp_callbacks_editor.rs"] mod sftp_callbacks_editor;

pub(super) fn wire_sftp_callbacks(window: &AppWindow, ctx: &AppContext) {
    sftp_callbacks_nav::wire_sftp_nav(window, ctx);
    sftp_callbacks_listing::wire_sftp_listing(window, ctx);
    sftp_callbacks_actions::wire_sftp_actions(window, ctx);
    sftp_callbacks_editor::wire_sftp_editor(window, ctx);
}

#[cfg(test)]
mod editor_find_tests {
    use super::editor_find_next;

    #[test]
    fn finds_forward_from_the_current_selection() {
        let result = editor_find_next("aaa bbb aaa", "aaa", -1, false);
        assert_eq!((result.start, result.end, result.count), (0, 3, 2));

        // Advance past the first match.
        let next = editor_find_next("aaa bbb aaa", "aaa", 0, false);
        assert_eq!((next.start, next.end, next.count), (8, 11, 2));
    }

    #[test]
    fn wraps_around_at_the_end_and_is_case_sensitive() {
        let result = editor_find_next("AAA aaa", "aaa", 4, false);
        assert_eq!((result.start, result.end, result.count), (4, 7, 1));

        // "AAA" does not match the lowercase query.
        let result = editor_find_next("AAA", "aaa", -1, false);
        assert_eq!(result.count, 0);
    }

    #[test]
    fn finds_backward_and_wraps_to_the_last_match() {
        let result = editor_find_next("aaa bbb aaa", "aaa", 0, true);
        assert_eq!((result.start, result.end, result.count), (8, 11, 2));

        // Before the first match wraps to the last one.
        let result = editor_find_next("aaa bbb aaa", "aaa", -1, true);
        assert_eq!((result.start, result.end, result.count), (8, 11, 2));
    }

    #[test]
    fn empty_query_returns_zero_range() {
        let result = editor_find_next("anything", "", 0, false);
        assert_eq!((result.start, result.end, result.count), (0, 0, 0));
    }
}
