//! A02: large input stays outside active context; full data remains usable.
//! Ported from packages/coding-agent/test/ultron-a02-large-data.test.ts with its literal expectations: the real
//! runtime.py under the Rust kernel supervisor, the Rust `rlm` tool text (crates/host `cell_output`), and the
//! `ultron` binary in RPC mode (`--mode rpc --no-session`, as the TS test drives it: server generation plus session
//! worker) against a scripted OpenAI-compatible provider that records every request body.

mod common;

use common::{Install, Provider, Reply, kernel_in, null_handler, run};
use host::cell::{CellFormat, cell_output};
use host::secrets::OutputMasker;
use kernel::Status;

const OUTPUT_BUDGET: usize = 20_000;
/// The budget plus the truncation warning and marker lines.
const TOOL_RESULT_BOUND: usize = OUTPUT_BUDGET + 512;
const ROWS: u64 = 600_000;
/// On a middle row: the head and tail of a flood reach the model, the middle never does.
const HIDDEN_SENTINEL: &str = "HIDDEN_SENTINEL_ROW_7f3a";

struct Dataset {
    bytes: usize,
    sum: u64,
    max: u64,
}

/// Deterministic rows `id,value,tag`; the middle row carries the sentinel.
fn write_dataset(dir: &std::path::Path) -> Dataset {
    let mut text = String::from("id,value,tag\n");
    let (mut sum, mut max) = (0, 0);
    for id in 0..ROWS {
        let value = (id * 7919) % 100_003;
        sum += value;
        max = max.max(value);
        let tag = if id == ROWS / 2 {
            HIDDEN_SENTINEL.to_string()
        } else {
            format!("t{}", id % 97)
        };
        text.push_str(&format!("{id},{value},{tag}\n"));
    }
    std::fs::write(dir.join("large.csv"), &text).unwrap();
    Dataset {
        bytes: text.len(),
        sum,
        max,
    }
}

fn kernel_part() {
    let dir = tempfile::tempdir().unwrap();
    let data = write_dataset(dir.path());
    assert!(data.bytes > 8 * 1024 * 1024);
    let kernel = kernel_in(dir.path(), null_handler());
    let masker = OutputMasker::default();
    let spill = dir.path().join("spill");
    // The `rlm` tool's text for a cell, as print mode builds it, with the default output budget.
    let tool = |code: &str| -> String {
        let result = run(&kernel, code);
        assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
        let format = CellFormat {
            masker: &masker,
            mask_secrets: None,
            max_bytes: kernel::rlm_output_budget(None) as usize,
            spill_dir: Some(&spill),
        };
        let text = cell_output(&result, &format).unwrap().text;
        assert!(text.len() <= TOOL_RESULT_BOUND, "{} bytes", text.len());
        assert!(!text.contains(HIDDEN_SENTINEL));
        text
    };
    let path = serde_json::to_string(&dir.path().join("large.csv")).unwrap();
    let loaded = tool(&format!(
        "import sys\nraw = open({path}).read()\nrows = [line.split(',') for line in raw.splitlines()[1:]]\nprint(raw)\nprint(raw, file=sys.stderr)\nrows"
    ));
    assert!(loaded.contains("id,value,tag\n0,0,t0\n"));
    assert!(loaded.contains("Warning: truncated output"));
    assert!(loaded.contains("bytes truncated"));
    assert!(loaded.contains("<list: 600,000 items; item types list>"));

    // Later cells still see every row.
    let aggregate = tool(&format!(
        "values = [int(row[1]) for row in rows]\n(len(rows), sum(values), max(values), sum(1 for row in rows if row[2] == {HIDDEN_SENTINEL:?}))"
    ));
    assert_eq!(aggregate, format!("({ROWS}, {}, {}, 1)", data.sum, data.max));

    // A huge single value and a huge int are previewed, not returned whole.
    let huge = tool("raw");
    let header = huge.lines().next().unwrap();
    assert!(
        header.starts_with("<str: ") && header.contains(" chars, ") && header.contains(" lines, sha256 "),
        "{header}"
    );
    assert!(huge.lines().nth(1).unwrap().starts_with("head: 'id,value,tag"));
    assert!(huge.contains("kept in the kernel as `_`"));
    let big_int = tool("10 ** 200000");
    assert!(
        big_int.starts_with("<int with ") && big_int.contains(" bits>"),
        "{big_int}"
    );
    kernel.shutdown();
}

fn end_to_end_part() {
    let code = "import sys\nraw = open('large.csv').read()\nrows = [line.split(',') for line in raw.splitlines()[1:]]\nprint(raw)\ntotal = sum(int(row[1]) for row in rows)\nf'ROWS={len(rows)} SUM={total}'";
    let provider = Provider::start(move |request| {
        if request.tool_results().is_empty() {
            Reply::Rlm(code.into())
        } else {
            Reply::Text("A02_DONE".into())
        }
    });
    let install = Install::new(&provider);
    let data = write_dataset(&install.project());
    let mut client = install.rpc(&["--no-session"], &[]);
    client
        .prompt_and_wait("Sum the value column of large.csv", common::HANG)
        .unwrap();
    assert_eq!(client.get_last_assistant_text().unwrap().as_deref(), Some("A02_DONE"));
    assert_eq!(client.stop(), Some(0));

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let (first, second) = (requests[0].raw.len(), requests[1].raw.len());
    // The follow-up request grows only by the tool call and the bounded tool result.
    assert!(second - first <= TOOL_RESULT_BOUND + 4096, "grew by {}", second - first);
    assert!(second < data.bytes / 100);
    assert!(requests[1].raw.contains(&format!("ROWS={ROWS} SUM={}", data.sum)));
    assert!(requests[1].raw.contains("bytes truncated"));
    assert!(!requests[1].raw.contains(HIDDEN_SENTINEL));
}

#[test]
fn acceptance_a02_large_input_stays_in_the_kernel_and_the_model_sees_only_bounded_results() {
    kernel_part();
    end_to_end_part();
}
