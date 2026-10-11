//! The shell job journal (session value `ultron.module/jobs`) and job-done events, ported from
//! ultron/rlm/shell-jobs.ts (`#load`, `#persist`, `snapshot`, `read`, `jobSummary`, `#fileTail`) and
//! experimental/session-worker.ts `jobEvent`.
//!
//! Processes do not outlive the worker that owned them: on restore, a job the journal still lists as running becomes
//! `interrupted` ("worker restarted") and the journal is rewritten. Restored jobs answer `shell.list`, `shell.get`,
//! `shell.result` and `shell.read` from their records and spill files.

use serde_json::{Map, Value, json};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

pub const JOURNAL_NAMESPACE: &str = "ultron.module/jobs";
const JOURNAL_VERSION: u64 = 1;
const JOB_TEXT_BYTES: u64 = 16 * 1024;

/// One journaled job (camelCase on the wire, as TypeScript stores it).
#[derive(Clone, Debug, PartialEq)]
pub struct JobRecord {
    pub id: String,
    pub lane: String,
    pub root_id: Option<String>,
    pub command: String,
    pub cwd: String,
    pub timeout: Option<f64>,
    pub status: String,
    pub exit_code: Option<i64>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub output_path: String,
    pub output_bytes: u64,
    pub retained_bytes: u64,
    pub tail: String,
    pub error: Option<String>,
}

fn text_of(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn int_of(value: &Value, key: &str) -> Option<u64> {
    value
        .get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
}

/// A number as JSON.stringify writes it: integral values without a fraction.
fn js_number(value: f64) -> Value {
    if value.fract() == 0.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

impl JobRecord {
    pub fn from_value(value: &Value) -> JobRecord {
        JobRecord {
            id: text_of(value, "id"),
            lane: text_of(value, "lane"),
            root_id: value.get("rootId").and_then(Value::as_str).map(str::to_owned),
            command: text_of(value, "command"),
            cwd: text_of(value, "cwd"),
            timeout: value.get("timeout").and_then(Value::as_f64),
            status: text_of(value, "status"),
            exit_code: value.get("exitCode").and_then(Value::as_i64),
            started_at: int_of(value, "startedAt").unwrap_or(0),
            ended_at: int_of(value, "endedAt"),
            output_path: text_of(value, "outputPath"),
            output_bytes: int_of(value, "outputBytes").unwrap_or(0),
            retained_bytes: int_of(value, "retainedBytes").unwrap_or(0),
            tail: text_of(value, "tail"),
            error: value.get("error").and_then(Value::as_str).map(str::to_owned),
        }
    }

    pub fn to_value(&self) -> Value {
        let mut out = json!({
            "id": self.id,
            "lane": self.lane,
            "rootId": self.root_id,
            "command": self.command,
            "cwd": self.cwd,
            "timeout": self.timeout.map(js_number),
            "status": self.status,
            "exitCode": self.exit_code,
            "startedAt": self.started_at,
            "endedAt": self.ended_at,
            "outputPath": self.output_path,
            "outputBytes": self.output_bytes,
            "retainedBytes": self.retained_bytes,
            "tail": self.tail,
        });
        if let Some(error) = &self.error {
            out["error"] = json!(error);
        }
        out
    }
}

fn read_text(path: &str, cursor: u64, max: u64) -> String {
    let mut buffer = Vec::new();
    if let Ok(mut file) = File::open(path)
        && file.seek(SeekFrom::Start(cursor)).is_ok()
    {
        let _ = file.take(max).read_to_end(&mut buffer);
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// The last `max` bytes of `text`, starting on a character boundary.
fn keep_tail(text: &str, max: usize) -> &str {
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// The last bytes of a job's spill file, when the file holds all of its output.
fn file_tail(record: &JobRecord) -> String {
    if record.retained_bytes != record.output_bytes || record.retained_bytes == 0 {
        return String::new();
    }
    let half = JOB_TEXT_BYTES / 2;
    let text = read_text(
        &record.output_path,
        record.retained_bytes.saturating_sub(half + 4),
        half + 4,
    );
    keep_tail(&text, half as usize).to_owned()
}

/// The jobs restored from a journal, and the document to write back when restoring changed a record.
pub struct Restored {
    pub records: Vec<JobRecord>,
    pub persist: Option<Value>,
}

/// Restore the journal (`saved` is the session value, None when unset) at time `now` (ms).
pub fn restore(saved: Option<&Value>, now: u64) -> Result<Restored, String> {
    let jobs = match saved {
        Some(Value::Object(document)) => {
            if let Some(version) = document.get("version")
                && version.as_f64() != Some(JOURNAL_VERSION as f64)
            {
                return Err(format!(
                    "Cannot resume: session value {JOURNAL_NAMESPACE} has format version {version}, but this Ultron reads only format version {JOURNAL_VERSION}. It was left unchanged; open the session with the Ultron version that wrote it, or upgrade."
                ));
            }
            document
                .get("jobs")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };
    let mut changed = false;
    let records = jobs
        .iter()
        .map(|saved| {
            let mut job = JobRecord::from_value(saved);
            if job.tail.is_empty() {
                job.tail = file_tail(&job);
            }
            if job.status == "running" {
                changed = true;
                job.status = "interrupted".into();
                job.ended_at = Some(job.ended_at.unwrap_or(now));
                job.error = Some("worker restarted".into());
            }
            job
        })
        .collect::<Vec<_>>();
    let persist = changed.then(|| persisted(&records));
    Ok(Restored { records, persist })
}

/// The journal document: every record with its tail left out (it would be rewritten with every job).
pub fn persisted(records: &[JobRecord]) -> Value {
    let jobs: Vec<Value> = records
        .iter()
        .map(|record| {
            let mut value = record.to_value();
            value["tail"] = json!("");
            value
        })
        .collect();
    json!({ "version": JOURNAL_VERSION, "jobs": jobs })
}

/// Whole output when it fits, else the first and last halves around a marker naming how to read the rest.
fn job_text(record: &JobRecord) -> (String, bool) {
    let half = JOB_TEXT_BYTES / 2;
    if record.output_bytes <= JOB_TEXT_BYTES && record.retained_bytes == record.output_bytes {
        return (read_text(&record.output_path, 0, JOB_TEXT_BYTES), false);
    }
    let head = read_text(&record.output_path, 0, half);
    let omitted = record.output_bytes.saturating_sub(half + record.tail.len() as u64);
    let marker = format!(
        "\n[... {omitted} bytes omitted; {} bytes kept in {}: await job.read(cursor={half}, max_bytes=65536) ...]\n",
        record.retained_bytes, record.output_path
    );
    (format!("{head}{marker}{}", record.tail), true)
}

/// The kernel's wire form of a job at time `now` (ms).
pub fn snapshot(record: &JobRecord, now: u64) -> Value {
    let (text, truncated) = job_text(record);
    let elapsed = record.ended_at.unwrap_or(now).saturating_sub(record.started_at) as f64;
    json!({
        "id": record.id,
        "lane": record.lane,
        "command": record.command,
        "status": record.status,
        "running": record.status == "running",
        "exit_code": record.exit_code,
        "ok": record.status == "completed" && record.exit_code == Some(0),
        "timed_out": record.status == "timed_out",
        "cancelled": record.status == "cancelled",
        "text": text,
        "truncated": truncated,
        "output_path": record.output_path,
        "output_bytes": record.output_bytes,
        "elapsed_seconds": js_number((elapsed / 100.0 + 0.5).floor() / 10.0),
        "error": record.error,
    })
}

fn fields(payload: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match payload.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!("Unknown payload field: {key}")),
        None => Ok(()),
    }
}

/// The root lane sees every job; a child lane sees its own.
fn visible<'a>(records: &'a [JobRecord], id: Option<&Value>, lane: &str) -> Result<&'a JobRecord, String> {
    let id = match id {
        Some(Value::String(id)) if !id.is_empty() => id,
        _ => return Err("job id must be a nonempty string".into()),
    };
    records
        .iter()
        .find(|record| &record.id == id && (lane == "main" || record.lane == lane))
        .ok_or_else(|| format!("Unknown shell job {id}"))
}

fn safe_integer(value: Option<&Value>, default: u64) -> Option<u64> {
    match value {
        None => Some(default),
        Some(value) => value.as_u64().or_else(|| {
            value
                .as_f64()
                .filter(|f| f.fract() == 0.0 && *f >= 0.0 && *f <= 9_007_199_254_740_991.0)
                .map(|f| f as u64)
        }),
    }
}

/// `job.read(cursor, max_bytes)`: retained output from `cursor`.
pub fn read(record: &JobRecord, cursor: Option<&Value>, max_bytes: Option<&Value>) -> Result<Value, String> {
    let cursor = safe_integer(cursor, 0).ok_or("cursor must be a non-negative integer")?;
    let max = safe_integer(max_bytes, 65_536)
        .filter(|max| (1..=1_048_576).contains(max))
        .ok_or("max_bytes must be an integer between 1 and 1048576")?;
    let start = cursor.min(record.retained_bytes);
    let text = read_text(&record.output_path, start, max.min(record.retained_bytes - start));
    let next = start + text.len() as u64;
    Ok(json!({
        "text": text,
        "next_cursor": next,
        "done": record.status != "running" && next >= record.retained_bytes,
        "truncated": record.retained_bytes < record.output_bytes,
    }))
}

/// `shell.list`, `shell.get`, `shell.result` and `shell.read` over restored (finished) jobs, for `lane`.
pub fn handle(
    records: &[JobRecord],
    kind: &str,
    payload: &Map<String, Value>,
    lane: &str,
    now: u64,
) -> Result<Value, String> {
    match kind {
        "shell.list" => {
            fields(payload, &[])?;
            let list: Vec<Value> = records
                .iter()
                .rev()
                .filter(|record| lane == "main" || record.lane == lane)
                .map(|record| {
                    let mut value = snapshot(record, now);
                    value["text"] = Value::Null;
                    value
                })
                .collect();
            Ok(Value::Array(list))
        }
        "shell.get" | "shell.result" => {
            fields(payload, if kind == "shell.get" { &["id"] } else { &["id", "wait"] })?;
            Ok(snapshot(visible(records, payload.get("id"), lane)?, now))
        }
        "shell.read" => {
            fields(payload, &["id", "cursor", "max_bytes"])?;
            read(
                visible(records, payload.get("id"), lane)?,
                payload.get("cursor"),
                payload.get("max_bytes"),
            )
        }
        _ => Err(format!("Ultron RLM host request is not wired: {kind}")),
    }
}

/// One-line completion summary: the last three non-empty output lines, at most `max_chars` UTF-16 units.
pub fn job_summary(record: &JobRecord, max_chars: usize) -> String {
    let lines: Vec<&str> = record
        .tail
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let text = lines[lines.len().saturating_sub(3)..].join(" | ");
    let status = match (&record.status[..], &record.error) {
        ("completed", _) => format!(
            "exit {}",
            record.exit_code.map_or("null".to_owned(), |code| code.to_string())
        ),
        (status, Some(error)) if !error.is_empty() => format!("{status}: {error}"),
        (status, _) => status.to_owned(),
    };
    let units: Vec<u16> = text.encode_utf16().collect();
    let body = if units.len() > max_chars {
        format!("…{}", String::from_utf16_lossy(&units[units.len() - (max_chars - 1)..]))
    } else {
        text
    };
    if body.is_empty() {
        status
    } else {
        format!("{status}; {body}")
    }
}

/// The `job_done` event for a job that ended while nobody waited on it and its root turn was not aborted.
pub fn job_event(record: &JobRecord, awaited: bool, root_aborted: bool) -> Option<Value> {
    if awaited || root_aborted {
        return None;
    }
    let mut event = json!({
        "kind": "job_done",
        "id": record.id,
        "status": record.status,
        "summary": job_summary(record, 240),
        "fetch": format!("await rlm.job(\"{}\")", record.id),
        "lane": record.lane,
    });
    if let Some(root) = &record.root_id {
        event["rootId"] = json!(root);
    }
    Some(event)
}
