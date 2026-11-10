#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WinActivity {
    Active,     // focused & visible → full rate
    Background, // visible but unfocused → throttled
    Hidden,     // minimized / occluded → paused
}
