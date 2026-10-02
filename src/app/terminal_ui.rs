use super::*;
use crate::ui::{ AnimationSettings, Theme };

pub(crate) fn history_model(store: &ConfigStore) -> ModelRc<SharedString> {
    let rows: Vec<SharedString> = store
        .command_history()
        .iter()
        .map(|s| s.clone().into())
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

pub(super) fn output_highlight_rule_model(store: &ConfigStore) -> ModelRc<OutputRuleItem> {
    let rows: Vec<OutputRuleItem> = store
        .output_highlight_rules()
        .iter()
        .map(|rule| OutputRuleItem {
            pattern: rule.pattern.clone().into(),
            regex: rule.regex,
            case_sensitive: rule.case_sensitive,
            whole_line: rule.whole_line,
            color: if crate::terminal::highlight_color_index(&rule.color) != 9 {
                rule.color.clone()
            } else {
                "red".to_string()
            }
            .into(),
            enabled: rule.enabled,
        })
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

pub(super) fn parse_hex_color(value: &str) -> Option<slint::Color> {
    let digits = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let red = u8::from_str_radix(&digits[0..2], 16).ok()?;
    let green = u8::from_str_radix(&digits[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&digits[4..6], 16).ok()?;
    Some(slint::Color::from_rgb_u8(red, green, blue))
}

/// `parse_hex_color` 的反函数：三个通道 → `#RRGGBB`（大写）。
///
/// 需要它是因为 Slint 侧**没有 hex 格式化能力**：调色盘只能把分量送过来，由这边转成
/// 配置里那种规范形式（`normalize_*` 只认 6 位十六进制）。
pub(super) fn hex_from_rgb(red: i32, green: i32, blue: i32) -> String {
    let c = |v: i32| v.clamp(0, 255) as u8;
    format!("#{:02X}{:02X}{:02X}", c(red), c(green), c(blue))
}

pub(super) fn validate_output_highlight_rule(
    pattern: &str,
    is_regex: bool,
    case_sensitive: bool,
) -> std::result::Result<(), String> {
    if pattern.is_empty() {
        return Err(t(
            "请输入关键词或正则表达式",
            "Enter a keyword or regular expression",
        )
        .into());
    }
    if pattern.chars().count() > 512 {
        return Err(t(
            "规则不能超过 512 个字符",
            "Rules cannot exceed 512 characters",
        )
        .into());
    }
    if is_regex {
        regex::RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|error| {
                format!(
                    "{}: {error}",
                    t("无效的正则表达式", "Invalid regular expression")
                )
            })?;
    }
    Ok(())
}

pub(super) fn history_view_rows(history: &[String], query: &str) -> Vec<SharedString> {
    let q = query.trim().to_lowercase();
    // Oldest first, newest at the bottom — same storage order as ↑/↓ recall
    // (#55, #101). Upstream 0.7.1 reverted the #331 newest-first flip.
    history
        .iter()
        .filter(|command| q.is_empty() || command.to_lowercase().contains(&q))
        .map(|command| command.clone().into())
        .collect()
}

pub(crate) fn history_view_model(store: &ConfigStore, query: &str) -> ModelRc<SharedString> {
    let rows = history_view_rows(store.command_history(), query);
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

/// 与 [`history_view_model`] 逐项对齐的**显示用**单行预览（上游 9725617）。
///
/// 历史上一条命令可能是多行（heredoc / 连续的命令），而历史行是固定 28px 高 —— 多行文本
/// 会溢出到相邻行。列表只显示压缩后的单行版本；回填 / ▶ 运行 / 复制仍用原始字符串。
pub(crate) fn history_preview_model(store: &ConfigStore, query: &str) -> ModelRc<SharedString> {
    let rows: Vec<SharedString> = history_view_rows(store.command_history(), query)
        .into_iter()
        .map(|row| crate::app::quick_commands::command_preview(row.as_str()).into())
        .collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

pub(crate) fn compute_find_matches(rows: &[String], query: &str) -> Vec<TermMatch> {
    let mut out: Vec<TermMatch> = Vec::new();
    if query.is_empty() {
        return out;
    }
    let q: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    if q.is_empty() {
        return out;
    }
    for (r, line) in rows.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let lower: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
        let prefix = cell_prefix(&chars);
        let mut i = 0usize;
        while i + q.len() <= lower.len() {
            if lower[i..i + q.len()] == q[..] {
                let col = prefix[i] as i32;
                let len = (prefix[i + q.len()] - prefix[i]) as i32;
                out.push(TermMatch {
                    row: r as i32,
                    col,
                    len,
                });
                i += q.len();
            } else {
                i += 1;
            }
        }
    }
    out
}

pub(crate) fn apply_terminal_resize(
    handles: &Rc<RefCell<HashMap<String, SessionHandle>>>,
    bufs: &TermBuffers,
    last_term_size: &Arc<Mutex<(u32, u32)>>,
    tab_id: &str,
    cols: u32,
    rows: u32,
) {
    *last_term_size.lock().unwrap_or_else(|e| e.into_inner()) = (cols, rows);
    if let Some(handle) = handles.borrow().get(tab_id) {
        handle.resize(cols, rows);
    }
    if let Some(h) = term_buf(bufs, tab_id) {
        let mut buf = h.lock().unwrap_or_else(|e| e.into_inner());
        let (old_rows, old_cols) = crate::terminal::term_size(&buf.term);
        let (new_rows, new_cols) = (rows as u16, cols as u16);
        if (new_rows, new_cols) != (old_rows, old_cols) {
            if crate::terminal::is_alt(&buf.term) {
                // Alt-screen (tmux/vim/btop): the remote redraws the whole screen
                // on SIGWINCH, so just resize the grid and let that redraw fill it.
                crate::terminal::resize_term(&mut buf.term, new_rows, new_cols);
                // 但列宽变了 ⇒ 两类缓存里按**旧列宽**算出的 col/cells 不能再命中
                //（非 alt 的 `reflow()` 会做同样两件事，alt 分支以前漏了）。
                buf.bump_render_gen();
                buf.drop_scroll_cache();
            } else {
                // Reflow already-printed output to the new width by replaying the
                // byte stream — vt100's set_size only truncates/pads (#169).
                buf.reflow(new_rows, new_cols);
            }
        }
    }
}

/// 一个标签页渲染所需的全套计算结果（纯数据，不碰 Slint）。
pub(crate) struct TabDisplay {
    pub(crate) screen: crate::terminal::BuiltScreen,
    pub(crate) matches: Vec<TermMatch>,
    pub(crate) selection: Vec<TermMatch>,
}

/// 把终端缓冲区算成 UI 需要的那组值。**纯计算**、不接触 Slint 句柄，
/// 因此可以直接单测（`rebuild_tab_display` 只剩"把结果写进模型"这一步）。
///
/// 搜索高亮的坐标在这里定型：`matches` 的 `col`/`len` 是**终端单元格**数
/// （CJK 一格占 2），与 `spans` 的绘制单位一致 —— 这两者必须同源，否则高亮会偏移。
pub(crate) fn compute_tab_display(buf: &mut TermBuffer) -> TabDisplay {
    let cols = crate::terminal::term_size(&buf.term).1;
    let b = buf.render(); // also refreshes buf.displayed_text
    // 阶段 0 的观测点：默认不打印，RUST_LOG=rudder::perf=debug 时每帧一行。
    tracing::debug!(
        target: "rudder::perf",
        rebuilt = buf.frame_stats.rebuilt,
        reused = buf.frame_stats.reused,
        spans = b.spans.len(),
        "frame"
    );
    let matches = compute_find_matches(&buf.displayed_text, &buf.find_query);
    let selection = buf.selection_rects_visible(cols);
    TabDisplay {
        screen: b,
        matches,
        selection,
    }
}

/// 就地写模型（复用 `VecModel`、只写变化项）；模型不可复用时新建。返回"内容是否变化"。
fn write_or_replace<T: Clone + PartialEq + 'static>(slot: &mut ModelRc<T>, next: &[T]) -> bool {
    match super::resource_ui::try_write_rows_changed(slot, next) {
        Some(changed) => changed,
        None => {
            *slot = ModelRc::from(Rc::new(VecModel::from(next.to_vec())));
            true
        }
    }
}

pub(crate) fn rebuild_tab_display(win: &AppWindow, bufs: &TermBuffers, tab_id: &str) {
    let Some(display) = with_term_buf(bufs, tab_id, compute_tab_display) else {
        return;
    };
    let TabDisplay {
        screen,
        matches,
        selection,
    } = display;
    let (cr, cc, ru, alt) = (
        screen.cursor_row,
        screen.cursor_col,
        screen.rows_used,
        screen.is_alt,
    );
    let (smax, soff) = (screen.scroll_max, screen.scroll_offset);
    let mouse_tracked = screen.mouse_tracked;
    // 有没有任何东西真的变了 —— 决定最后要不要请求重绘。**这不是小事**：`request_redraw()`
    // 会让 Slint 重画整个窗口（在 wgpu 档上还要重新申请一批 GPU 资源，我们量到过 ~100MB 的
    // 前台峰值），而终端在空闲时（只有光标闪烁那类定时器在跑）本来没有内容变化。
    let changed = Rc::new(std::cell::Cell::new(false));
    let changed_in = changed.clone();
    set_terminal_row(win, tab_id, move |row| {
        // **增量写**，复用行里已有的 `VecModel`：以前每帧都 `VecModel::from(...)` 再整体替换，
        // Repeater 会把这一行的每一项都当成新的重建 —— 而一屏是**上千个 span**（逐单元格产出），
        // 这是终端刷屏时最贵的一笔固定开销。现在的写法只在内容真的变了时才通知 Slint。
        if write_or_replace(&mut row.spans, &screen.spans) {
            changed_in.set(true);
        }
        if row.cursor_row != cr
            || row.cursor_col != cc
            || row.rows_used != ru
            || row.is_alt_screen != alt
            || row.mouse_tracked != mouse_tracked
            || row.scroll_max != smax
            || row.scroll_offset != soff
        {
            changed_in.set(true);
        }
        row.cursor_row = cr;
        row.cursor_col = cc;
        row.rows_used = ru;
        row.is_alt_screen = alt;
        row.mouse_tracked = mouse_tracked;
        if write_or_replace(&mut row.find_matches, &matches) {
            changed_in.set(true);
        }
        if write_or_replace(&mut row.selection, &selection) {
            changed_in.set(true);
        }
        row.scroll_max = smax;
        row.scroll_offset = soff;
    });
    if changed.get() {
        win.window().request_redraw();
    }
}

pub(crate) fn refresh_terminal_selection(win: &AppWindow, bufs: &TermBuffers, tab_id: &str) {
    let selection = with_term_buf(bufs, tab_id, |buf| {
        let cols = crate::terminal::term_size(&buf.term).1;
        buf.selection_rects_visible(cols)
    });
    let Some(selection) = selection else {
        return;
    };
    let model = ModelRc::from(Rc::new(VecModel::from(selection)));
    set_terminal_row(win, tab_id, move |row| {
        row.selection = model.clone();
    });
    win.window().request_redraw();
}

/// Apply `apply` to every open terminal buffer, then rebuild each tab.
///
/// The three `apply_*` settings tweaks below are identical apart from the field
/// they touch, so the walk lives here. Handles are collected under one short
/// lock and each buffer is then locked individually: a poisoned buffer skips
/// just itself instead of aborting the process (release uses panic = "abort").
pub(super) fn for_each_buffer(window: &AppWindow, bufs: &TermBuffers, apply: impl Fn(&mut TermBuffer)) {
    let Ok(map) = bufs.lock() else {
        return;
    };
    let handles: Vec<TermBufferHandle> = map.values().cloned().collect();
    let tab_ids: Vec<String> = map.keys().cloned().collect();
    drop(map);
    for h in handles {
        if let Ok(mut b) = h.lock() {
            // The apply callbacks below change how lines are coloured, which
            // no amount of grid damage tracking can detect — the caches have
            // to be invalidated explicitly or stale spans stay on screen.
            b.bump_render_gen();
            apply(&mut b);
        }
    }
    for tid in tab_ids {
        rebuild_tab_display(window, bufs, &tid);
    }
}

pub(super) fn apply_dark_mode(window: &AppWindow, bufs: &TermBuffers, dark: bool) {
    window.global::<Theme>().set_dark(dark);
    for_each_buffer(window, bufs, |b| b.is_dark = dark);
}

pub(super) fn apply_output_highlight(
    window: &AppWindow,
    bufs: &TermBuffers,
    enabled: bool,
    preset: &str,
) {
    let mode = OutputHighlightPreset::from_settings(enabled, preset);
    for_each_buffer(window, bufs, |b| b.output_highlight = mode);
}

pub(super) fn apply_custom_output_rules(
    window: &AppWindow,
    bufs: &TermBuffers,
    rules: &[OutputHighlightRule],
) {
    let compiled = compile_output_rules(rules);
    for_each_buffer(window, bufs, |b| b.custom_highlight_rules = compiled.clone());
}

/// 换壁纸的 generation 计数：只有**最新一次**切换的定时器才允许改状态。
/// 快速连切壁纸时，上一次切换留下的 480ms 撤层定时器仍会到点，把 `fading`
/// 提前置 false —— 把进行中的淡化硬切掉，于是又闪一次。
static WALLPAPER_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 交叉淡化的起点与时长：Slint 侧 prev 图在 30ms（等首帧）后开始淡出、400ms 淡完。
/// 派生色的插值必须落在**同一区间**且按**真实时间**推进（不是计步），这样即使
/// 定时器有抖动，背景层与壁纸图像也严格同步演化 —— 全程没有跳变。
const FADE_START_MS: f32 = 30.0;
const FADE_LEN_MS: f32 = 400.0;

/// 两个颜色之间线性插值。图像淡化的曲线实测就是线性（`e-standard-decelerate`
/// = `cubic-bezier(0, 0, 0, 1)`，其 x(t) 与 y(t) 相等），所以这里也用线性 ——
/// 背景层与图像才会同步演化。
fn lerp_color(a: slint::Color, b: slint::Color, t: f32) -> slint::Color {
    // `Color::red()/green()/blue()` 返回 u8（0–255），在 u8 空间插值再回填。
    let m = |x: u8, y: u8| -> u8 {
        (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8
    };
    slint::Color::from_rgb_u8(
        m(a.red(), b.red()),
        m(a.green(), b.green()),
        m(a.blue(), b.blue()),
    )
}

/// 派生色渐变的推进状态。用**递归 `Timer::single_shot`** 而非 `Timer::start`：
/// `single_shot` 是关联函数，不需要持有 `Timer` 实例，因此没有"定时器必须活得
/// 比回调久"的束缚（`Timer` 在 1.18 也没有 `as_weak()`）。
struct PaletteFade {
    weak: slint::Weak<AppWindow>,
    bufs: TermBuffers,
    apply_builtin_theme: bool,
    /// (accent, tint, base) 的起点 —— 换壁纸前主题里的旧值。
    from: (slint::Color, slint::Color, slint::Color),
    /// (accent, tint, base) 的终点 —— 新壁纸取色算出的值。
    to: (slint::Color, slint::Color, slint::Color),
    is_dark: bool,
    seq: u64,
    t0: std::time::Instant,
}

impl PaletteFade {
    fn spawn(self) {
        slint::Timer::single_shot(std::time::Duration::from_millis(16), move || {
            self.step();
        });
    }

    fn step(self) {
        let Some(w) = self.weak.upgrade() else { return };
        // generation 变了 = 用户又切了壁纸，本次插值作废
        if WALLPAPER_SEQ.load(std::sync::atomic::Ordering::SeqCst) != self.seq {
            return;
        }
        // 进度按**真实时间**算（不是计步）：定时器抖动也不会让背景层与图像脱节。
        // 区间与 Slint 侧淡出完全一致（30ms 起、400ms 走完）。
        let elapsed = self.t0.elapsed().as_secs_f32() * 1000.0;
        let t01 = ((elapsed - FADE_START_MS) / FADE_LEN_MS).clamp(0.0, 1.0);
        let g = w.global::<Theme>();
        g.set_wp_accent(lerp_color(self.from.0, self.to.0, t01));
        g.set_wp_tint(lerp_color(self.from.1, self.to.1, t01));
        g.set_wp_base(lerp_color(self.from.2, self.to.2, t01));
        if t01 < 1.0 {
            self.spawn();
        } else if self.apply_builtin_theme {
            // 亮暗档是 bool、驱动整个 Palette 的两档文字/边框色，无法插值。
            // 放在插值末点切：此时背景色已到位，两档跳变的观感最小。
            apply_dark_mode(&w, &self.bufs, self.is_dark);
        }
    }
}

/// 把壁纸派生的配色**一次性**写进主题（无过渡路径用：首帧 / 动画关闭 / 关壁纸）。
/// 交叉淡化路径不走这里 —— 它让派生色随图像渐变，见 `lerp_color` 上方的说明。
#[allow(clippy::too_many_arguments)]
fn paint_wallpaper_palette(
    w: &AppWindow,
    bufs: &TermBuffers,
    apply_builtin_theme: bool,
    accent: (u8, u8, u8),
    tint: (u8, u8, u8),
    base: (u8, u8, u8),
    is_dark: bool,
) {
    let g = w.global::<Theme>();
    g.set_wp_accent(slint::Color::from_rgb_u8(accent.0, accent.1, accent.2));
    g.set_wp_tint(slint::Color::from_rgb_u8(tint.0, tint.1, tint.2));
    g.set_wp_base(slint::Color::from_rgb_u8(base.0, base.1, base.2));
    // 深浅档统一由**壁纸**决定：内置深色 / 浅色图各自带档位，上传的图片按
    // 自己的主导色明暗判档（定档权只有一个，不再有"开关 vs 壁纸"打架）。
    if apply_builtin_theme {
        apply_dark_mode(w, bufs, is_dark);
    }
}

pub(super) fn apply_wallpaper(
    window: &AppWindow,
    store: &ConfigStore,
    bufs: &TermBuffers,
    id: &str,
    apply_builtin_theme: bool,
) {
    // 壁纸分区暂时关闭时（见 `WALLPAPER_UI_ENABLED`）整体按"没有壁纸"处理：只藏界面不够
    // ——配置里默认的 `builtin:dark` 会继续压在 `window-base` 上，浅色主题只剩面板是浅的。
    let id = if crate::app::settings::appearance::WALLPAPER_UI_ENABLED {
        id
    } else {
        ""
    };
    match crate::wallpaper::load(id) {
        Some(wp) => {
            // ==============================================================
            // 换壁纸 = 400ms 交叉淡化 + 派生色**同轴渐变**
            //
            // 此前的问题：壁纸图像渐变 400ms，而叠在它之上的那层背景
            // （`bg-root` / `bg-panel` / `term-bg` —— 全部由 `wp_base`、`wp_tint`
            // 派生，见 theme.slint 的 `frost()`）却在**同一 tick 瞬变**：
            // 图像在渐变、背景层在跳变，两者不同步。浅色↔深色切换时对比最强，
            // 表现为这一层"闪一下"。
            //
            // 中点切换只是把跳变挪了位置、跳变本身还在，所以改为**同轴渐变**：
            // 派生色按与图像相同的进度插值，全程无跳变。
            // 时序（Slint 侧 400ms 自 t=30ms 起，`e-standard-decelerate` 实为线性）：
            //   t=0     新壁纸图像就位（下层）；旧图存入 prev 盖在上层 opacity=1
            //   t=30ms  prev opacity→0，启动交叉淡化
            //   t=30~430ms 派生色随图像进度插值（末点切换亮暗档）
            //   t=480ms 撤掉 prev 层
            //
            // 尊重「界面动画」开关；启动首帧（此前无壁纸）无旧图可交叉，直接应用。
            // ==============================================================
            let cross_fade = window.global::<Theme>().get_wallpaper_active()
                && window.global::<AnimationSettings>().get_enabled();

            let seq = WALLPAPER_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let (ar, ag, ab) = wp.palette.accent;
            let (tr, tg, tb) = wp.palette.tint;
            let (br, bg, bb) = wp.palette.base;
            let is_dark = wp.palette.is_dark;

            if cross_fade {
                let prev = window.global::<Theme>().get_wallpaper();
                window.global::<Theme>().set_wallpaper_prev(prev);
                window.global::<Theme>().set_wallpaper_fading(true);
                window.global::<Theme>().set_wallpaper_fade_opacity(1.0);

                let weak = window.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_millis(30), move || {
                    if let Some(w) = weak.upgrade()
                        && WALLPAPER_SEQ.load(std::sync::atomic::Ordering::SeqCst) == seq
                    {
                        w.global::<Theme>().set_wallpaper_fade_opacity(0.0);
                    }
                });

                // ── 派生色与图像**同轴渐变** ─────────────────────────────────
                // 面板底色（内容区 / 侧栏 / 终端底的半透明层）全部由 `wp_base`、
                // `wp_tint` 派生。让它们跟着图像的淡化进度一起插值，这一层就不再
                // 跳变 —— 之前"闪一下"就是图像渐变、背景层瞬变两条时间轴脱节。
                PaletteFade {
                    weak: window.as_weak(),
                    bufs: (*bufs).clone(),
                    apply_builtin_theme,
                    from: (
                        window.global::<Theme>().get_wp_accent(),
                        window.global::<Theme>().get_wp_tint(),
                        window.global::<Theme>().get_wp_base(),
                    ),
                    to: (
                        slint::Color::from_rgb_u8(ar, ag, ab),
                        slint::Color::from_rgb_u8(tr, tg, tb),
                        slint::Color::from_rgb_u8(br, bg, bb),
                    ),
                    is_dark,
                    seq,
                    t0: std::time::Instant::now(),
                }
                .spawn();

                // 撤 prev 层的时刻必须**晚于**淡出动画结束（400ms）：早撤会把还没淡完
                // 的旧图硬切掉。480ms = 400ms 动画 + 80ms 余量。
                let weak = window.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_millis(480), move || {
                    if let Some(w) = weak.upgrade()
                        && WALLPAPER_SEQ.load(std::sync::atomic::Ordering::SeqCst) == seq
                    {
                        w.global::<Theme>().set_wallpaper_fading(false);
                    }
                });
            }

            // 新壁纸图像立即就位（它是交叉淡化的目标层，延迟了淡化无从下手）
            window.global::<Theme>().set_wallpaper(wp.image);
            if !cross_fade {
                paint_wallpaper_palette(
                    window, bufs, apply_builtin_theme, (ar, ag, ab), (tr, tg, tb), (br, bg, bb),
                    is_dark,
                );
            }
            window.global::<Theme>().set_wallpaper_active(true);
            window.set_current_wallpaper(id.into());
            let name = if crate::wallpaper::is_builtin(id) {
                String::new()
            } else {
                std::path::Path::new(id)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            window.set_custom_wallpaper_name(name.into());
        }
        None => {
            // 关掉壁纸：作废在途的淡化定时器（否则要等 480ms 才撤层），并立即撤掉
            // prev 图 —— 残留的旧图会一直盖在新状态上面。
            WALLPAPER_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            window.global::<Theme>().set_wallpaper_fading(false);
            window.global::<Theme>().set_wallpaper_fade_opacity(0.0);
            window.global::<Theme>().set_wallpaper_active(false);
            window.set_current_wallpaper("".into());
            window.set_custom_wallpaper_name("".into());
            apply_dark_mode(window, bufs, store.dark());
        }
    }
}

pub(super) fn selected_iface(st: &TabStatus) -> (String, u64, u64) {
    if !st.selected_iface.is_empty()
        && let Some(e) = st.net.iter().find(|e| e.0 == st.selected_iface)
    {
        return e.clone();
    }
    st.net.first().cloned().unwrap_or_default()
}

pub(super) fn conn_ip(host: &str) -> String {
    host.rsplit('@').next().unwrap_or(host).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    // ---------- compute_find_matches ----------

    /// 空查询不产生匹配（UI 语义 = 未开启搜索）。
    #[test]
    fn find_matches_empty_query_yields_nothing() {
        assert!(compute_find_matches(&lines(&["hello"]), "").is_empty());
    }

    /// `col`/`len` 是**终端单元格**数：CJK 一字符占 2 格。
    /// 这是搜索高亮与 `spans` 绘制能对齐的前提（#54 那一类偏移的根因）。
    #[test]
    fn find_matches_reports_cell_columns_for_cjk() {
        let m = compute_find_matches(&lines(&["a中b"]), "a中");
        assert_eq!((m[0].row, m[0].col, m[0].len), (0, 0, 3), "a(1) + 中(2)");
        let m = compute_find_matches(&lines(&["a中b"]), "中b");
        assert_eq!((m[0].col, m[0].len), (1, 3), "起点是单元格 1，跨度 2+1");
        let m = compute_find_matches(&lines(&["中文字"]), "文字");
        assert_eq!((m[0].col, m[0].len), (2, 4), "中(2) 之后才是文");
    }

    /// 命中后按**匹配长度**推进 → 结果互不重叠（"aaaa" 找 "aa" 只有 2 处，不是 3 处）。
    #[test]
    fn find_matches_advances_by_match_len() {
        let m = compute_find_matches(&lines(&["aaaa"]), "aa");
        assert_eq!(m.iter().map(|x| x.col).collect::<Vec<_>>(), vec![0, 2]);
    }

    /// 大小写折叠只做 ASCII（`to_ascii_lowercase`）：拉丁不敏感，非 ASCII 原样比较。
    /// 锁定现状 —— 哪天要改成完整 Unicode 折叠，必须是**有意**的改动。
    #[test]
    fn find_matches_folds_ascii_case_only() {
        assert_eq!(compute_find_matches(&lines(&["ABC"]), "abc").len(), 1);
        assert_eq!(compute_find_matches(&lines(&["abc"]), "ABC").len(), 1);
        assert!(compute_find_matches(&lines(&["ÄÖÜ"]), "ä").is_empty());
    }

    /// 跨行：逐行独立扫描，行号就是可见区内的行序。
    #[test]
    fn find_matches_walks_every_row() {
        let m = compute_find_matches(&lines(&["x", "needle", "needle needle"]), "needle");
        assert_eq!(m.iter().map(|x| x.row).collect::<Vec<_>>(), vec![1, 2, 2]);
        assert_eq!(m[2].col, 7, "同一行的第二处");
    }

    // ---------- compute_tab_display（真实缓冲区 → UI 数据） ----------

    /// 匹配来自**渲染后的** `displayed_text`：搜索高亮与屏幕内容同源。
    #[test]
    fn tab_display_finds_matches_in_rendered_text() {
        let mut buf = TermBuffer::new(24, 80, 1000);
        buf.ingest(b"hello world\r\n");
        buf.find_query = "world".into();
        let d = compute_tab_display(&mut buf);
        assert_eq!(d.matches.len(), 1, "应命中一次");
        assert_eq!((d.matches[0].row, d.matches[0].col, d.matches[0].len), (0, 6, 5));
        assert_eq!(d.screen.rows_used, 1, "只有一行内容");
    }

    /// CJK 端到端：中文经缓冲区渲染到匹配坐标，仍是单元格单位（列 2、跨度 4）。
    #[test]
    fn tab_display_cjk_match_uses_cells() {
        let mut buf = TermBuffer::new(24, 80, 1000);
        buf.ingest("中文字\r\n".as_bytes());
        buf.find_query = "文字".into();
        let d = compute_tab_display(&mut buf);
        assert_eq!(d.matches.len(), 1);
        assert_eq!((d.matches[0].col, d.matches[0].len), (2, 4));
    }

    /// 抽取后的回归：没有搜索时匹配为空，但**渲染照常**产出 span、选区为空。
    #[test]
    fn tab_display_without_query_still_renders() {
        let mut buf = TermBuffer::new(24, 80, 1000);
        buf.ingest(b"\x1b[31mred\x1b[0m plain\r\n");
        let d = compute_tab_display(&mut buf);
        assert!(d.matches.is_empty());
        assert!(!d.screen.spans.is_empty(), "有内容就必须产出 span");
        assert!(d.selection.is_empty(), "没有选区");
    }

    // ---------- 其余纯函数 ----------

    #[test]
    fn parse_hex_color_accepts_six_digit_forms() {
        let red = slint::Color::from_rgb_u8(255, 0, 0);
        assert_eq!(parse_hex_color("#ff0000"), Some(red));
        assert_eq!(parse_hex_color("ff0000"), Some(red), "可不带 #");
        assert_eq!(parse_hex_color("  #FF0000  "), Some(red), "两端空白应忽略");
    }

    /// `hex_from_rgb` 是 `parse_hex_color` 的反函数（调色盘只送分量过来）；
    /// 越界分量夹住而不是回绕。
    #[test]
    fn hex_from_rgb_round_trips_through_parse_hex_color() {
        assert_eq!(hex_from_rgb(0x4a, 0x90, 0xe2), "#4A90E2");
        assert_eq!(hex_from_rgb(0, 0, 0), "#000000");
        assert_eq!(hex_from_rgb(-5, 300, 128), "#00FF80");
        let c = parse_hex_color(&hex_from_rgb(18, 52, 86)).expect("round-trip");
        assert_eq!((c.red(), c.green(), c.blue()), (18, 52, 86));
    }

    #[test]
    fn parse_hex_color_rejects_bad_input() {
        for bad in ["", "#", "#fff", "#12345", "#1234567", "#gg0000", "12 34 56"] {
            assert_eq!(parse_hex_color(bad), None, "{bad:?} 不应被接受");
        }
    }

    /// 校验只约束"非空 / ≤512 字符 / 正则模式下必须能编译"，关键词模式不校验正则语法。
    #[test]
    fn highlight_rule_validation() {
        assert!(validate_output_highlight_rule("", false, false).is_err(), "空规则");
        assert!(validate_output_highlight_rule("ERROR", false, false).is_ok());
        assert!(
            validate_output_highlight_rule("[", false, false).is_ok(),
            "关键词模式不校验正则语法"
        );
        assert!(validate_output_highlight_rule("[", true, false).is_err(), "无效正则");
        assert!(validate_output_highlight_rule("a+b", true, true).is_ok());

        let at_limit = "x".repeat(512);
        assert!(validate_output_highlight_rule(&at_limit, false, false).is_ok(), "512 是上界内");
        let over = "x".repeat(513);
        assert!(validate_output_highlight_rule(&over, false, false).is_err(), "513 超限");
    }

    /// 命令历史的过滤：保持存储顺序（最旧在上）、忽略查询两端空白、大小写不敏感。
    #[test]
    fn history_view_rows_keeps_order_and_filters() {
        let hist = lines(&["ls -la", "git status", "LS /tmp"]);
        let all = history_view_rows(&hist, "");
        assert_eq!(all.len(), 3, "空查询 = 全部");
        assert_eq!(all[0].as_str(), "ls -la", "最旧的在最上");

        let hit = history_view_rows(&hist, "  ls  ");
        assert_eq!(hit.len(), 2, "空白应忽略、大小写不敏感");
        assert_eq!(hit[0].as_str(), "ls -la");
        assert_eq!(hit[1].as_str(), "LS /tmp");
    }

    #[test]
    fn conn_ip_strips_the_user_part() {
        assert_eq!(conn_ip("root@10.0.0.5"), "10.0.0.5");
        assert_eq!(conn_ip("10.0.0.5"), "10.0.0.5");
        assert_eq!(conn_ip("a@b@c"), "c", "只取最后一段");
        assert_eq!(conn_ip("  root@10.0.0.5  "), "10.0.0.5", "两端空白应去掉");
        assert_eq!(conn_ip(""), "");
    }
}
