//! 终端页：字体 / 光标 / 回滚 / 高亮 / 粘贴行尾 / OSC52 / JSON 格式化。
//!
//! 本页落点最多，几条容易漏的：
//! * `term-font-index`（字体选择器索引）—— 只改 family 会让下拉框停在旧项；
//! * 高亮规则变化后要刷新 `output-highlight-rule-status`；
//! * 回滚行数变化要让每个终端缓冲 reset；
//! * OSC52 开关要同步 `vt_adapter::OSC52_ENABLED` 这个运行时原子标志。

use slint::{ComponentHandle, SharedString};

use crate::app::fonts_ui::{family_from_label, term_font_covers_cjk};
use crate::app::terminal_ui::{
    apply_custom_output_rules, apply_output_highlight, for_each_buffer,
    output_highlight_rule_model, parse_hex_color, validate_output_highlight_rule,
};
use crate::config::OutputHighlightRule;
use crate::i18n::t;

use super::{FontCatalog, Store};
use crate::terminal::TermBuffers;
use crate::ui::AppWindow;

/// 播种 + 注册持久化回调。
pub(crate) fn bind(window: &AppWindow, store: &Store, bufs: &TermBuffers) {
    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_cursor_color(move |value: SharedString| {
            let Some(color) = parse_hex_color(value.as_str()) else {
                return false;
            };
            {
                let mut s = store.borrow_mut();
                if !s.set_terminal_cursor_color(value.as_str()) {
                    return false;
                }
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_cursor_color(color);
            }
            true
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_add_output_highlight_rule(
            move |pattern: SharedString,
                  is_regex,
                  case_sensitive,
                  whole_line,
                  color: SharedString| {
                let pattern = pattern.trim().to_string();
                let validation = validate_output_highlight_rule(&pattern, is_regex, case_sensitive);
                let Some(w) = weak.upgrade() else {
                    return false;
                };
                if let Err(message) = validation {
                    w.set_output_highlight_rule_status(message.into());
                    return false;
                }
                if store.borrow().output_highlight_rules().len() >= 128 {
                    w.set_output_highlight_rule_status(
                        t("自定义规则最多 128 条", "Custom rules are limited to 128").into(),
                    );
                    return false;
                }
                {
                    let mut s = store.borrow_mut();
                    s.add_output_highlight_rule(OutputHighlightRule {
                        pattern,
                        regex: is_regex,
                        case_sensitive,
                        whole_line,
                        color: color.to_string(),
                        enabled: true,
                    });
                    let _ = s.save();
                    w.set_output_highlight_rules(output_highlight_rule_model(&s));
                    // 规则清单变了，上一次的"规则已添加/已删除"提示文案必须清掉。
                    w.set_output_highlight_rule_status("".into());
                    apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
                }
                w.set_output_highlight_rule_status("".into());
                true
            },
        );
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_remove_output_highlight_rule(move |index| {
            let Some(w) = weak.upgrade() else { return };
            let mut s = store.borrow_mut();
            s.remove_output_highlight_rule(index.max(0) as usize);
            let _ = s.save();
            w.set_output_highlight_rules(output_highlight_rule_model(&s));
            // 规则清单变了，上一次的"规则已添加/已删除"提示文案必须清掉。
            w.set_output_highlight_rule_status("".into());
            apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
            w.set_output_highlight_rule_status("".into());
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_output_highlight_rule_enabled(move |index, enabled| {
            let Some(w) = weak.upgrade() else { return };
            let mut s = store.borrow_mut();
            s.set_output_highlight_rule_enabled(index.max(0) as usize, enabled);
            let _ = s.save();
            w.set_output_highlight_rules(output_highlight_rule_model(&s));
            // 规则清单变了，上一次的"规则已添加/已删除"提示文案必须清掉。
            w.set_output_highlight_rule_status("".into());
            apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font(move |label: SharedString| {
            // The picker labels entries with their source; store only the
            // bare family name so the config stays portable. Group headers
            // (▍…) are not selectable — ignore them.
            let Some(family) = family_from_label(&label) else {
                return;
            };
            {
                let mut s = store.borrow_mut();
                s.set_font_family(family.to_string());
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_font_family(family.into());
                w.set_term_font_cjk(term_font_covers_cjk(family));
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_output_highlight(move |enabled, preset: SharedString| {
            let preset = preset.to_string();
            {
                let mut s = store.borrow_mut();
                s.set_output_highlight_enabled(enabled);
                s.set_output_highlight_preset(preset.clone());
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                apply_output_highlight(&w, &bufs, enabled, &preset);
            }
        });
    }

    {
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_json_format_output(move |enabled| {
            {
                let mut s = store.borrow_mut();
                s.set_json_format_output(enabled);
                let _ = s.save();
            }
            // Flip live buffers so the change applies without reconnecting.
            for buffer in bufs.lock().unwrap_or_else(|e| e.into_inner()).values() {
                buffer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .json_format_output = enabled;
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font_size(move |size: i32| {
            {
                let mut s = store.borrow_mut();
                s.set_font_size(size as u32);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_font_size(size as f32);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_font_bold(move |bold: bool| {
            {
                let mut s = store.borrow_mut();
                s.set_terminal_bold(bold);
                let _ = s.save();
            }
            if let Some(w) = weak.upgrade() {
                w.set_term_font_bold(bold);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_scrollback_lines(move |lines: slint::SharedString| -> bool {
            // Validate: 100..=1_000_000. Malformed input is rejected (UI shows
            // the invalid state) and nothing is persisted.
            let digits: String = lines.chars().filter(|c| c.is_ascii_digit()).collect();
            match digits.parse::<usize>() {
                Ok(n) if (100..=1_000_000).contains(&n) => {
                    let mut s = store.borrow_mut();
                    s.set_scrollback_lines(n);
                    let _ = s.save();
                    // Write the canonical value back to the UI so the settings
                    // panel (conditionally rendered) shows the new value when
                    // reopened — without this it reverts to the stale one.
                    if let Some(w) = weak.upgrade() {
                        w.set_scrollback_lines(digits.into());
                    }
                    true
                }
                _ => false,
            }
        });
    }

    {
        let store = store.clone();
        window.on_set_convert_eol(move |v: bool| {
            let mut s = store.borrow_mut();
            s.set_convert_eol(v);
            let _ = s.save();
        });
    }

    {
        let store = store.clone();
        window.on_set_osc52_clipboard(move |v: bool| {
            let mut s = store.borrow_mut();
            s.set_osc52_clipboard(v);
            let _ = s.save();
            crate::terminal::vt_adapter::OSC52_ENABLED
                .store(v, std::sync::atomic::Ordering::Relaxed);
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
                let _ = s.save();
                normalized
            };
            if let Some(w) = weak.upgrade() {
                w.set_term_cursor_style(normalized.into());
            }
        });
    }
}

/// 终端页：字体 / 光标 / 回滚 / 高亮 / 粘贴行尾 / OSC52 / JSON 格式化
pub(crate) fn reset(w: &AppWindow, store: &Store, bufs: &TermBuffers, fonts: &FontCatalog) {
    let d = crate::config::fresh_config();
    {
        let mut s = store.borrow_mut();
        s.set_font_family(d.terminal.font_family.clone());
        s.set_font_size(d.terminal.font_size);
        s.set_terminal_bold(d.terminal.terminal_bold);
        s.set_terminal_cursor_style(d.terminal.terminal_cursor_style.clone());
        s.set_terminal_cursor_color(&d.terminal.terminal_cursor_color);
        s.set_scrollback_lines(d.terminal.scrollback_lines);
        s.set_output_highlight_enabled(!d.terminal.output_highlight_disabled);
        s.set_output_highlight_preset(d.terminal.output_highlight_preset.clone());
        // 终端页其余 A 类项：粘贴行尾 / OSC52 / JSON 格式化
        s.set_convert_eol(d.terminal.convert_eol);
        s.set_osc52_clipboard(d.terminal.osc52_clipboard);
        s.set_json_format_output(!d.terminal.json_format_disabled);
        // B 类：自定义规则数据保留，仅取消使用（enabled=false）
        for index in 0..s.output_highlight_rules().len() {
            s.set_output_highlight_rule_enabled(index, false);
        }
        if let Err(error) = s.save() {
            tracing::warn!("failed to save config: {error:#}");
        }
    }
    // UI 刷新走 getter（带 0 → 默认 的哨兵映射），保证显示的就是真实生效值，
    // 而不是把派生 Default 的 0 / "" 原样写进控件。
    let rules;
    {
        let s = store.borrow();
        w.set_term_font_family(s.font_family().into());
        // 选择器索引必须跟着 family 一起还原，否则下拉框停在旧项：显示与实际
        // 字体不符，用户再动一次选择器还会用旧索引反推回旧字体。
        w.set_term_font_index(fonts.term_index(s.font_family()));
        w.set_term_font_size(s.font_size() as f32);
        w.set_term_font_bold(s.terminal_bold());
        w.set_term_cursor_style(s.terminal_cursor_style().into());
        w.set_term_cursor_color_hex(s.terminal_cursor_color().into());
        if let Some(color) = parse_hex_color(s.terminal_cursor_color()) {
            w.set_term_cursor_color(color);
        }
        w.set_scrollback_lines(s.scrollback_lines().to_string().into());
        w.set_output_highlight_enabled(s.output_highlight_enabled());
        w.set_output_highlight_preset(s.output_highlight_preset().into());
        w.set_output_highlight_rules(output_highlight_rule_model(&s));
        // 规则清单变了，上一次的"规则已添加/已删除"提示文案必须清掉。
        w.set_output_highlight_rule_status("".into());
        w.set_convert_eol(s.convert_eol());
        w.set_osc52_clipboard(s.osc52_clipboard());
        w.set_json_format_output(s.json_format_output());
        rules = s.output_highlight_rules().to_vec();
    }
    // 回滚行数变更 → 终端缓冲 reset；高亮按新 preset / 规则重编译。
    for_each_buffer(w, bufs, |b| b.reset(d.terminal.scrollback_lines));
    apply_output_highlight(
        w,
        bufs,
        !d.terminal.output_highlight_disabled,
        &d.terminal.output_highlight_preset,
    );
    apply_custom_output_rules(w, bufs, &rules);
}
