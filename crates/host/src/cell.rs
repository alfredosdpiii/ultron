//! One cell's tool result text: `UltronRlmKernel.executeCell` (experimental/session-worker.ts) over
//! `truncateToolOutput` (ultron/rlm/output-truncation.ts) and `maskCellOutputCounted` (ultron/rlm/output-secrets.ts).
//!
//! A cell's stdout, stderr and value are joined by newlines, secrets are masked, and an oversized text keeps its head
//! and tail (the whole text spilled to a private file when a spill directory is given). A failed cell becomes
//! [`CellFailure`]: what it printed, then the traceback.

use crate::secrets::OutputMasker;
use kernel::{ExecutionResult, Status};
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Spill files kept per directory.
pub const SPILL_KEEP: usize = 20;

/// A cell's tool result: its text and how many secrets were masked in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellOutput {
    pub text: String,
    pub masked: usize,
}

/// A cell that raised (`RlmCellError`): the message is the output and traceback; `ename` the exception type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellFailure {
    pub message: String,
    pub ename: String,
    pub masked: usize,
}

/// How a cell's text is finished: the masker, `ULTRON_MASK_SECRETS`, the byte budget and the spill directory.
pub struct CellFormat<'a> {
    pub masker: &'a OutputMasker,
    pub mask_secrets: Option<&'a str>,
    pub max_bytes: usize,
    pub spill_dir: Option<&'a Path>,
}

/// `.replace(/\n$/, "")`.
fn drop_final_newline(text: &str) -> &str {
    text.strip_suffix('\n').unwrap_or(text)
}

fn join_present(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

/// `executeCell`'s text for a kernel result.
pub fn cell_output(result: &ExecutionResult, format: &CellFormat<'_>) -> Result<CellOutput, CellFailure> {
    let stdout = drop_final_newline(&result.stdout);
    let stderr = drop_final_newline(&result.stderr);
    if result.status == Status::Error {
        let ename = result
            .error
            .as_ref()
            .map_or("PythonError", |error| error.ename.as_str());
        let evalue = result
            .error
            .as_ref()
            .map_or("Execution failed", |error| error.evalue.as_str());
        let summary = format!("{ename}: {evalue}");
        let traceback = result
            .error
            .as_ref()
            .map(|error| error.traceback.join("\n"))
            .unwrap_or_default();
        let failure = if traceback.is_empty() {
            summary
        } else if traceback.ends_with(&summary) {
            traceback
        } else {
            format!("{traceback}\n{summary}")
        };
        let (text, masked) = format
            .masker
            .mask_counted(&join_present(&[stdout, stderr, &failure]), format.mask_secrets);
        return Err(CellFailure {
            message: truncate_tool_output(&text, format.max_bytes, format.spill_dir),
            ename: ename.to_string(),
            masked,
        });
    }
    let value = result.result.as_deref().unwrap_or("");
    let (text, masked) = format
        .masker
        .mask_counted(&join_present(&[stdout, stderr, value]), format.mask_secrets);
    Ok(CellOutput {
        text: truncate_tool_output(&text, format.max_bytes, format.spill_dir),
        masked,
    })
}

/// `spillOutput`: `text` in a new 0600 file in the 0700 `dir`, dropping the oldest `.txt` files beyond
/// [`SPILL_KEEP`]; the path, or `None` when it cannot be written.
pub fn spill_output(text: &str, dir: &Path, name: &str) -> Option<String> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir).ok()?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).ok()?;
    let mut files: Vec<(SystemTime, std::path::PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".txt"))
        .map(|entry| {
            let modified = entry.metadata().and_then(|meta| meta.modified()).unwrap_or(UNIX_EPOCH);
            (modified, entry.path())
        })
        .collect();
    files.sort();
    let excess = files.len().saturating_sub(SPILL_KEEP - 1);
    for (_, path) in files.iter().take(excess) {
        std::fs::remove_file(path).ok()?;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let path = dir.join(format!("{nanos}-{}-{name}.txt", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .ok()?;
    file.write_all(text.as_bytes()).ok()?;
    Some(path.display().to_string())
}

/// `truncateToolOutput`: keep the head and tail of `text` within `max_bytes` (UTF-8) and say what was elided.
pub fn truncate_tool_output(text: &str, max_bytes: usize, spill_dir: Option<&Path>) -> String {
    let data = text.as_bytes();
    if data.len() <= max_bytes {
        return text.to_string();
    }
    let saved = spill_dir.and_then(|dir| spill_output(text, dir, "cell"));
    let keep = max_bytes / 2;
    // A cut inside a multi-byte character drops that character rather than adding a replacement glyph.
    let head = String::from_utf8_lossy(&data[..keep]);
    let head = head.strip_suffix('\u{fffd}').unwrap_or(&head);
    let tail = String::from_utf8_lossy(&data[data.len() - keep..]);
    let tail = tail.strip_prefix('\u{fffd}').unwrap_or(&tail);
    let lines = text.matches('\n').count() + 1;
    let elided = data.len() - 2 * keep;
    let saved = saved.map_or_else(String::new, |path| format!("; full output in {path}"));
    [
        format!(
            "Warning: truncated output ({} bytes, {lines} lines; the middle {elided} bytes were cut). Process large data in Python and print only what you need.",
            data.len()
        ),
        String::new(),
        head.to_string(),
        format!("[... {elided} bytes truncated{saved} ...]"),
        tail.to_string(),
    ]
    .join("\n")
}
