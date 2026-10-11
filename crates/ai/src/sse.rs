//! Server-sent event decoding shared by the Anthropic client (`iterateSseMessages`) and the OpenAI SDK
//! (`_iterSSEMessages`): lines end at LF, CRLF or CR; `event:`/`data:` fields; a blank line ends an event.

#[derive(Clone, Debug, PartialEq)]
pub struct ServerSentEvent {
    pub event: Option<String>,
    pub data: String,
    /// Every non-blank line of the event, comments included.
    pub raw: Vec<String>,
}

#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    raw: Vec<String>,
}

impl SseDecoder {
    /// Decode the complete lines in `chunk`. A CR at the end of the buffered bytes waits for the next chunk, so a CRLF
    /// split across chunks stays one line break (upstream's Anthropic decoder reads it as two).
    pub fn push(&mut self, chunk: &[u8]) -> Vec<ServerSentEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut start = 0;
        let mut index = 0;
        while index < self.buffer.len() {
            let byte = self.buffer[index];
            if byte == b'\n' || byte == b'\r' {
                if byte == b'\r' && index + 1 == self.buffer.len() {
                    break;
                }
                let line = String::from_utf8_lossy(&self.buffer[start..index]).into_owned();
                let next = if byte == b'\r' && self.buffer[index + 1] == b'\n' {
                    index + 2
                } else {
                    index + 1
                };
                events.extend(self.line(line));
                start = next;
                index = next;
            } else {
                index += 1;
            }
        }
        self.buffer.drain(..start);
        events
    }

    /// End of body: the unterminated last line is decoded; `flush_trailing` also emits an event that no blank line
    /// closed (the Anthropic decoder does, the OpenAI SDK does not).
    pub fn finish(&mut self, flush_trailing: bool) -> Vec<ServerSentEvent> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            let text = String::from_utf8_lossy(&rest);
            let line = text.strip_suffix('\r').unwrap_or(&text).to_string();
            events.extend(self.line(line));
        }
        if flush_trailing {
            events.extend(self.flush());
        }
        events
    }

    fn flush(&mut self) -> Option<ServerSentEvent> {
        if self.event.is_none() && self.data.is_empty() {
            return None;
        }
        Some(ServerSentEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data).join("\n"),
            raw: std::mem::take(&mut self.raw),
        })
    }

    fn line(&mut self, line: String) -> Option<ServerSentEvent> {
        if line.is_empty() {
            return self.flush();
        }
        self.raw.push(line.clone());
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.find(':') {
            Some(i) => (&line[..i], &line[i + 1..]),
            None => (line.as_str(), ""),
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }
}
