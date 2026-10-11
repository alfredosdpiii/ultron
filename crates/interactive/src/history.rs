//! Prompt history across runs (experimental/prompt-history.ts): `<agentDir>/prompt-history.jsonl`, one
//! `{text, cwd, timestamp}` per submitted prompt. Up/Down browse this project's prompts first, then other
//! projects'. Appends are written on a background thread so submitting never waits on the disk.

use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

pub const PROMPT_HISTORY_FILE: &str = "prompt-history.jsonl";
pub const PROMPT_HISTORY_LIMIT: usize = 500;
const TAIL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct TimedPrompt {
    pub text: String,
    pub timestamp: i64,
}

pub struct PromptHistoryStore {
    path: Option<PathBuf>,
    cwd: String,
    limit: usize,
    last: Option<String>,
    current: Vec<TimedPrompt>,
    others: Vec<TimedPrompt>,
    writer: Option<mpsc::Sender<String>>,
}

/// The last `TAIL_BYTES` of the file, from its first whole line.
fn read_tail(path: &Path) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0 {
        text = text.split_once('\n').map(|x| x.1.to_string()).unwrap_or_default();
    }
    Ok(text)
}

/// `mergePromptHistory`: groups in order, trimmed, first occurrence kept, at most `limit`.
pub fn merge_prompt_history(groups: &[Vec<String>], limit: usize) -> Vec<String> {
    let mut merged: Vec<String> = Vec::new();
    for group in groups {
        for raw in group {
            let text = raw.trim();
            if text.is_empty() || merged.iter().any(|m| m == text) {
                continue;
            }
            merged.push(text.to_string());
            if merged.len() >= limit {
                return merged;
            }
        }
    }
    merged
}

impl PromptHistoryStore {
    /// `path` `None` keeps history in memory.
    pub fn new(path: Option<PathBuf>, cwd: &str) -> Self {
        let writer = path.clone().map(|path| {
            let (tx, rx) = mpsc::channel::<String>();
            std::thread::spawn(move || {
                for line in rx {
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let separator = match std::fs::read(&path) {
                        Ok(bytes) if !bytes.is_empty() && !bytes.ends_with(b"\n") => "\n",
                        _ => "",
                    };
                    let mut options = std::fs::OpenOptions::new();
                    options.create(true).append(true);
                    #[cfg(unix)]
                    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
                    if let Ok(mut file) = options.open(&path) {
                        let _ = file.write_all(format!("{separator}{line}\n").as_bytes());
                    }
                }
            });
            tx
        });
        PromptHistoryStore {
            path,
            cwd: cwd.to_string(),
            limit: PROMPT_HISTORY_LIMIT,
            last: None,
            current: Vec::new(),
            others: Vec::new(),
            writer,
        }
    }

    /// Read the file; returns the merged history, newest first.
    pub fn load(&mut self) -> Vec<String> {
        let mut entries = Vec::new();
        if let Some(text) = self.path.as_deref().and_then(|p| read_tail(p).ok()) {
            for line in text.split('\n').filter(|l| !l.trim().is_empty()) {
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                let Some(text) = value["text"].as_str().map(str::trim).filter(|t| !t.is_empty()) else {
                    continue;
                };
                let cwd = value["cwd"].as_str().unwrap_or("").to_string();
                let timestamp = value["timestamp"].as_f64().unwrap_or(0.0) as i64;
                entries.push((text.to_string(), cwd, timestamp));
            }
        }
        self.last = entries.last().map(|e| e.0.clone());
        self.current.clear();
        self.others.clear();
        for (text, cwd, timestamp) in entries.into_iter().rev() {
            let prompt = TimedPrompt { text, timestamp };
            if cwd == self.cwd {
                self.current.push(prompt);
            } else {
                self.others.push(prompt);
            }
        }
        self.history(&[])
    }

    /// This project's prompts (with `session` prompts, by time), then other projects'.
    pub fn history(&self, session: &[TimedPrompt]) -> Vec<String> {
        let mut current: Vec<&TimedPrompt> = self.current.iter().chain(session).collect();
        current.sort_by_key(|p| std::cmp::Reverse(p.timestamp));
        merge_prompt_history(
            &[
                current.iter().map(|p| p.text.clone()).collect(),
                self.others.iter().map(|p| p.text.clone()).collect(),
            ],
            self.limit,
        )
    }

    /// Record a submitted prompt; consecutive duplicates are skipped.
    pub fn append(&mut self, text: &str, now: i64) -> Option<String> {
        let trimmed = text.trim();
        if trimmed.is_empty() || self.last.as_deref() == Some(trimmed) {
            return None;
        }
        self.last = Some(trimmed.to_string());
        self.current.insert(
            0,
            TimedPrompt {
                text: trimmed.to_string(),
                timestamp: now,
            },
        );
        if let Some(writer) = &self.writer {
            let entry = json!({"text": trimmed, "cwd": self.cwd, "timestamp": now});
            let _ = writer.send(entry.to_string());
        }
        Some(trimmed.to_string())
    }
}
