//! Kitty keyboard protocol negotiation (`ProcessTerminal` in packages/tui/src/terminal.ts): start pushes the desired
//! flags and queries them, with a device-attributes query as the sentinel (`CSI > 7 u`, `CSI ? u`, `CSI c`). A
//! non-zero flags reply turns the protocol on (and modifyOtherKeys off); a zero reply, or device attributes before
//! any flags reply, turns on the modifyOtherKeys fallback (`CSI > 4 ; 2 m`). The replies never reach the
//! components; a reply split across reads is held (150 ms after its last fragment) and replayed as input when it
//! turns out not to be one. Stop pops the flags (`CSI < u`) and resets modifyOtherKeys (`CSI > 4 ; 0 m`).

use std::time::{Duration, Instant};

/// `KITTY_KEYBOARD_PROTOCOL_QUERY`: push flags 7 (disambiguate, event types, alternate keys), query, DA sentinel.
pub const KITTY_KEYBOARD_PROTOCOL_QUERY: &str = "\x1b[>7u\x1b[?u\x1b[c";
/// Pops the pushed Kitty flags.
pub const KITTY_KEYBOARD_PROTOCOL_POP: &str = "\x1b[<u";
pub const MODIFY_OTHER_KEYS_ENABLE: &str = "\x1b[>4;2m";
pub const MODIFY_OTHER_KEYS_DISABLE: &str = "\x1b[>4;0m";
/// `KEYBOARD_PROTOCOL_RESPONSE_FRAGMENT_TIMEOUT_MS`.
pub const RESPONSE_FRAGMENT_TIMEOUT: Duration = Duration::from_millis(150);

/// `KeyboardProtocolNegotiationSequence`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NegotiationSequence {
    KittyFlags(u32),
    DeviceAttributes,
}

/// `parseKeyboardProtocolNegotiationSequence`: `CSI ? <digits> u` or `CSI ? <digits and ;> c`.
pub fn parse_negotiation_sequence(sequence: &str) -> Option<NegotiationSequence> {
    let body = sequence.strip_prefix("\x1b[?")?;
    if let Some(digits) = body.strip_suffix('u')
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        // `Number.parseInt`: past u32 the flags are still non-zero.
        return Some(NegotiationSequence::KittyFlags(
            digits
                .parse()
                .unwrap_or(if digits.bytes().all(|b| b == b'0') { 0 } else { u32::MAX }),
        ));
    }
    let params = body.strip_suffix('c')?;
    params
        .bytes()
        .all(|b| b.is_ascii_digit() || b == b';')
        .then_some(NegotiationSequence::DeviceAttributes)
}

/// `isKeyboardProtocolNegotiationSequencePrefix`: `CSI` or `CSI ?` with digits and `;` so far.
fn is_negotiation_prefix(sequence: &str) -> bool {
    sequence == "\x1b["
        || sequence
            .strip_prefix("\x1b[?")
            .is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_digit() || b == b';'))
}

/// What one input sequence did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Negotiated {
    /// Input for the components, in order.
    pub forward: Vec<String>,
    /// Bytes for the terminal.
    pub write: String,
    /// The Kitty protocol turned on (`setKittyProtocolActive(true)`).
    pub kitty_activated: bool,
}

/// The negotiation state of one terminal.
#[derive(Clone, Debug, Default)]
pub struct KeyboardProtocol {
    pushed: bool,
    kitty_active: bool,
    modify_other_keys_active: bool,
    buffer: String,
    flush_at: Option<Instant>,
}

impl KeyboardProtocol {
    pub fn new() -> Self {
        Self::default()
    }

    /// `queryAndEnableKittyProtocol`: the bytes to write when the terminal starts.
    pub fn query(&mut self) -> &'static str {
        self.pushed = true;
        self.clear_buffer();
        KITTY_KEYBOARD_PROTOCOL_QUERY
    }

    pub fn kitty_active(&self) -> bool {
        self.kitty_active
    }

    pub fn modify_other_keys_active(&self) -> bool {
        self.modify_other_keys_active
    }

    /// One input sequence from the stdin buffer, at `now`.
    pub fn input(&mut self, sequence: &str, now: Instant) -> Negotiated {
        let mut out = Negotiated::default();
        if !self.buffer.is_empty() {
            let buffered = format!("{}{sequence}", self.buffer);
            if let Some(reply) = parse_negotiation_sequence(&buffered) {
                self.clear_buffer();
                self.handle(reply, &mut out);
                return out;
            }
            if is_negotiation_prefix(&buffered) {
                self.hold(buffered, now);
                return out;
            }
            out.forward.push(std::mem::take(&mut self.buffer));
            self.flush_at = None;
        }
        if let Some(reply) = parse_negotiation_sequence(sequence) {
            self.handle(reply, &mut out);
        } else if is_negotiation_prefix(sequence) {
            self.hold(sequence.to_string(), now);
        } else {
            out.forward.push(sequence.to_string());
        }
        out
    }

    /// When a held fragment is replayed as input, if one is held.
    pub fn flush_deadline(&self) -> Option<Instant> {
        self.flush_at
    }

    /// The held fragment once its deadline passed (`flushKeyboardProtocolNegotiationBufferAsInput`).
    pub fn poll(&mut self, now: Instant) -> Option<String> {
        let at = self.flush_at?;
        if now < at {
            return None;
        }
        self.flush_at = None;
        Some(std::mem::take(&mut self.buffer)).filter(|b| !b.is_empty())
    }

    /// `stop()` / `drainInput()`: pop the Kitty flags when they were pushed and reset modifyOtherKeys.
    pub fn stop(&mut self) -> String {
        let mut out = String::new();
        let pop = self.pushed || self.kitty_active;
        self.clear_buffer();
        if pop {
            out.push_str(KITTY_KEYBOARD_PROTOCOL_POP);
            self.pushed = false;
            self.kitty_active = false;
        }
        out.push_str(&self.disable_modify_other_keys());
        out
    }

    fn hold(&mut self, buffered: String, now: Instant) {
        // `setKeyboardProtocolNegotiationBuffer` clears the timer; the flush is scheduled again from this fragment.
        self.buffer = buffered;
        self.flush_at = Some(now + RESPONSE_FRAGMENT_TIMEOUT);
    }

    fn clear_buffer(&mut self) {
        self.buffer.clear();
        self.flush_at = None;
    }

    fn handle(&mut self, reply: NegotiationSequence, out: &mut Negotiated) {
        self.clear_buffer();
        match reply {
            NegotiationSequence::KittyFlags(flags) if flags != 0 => {
                out.write.push_str(&self.disable_modify_other_keys());
                if !self.kitty_active {
                    self.kitty_active = true;
                    out.kitty_activated = true;
                }
            }
            NegotiationSequence::KittyFlags(_) => out.write.push_str(&self.enable_modify_other_keys()),
            NegotiationSequence::DeviceAttributes => {
                if !self.kitty_active {
                    out.write.push_str(&self.enable_modify_other_keys());
                }
            }
        }
    }

    fn enable_modify_other_keys(&mut self) -> String {
        if self.kitty_active || self.modify_other_keys_active {
            return String::new();
        }
        self.modify_other_keys_active = true;
        MODIFY_OTHER_KEYS_ENABLE.into()
    }

    fn disable_modify_other_keys(&mut self) -> String {
        if !self.modify_other_keys_active {
            return String::new();
        }
        self.modify_other_keys_active = false;
        MODIFY_OTHER_KEYS_DISABLE.into()
    }
}
