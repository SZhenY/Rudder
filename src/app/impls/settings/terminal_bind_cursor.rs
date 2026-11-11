use super::*;

pub(super) fn wire_cursor(window: &AppWindow, store: &Store, bufs: &TermBuffers) {
    {
        // 推荐色块的专用入口：**用户点一下就是明确选择** → 直接存，不走下面那条「回声」判定。
        //
        // 为什么必须分开：回声判定的目的是别让"程序化回填输入框"把"跟随主题"写死；但它的判据
        // 是"这个值 == 当前解析出来的生效色"，于是**深色档下点 `#D4D4D4`**（正好等于跟随主题
        // 的解析结果）会被当成回填而忽略掉 —— 用户看到的就是"点了没反应"（#FFFFFF 更早还有
        // 一层历史归一化，点它会高亮到第一项）。
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_cursor_preset(move |value: SharedString| {
            let v = value.as_str().trim();
            if !v.is_empty() && parse_hex_color(v).is_none() {
                return;
            }
            {
                let mut s = store.borrow_mut();
                if s.terminal_cursor_color() != v {
                    if !s.set_terminal_cursor_color(v) {
                        return;
                    }
                    s.save_logging();
                }
            }
            if let Some(w) = weak.upgrade() {
                let stored = store.borrow().terminal_cursor_color().to_string();
                apply_cursor_color(&w, &stored);
            }
        });
    }

    {
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_cursor_color(move |value: SharedString| {
            // 空串 = 回到"跟随主题"；其余必须是合法 hex（与界面上的红框校验一致）。
            let v = value.as_str().trim();
            if !v.is_empty() && parse_hex_color(v).is_none() {
                return false;
            }
            let Some(w) = weak.upgrade() else {
                return false;
            };
            // 「回声」判定：跟随光标显示的就是当前主题色，回填触发时不算用户改动。
            {
                let stored = store.borrow().terminal_cursor_color().to_string();
                if !v.is_empty() && is_follow_echo(v, &stored, &follow_cursor_hex(&w)) {
                    return true;
                }
            }
            {
                let mut s = store.borrow_mut();
                // 播种 / 换深浅档的程序化回填也走这条回调：值没变就**不要写盘**
                // （否则每次启动都会把配置重写一遍，还让"上次修改时间"平白变化）。
                if s.terminal_cursor_color() != v {
                    if !s.set_terminal_cursor_color(v) {
                        return false;
                    }
                    s.save_logging();
                }
            }
            let stored = store.borrow().terminal_cursor_color().to_string();
            apply_cursor_color(&w, &stored);
            true
        });
    }

    {
        // 调色盘提交（拖动松手时一次）：Slint 侧没有 hex 格式化能力，送过来的是三个通道值。
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_cursor_color_rgb(move |red: i32, green: i32, blue: i32| {
            let hex = hex_from_rgb(red, green, blue);
            let Some(w) = weak.upgrade() else {
                return false;
            };
            // 与 hex 输入同一条「回声」判定：拖到与当前主题色相同的位置时保持"跟随"。
            {
                let stored = store.borrow().terminal_cursor_color().to_string();
                if is_follow_echo(&hex, &stored, &follow_cursor_hex(&w)) {
                    return true;
                }
            }
            {
                let mut s = store.borrow_mut();
                if !s.set_terminal_cursor_color(&hex) {
                    return false;
                }
                s.save_logging();
            }
            let stored = store.borrow().terminal_cursor_color().to_string();
            apply_cursor_color(&w, &stored);
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
                    s.save_logging();
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
            s.save_logging();
            w.set_output_highlight_rules(output_highlight_rule_model(&s));
            // 规则清单变了，上一次的"规则已添加/已删除"提示文案必须清掉。
            w.set_output_highlight_rule_status("".into());
            apply_custom_output_rules(&w, &bufs, s.output_highlight_rules());
            w.set_output_highlight_rule_status("".into());
        });
    }
}
