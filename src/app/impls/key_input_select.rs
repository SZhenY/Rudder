use super::*;

pub(super) fn wire_key_select(window: &AppWindow, app: &AppContext) {
    let handles = app.handles.clone();
    let bufs = app.bufs.clone();
    let gates_scroll = app.render_gates.clone();
    let gates_scroll2 = app.render_gates.clone();
    let store = app.store.clone();
    let store_clear = store.clone();

    // Context menu → 清空缓存: reset the local vt100 buffer (drops scrollback),
    // wipe the displayed screen, then nudge the remote to redraw a fresh prompt.
    {
        let bufs_clear = bufs.clone();
        let handles_clear = handles.clone();
        let weak = window.as_weak();
        window.on_clear_terminal(move |tab_id: SharedString| {
            let tid = tab_id.to_string();
            if let Some(h) = term_buf(&bufs_clear, &tid) {
                let mut buf = h.lock().unwrap_or_else(|e| e.into_inner());
                buf.reset(store_clear.borrow().scrollback_lines());
                buf.find_query.clear();
            }
            if let Some(win) = weak.upgrade() {
                set_terminal_row(&win, &tid, |row| {
                    row.spans = ModelRc::from(Rc::new(VecModel::<TermSpan>::default()));
                    row.find_matches = ModelRc::from(Rc::new(VecModel::<TermMatch>::default()));
                    row.selection = ModelRc::from(Rc::new(VecModel::<TermMatch>::default()));
                    row.cursor_row = 0;
                    row.cursor_col = 0;
                    row.rows_used = 0;
                    row.scroll_max = 0;
                    row.scroll_offset = 0;
                    // 缓冲已经 `reset()`，这两个也必须跟着归位：否则在下次远端输出之前，
                    // UI 仍以为处于备用屏 / 鼠标跟踪态，滚轮与命中判定会走错分支。
                    row.is_alt_screen = false;
                    row.mouse_tracked = false;
                });
            }
            if let Some(h) = handles_clear.borrow().get(&tid) {
                h.send_raw(vec![0x0c]); // Ctrl+L → shell clears + redraws prompt
            }
        });
    }

    // Context menu → 查找: store the query and recompute highlight rectangles.
    {
        let bufs_find = bufs.clone();
        let weak = window.as_weak();
        window.on_find_query_changed(move |tab_id: SharedString, query: SharedString| {
            let tid = tab_id.to_string();
            let q = query.to_string();
            let (matches, jumped) = with_term_buf(&bufs_find, &tid, |buf| {
                buf.find_query = q.clone();
                let mut matches = compute_find_matches(&buf.displayed_text, &q);
                let jumped = matches.is_empty() && buf.scroll_to_first_find_match(&q);
                if jumped {
                    buf.render();
                    matches = compute_find_matches(&buf.displayed_text, &q);
                }
                (matches, jumped)
            })
            .unwrap_or_default();
            if let Some(win) = weak.upgrade() {
                if jumped {
                    rebuild_tab_display(&win, &bufs_find, &tid);
                    return;
                }
                let model = ModelRc::from(Rc::new(VecModel::from(matches)));
                set_terminal_row(&win, &tid, |row| {
                    row.find_matches = model.clone();
                });
            }
        });
    }

    // Enter / Shift+Enter in the find bar → next / previous match (#find-nav).
    {
        let bufs_find = bufs.clone();
        let weak = window.as_weak();
        let nav = move |tab_id: &str, forward: bool| {
            let tid = tab_id.to_string();
            let moved = with_term_buf(&bufs_find, &tid, |buf| {
                let q = buf.find_query.clone();
                if q.is_empty() {
                    return false;
                }
                if buf.scroll_to_find_match(&q, forward) {
                    buf.render();
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
            if moved && let Some(win) = weak.upgrade() {
                rebuild_tab_display(&win, &bufs_find, &tid);
                // Recompute the highlight rectangles for the new viewport.
                let matches = with_term_buf(&bufs_find, &tid, |buf| {
                    compute_find_matches(&buf.displayed_text, &buf.find_query)
                })
                .unwrap_or_default();
                let model = ModelRc::from(Rc::new(VecModel::from(matches)));
                set_terminal_row(&win, &tid, |row| {
                    row.find_matches = model.clone();
                });
            }
        };
        let nav_next = nav.clone();
        window.on_find_next(move |tab_id: SharedString| nav_next(&tab_id, true));
        window.on_find_prev(move |tab_id: SharedString| nav(&tab_id, false));
    }

    // Mouse-wheel → scroll the scrollback history.
    {
        let bufs_scroll = bufs.clone();
        let weak = window.as_weak();
        window.on_terminal_scroll(move |tab_id: SharedString, delta: i32| {
            let tid = tab_id.to_string();
            with_term_buf(&bufs_scroll, &tid, |buf| {
                // Scroll within our own session scrollback (history lines above
                // the live screen).  Offset 0 = live bottom.
                let max_off = buf
                    .term
                    .total_lines()
                    .saturating_sub(buf.term.screen_lines()) as i64;
                let cur = buf.view_offset as i64;
                buf.view_offset = (cur + delta as i64).clamp(0, max_off) as usize;
            });
            // 走渲染闸门：以前每来一个滚轮 / 滚动条事件就**整屏重建**（惯性滚动每秒几十次，
            // 既没有节流也没有合并）。闸门会把它们并成一帧，延迟仍在一帧以内。
            request_tab_render_from_ui(weak.clone(), &tid, &bufs_scroll, &gates_scroll);
        });
    }

    // Home / End / PageUp / PageDown while viewing normal-screen history.
    // 返回 false 表示没消费 —— Slint 会把原键照旧透传给 PTY，实时终端与 TUI
    // 程序（less / vim / tmux…）仍然拥有它们（上游 35158dd）。
    {
        let bufs_key = bufs.clone();
        let gates_key = app.render_gates.clone();
        let weak = window.as_weak();
        window.on_terminal_scrollback_key(move |tab_id: SharedString, key: SharedString| {
            let Some(scroll_key) = scrollback_key_from_text(key.as_str()) else {
                return false;
            };
            let tid = tab_id.to_string();
            let handled = with_term_buf(&bufs_key, &tid, |buf| buf.scrollback_navigate(scroll_key))
                .unwrap_or(false);
            if handled {
                request_tab_render_from_ui(weak.clone(), &tid, &bufs_key, &gates_key);
            }
            handled
        });
    }

    // Wheel inside an alt-screen program (tmux / less / vim): forward it to the PTY
    // so the program scrolls, instead of doing nothing (#170 — FinalShell /
    // MobaXterm behave this way). If the app is tracking the mouse (e.g. tmux with
    // `mouse on`), send a real wheel mouse-event in the encoding it asked for;
    // otherwise fall back to arrow keys (xterm "alternate scroll"), which scrolls
    // less / man / vim.
    {
        let bufs_wheel = bufs.clone();
        let handles_wheel = handles.clone();
        window.on_terminal_wheel(move |tab_id: SharedString, dir: i32, col: i32, row: i32| {
            let tid = tab_id.to_string();
            let bytes = term_buf(&bufs_wheel, &tid).map(|h| {
                let buf = h.lock().unwrap_or_else(|e| e.into_inner());
                let report = crate::terminal::mouse_report(&buf.term);
                if report != crate::terminal::MouseReport::None {
                    // Wheel up / down are button codes 64 / 65; encode_mouse_event
                    // handles the 1-based clamping and both wire encodings.
                    let (rows, cols) = crate::terminal::term_size(&buf.term);
                    let btn: u8 = if dir > 0 { 64 } else { 65 };
                    crate::terminal::encode_mouse_event(btn, false, col, row, cols, rows, report)
                } else {
                    // alternate-scroll: 3 arrow presses per notch, app-cursor aware.
                    let one: &[u8] = if dir > 0 {
                        if crate::terminal::app_cursor(&buf.term) {
                            b"\x1bOA"
                        } else {
                            b"\x1b[A"
                        }
                    } else if crate::terminal::app_cursor(&buf.term) {
                        b"\x1bOB"
                    } else {
                        b"\x1b[B"
                    };
                    one.repeat(3)
                }
            });
            if let (Some(bytes), Some(h)) = (bytes, handles_wheel.borrow().get(&tid)) {
                h.send_raw(bytes);
            }
        });
    }

    // Mouse press / release / drag-motion forwarded to the PTY for
    // mouse-tracking TUI apps (btop/htop/mc, upstream d8eff40). The Slint side
    // only calls this when the remote enabled a mouse protocol
    // (mouse_protocol_mode != None, cached as TermBuffer::mouse_tracked), so a
    // click inside e.g. btop activates the widget under the pointer instead of
    // starting a local drag-selection. Returns true when bytes were written,
    // so the caller can skip its own local handling.
    {
        let bufs_mouse = bufs.clone();
        let handles_mouse = handles.clone();
        window.on_terminal_mouse(
            move |tab_id: SharedString, kind: i32, button: i32, col: i32, row: i32| -> bool {
                let tid = tab_id.to_string();
                let Some(bytes) = term_buf(&bufs_mouse, &tid).map(|h| {
                    let buf = h.lock().unwrap_or_else(|e| e.into_inner());
                    if !buf.mouse_tracked {
                        return None;
                    }
                    let encoding = crate::terminal::mouse_report(&buf.term);
                    if encoding == crate::terminal::MouseReport::None {
                        return None;
                    }
                    let (rows, cols) = crate::terminal::term_size(&buf.term);
                    let (btn, release) = match kind {
                        1 => (button as u8, true),  // release
                        2 => (35, false),           // drag motion with button held
                        _ => (button as u8, false), // press
                    };
                    Some(crate::terminal::encode_mouse_event(
                        btn, release, col, row, cols, rows, encoding,
                    ))
                }) else {
                    return false;
                };
                if let (Some(bytes), Some(h)) = (bytes, handles_mouse.borrow().get(&tid)) {
                    h.send_raw(bytes);
                    return true;
                }
                false
            },
        );
    }

    // Scrollbar drag → jump to an absolute scrollback offset (#103).
    {
        let bufs_scroll = bufs.clone();
        let weak = window.as_weak();
        window.on_terminal_scroll_to(move |tab_id: SharedString, offset: i32| {
            let tid = tab_id.to_string();
            with_term_buf(&bufs_scroll, &tid, |buf| {
                let max_off = buf
                    .term
                    .total_lines()
                    .saturating_sub(buf.term.screen_lines()) as i64;
                buf.view_offset = (offset as i64).clamp(0, max_off) as usize;
            });
            // 走渲染闸门：以前每来一个滚轮 / 滚动条事件就**整屏重建**（惯性滚动每秒几十次，
            // 既没有节流也没有合并）。闸门会把它们并成一帧，延迟仍在一帧以内。
            request_tab_render_from_ui(weak.clone(), &tid, &bufs_scroll, &gates_scroll2);
        });
    }

    // Drag-selection lifecycle — alacritty native Selection (grid-coordinate,
    // survives view_offset changes so the anchor doesn't drift on scroll).
    {
        let bufs_sel = bufs.clone();
        let weak = window.as_weak();
        window.on_term_select_start(
            move |tab_id, row: i32, col: i32, left_half: bool, _ctrl: bool, _shift: bool| {
                let tid = tab_id.to_string();
                with_term_buf(&bufs_sel, &tid, |buf| {
                    let (rows, cols) = crate::terminal::term_size(&buf.term);
                    let r = row.clamp(0, rows.saturating_sub(1) as i32);
                    let c = col.clamp(0, cols.saturating_sub(1) as i32);
                    let pt = alacritty_terminal::index::Point {
                        line: alacritty_terminal::index::Line(r - buf.view_offset as i32),
                        column: alacritty_terminal::index::Column(c.max(0) as usize),
                    };
                    // The anchor side is derived from where the pointer landed inside
                    // the cell: on the left half the cell itself is selected, on the
                    // right half the selection starts at the next cell.  A fixed
                    // `Side::Right` previously skipped the first character of a
                    // selection (the very first cell of a row could never be selected).
                    let side = if left_half {
                        alacritty_terminal::index::Side::Left
                    } else {
                        alacritty_terminal::index::Side::Right
                    };
                    buf.term.selection = Some(alacritty_terminal::selection::Selection::new(
                        alacritty_terminal::selection::SelectionType::Simple,
                        pt,
                        side,
                    ));
                });
                if let Some(win) = weak.upgrade() {
                    refresh_terminal_selection(&win, &bufs_sel, &tid);
                }
            },
        );
    }
    {
        // Double-click → word selection, triple-click → whole line. Both use
        // alacritty's native Semantic/Lines selection kinds (#select-double-click).
        let bufs_sel = bufs.clone();
        let weak = window.as_weak();
        let quick_select = move |tab_id: &str, row: i32, col: i32, ty| {
            use alacritty_terminal::index::{Column, Line as GridLine, Point as GridPoint, Side};
            use alacritty_terminal::selection::SelectionType;
            let tid = tab_id.to_string();
            with_term_buf(&bufs_sel, &tid, |buf| {
                let (rows, cols) = crate::terminal::term_size(&buf.term);
                let r = row.clamp(0, rows.saturating_sub(1) as i32);
                let line = GridLine(r - buf.view_offset as i32);
                let anchor_col = if ty == SelectionType::Lines {
                    0usize
                } else {
                    col.clamp(0, cols.saturating_sub(1) as i32) as usize
                };
                let start = GridPoint {
                    line,
                    column: Column(anchor_col),
                };
                let mut sel = alacritty_terminal::selection::Selection::new(ty, start, Side::Left);
                // Extend by one cell so semantic/line expansion kicks in:
                // word → expands to the whole word, line → to the full row.
                let end_col = if ty == SelectionType::Lines {
                    cols.saturating_sub(1) as usize
                } else {
                    anchor_col
                };
                sel.update(
                    GridPoint {
                        line,
                        column: Column(end_col.saturating_add(1)),
                    },
                    Side::Right,
                );
                buf.term.selection = Some(sel);
            });
            if let Some(win) = weak.upgrade() {
                refresh_terminal_selection(&win, &bufs_sel, &tid);
            }
        };
        let qw = quick_select.clone();
        window.on_term_select_word(move |tab_id: SharedString, row: i32, col: i32| {
            use alacritty_terminal::selection::SelectionType;
            qw(tab_id.as_str(), row, col, SelectionType::Semantic)
        });
        window.on_term_select_line(move |tab_id: SharedString, row: i32, col: i32| {
            use alacritty_terminal::selection::SelectionType;
            quick_select(tab_id.as_str(), row, col, SelectionType::Lines)
        });
    }
    {
        let bufs_sel = bufs.clone();
        let weak = window.as_weak();
        window.on_term_select_update(move |tab_id, row: i32, col: i32, left_half: bool| {
            let tid = tab_id.to_string();
            with_term_buf(&bufs_sel, &tid, |buf| {
                let (rows, cols) = crate::terminal::term_size(&buf.term);
                let r = row.clamp(0, rows.saturating_sub(1) as i32);
                let c = col.clamp(0, cols.saturating_sub(1) as i32);
                if let Some(ref mut sel) = buf.term.selection {
                    let pt = alacritty_terminal::index::Point {
                        line: alacritty_terminal::index::Line(r - buf.view_offset as i32),
                        column: alacritty_terminal::index::Column(c.max(0) as usize),
                    };
                    let side = if left_half {
                        alacritty_terminal::index::Side::Left
                    } else {
                        alacritty_terminal::index::Side::Right
                    };
                    sel.update(pt, side);
                }
            });
            if let Some(win) = weak.upgrade() {
                refresh_terminal_selection(&win, &bufs_sel, &tid);
            }
        });
    }
    {
        let bufs_sel = bufs.clone();
        let weak = window.as_weak();
        window.on_term_select_end(move |tab_id| {
            let tid = tab_id.to_string();
            let text = with_term_buf(&bufs_sel, &tid, |buf| {
                let text = buf.term.selection_to_string().unwrap_or_default();
                if text.is_empty() {
                    buf.term.selection = None;
                    None
                } else {
                    Some(text)
                }
            })
            .flatten();
            if let Some(t) = text
                && !t.is_empty()
            {
                std::thread::spawn(move || clipboard_set_text(t));
            }
            if let Some(win) = weak.upgrade() {
                refresh_terminal_selection(&win, &bufs_sel, &tid);
            }
        });
    }
    {
        let bufs_sel = bufs.clone();
        let weak = window.as_weak();
        window.on_term_select_autoscroll(move |tab_id, dir: i32| {
            let tid = tab_id.to_string();
            let Some(h) = term_buf(&bufs_sel, &tid) else {
                return;
            };
            {
                let mut buf = h.lock().unwrap_or_else(|e| e.into_inner());
                if crate::terminal::is_alt(&buf.term) || buf.term.selection.is_none() {
                    return;
                }
                let rows = crate::terminal::term_size(&buf.term).0;
                let max_off = buf
                    .term
                    .total_lines()
                    .saturating_sub(buf.term.screen_lines());
                let step = 2usize;
                let edge_line = if dir < 0 {
                    let new_off = (buf.view_offset + step).min(max_off);
                    if new_off == buf.view_offset {
                        return;
                    }
                    buf.view_offset = new_off;
                    alacritty_terminal::index::Line(0 - buf.view_offset as i32)
                } else if dir > 0 {
                    let new_off = buf.view_offset.saturating_sub(step);
                    if new_off == buf.view_offset {
                        return;
                    }
                    buf.view_offset = new_off;
                    alacritty_terminal::index::Line(rows as i32 - 1 - buf.view_offset as i32)
                } else {
                    return;
                };
                if let Some(ref mut sel) = buf.term.selection {
                    let pt = alacritty_terminal::index::Point {
                        line: edge_line,
                        column: alacritty_terminal::index::Column(0),
                    };
                    sel.update(pt, alacritty_terminal::index::Side::Right);
                }
            }
            if let Some(win) = weak.upgrade() {
                refresh_terminal_selection(&win, &bufs_sel, &tid);
            }
        });
    }
}
