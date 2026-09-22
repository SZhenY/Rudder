use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::Processor;

use crate::ui::TermSpan;

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CtrlKeySide {
    Left,
    Right,
}

/// Alacritty terminal handle used across Rudder.
pub(crate) type ATerm = Term<VoidListener>;

/// Per-terminal state used by normal and alternate-screen rendering.
pub(crate) struct TermBuffer {
    /// Alacritty terminal emulator (grid + native scrollback).
    pub(crate) term: ATerm,
    /// Persistent ANSI state machine feeding bytes into `term`.
    pub(crate) processor: Processor,
    pub(crate) find_query: String,
    pub(crate) is_dark: bool,
    pub(crate) output_highlight: OutputHighlightPreset,
    pub(crate) custom_highlight_rules: Vec<CompiledOutputRule>,
    /// Snapshot of visible lines from last ingest_chunk, used by damage-based

    pub(crate) view_offset: usize,
    pub(crate) displayed_text: Vec<String>,
    pub(crate) csi_state: CsiState,
    pub(crate) csi_pending: Vec<u8>,
    /// Whether the remote application is tracking the mouse (alacritty
    /// `mouse_protocol_mode() != None`). When true, clicks and drags are
    /// forwarded to the PTY instead of starting a local drag-selection, so
    /// btop/htop/mc can be operated with the mouse (upstream d8eff40).
    pub(crate) mouse_tracked: bool,
    pub(crate) raw: VecDeque<u8>,
    /// Row-level render cache: Some(line) when the live grid row has not
    /// changed since the last render, None for cold/invalidated rows.
    /// 上一帧实时视口的构建统计（阶段 0 的测量基础）。
    ///
    /// 用途只有一个：让"这一帧到底做了多少行"可观测 —— 有了它，才能判断
    /// alacritty 式的"只重建脏行"值不值得做（`flood_profile` 里 render 只占 p50 22µs，
    /// 而 ingest 是 304µs，所以**先测再改**）。`rebuilt + reused` 应恒等于可见行数。
    pub(crate) frame_stats: FrameStats,
    pub(crate) rendered: Vec<Option<RenderedLine>>,
    /// Row-level render cache for the SCROLLBACK view, keyed by absolute grid
    /// line (`GridLine`, negative into history).  Scrollback is immutable, so
    /// these entries survive across frames — unlike live rows they cannot be
    /// indexed by screen position, because one screen row maps to a different
    /// history line whenever `view_offset` moves.
    pub(crate) scroll_cache: HashMap<i32, ScrollLine>,
    /// 实时视图下连续渲染的帧数（B1.3）。`scroll_cache` 只在**回滚视图**里被读取，
    /// 所以回到实时视图后它只是纯占内存（一个 20 万行的会话能驻留 1.6–3.1 MB/标签页）。
    /// 连续若干帧没有回滚就把清掉；期间用户若又滚回去，计数会被重置（见 `render()`）。
    pub(crate) scroll_live_frames: u16,
    /// Bumped whenever something *outside* the grid changes the way a row
    /// renders (theme, highlight preset, custom rules, resize).  Cached lines
    /// carrying an older generation are ignored and rebuilt.
    pub(crate) render_gen: u64,
    /// SGR 53 (overline) interceptor state.  vte 0.15 and alacritty 0.26 both
    /// drop the overline attribute, so `ingest` scans the raw byte stream for
    /// `ESC [ … 53 … m` and records the affected column ranges itself.
    pub(crate) overline_active: bool,
    pub(crate) overline_start: Option<(i32, i32)>,
    pub(crate) overline_ranges: Vec<OverlineRange>,
    /// Tail of an incomplete CSI sequence split across ingest chunks (SSH /
    /// pipe reads are arbitrary).  The parser itself is chunk-agnostic, but
    /// our SGR interceptor must reassemble the sequence before it can act on
    /// it — otherwise `ESC [ 5` + `3 m` across two chunks silently loses the
    /// overline (SGR 53) or double-underline (21) attribute.
    pub(crate) sgr_buf: Vec<u8>,
    /// Echo produced shortly after a physical keypress should feel immediate.
    /// While `now < interactive_echo_until`, the tab's render throttle drops
    /// from ~30 Hz to ~120 Hz; it falls back automatically once typing stops
    /// so firehose output keeps its CPU protection.
    pub(crate) interactive_echo_until: std::time::Instant,
    /// Per-tab switch for pretty-printing + colouring complete JSON lines
    /// (#338). Seeded from the global setting when the buffer is created and
    /// flipped live by the settings toggle.
    pub(crate) json_format_output: bool,
}

/// 一帧实时视口的构建统计（见 `TermBuffer::frame_stats`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameStats {
    /// 走 `build_spans` 重排的行数（含"缓存未命中"与"内容真的变了"）。
    pub(crate) rebuilt: u32,
    /// 命中行缓存、只重跑 `render_term_span` 的行数（**仍然会产出新的 `Vec<TermSpan>`**）。
    pub(crate) reused: u32,
}

/// Cached rendering for one live-screen row.  Stores raw HistSpan runs (our
/// own type — `Send`) so the cache can live inside an `Arc<Mutex<TermBuffer>>`.
/// Span→TermSpan conversion (which creates `slint::Image` emoji icons that are
/// not `Send`) happens lazily during render.
#[derive(Clone)]
pub(crate) struct RenderedLine {
    pub(crate) plain_key: String,
    /// 与 `build_row` 的**原始**输出逐字比对（高亮**前**）。
    ///
    /// 键里必须含这一份：以前存的是"高亮后"、比的却是"高亮前"，于是**只要该行命中任一高亮
    /// 规则就永远判为变化** —— 每帧重跑全部规则并重建该行（#cache-style-key）。
    pub(crate) raw_runs: Vec<HistSpan>,
    /// 高亮后的 runs（真正用来产出 `TermSpan` 的那份）。
    pub(crate) runs: Vec<HistSpan>,
    /// 这份条目是在"跑高亮"的模式下建的吗 —— alt-screen 不跑高亮，退出 alt 后旧条目不能复用，
    /// 否则文本与原始样式恰好相同的行会命中"未高亮"的版本（高亮丢失）。
    pub(crate) highlighted: bool,
}

/// One cached scrollback line. `gen` guards against render-setting changes
/// (theme / highlight rules / width) that alter output without touching the
/// terminal grid, which would otherwise leave stale spans on screen.
pub(crate) struct ScrollLine {
    pub(crate) generation: u64,
    pub(crate) plain_key: String,
    /// 同 `RenderedLine::raw_runs`：回滚缓存的键是"相对行号 + 裁剪后的文本"，光凭它分不出
    /// "文本相同、样式不同"的另一行 —— 而回滚窗口底部往往还压在**活行**上，这种撞车很常见。
    pub(crate) raw_runs: Vec<HistSpan>,
    pub(crate) runs: Vec<HistSpan>,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum CsiState {
    Normal,
    Esc,
    Csi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputHighlightPreset {
    Off,
    Log,
    DevOps,
    /// tailspin-style built-in rule set (numbers, URLs, IPs, UUIDs, dates,
    /// key-value pairs, quoted strings, severity keywords, …).
    Builtin,
}

#[derive(Clone)]
pub(crate) struct CompiledOutputRule {
    pub(crate) matcher: regex::Regex,
    pub(crate) whole_line: bool,
    pub(crate) ansi_index: u8,
}

pub(crate) type TermBufferHandle = Arc<Mutex<TermBuffer>>;
pub(crate) type TermBuffers = Arc<Mutex<HashMap<String, TermBufferHandle>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RenderWaitResult {
    Settled,
    Closed,
    TimedOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RenderGatePhase {
    Idle,
    Scheduled,
    Flushing,
}

pub(super) struct RenderGateState {
    pub(super) requested: u64,
    pub(super) settled: u64,
    pub(super) phase: RenderGatePhase,
    pub(super) closed: bool,
    pub(super) last_visible_flush: std::time::Instant,
    /// 进入 `Flushing` 的时刻。若 `begin_flush()` 与 `finish_flush()` 之间没走完（panic /
    /// 提前返回），phase 会永远停在 `Flushing` —— 该标签页此后不再渲染，泵线程的背压也静默失效。
    /// 带上时间戳后由 `request()` 超时复位（自愈）。
    pub(super) flushing_since: Option<std::time::Instant>,
}

/// Coalesces and acknowledges UI snapshot flushes for one terminal tab.
pub(crate) struct TabRenderGate {
    pub(super) state: Mutex<RenderGateState>,
    pub(super) settled_cv: Condvar,
}

pub(crate) type RenderGates = Arc<Mutex<HashMap<String, Arc<TabRenderGate>>>>;

/// A coloured, cursor-annotated snapshot ready for the Slint terminal grid.
pub(crate) struct BuiltScreen {
    pub(crate) spans: Vec<TermSpan>,
    pub(crate) cursor_row: i32,
    pub(crate) cursor_col: i32,
    pub(crate) rows_used: i32,
    pub(crate) is_alt: bool,
    pub(crate) scroll_max: i32,
    pub(crate) scroll_offset: i32,
    pub(crate) mouse_tracked: bool,
}

/// Terminal colour, decoupled from the VT parser crate so presentation logic
/// doesn't depend on alacritty internals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TermColor {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

impl From<&alacritty_terminal::vte::ansi::Color> for TermColor {
    fn from(color: &alacritty_terminal::vte::ansi::Color) -> Self {
        use alacritty_terminal::vte::ansi::NamedColor;
        match color {
            // The 16 ANSI colours (Black=0 .. BrightWhite=15) must be kept so
            // the presentation layer can map them through our palettes.  Before
            // this branch existed they were collapsed into `Default`, which is
            // why SGR 30-37/40-47/90-97/100-107 all rendered black-on-white.
            alacritty_terminal::vte::ansi::Color::Named(name) => match name {
                NamedColor::Black
                | NamedColor::Red
                | NamedColor::Green
                | NamedColor::Yellow
                | NamedColor::Blue
                | NamedColor::Magenta
                | NamedColor::Cyan
                | NamedColor::White
                | NamedColor::BrightBlack
                | NamedColor::BrightRed
                | NamedColor::BrightGreen
                | NamedColor::BrightYellow
                | NamedColor::BrightBlue
                | NamedColor::BrightMagenta
                | NamedColor::BrightCyan
                | NamedColor::BrightWhite => TermColor::Idx(*name as u8),
                // SGR 39/49 (default fg/bg) plus every other special slot
                // (Cursor, Dim* — vte keeps them as colour names) fall back
                // to the terminal default.
                _ => TermColor::Default,
            },
            alacritty_terminal::vte::ansi::Color::Indexed(i) => TermColor::Idx(*i),
            alacritty_terminal::vte::ansi::Color::Spec(rgb) => TermColor::Rgb(rgb.r, rgb.g, rgb.b),
        }
    }
}

/// Underline style, mirroring alacritty's `Flags::ALL_UNDERLINES` family.
/// SGR 4:0 = none, 4 = single, 4:2 = double, 4:3 = curly, 4:4 = dotted,
/// 4:5 = dashed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl UnderlineStyle {
    pub(crate) fn from_flags(flags: alacritty_terminal::term::cell::Flags) -> Self {
        use alacritty_terminal::term::cell::Flags;
        if flags.contains(Flags::DOUBLE_UNDERLINE) {
            UnderlineStyle::Double
        } else if flags.contains(Flags::UNDERCURL) {
            UnderlineStyle::Curly
        } else if flags.contains(Flags::DOTTED_UNDERLINE) {
            UnderlineStyle::Dotted
        } else if flags.contains(Flags::DASHED_UNDERLINE) {
            UnderlineStyle::Dashed
        } else if flags.contains(Flags::UNDERLINE) {
            UnderlineStyle::Single
        } else {
            UnderlineStyle::None
        }
    }
}

/// One coloured run within a terminal line.
#[derive(Clone, PartialEq)]
pub(crate) struct HistSpan {
    pub(crate) text: String,
    pub(crate) fg: TermColor,
    pub(crate) bg: TermColor,
    pub(crate) bold: bool,
    pub(crate) dim: bool,
    pub(crate) italic: bool,
    pub(crate) underline: UnderlineStyle,
    pub(crate) hidden: bool,
    pub(crate) strike: bool,
    pub(crate) overline: bool,
    pub(crate) inverse: bool,
    pub(crate) col: i32,
    pub(crate) cells: i32,
}

/// A column range on one grid row marked by our SGR-53 (overline) interceptor.
///
/// vte 0.15 / alacritty 0.26 both drop SGR 53, so `ingest` scans the raw byte
/// stream itself and records where an overline was active.  Ranges are closed
/// when the overline SGR is reset (0m / 22m / 24m / 29m or any SGR without 53).
/// `col_end` is exclusive.
///
/// `abs` is the grid's *absolute* row position at record time
/// (`display_offset() + row`).  Rendering matches against the same absolute
/// position, so ranges follow their content when the screen scrolls (live
/// rows as well as scrollback lines) instead of drifting to whatever new
/// content occupies the same relative row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OverlineRange {
    pub(crate) abs: i64,
    pub(crate) col_start: i32,
    pub(crate) col_end: i32,
}

pub(crate) type Line = (String, Vec<HistSpan>, bool);
