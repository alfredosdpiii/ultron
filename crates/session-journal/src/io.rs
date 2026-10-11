//! File helpers (`jsonl/io.ts`): complete lines, first line, atomic publication through `<path>.tmp`.

use std::fs;
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};

/// `splitCompleteLines`: lines that end in `\n`, and whether an unterminated tail followed them.
pub fn split_complete_lines(content: &str) -> (Vec<&str>, bool) {
    if let Some(body) = content.strip_suffix('\n') {
        return (body.split('\n').collect(), false);
    }
    match content.rfind('\n') {
        None => (Vec::new(), true),
        Some(last) => (content[..last].split('\n').collect(), true),
    }
}

/// Complete lines only (the reader's `terminated` lines).
pub fn complete_lines(content: &str) -> Vec<&str> {
    split_complete_lines(content).0
}

/// The first line when it is newline-terminated; `None` for an empty or unterminated first line.
pub fn read_first_line(path: &Path) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    BufReader::new(fs::File::open(path)?).read_until(b'\n', &mut line)?;
    if line.last() != Some(&b'\n') {
        return Ok(None);
    }
    line.pop();
    Ok(Some(String::from_utf8_lossy(&line).into_owned()))
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// `publishFileAtomically`: write `content` to `<path>.tmp`, then rename it over `path`.
pub fn publish(path: &Path, content: &str) -> Result<(), String> {
    let temp = temp_path(path);
    let result = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    result.map_err(|e| {
        let _ = fs::remove_file(&temp);
        format!("Failed to publish JSONL storage {}: {e}", path.display())
    })
}

/// Append one line.
pub fn append(path: &Path, line: &str) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| format!("Failed to append JSONL storage {}: {e}", path.display()))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("Failed to append JSONL storage {}: {e}", path.display()))
}

/// File modification time in milliseconds (`mtimeMs`).
pub fn mtime_ms(path: &Path) -> Option<f64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(since.as_nanos() as f64 / 1_000_000.0)
}
