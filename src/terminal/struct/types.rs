use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::Processor;

use crate::ui::TermSpan;

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
    /// incremental rebuild (Partial → clone prev, overwrite damaged rows).
    pub(crate) prev: Vec<Line>,
    pub(crate) view_offset: usize,
    pub(crate) displayed_text: Vec<String>,
    pub(crate) csi_state: CsiState,
    pub(crate) csi_pending: Vec<u8>,
    pub(crate) raw: VecDeque<u8>,
    /// Row-level render cache: Some(line) when the live grid row has not
    /// changed since the last render, None for cold/invalidated rows.
    pub(crate) rendered: Vec<Option<RenderedLine>>,
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
}

/// Cached rendering for one live-screen row.  Stores raw HistSpan runs (our
/// own type — `Send`) so the cache can live inside an `Arc<Mutex<TermBuffer>>`.
/// Span→TermSpan conversion (which creates `slint::Image` emoji icons that are
/// not `Send`) happens lazily during render.
#[derive(Clone)]
pub(crate) struct RenderedLine {
    pub(crate) plain_key: String,
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
#[derive(Clone)]
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
