use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::Processor;

use crate::ui::TermSpan;

/// Alacritty terminal handle used across MeatShell.
pub(crate) type ATerm = Term<VoidListener>;

/// Per-terminal state used by normal and alternate-screen rendering.
pub(crate) struct TermBuffer {
    /// Alacritty terminal emulator (grid + native scrollback).
    pub(crate) term: ATerm,
    /// Persistent ANSI state machine feeding bytes into `term`.
    pub(crate) processor: Processor,
    /// Terminal config (scrolling_history etc.), kept for rebuilds.
    #[allow(dead_code)]
    pub(crate) config: TermConfig,
    pub(crate) find_query: String,
    pub(crate) is_dark: bool,
    pub(crate) output_highlight: OutputHighlightPreset,
    pub(crate) custom_highlight_rules: Vec<CompiledOutputRule>,
    /// Scrollback lazily populated from the grid.  Index 0 = most recently
    /// scrolled-off line (grid Line(-1)), index 1 = second-most, etc.
    pub(crate) history: VecDeque<Line>,
    pub(crate) view_offset: usize,
    pub(crate) displayed_text: Vec<String>,
    pub(crate) csi_state: CsiState,
    pub(crate) raw: VecDeque<u8>,
    /// Row-level render cache: Some(line) when the live grid row has not
    /// changed since the last render, None for cold/invalidated rows.
    pub(crate) rendered: Vec<Option<RenderedLine>>,
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
        match color {
            alacritty_terminal::vte::ansi::Color::Named(_) => TermColor::Default,
            alacritty_terminal::vte::ansi::Color::Indexed(i) => TermColor::Idx(*i),
            alacritty_terminal::vte::ansi::Color::Spec(rgb) => TermColor::Rgb(rgb.r, rgb.g, rgb.b),
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
    pub(crate) inverse: bool,
    pub(crate) col: i32,
    pub(crate) cells: i32,
}

pub(crate) type Line = (String, Vec<HistSpan>, bool);
