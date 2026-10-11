//! SGR mouse reports for fullscreen overlays (`parseSgrMouse` of oh-my-pi packages/tui/src/mouse.ts). Overlays
//! that own the alternate screen turn on mouse tracking and receive `ESC [ < b ; x ; y M|m`.

/// One decoded report. Rows and columns are 0-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SgrMouse {
    /// Raw button code (bit 32 = motion, bit 64 = wheel, low bits = button).
    pub button: u32,
    pub col: i64,
    pub row: i64,
    /// A release report (`m`).
    pub release: bool,
    /// Vertical wheel: -1 up, 1 down. Horizontal wheel reports (66/67) are not a direction.
    pub wheel: Option<i64>,
    /// The pointer moved (hover or drag) rather than clicked.
    pub motion: bool,
    /// A left-button press (not motion, release or wheel).
    pub left_click: bool,
}

/// Decode `data`, or `None` when it is not exactly one SGR mouse report.
pub fn parse_sgr_mouse(data: &str) -> Option<SgrMouse> {
    let body = data.strip_prefix("\x1b[<")?;
    let (body, release) = match body.as_bytes().last()? {
        b'M' => (&body[..body.len() - 1], false),
        b'm' => (&body[..body.len() - 1], true),
        _ => return None,
    };
    let mut parts = body.split(';');
    let mut number = || -> Option<i64> {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        // JS `Number()` of a long digit run is a float; reports this large never come from a terminal.
        part.parse::<i64>().ok()
    };
    let button = number()?;
    let col = number()? - 1;
    let row = number()? - 1;
    if parts.next().is_some() {
        return None;
    }
    let button = u32::try_from(button).ok()?;
    let wheel = (button & 64 != 0 && button & 2 == 0).then_some(if button & 1 != 0 { 1 } else { -1 });
    let motion = button & 32 != 0 && wheel.is_none();
    let left_click = !release && wheel.is_none() && !motion && button & 3 == 0;
    Some(SgrMouse {
        button,
        col,
        row,
        release,
        wheel,
        motion,
        left_click,
    })
}
