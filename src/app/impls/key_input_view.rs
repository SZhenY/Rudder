use super::*;

pub(super) fn wire_key_view(window: &AppWindow, app: &AppContext) {
    let handles = app.handles.clone();
    let bufs = app.bufs.clone();
    let last_term_size = app.last_term_size.clone();

    // Propagate PTY resize to the SSH worker and vt100 parser. Slint measures
    // the cell size and passes whole column/row counts directly.
    //
    // terminal_view.slint now passes the FocusScope height (not the full
    // TerminalView height), so the SFTP panel is already excluded.
    // Layout breakdown for the FocusScope:
    //   16 px  – bottom strip (TouchArea for focus-regain)
    //    8 px  – y-offset of the output Text element inside the Flickable
    // = 24 px  total vertical chrome within FocusScope
    {
        let handles = handles.clone();
        let bufs_resize = bufs.clone(); // keep bufs alive for the copy handler below
        let weak_resize = window.as_weak();
        // The Slint side measures the cell size (via a hidden probe Text) and
        // passes whole column/row counts directly, so there is no pixel→cell
        // guesswork here. This keeps full-screen programs like nano from
        // over-counting rows and clipping their bottom shortcut bar.
        // Debounce PTY resizes (#163): a layout reflow (a tab becoming visible,
        // the SFTP panel expanding, a window drag) can momentarily report a
        // near-zero width, which collapses term-cols to its 10-col floor.
        // Applying that to the remote PTY immediately resizes the server to 10
        // columns and reflows vt100 — garbling running output (e.g. a `git clone`
        // progress meter wraps at 10 chars). Coalesce rapid changes and apply
        // only the size that's still set after a short quiet period, so a
        // transient bad value never reaches the server.
        let pending_size: Rc<RefCell<HashMap<String, (u32, u32)>>> =
            Rc::new(RefCell::new(HashMap::new()));
        // 按 tab 记住上次下发的尺寸，合并一次缩放里的重复上报。
        let last_applied_size: Rc<RefCell<HashMap<String, (u32, u32)>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let resize_debounce = Rc::new(slint::Timer::default());
        window.on_terminal_resize(move |tab_id: SharedString, cols_f: f32, rows_f: f32| {
            // A hidden terminal (inactive tab, or a split sibling not currently
            // shown) reports 0 width/height. Ignore those: flooring 0 to the 10-col
            // minimum and applying it would shrink that tab's PTY *and* poison
            // `last_term_size`, so the next connection (e.g. "Duplicate connection")
            // would start at 10 cols and wrap its first output to ~10 chars (#v0.5).
            // Only genuine, visible sizes drive a resize.
            if cols_f < 1.0 || rows_f < 1.0 {
                return;
            }
            let cols = (cols_f as u32).max(10);
            let rows = (rows_f as u32).max(5);
            pending_size
                .borrow_mut()
                .insert(tab_id.to_string(), (cols, rows));
            let pending = pending_size.clone();
            let applied = last_applied_size.clone();
            let handles = handles.clone();
            let bufs = bufs_resize.clone();
            let last = last_term_size.clone();
            let weak = weak_resize.clone();
            // (Re)arm the single-shot timer; rapid changes keep resetting it so
            // only the final, settled size is applied.
            resize_debounce.start(
                slint::TimerMode::SingleShot,
                std::time::Duration::from_millis(150),
                move || {
                    let settled: Vec<(String, (u32, u32))> = pending.borrow_mut().drain().collect();
                    for (tab, (cols, rows)) in settled {
                        // 尺寸未变则不再下发 PTY。
                        let duplicate = applied.borrow().get(&tab).copied() == Some((cols, rows));
                        if duplicate {
                            continue;
                        }
                        {
                            // 只做去重，不清理；够长就整体丢弃（代价是重下发一次尺寸）。
                            let mut applied_map = applied.borrow_mut();
                            if applied_map.len() >= 256 && !applied_map.contains_key(&tab) {
                                applied_map.clear();
                            }
                            applied_map.insert(tab.clone(), (cols, rows));
                        }
                        tracing::debug!("terminal_resize tab={} cols={} rows={}", tab, cols, rows);
                        apply_terminal_resize(&handles, &bufs, &last, &tab, cols, rows);
                        // Re-render so the reflowed (or resized) grid shows at once
                        // instead of waiting for the next remote output (#169).
                        if let Some(win) = weak.upgrade() {
                            rebuild_tab_display(&win, &bufs, &tab);
                        }
                    }
                },
            );
        });
    }

    // Ctrl+Shift+C: copy current terminal screen to clipboard.
    {
        let bufs = bufs.clone();
        window.on_copy_terminal_text(move |tab_id: SharedString| {
            let text = term_buf(&bufs, tab_id.as_str())
                .map(|h| {
                    let buf = h.lock().unwrap_or_else(|e| e.into_inner());
                    // Copy the drag-selection when there is one, else the
                    // whole displayed screen.
                    let sel = buf.term.selection_to_string().unwrap_or_default();
                    if sel.is_empty() {
                        buf.displayed_text.join("\n")
                    } else {
                        sel
                    }
                })
                .unwrap_or_default();
            // Run the clipboard write on a dedicated OS thread.  arboard's
            // Windows backend opens the clipboard and pumps Win32 messages;
            // doing that on the Slint/winit event-loop thread re-enters the
            // message loop and dead-locks the whole UI.
            std::thread::spawn(move || clipboard_set_text(text));
        });
    }

    // Middle-click / Ctrl+Shift+V: paste clipboard text into PTY.
    //
}
