//! Normalized mouse events and component results (`TuiMouseEvent` / `TuiMouseEventResult` of
//! packages/tui/src/tui.ts).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseEventType {
    Press,
    Release,
    Move,
    Drag,
    Click,
    Wheel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    None,
}

/// Zero-based cell event; `x`/`y` are local to the receiving component and may be negative after an offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: MouseEventType,
    pub button: MouseButton,
    pub x: i64,
    pub y: i64,
    pub screen_x: i64,
    pub screen_y: i64,
    pub width: usize,
    pub height: usize,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    /// Logical lines; negative scrolls up.
    pub wheel_delta: Option<i64>,
    pub click_count: Option<u32>,
}

/// Unset fields are absent in TS.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseResult {
    pub handled: Option<bool>,
    pub capture: Option<bool>,
    pub focus: Option<bool>,
    pub render: Option<bool>,
}
