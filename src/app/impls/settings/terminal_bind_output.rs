use super::*;

pub(super) fn wire_output(window: &AppWindow, store: &Store) {
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font_bold(move |bold: bool| {
            {
                let mut s = store.borrow_mut();
                s.set_terminal_bold(bold);
                s.save_logging();
            }
            if let Some(w) = weak.upgrade() {
                w.global::<Theme>().set_term_font_bold(bold);
            }
        });
    }

    {
        // A6：大回滚缓冲区开关。关闭时会把回滚现值收回常规上限，因此要把新值
        // 回写到输入框（否则面板里还显示着旧的大数字）。
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_large_scrollback(move |on: bool| {
            let lines = {
                let mut s = store.borrow_mut();
                s.set_large_scrollback(on);
                s.save_logging();
                s.scrollback_lines()
            };
            if let Some(w) = weak.upgrade() {
                w.set_large_scrollback(on);
                w.set_scrollback_lines(lines.to_string().into());
            }
        });
    }
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_scrollback_lines(move |lines: slint::SharedString| -> bool {
            // Validate: 100..=（常规 10 万 / 开开关后 100 万）。非法输入被拒绝
            // （界面显示为非法状态），不写入任何东西。
            let max = if store.borrow().large_scrollback() {
                crate::config::SCROLLBACK_MAX_LARGE
            } else {
                crate::config::SCROLLBACK_MAX
            };
            let Some(n) = parse_scrollback(lines.as_str(), max) else {
                return false;
            };
            {
                // 借用单独成块：写盘与回写 UI 都在借出期间之外（原写法把 RefCell
                // 借用一直握到 `weak.upgrade()` 之后）。
                let mut s = store.borrow_mut();
                s.set_scrollback_lines(n);
                s.save_logging();
            }
            // 回写规范化后的值：设置面板是条件渲染的，不回写就会在重开时显示旧值。
            if let Some(w) = weak.upgrade() {
                w.set_scrollback_lines(n.to_string().into());
            }
            true
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_convert_eol(move |v: bool| {
            {
                let mut s = store.borrow_mut();
                s.set_convert_eol(v);
                s.save_logging();
            }
            // 回写主窗口属性：设置窗每次打开都从它播种，不回写就会"改了 → 关掉 →
            // 重开显示旧值"（配置其实已经生效，纯粹是界面层滞后）。
            if let Some(w) = weak.upgrade() {
                w.set_convert_eol(v);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_osc52_clipboard(move |v: bool| {
            {
                let mut s = store.borrow_mut();
                s.set_osc52_clipboard(v);
                s.save_logging();
            }
            crate::terminal::vt_adapter::OSC52_ENABLED
                .store(v, std::sync::atomic::Ordering::Relaxed);
            if let Some(w) = weak.upgrade() {
                w.set_osc52_clipboard(v);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_cursor_style(move |style: SharedString| {
            let normalized = {
                let mut s = store.borrow_mut();
                s.set_terminal_cursor_style(style.to_string());
                let normalized = s.terminal_cursor_style().to_string();
                s.save_logging();
                normalized
            };
            if let Some(w) = weak.upgrade() {
                w.set_term_cursor_style(normalized.into());
            }
        });
    }
}
