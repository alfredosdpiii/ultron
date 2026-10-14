//! A27: RLM handles a self-contained task without graph/delegation/memory/refinement.
//! Ported from packages/coding-agent/test/ultron-a27-self-contained-rlm.test.ts: a real kernel with a recording
//! (and a rejecting) host handler; the Rust host operations (crates/host) behind a
//! recorder with an empty Rust task supervisor; and the `ultron` binary running the task as `rlm` cells.

mod common;

use common::{Install, Provider, Recorder, Reply, kernel_in, run, stdout_of, value};
use harness::supervisor::TaskSupervisor;
use harness::task_store::TaskDocumentStore;
use host::cell::{CellFormat, cell_output};
use host::secrets::OutputMasker;
use host::{AllowAll, Host};
use kernel::{HostHandler, Status};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

const REGIONS: [&str; 4] = ["north", "south", "east", "west"];
const ROWS: usize = 5_000;

/// Self-contained task: build CSV text, parse it, and aggregate, all in Python.
fn task_cells() -> [String; 3] {
    [
        format!(
            "import csv, io, json, statistics\nlines = ['id,region,amount,status']\nregions = {}\nfor i in range({ROWS}):\n    lines.append(f\"{{i}},{{regions[i % 4]}},{{(i * 37) % 101}},{{'void' if i % 7 == 0 else 'ok'}}\")\nraw = '\\n'.join(lines)\nlen(raw.splitlines())",
            json!(REGIONS)
        ),
        "rows = list(csv.DictReader(io.StringIO(raw)))\ntotals = {}\nfor row in rows:\n    if row['status'] == 'ok':\n        totals[row['region']] = totals.get(row['region'], 0) + int(row['amount'])\nvoided = sum(1 for row in rows if row['status'] == 'void')\ntop = max(totals, key=totals.get)\nmedian = statistics.median(int(row['amount']) for row in rows)".into(),
        "print(json.dumps({'totals': totals, 'voided': voided, 'top': top, 'median': median}, sort_keys=True))".into(),
    ]
}

/// The answer computed independently, in Python's `json.dumps(sort_keys=True)` format (float median).
fn expected_answer() -> String {
    let mut totals: BTreeMap<&str, u64> = BTreeMap::new();
    let mut voided = 0;
    let mut amounts = Vec::new();
    for i in 0..ROWS {
        let amount = (i as u64 * 37) % 101;
        amounts.push(amount);
        if i % 7 == 0 {
            voided += 1;
        } else {
            *totals.entry(REGIONS[i % 4]).or_default() += amount;
        }
    }
    amounts.sort_unstable();
    let twice = amounts[ROWS / 2 - 1] + amounts[ROWS / 2];
    let median = if twice % 2 == 0 {
        format!("{}.0", twice / 2)
    } else {
        format!("{}.5", twice / 2)
    };
    // `max(totals, key=totals.get)`: the first region (in insertion order) with the largest total.
    let mut top = REGIONS[0];
    for region in REGIONS {
        if totals[region] > totals[top] {
            top = region;
        }
    }
    let totals_text: Vec<String> = totals.iter().map(|(k, v)| format!("\"{k}\": {v}")).collect();
    format!(
        "{{\"median\": {median}, \"top\": \"{top}\", \"totals\": {{{}}}, \"voided\": {voided}}}\n",
        totals_text.join(", ")
    )
}

fn recording_kernel_part() {
    let cells = task_cells();
    let (recorder, kinds) = Recorder::new(|_: &str, _, _: &_| Ok(json!({"recorded": true})));
    let kernel = kernel_in(std::path::Path::new(env!("CARGO_MANIFEST_DIR")), recorder);
    assert_eq!(value(&kernel, &cells[0]), (ROWS + 1).to_string());
    assert_eq!(run(&kernel, &cells[1]).status, Status::Ok);
    let last = run(&kernel, &cells[2]);
    assert_eq!((last.status, last.stdout), (Status::Ok, expected_answer()));
    // No graph, delegation, memory, refinement, or any other host request was made.
    assert!(kinds.lock().unwrap().is_empty());
    kernel.shutdown();
}

fn rejecting_kernel_part() {
    let cells = task_cells();
    let (recorder, kinds) = Recorder::new(|kind: &str, _, _: &_| Err(format!("optional service unavailable: {kind}")));
    let kernel = kernel_in(std::path::Path::new(env!("CARGO_MANIFEST_DIR")), recorder);
    for cell in &cells[..2] {
        assert_eq!(run(&kernel, cell).status, Status::Ok);
    }
    assert_eq!(run(&kernel, &cells[2]).stdout, expected_answer());
    assert!(kinds.lock().unwrap().is_empty());
    // Absent services surface as explicit errors, never as fabricated results.
    let failed = run(&kernel, "await memory.prepare('regional totals')");
    assert_eq!(failed.status, Status::Error);
    let error = failed.error.unwrap();
    assert_eq!(
        (error.ename.as_str(), error.evalue.as_str()),
        ("RuntimeError", "optional service unavailable: memory.prepare")
    );
    assert_eq!(*kinds.lock().unwrap(), ["memory.prepare"]);
    // The computed state survives the failed optional call.
    assert!(value(&kernel, "top, voided").starts_with("('"));
    kernel.shutdown();
}

#[derive(Default)]
struct MemoryStore(Option<Value>);

impl TaskDocumentStore for MemoryStore {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(self.0.clone())
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        self.0 = Some(document.clone());
        Ok(())
    }
}

/// The Rust host operations behind a recorder: the task makes no request,
/// and the Rust task supervisor holds no task.
fn native_host_part() {
    let cells = task_cells();
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(Host::new(
        dir.path(),
        dir.path().join("jobs"),
        dir.path().join("spill"),
        Box::new(AllowAll),
    ));
    let (recorder, kinds) = Recorder::new(move |kind: &str, payload, signal: &_| host.handle(kind, payload, signal));
    let kernel = kernel_in(dir.path(), recorder);
    let masker = OutputMasker::default();
    let format = CellFormat {
        masker: &masker,
        mask_secrets: None,
        max_bytes: kernel::rlm_output_budget(None) as usize,
        spill_dir: None,
    };
    let mut last = String::new();
    for cell in &cells {
        last = cell_output(&run(&kernel, cell), &format).unwrap().text;
    }
    assert_eq!(last.split('\n').next().unwrap(), expected_answer().trim_end());
    assert!(kinds.lock().unwrap().is_empty());
    let mut supervisor = TaskSupervisor::new(MemoryStore::default());
    assert_eq!(supervisor.status(None).unwrap(), Vec::<Value>::new());
    // Optional services are genuinely absent here: asking for one fails explicitly.
    let refused = cell_output(&run(&kernel, "await refinements.list()"), &format).unwrap_err();
    assert!(
        refused
            .message
            .contains("Ultron RLM host request is not available in this host yet: refinements.list"),
        "{}",
        refused.message
    );
    kernel.shutdown();
}

/// The same task as `rlm` cells of a real `ultron -p` turn.
fn end_to_end_part() {
    let cells = task_cells();
    let provider = Provider::start(move |request| match cells.get(request.tool_results().len()) {
        Some(cell) => Reply::Rlm(cell.clone()),
        None if request.tool_results().len() == 3 => Reply::Rlm("await refinements.list()".into()),
        None => Reply::Text("A27_DONE".into()),
    });
    let install = Install::new(&provider);
    assert_eq!(stdout_of(&install.prompt("aggregate the regions", &[])), "A27_DONE\n");
    let requests = provider.requests();
    assert_eq!(requests.len(), 5);
    let results = requests[4].tool_results();
    assert_eq!(results[0], (ROWS + 1).to_string());
    assert_eq!(results[1], "(no result)");
    assert_eq!(results[2], expected_answer().trim_end());
    // The shipped stack serves the local services (session-core/src/compose.rs), as the TS session worker does: the
    // self-contained run wrote no refinement, so the ledger is empty. (The TS refusal is the bare host above.)
    assert_eq!(results[3], "[]");
}

#[test]
fn acceptance_a27_rlm_completes_a_self_contained_task_without_host_services() {
    recording_kernel_part();
    rejecting_kernel_part();
    native_host_part();
    end_to_end_part();
}
