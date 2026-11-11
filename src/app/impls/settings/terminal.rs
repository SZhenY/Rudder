//! 终端页：字体 / 光标 / 回滚 / 高亮 / 粘贴行尾 / OSC52 / JSON 格式化。
//!
//! 本页落点最多，几条容易漏的：
//! * `term-font-index`（字体选择器索引）—— 只改 family 会让下拉框停在旧项；
//! * 高亮规则变化后要刷新 `output-highlight-rule-status`；
//! * 回滚行数变化要让每个终端缓冲 reset；
//! * OSC52 开关要同步 `vt_adapter::OSC52_ENABLED` 这个运行时原子标志。

use slint::{ComponentHandle, Model as _, SharedString};

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

/// 光标色的解析结果：`stored` 为空（跟随主题）时用 `follow_hex`（= 当前主题色，有壁纸时
/// 即壁纸互补色）；用户显式挑过就用那个值（不随主题变）。
pub(crate) fn resolve_cursor_color(follow_hex: &str, stored: &str) -> String {
    if stored.is_empty() {
        follow_hex.to_string()
    } else {
        stored.to_string()
    }
}

/// 「跟随主题」时光标该用的颜色：当前主题色（有壁纸 = 壁纸互补色）。
fn follow_cursor_hex(w: &AppWindow) -> String {
    let c = w.global::<Theme>().get_accent_solid();
    hex_from_rgb(c.red() as i32, c.green() as i32, c.blue() as i32)
}

/// 是不是"回填造成的回声"：输入框显示的是我们回填的「跟随」值（= 当前主题色）。
/// `stored` 非空（用户显式选过颜色）时不存在"跟随"，直接不算回声。
fn is_follow_echo(v: &str, stored: &str, follow_hex: &str) -> bool {
    stored.is_empty() && v.eq_ignore_ascii_case(follow_hex)
}

/// 把光标色写到窗口 —— 并在"跟随主题"时按**当前深浅档**解析。
///
/// ⚠️ 换深浅档后**必须再调一次**，与「配色」分区的主题色是同一个道理。
pub(crate) fn apply_cursor_color(w: &AppWindow, stored: &str) {
    let follow = follow_cursor_hex(w);
    apply_cursor_color_with(w, stored, &follow);
}

/// 与 [`apply_cursor_color`] 相同，但「跟随主题」的取值由调用方给出。
///
/// 启动播种时要走这条：那时 `Theme.accent-solid` 的过渡动画还没开始，直接读全局只能拿到
/// 起始帧（出厂蓝），所以必须把 `apply_accent` 算出的生效色显式传进来。
pub(crate) fn apply_cursor_color_with(w: &AppWindow, stored: &str, follow_hex: &str) {
    let effective = resolve_cursor_color(follow_hex, stored);
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
#[path = "terminal_bind_cursor.rs"] mod terminal_bind_cursor;
#[path = "terminal_bind_highlight.rs"] mod terminal_bind_highlight;
#[path = "terminal_bind_output.rs"] mod terminal_bind_output;

pub(crate) fn bind(window: &AppWindow, store: &Store, bufs: &TermBuffers) {
    terminal_bind_cursor::wire_cursor(window, store, bufs);
    terminal_bind_highlight::wire_highlight(window, store, bufs);
    terminal_bind_output::wire_output(window, store);
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

    /// 「回声」判定（光标色）：跟随值时不该被当成用户改动（大小写不敏感）。
    #[test]
    fn follow_echo_ignores_both_modes() {
        assert!(is_follow_echo("#FF8800", "", "#ff8800"));
        assert!(!is_follow_echo("#D4D4D4", "", "#FF8800"));
        // 用户显式选过颜色 → 没有"跟随"这回事。
        assert!(!is_follow_echo("#FF8800", "#FF8800", "#FF8800"));
    }

    /// 光标色跟随主题色；用户显式挑过则以显式值为准。
    #[test]
    fn cursor_color_follows_theme_until_set_explicitly() {
        // 跟随：用传进来的 follow（= 当前主题色 / 壁纸互补色）。
        assert_eq!(resolve_cursor_color("#FF8800", ""), "#FF8800");
        // 显式值不随主题变。
        assert_eq!(resolve_cursor_color("#FF8800", "#123456"), "#123456");
        assert_eq!(resolve_cursor_color("#000000", "#123456"), "#123456");
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
