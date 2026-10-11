//! Replays crates/host/fixtures/cell.json, recorded by fixtures/cell.mjs from TypeScript's `executeCell` (with a
//! scripted kernel result) and `truncateToolOutput`: the same results must give the same text, failure and count.

use host::cell::{CellFormat, cell_output, truncate_tool_output};
use host::secrets::OutputMasker;
use kernel::{CellError, ExecutionResult, Status};
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/cell.json")).unwrap()
}

fn string(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

fn execution_result(value: &Value) -> ExecutionResult {
    ExecutionResult {
        status: if value["status"] == "ok" {
            Status::Ok
        } else {
            Status::Error
        },
        stdout: string(&value["stdout"]),
        stderr: string(&value["stderr"]),
        result: value["result"].as_str().map(str::to_string),
        error: value.get("error").map(|error| CellError {
            ename: string(&error["ename"]),
            evalue: string(&error["evalue"]),
            traceback: error["traceback"].as_array().unwrap().iter().map(string).collect(),
        }),
    }
}

#[test]
fn host_cell_output_matches_typescript_execute_cell() {
    let masker = OutputMasker::default();
    let format = CellFormat {
        masker: &masker,
        mask_secrets: None,
        max_bytes: 20_000,
        spill_dir: None,
    };
    let fixture = fixture();
    let cells = fixture["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 16);
    for (index, cell) in cells.iter().enumerate() {
        let got = cell_output(&execution_result(&cell["result"]), &format);
        match (&cell["ok"], &cell["error"], got) {
            (ok, Value::Null, Ok(output)) => {
                assert_eq!(output.text, string(&ok["text"]), "cell {index} text");
                assert_eq!(
                    output.masked as u64,
                    ok["masked"].as_u64().unwrap(),
                    "cell {index} masked"
                );
            }
            (Value::Null, error, Err(failure)) => {
                assert_eq!(failure.message, string(&error["message"]), "cell {index} message");
                assert_eq!(failure.ename, string(&error["ename"]), "cell {index} ename");
                assert_eq!(
                    failure.masked as u64,
                    error["masked"].as_u64().unwrap(),
                    "cell {index} masked"
                );
            }
            (_, _, got) => panic!("cell {index}: {got:?} where TypeScript gave {cell}"),
        }
    }
}

#[test]
fn host_cell_truncation_matches_typescript() {
    for case in fixture()["truncations"].as_array().unwrap() {
        let max = case["maxBytes"].as_u64().unwrap() as usize;
        let text = string(&case["text"]);
        assert_eq!(
            truncate_tool_output(&text, max, None),
            string(&case["out"]),
            "{max} bytes of {}",
            text.chars().take(20).collect::<String>()
        );
    }
}

/// With a spill directory, the whole text is saved to a private file named in the marker (output-truncation.ts).
#[test]
fn host_cell_truncation_spills_whole_text() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let spill = dir.path().join("lane");
    let text = "y".repeat(3000);
    let out = truncate_tool_output(&text, 1024, Some(&spill));
    let marker = out
        .lines()
        .find(|line| line.starts_with("[... 1976 bytes truncated; full output in "))
        .unwrap();
    let path = marker
        .trim_start_matches("[... 1976 bytes truncated; full output in ")
        .trim_end_matches(" ...]");
    assert!(path.starts_with(&spill.display().to_string()) && path.ends_with("-cell.txt"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), text);
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!((mode(&spill), mode(std::path::Path::new(path))), (0o700, 0o600));
    for _ in 0..25 {
        truncate_tool_output(&text, 1024, Some(&spill));
    }
    assert_eq!(std::fs::read_dir(&spill).unwrap().count(), 20);
}
