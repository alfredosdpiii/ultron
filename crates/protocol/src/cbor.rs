//! Strict, definite-length RFC 8949 subset used on the wire (port of `src/cbor`).
//!
//! Values mirror what JavaScript can hold: integers are limited to the safe range
//! (`|n| <= 2^53 - 1`) and an integral float other than `-0` is the same value as the integer.

use std::fmt;

pub const DEFAULT_MAX_CBOR_BYTE_LENGTH: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_CBOR_CONTAINER_LENGTH: usize = 1_000_000;
pub const DEFAULT_MAX_CBOR_DEPTH: usize = 64;
const MAX_UINT32: u64 = 0xffff_ffff;
const MAX_CONFIGURED_DEPTH: usize = 512;
const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    /// Entries in wire order; keys are unique.
    Map(Vec<(String, Value)>),
}

impl Value {
    pub fn text(value: impl Into<String>) -> Self {
        Value::Text(value.into())
    }

    pub fn map<const N: usize>(entries: [(&str, Value); N]) -> Self {
        Value::Map(entries.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The JavaScript number this value denotes, if it is one.
    fn number(&self) -> Option<f64> {
        match self {
            Value::Int(n) => Some(*n as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }
}

impl PartialEq for Value {
    /// JavaScript equality of decoded values: `1` and `1.0` are the same number, `-0` is not `0`.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Text(a), Value::Text(b)) => a == b,
            (Value::Bytes(a), Value::Bytes(b)) => a == b,
            (Value::Array(a), Value::Array(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => {
                a.len() == b.len() && a.iter().all(|(k, v)| b.iter().any(|(bk, bv)| bk == k && bv == v))
            }
            _ => match (self.number(), other.number()) {
                (Some(a), Some(b)) => a == b && a.is_sign_negative() == b.is_sign_negative(),
                _ => false,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CborError(pub String);

impl fmt::Display for CborError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CborError {}

fn err<T>(message: impl Into<String>) -> Result<T, CborError> {
    Err(CborError(message.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CborOptions {
    /// Maximum encoded input/output bytes and maximum byte/text string length.
    pub max_byte_length: usize,
    /// Maximum number of elements in an array or entries in a map.
    pub max_container_length: usize,
    /// Maximum recursive item depth.
    pub max_depth: usize,
}

impl Default for CborOptions {
    fn default() -> Self {
        Self {
            max_byte_length: DEFAULT_MAX_CBOR_BYTE_LENGTH,
            max_container_length: DEFAULT_MAX_CBOR_CONTAINER_LENGTH,
            max_depth: DEFAULT_MAX_CBOR_DEPTH,
        }
    }
}

impl CborOptions {
    fn validate(&self) -> Result<(), CborError> {
        let limits = [
            ("maxByteLength", self.max_byte_length, MAX_UINT32 as usize),
            ("maxContainerLength", self.max_container_length, MAX_UINT32 as usize),
            ("maxDepth", self.max_depth, MAX_CONFIGURED_DEPTH),
        ];
        for (name, value, maximum) in limits {
            if value > maximum {
                return err(format!("{name} must be an integer between 0 and {maximum}"));
            }
        }
        Ok(())
    }
}

struct Writer<'a> {
    out: Vec<u8>,
    options: &'a CborOptions,
}

impl Writer<'_> {
    fn bytes(&mut self, bytes: &[u8]) -> Result<(), CborError> {
        if self.out.len() + bytes.len() > self.options.max_byte_length {
            return err(format!(
                "CBOR byte length exceeds configured limit of {}",
                self.options.max_byte_length
            ));
        }
        self.out.extend_from_slice(bytes);
        Ok(())
    }

    fn argument(&mut self, major: u8, value: u64) -> Result<(), CborError> {
        let prefix = major << 5;
        if value < 24 {
            self.bytes(&[prefix | value as u8])
        } else if value <= 0xff {
            self.bytes(&[prefix | 24, value as u8])
        } else if value <= 0xffff {
            self.bytes(&[prefix | 25])?;
            self.bytes(&(value as u16).to_be_bytes())
        } else if value <= MAX_UINT32 {
            self.bytes(&[prefix | 26])?;
            self.bytes(&(value as u32).to_be_bytes())
        } else {
            self.bytes(&[prefix | 27])?;
            self.bytes(&value.to_be_bytes())
        }
    }

    fn text(&mut self, value: &str) -> Result<(), CborError> {
        if value.len() > self.options.max_byte_length {
            return err(format!(
                "CBOR text string length exceeds configured limit of {}",
                self.options.max_byte_length
            ));
        }
        self.argument(3, value.len() as u64)?;
        self.bytes(value.as_bytes())
    }

    fn integer(&mut self, value: i64) -> Result<(), CborError> {
        if !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&value) {
            return err("CBOR integers must be safe JavaScript integers");
        }
        if value >= 0 {
            self.argument(0, value as u64)
        } else {
            self.argument(1, (-1 - value) as u64)
        }
    }

    fn value(&mut self, value: &Value, depth: usize) -> Result<(), CborError> {
        let options = self.options;
        if depth > options.max_depth {
            return err(format!(
                "CBOR nesting depth exceeds configured limit of {}",
                options.max_depth
            ));
        }
        match value {
            Value::Null => self.bytes(&[0xf6]),
            Value::Bool(b) => self.bytes(&[if *b { 0xf5 } else { 0xf4 }]),
            Value::Int(n) => self.integer(*n),
            Value::Float(f) => {
                if !f.is_finite() {
                    return err("CBOR numbers must be finite");
                }
                if f.fract() == 0.0 && !(*f == 0.0 && f.is_sign_negative()) {
                    if f.abs() > MAX_SAFE_INTEGER as f64 {
                        return err("CBOR integers must be safe JavaScript integers");
                    }
                    return self.integer(*f as i64);
                }
                self.bytes(&[0xfb])?;
                self.bytes(&f.to_be_bytes())
            }
            Value::Text(text) => self.text(text),
            Value::Bytes(bytes) => {
                if bytes.len() > options.max_byte_length {
                    return err(format!(
                        "CBOR byte string length exceeds configured limit of {}",
                        options.max_byte_length
                    ));
                }
                self.argument(2, bytes.len() as u64)?;
                self.bytes(bytes)
            }
            Value::Array(items) => {
                if items.len() > options.max_container_length {
                    return err(format!(
                        "CBOR array length exceeds configured limit of {}",
                        options.max_container_length
                    ));
                }
                self.argument(4, items.len() as u64)?;
                items.iter().try_for_each(|item| self.value(item, depth + 1))
            }
            Value::Map(entries) => {
                if entries.len() > options.max_container_length {
                    return err(format!(
                        "CBOR map length exceeds configured limit of {}",
                        options.max_container_length
                    ));
                }
                self.argument(5, entries.len() as u64)?;
                for (key, entry) in entries {
                    self.text(key)?;
                    self.value(entry, depth + 1)?;
                }
                Ok(())
            }
        }
    }
}

/// Encodes the protocol's strict, definite-length RFC 8949 subset.
pub fn encode_cbor(value: &Value, options: &CborOptions) -> Result<Vec<u8>, CborError> {
    options.validate()?;
    let mut writer = Writer {
        out: Vec::new(),
        options,
    };
    writer.value(value, 0)?;
    Ok(writer.out)
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
    options: &'a CborOptions,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, CborError> {
        let Some(&value) = self.bytes.get(self.offset) else {
            return err("Truncated CBOR payload");
        };
        self.offset += 1;
        Ok(value)
    }

    fn take(&mut self, length: u64) -> Result<&[u8], CborError> {
        if length > (self.bytes.len() - self.offset) as u64 {
            return err("Truncated CBOR payload");
        }
        let start = self.offset;
        self.offset += length as usize;
        Ok(&self.bytes[start..self.offset])
    }

    fn argument(&mut self, info: u8) -> Result<u64, CborError> {
        match info {
            0..=23 => Ok(u64::from(info)),
            24 => Ok(u64::from(self.byte()?)),
            25 => Ok(u64::from(u16::from_be_bytes(
                self.take(2)?.try_into().unwrap_or_default(),
            ))),
            26 => Ok(u64::from(u32::from_be_bytes(
                self.take(4)?.try_into().unwrap_or_default(),
            ))),
            27 => {
                let high = self.argument(26)?;
                let low = self.argument(26)?;
                if high > 0x1f_ffff {
                    return err("Decoded CBOR integer or length is outside the safe range");
                }
                Ok((high << 32) | low)
            }
            31 => err("Indefinite-length CBOR items are not supported"),
            _ => err("Malformed CBOR additional information"),
        }
    }

    fn length(&mut self, info: u8, kind: &str, limit: usize) -> Result<u64, CborError> {
        if info == 31 {
            return err(format!("Indefinite-length CBOR {kind}s are not supported"));
        }
        let length = self.argument(info)?;
        if length > limit as u64 {
            return err(format!("CBOR {kind} length exceeds configured limit of {limit}"));
        }
        Ok(length)
    }

    fn item(&mut self, depth: usize) -> Result<Value, CborError> {
        let options = self.options;
        if depth > options.max_depth {
            return err(format!(
                "CBOR nesting depth exceeds configured limit of {}",
                options.max_depth
            ));
        }
        let initial = self.byte()?;
        let info = initial & 0x1f;
        match initial >> 5 {
            0 => Ok(Value::Int(self.argument(info)? as i64)),
            1 => {
                let value = -1 - self.argument(info)? as i64;
                if value < -MAX_SAFE_INTEGER {
                    return err("Decoded CBOR integer is outside the safe range");
                }
                Ok(Value::Int(value))
            }
            2 => {
                let length = self.length(info, "byte string", options.max_byte_length)?;
                Ok(Value::Bytes(self.take(length)?.to_vec()))
            }
            3 => {
                let length = self.length(info, "text string", options.max_byte_length)?;
                match std::str::from_utf8(self.take(length)?) {
                    Ok(text) => Ok(Value::text(text)),
                    Err(_) => err("CBOR text string contains invalid UTF-8"),
                }
            }
            4 => {
                let length = self.length(info, "array", options.max_container_length)?;
                let mut items = Vec::new();
                for _ in 0..length {
                    items.push(self.item(depth + 1)?);
                }
                Ok(Value::Array(items))
            }
            5 => {
                let length = self.length(info, "map", options.max_container_length)?;
                let mut entries: Vec<(String, Value)> = Vec::new();
                for _ in 0..length {
                    let Value::Text(key) = self.item(depth + 1)? else {
                        return err("CBOR map keys must be strings");
                    };
                    if entries.iter().any(|(k, _)| *k == key) {
                        return err("CBOR map contains a duplicate key");
                    }
                    let value = self.item(depth + 1)?;
                    entries.push((key, value));
                }
                Ok(Value::Map(entries))
            }
            6 => err("CBOR tags are not supported"),
            _ => self.simple(info),
        }
    }

    fn simple(&mut self, info: u8) -> Result<Value, CborError> {
        match info {
            20 => Ok(Value::Bool(false)),
            21 => Ok(Value::Bool(true)),
            22 => Ok(Value::Null),
            27 => {
                let value = f64::from_be_bytes(self.take(8)?.try_into().unwrap_or_default());
                if !value.is_finite() {
                    return err("Decoded CBOR number must be finite");
                }
                if value.fract() == 0.0 && value.abs() > MAX_SAFE_INTEGER as f64 {
                    return err("Decoded CBOR integer is outside the safe range");
                }
                if value.fract() == 0.0 && !(value == 0.0 && value.is_sign_negative()) {
                    return Ok(Value::Int(value as i64));
                }
                Ok(Value::Float(value))
            }
            31 => err("CBOR break marker is not supported"),
            _ => err("Unsupported CBOR simple value or floating-point width"),
        }
    }
}

/// Decodes exactly one item from the protocol's strict RFC 8949 subset.
pub fn decode_cbor(bytes: &[u8], options: &CborOptions) -> Result<Value, CborError> {
    options.validate()?;
    if bytes.len() > options.max_byte_length {
        return err(format!(
            "CBOR byte length exceeds configured limit of {}",
            options.max_byte_length
        ));
    }
    let mut reader = Reader {
        bytes,
        offset: 0,
        options,
    };
    let value = reader.item(0)?;
    if reader.offset != bytes.len() {
        return err("CBOR payload contains trailing data");
    }
    Ok(value)
}
