//! Length-prefixed framing: each payload is preceded by its unsigned 32-bit big-endian length.

use std::fmt;

/// Default upper bound for one framed CBOR payload.
pub const DEFAULT_MAX_FRAME_LENGTH: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameError(pub String);

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FrameError {}

/// Prefixes a payload with its unsigned 32-bit big-endian byte length.
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let Ok(length) = u32::try_from(payload.len()) else {
        return Err(FrameError(
            "Frame payload exceeds the unsigned 32-bit length limit".into(),
        ));
    };
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Open,
    Ended,
    Failed,
}

/// Incrementally splits arbitrary byte chunks into length-prefixed payloads.
#[derive(Debug)]
pub struct FrameDecoder {
    header: Vec<u8>,
    payload: Vec<u8>,
    expected: Option<usize>,
    max_frame_length: usize,
    state: State,
}

impl FrameDecoder {
    pub fn new(max_frame_length: usize) -> Result<Self, FrameError> {
        if max_frame_length > u32::MAX as usize {
            return Err(FrameError(format!(
                "maxFrameLength must be an integer between 0 and {}",
                u32::MAX
            )));
        }
        Ok(Self {
            header: Vec::with_capacity(4),
            payload: Vec::new(),
            expected: None,
            max_frame_length,
            state: State::Open,
        })
    }

    fn check_open(&self) -> Result<(), FrameError> {
        match self.state {
            State::Open => Ok(()),
            State::Ended => Err(FrameError("Frame decoder has ended".into())),
            State::Failed => Err(FrameError("Frame decoder has failed".into())),
        }
    }

    pub fn push(&mut self, mut chunk: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
        self.check_open()?;
        let mut frames = Vec::new();
        while !chunk.is_empty() {
            let expected = match self.expected {
                Some(expected) => expected,
                None => {
                    let take = (4 - self.header.len()).min(chunk.len());
                    self.header.extend_from_slice(&chunk[..take]);
                    chunk = &chunk[take..];
                    if self.header.len() < 4 {
                        continue;
                    }
                    let length =
                        u32::from_be_bytes([self.header[0], self.header[1], self.header[2], self.header[3]]) as usize;
                    self.header.clear();
                    if length > self.max_frame_length {
                        return Err(self.fail(format!(
                            "Frame length {length} exceeds configured limit of {}",
                            self.max_frame_length
                        )));
                    }
                    if length == 0 {
                        frames.push(Vec::new());
                        continue;
                    }
                    self.expected = Some(length);
                    length
                }
            };
            let take = (expected - self.payload.len()).min(chunk.len());
            self.payload.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            if self.payload.len() == expected {
                frames.push(std::mem::take(&mut self.payload));
                self.expected = None;
            }
        }
        Ok(frames)
    }

    pub fn end(&mut self) -> Result<(), FrameError> {
        self.check_open()?;
        if !self.header.is_empty() || self.expected.is_some() {
            return Err(self.fail("Truncated frame at end of stream".into()));
        }
        self.state = State::Ended;
        Ok(())
    }

    fn fail(&mut self, message: String) -> FrameError {
        self.state = State::Failed;
        self.header.clear();
        self.payload = Vec::new();
        self.expected = None;
        FrameError(message)
    }
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self {
            header: Vec::with_capacity(4),
            payload: Vec::new(),
            expected: None,
            max_frame_length: DEFAULT_MAX_FRAME_LENGTH,
            state: State::Open,
        }
    }
}
