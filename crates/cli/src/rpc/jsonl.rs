//! `modes/rpc/jsonl.ts`: strict JSONL framing. Records are split on LF only (U+2028 and U+2029 stay inside
//! payloads), a trailing CR is dropped, and a final record without LF is still delivered.

use serde_json::Value;
use std::io::{BufRead, BufReader, Read};

/// `serializeJsonLine`: `JSON.stringify(value)` and LF.
pub fn serialize_json_line(value: &Value) -> String {
    let mut line = session_file::js::stringify(value);
    line.push('\n');
    line
}

/// `attachJsonlLineReader` as an iterator over the records of a byte stream. Invalid UTF-8 decodes to U+FFFD,
/// as Node's `StringDecoder` does.
pub struct JsonlLines<R> {
    reader: BufReader<R>,
}

impl<R: Read> JsonlLines<R> {
    pub fn new(reader: R) -> Self {
        JsonlLines {
            reader: BufReader::new(reader),
        }
    }
}

impl<R: Read> Iterator for JsonlLines<R> {
    type Item = std::io::Result<String>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut bytes = Vec::new();
        match self.reader.read_until(b'\n', &mut bytes) {
            Ok(0) => None,
            Ok(_) => {
                if bytes.last() == Some(&b'\n') {
                    bytes.pop();
                }
                if bytes.last() == Some(&b'\r') {
                    bytes.pop();
                }
                Some(Ok(String::from_utf8_lossy(&bytes).into_owned()))
            }
            Err(e) => Some(Err(e)),
        }
    }
}
