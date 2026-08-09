#[path = "struct/types.rs"]
mod types;

#[path = "impls/input.rs"]
mod input;
#[path = "impls/local.rs"]
pub(crate) mod local;
#[path = "impls/output_highlight.rs"]
mod output_highlight;
#[path = "impls/presentation.rs"]
mod presentation;
#[path = "impls/render.rs"]
mod render;
#[path = "impls/render_gate.rs"]
mod render_gate;
#[path = "impls/serial.rs"]
pub(crate) mod serial;
#[path = "impls/telnet.rs"]
pub(crate) mod telnet;
#[path = "impls/term_buffer.rs"]
mod term_buffer;
#[path = "impls/vt_adapter.rs"]
pub(crate) mod vt_adapter;
#[path = "impls/zmodem.rs"]
pub(crate) mod zmodem;

#[cfg(windows)]
pub(crate) use input::c0_letter_key_down;
#[cfg(test)]
pub(crate) use input::normalize_pasted_newlines;
pub(crate) use input::{
    bare_ctrl_marker_workaround_enabled, encode_command_bar_input, encode_pasted_text,
    key_to_pty_bytes, paste_requires_large_review, should_drop_bare_ctrl_marker,
    terminal_uses_bracketed_paste,
};
#[cfg(any(target_os = "windows", test))]
pub(crate) use input::{windows_process_ctrl_release, CtrlKeySide};
pub(crate) use output_highlight::compile_output_rules;
pub(crate) use presentation::{highlight_plain_output, render_term_span};
#[cfg(test)]
pub(crate) use presentation::{log_level_marker, text_cell_width};
pub(crate) use render::{
    build_line, build_row, cell_prefix, refresh_overlines, RAW_CAP,
};
pub(crate) use types::{
    ATerm, BuiltScreen, CompiledOutputRule, CsiState, HistSpan, Line, OutputHighlightPreset,
    OverlineRange, RenderGates, RenderedLine, TabRenderGate, TermBuffer, TermBufferHandle,
    TermBuffers, TermColor, UnderlineStyle,
};
pub(crate) use vt_adapter::{
    app_cursor, attr_from_cell, bracketed_paste, cell_attrs, cursor_pos, is_alt,
    is_wide_continuation, mouse_report, new_term, process_bytes, resize_term, row_wrapped,
    term_size, CellAttr, MouseReport,
};
