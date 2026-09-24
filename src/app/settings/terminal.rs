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
    apply_custom_output_rules, apply_output_highlight, for_each_buffer, hex_from_rgb,
    output_highlight_rule_model, parse_hex_color, validate_output_highlight_rule,
};
use crate::config::OutputHighlightRule;
use crate::i18n::t;

use super::{FontCatalog, Store};
use crate::terminal::TermBuffers;
use crate::ui::{ AppWindow, Theme };

/// 光标色的解析结果：空串（跟随主题）时按深浅档取 —— **深色档亮色 / 浅色档暗色**。
///
/// 两个取值沿用 `Theme.term-fg`（终端正文色）：光标与正文同色系，深浅两档都读得清。
/// 用户显式挑过就用那个值（不随主题变）。
pub(crate) fn resolve_cursor_color(dark: bool, stored: &str) -> String {
    if stored.is_empty() {
        return (if dark { "#D4D4D4" } else { "#2D2D2F" }).to_string();
    }
    stored.to_string()
}

/// 是不是"回填造成的回声"。
///
/// 输入框里显示的是我们**回填**的解析结果，而 Slint 的 `changed` 可能**晚于**回填执行 ——
/// 设置页是在打开面板时才创建的，那时深浅档可能已经翻过一轮。所以两档的取值都认；另外
/// `stored` 非空（用户显式选过颜色）时不存在"跟随"，直接不算回声。
fn is_follow_echo(v: &str, stored: &str) -> bool {
    stored.is_empty()
        && (v.eq_ignore_ascii_case(&resolve_cursor_color(true, ""))
            || v.eq_ignore_ascii_case(&resolve_cursor_color(false, "")))
}

/// 把光标色写到窗口 —— 并在"跟随主题"时按**当前深浅档**解析。
///
/// ⚠️ 换深浅档后**必须再调一次**，与「配色」分区的主题色是同一个道理。
pub(crate) fn apply_cursor_color(w: &AppWindow, stored: &str) {
    let dark = w.global::<Theme>().get_dark();
    let effective = resolve_cursor_color(dark, stored);
    // 输入框回填**当前生效的颜色**（跟随主题时就是解析出来的那个色号）—— 用户因此始终
    // 看得到真实值。这次程序化回填会触发输入框的 `changed text` → `on_set_term_cursor_color`，
    // 那里的「回声」判定把它当无操作，不会把"跟随主题"写死成具体颜色。
    w.set_term_cursor_color_hex(effective.as_str().into());
    // 色块的选中态看**存储值**（"" = 跟随主题那一项），输入框看生效色 —— 两者本就不同。
    w.set_term_cursor_choice(stored.into());
    if let Some(color) = parse_hex_color(&effective) {
        w.set_term_cursor_color(color);
    }
}

/// 播种 + 注册持久化回调。
pub(crate) fn bind(window: &AppWindow, store: &Store, bufs: &TermBuffers) {
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
            // 「回声」判定：输入框里显示的本来就是当前生效色（跟随主题时是解析结果），
            // 那次程序化回填会走到这里 —— 当成改动的话，"跟随主题"就被写死成具体颜色了。
            {
                let stored = store.borrow().terminal_cursor_color().to_string();
                if !v.is_empty() && is_follow_echo(v, &stored) {
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
            if let Some(w) = weak.upgrade() {
                let stored = store.borrow().terminal_cursor_color().to_string();
                apply_cursor_color(&w, &stored);
            }
            true
        });
    }

    {
        // 调色盘提交（拖动松手时一次）：Slint 侧没有 hex 格式化能力，送过来的是三个通道值。
        let weak = window.as_weak();
        let store = store.clone();
        window.on_set_term_cursor_color_rgb(move |red: i32, green: i32, blue: i32| {
            let hex = hex_from_rgb(red, green, blue);
            // 与 hex 输入同一条「回声」判定：拖到与当前生效色相同的位置时保持"跟随主题"。
            {
                let stored = store.borrow().terminal_cursor_color().to_string();
                if is_follow_echo(&hex, &stored) {
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
            if let Some(w) = weak.upgrade() {
                let stored = store.borrow().terminal_cursor_color().to_string();
                apply_cursor_color(&w, &stored);
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

    {
        let weak = window.as_weak();
        let store = store.clone();
        let bufs = bufs.clone();
        window.on_set_output_highlight_rule_enabled(move |index, enabled| {
            let Some(w) = weak.upgrade() else { return };
            let mut s = store.borrow_mut();
            s.set_output_highlight_rule_enabled(index.max(0) as usize, enabled);
            s.save_logging();
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
                s.save_logging();
            }
            if let Some(w) = weak.upgrade() {
                w.global::<Theme>().set_term_font_family(family.into());
                w.global::<Theme>().set_term_font_cjk(term_font_covers_cjk(family));
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
                s.save_logging();
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
                s.save_logging();
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
            // 字号滑条是逐帧触发的 → 走防抖出口（见 `settings::persist`）。
            super::persist(&store, |s| s.set_font_size(size as u32));
            if let Some(w) = weak.upgrade() {
                w.global::<Theme>().set_term_font_size(size as f32);
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
        let store = store.clone();
        window.on_set_convert_eol(move |v: bool| {
            let mut s = store.borrow_mut();
            s.set_convert_eol(v);
            s.save_logging();
        });
    }

    {
        let store = store.clone();
        window.on_set_osc52_clipboard(move |v: bool| {
            let mut s = store.borrow_mut();
            s.set_osc52_clipboard(v);
            s.save_logging();
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
                s.save_logging();
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
    // 回滚行数是 `Term` 的**构造期**参数：改它必须重建 alacritty 的网格，代价是
    // 屏幕与回滚全部清空。还原时它通常本来就等于默认值 —— 那种情况下重建只会把
    // 一个正在使用的会话清成空白（用户看到的就是"还原后终端一片黑，动一下字号
    // 才回来"，因为改字号触发 resize、shell 收到 SIGWINCH 才重绘）。
    // 所以只有值真的变了才重建。
    let prev_scrollback = store.borrow().scrollback_lines();
    {
        let mut s = store.borrow_mut();
        s.set_font_family(d.terminal.font_family.clone());
        s.set_font_size(d.terminal.font_size);
        s.set_terminal_bold(d.terminal.terminal_bold);
        s.set_terminal_cursor_style(d.terminal.terminal_cursor_style.clone());
        s.set_terminal_cursor_color(&d.terminal.terminal_cursor_color);
        // 先恢复开关（它决定 `set_scrollback_lines` 的 clamp 上界），再恢复行数。
        s.set_large_scrollback(d.terminal.large_scrollback);
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
        w.global::<Theme>().set_term_font_family(s.font_family().into());
        // 选择器索引必须跟着 family 一起还原，否则下拉框停在旧项：显示与实际
        // 字体不符，用户再动一次选择器还会用旧索引反推回旧字体。
        w.set_term_font_index(fonts.term_index(s.font_family()));
        w.global::<Theme>().set_term_font_size(s.font_size() as f32);
        w.global::<Theme>().set_term_font_bold(s.terminal_bold());
        w.set_term_cursor_style(s.terminal_cursor_style().into());
        // 跟随主题时按还原后的深浅档解析（还原出厂默认 = 空串 → 跟随主题）。
        apply_cursor_color(w, s.terminal_cursor_color());
        w.set_scrollback_lines(s.scrollback_lines().to_string().into());
        w.set_large_scrollback(s.large_scrollback());
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
    // 回滚行数**变更** → 终端缓冲 reset；高亮按新 preset / 规则重编译。
    if prev_scrollback != d.terminal.scrollback_lines {
        for_each_buffer(w, bufs, |b| b.reset(d.terminal.scrollback_lines));
    }
    apply_output_highlight(
        w,
        bufs,
        !d.terminal.output_highlight_disabled,
        &d.terminal.output_highlight_preset,
    );
    apply_custom_output_rules(w, bufs, &rules);
}
/// 解析用户输入的滚动行数：**只取数字**（`"10_000"` -> 10000），且必须落在
/// `100..=max` 内，否则拒绝 —— 拒绝时界面显示非法状态、不写入任何东西。
fn parse_scrollback(raw: &str, max: usize) -> Option<usize> {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    match digits.parse::<usize>() {
        Ok(n) if (100..=max).contains(&n) => Some(n),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 「回声」判定（光标色）：跟随主题时，两档的解析结果都不该被当成用户改动
    /// （设置页是打开面板时才创建的，`changed` 可能晚于回填、期间深浅档已翻过一轮）。
    #[test]
    fn follow_echo_ignores_both_modes() {
        assert!(is_follow_echo("#D4D4D4", ""));
        assert!(is_follow_echo("#2d2d2f", "")); // 大小写不敏感
        assert!(!is_follow_echo("#FF8800", ""));
        // 用户显式选过颜色 → 没有"跟随"这回事。
        assert!(!is_follow_echo("#D4D4D4", "#FF8800"));
    }

    /// 光标色按深浅档解析：**深色档亮 / 浅色档暗**；用户显式挑过则以显式值为准。
    #[test]
    fn cursor_color_follows_theme_until_set_explicitly() {
        assert_eq!(resolve_cursor_color(true, ""), "#D4D4D4");
        assert_eq!(resolve_cursor_color(false, ""), "#2D2D2F");
        // 显式值不随主题变。
        assert_eq!(resolve_cursor_color(true, "#FF8800"), "#FF8800");
        assert_eq!(resolve_cursor_color(false, "#FF8800"), "#FF8800");
    }

    #[test]
    fn parse_scrollback_accepts_the_valid_range_only() {
        assert_eq!(parse_scrollback("100", 100_000), Some(100), "下界");
        assert_eq!(parse_scrollback("5000", 100_000), Some(5_000));
        assert_eq!(parse_scrollback("100000", 100_000), Some(100_000), "上界");
        assert_eq!(parse_scrollback("99", 100_000), None, "低于下界");
        assert_eq!(parse_scrollback("100001", 100_000), None, "超过常规上限");
        assert_eq!(parse_scrollback("", 100_000), None, "空输入");
    }

    /// 只取数字：分隔符被丢掉；全非数字则拒绝（**不能静默变成 0 写进配置**）。
    #[test]
    fn parse_scrollback_strips_non_digits() {
        assert_eq!(parse_scrollback("10_000", 100_000), Some(10_000));
        assert_eq!(parse_scrollback("5 000", 100_000), Some(5_000));
        assert_eq!(parse_scrollback("abc", 100_000), None);
        assert_eq!(parse_scrollback("abc0", 100_000), None, "剩下的 0 也低于下界");
    }

    /// 上界由「大回滚缓冲区」开关决定：同一个 1 000 000，关着要拒、开着要收。
    #[test]
    fn parse_scrollback_upper_bound_follows_the_large_switch() {
        assert_eq!(
            parse_scrollback("1000000", crate::config::SCROLLBACK_MAX),
            None
        );
        assert_eq!(
            parse_scrollback("1000000", crate::config::SCROLLBACK_MAX_LARGE),
            Some(1_000_000)
        );
    }
}
