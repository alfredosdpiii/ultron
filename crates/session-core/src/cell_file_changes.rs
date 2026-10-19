//! The diff of what an rlm cell wrote, in its result's `details.fileChanges` (Rust-only; the TypeScript result's
//! details stay `{}`): `[{path, created, diff}]` per file written through the checked `write()`/`edit()`
//! ([`host::cell_writes`]), `diff` in the edit tool's display format (`generateDiffString`, context 2), or null for a
//! file too large or not text. The interactive mode shows it under the expanded cell.

use host::Host;
use serde_json::{Value, json};

/// Context lines around each change.
pub const CONTEXT_LINES: usize = 2;
/// Diff lines kept per file; the rest is counted in `omitted`.
pub const MAX_DIFF_LINES: usize = 400;

/// `details` of the result of `lane`'s cell that just ended: `{}`, or `{fileChanges: [...]}`. `mask` hides secrets
/// in each diff as in the cell's output (the files' texts are not masked on disk).
pub fn details(host: &Host, lane: &str, mask: &dyn Fn(&str) -> String) -> Value {
    let changes: Vec<Value> = host
        .take_cell_writes(lane)
        .into_iter()
        .map(|write| {
            let path =
                host::file_hooks::inside(host.cwd(), &write.path).unwrap_or_else(|| write.path.display().to_string());
            let mut change = json!({ "path": path, "created": write.created, "diff": null });
            if let Some((before, after)) = &write.texts {
                let (diff, _) = session_file::diff::generate_diff_string(before, after, CONTEXT_LINES);
                let diff = mask(&diff);
                let lines: Vec<&str> = diff.split('\n').collect();
                change["diff"] = json!(lines[..lines.len().min(MAX_DIFF_LINES)].join("\n"));
                if lines.len() > MAX_DIFF_LINES {
                    change["omitted"] = json!(lines.len() - MAX_DIFF_LINES);
                }
            }
            change
        })
        .collect();
    if changes.is_empty() {
        json!({})
    } else {
        json!({ "fileChanges": changes })
    }
}
