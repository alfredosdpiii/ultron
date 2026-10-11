//! Replays crates/host/fixtures/host.json, recorded by fixtures/generate.mjs from the TypeScript host operations
//! (host-bash.ts, shell-jobs.ts, routed as in session-worker.ts) serving the real runtime.py: the same cells run
//! through crates/kernel with this crate's Host and must give identical (normalized) results.

use host::{AllowAll, Guard, Host};
use kernel::{Kernel, KernelOptions};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

fn normalize(text: &str, cwd: &str, jobs: &str) -> String {
    let text = text.replace(jobs, "<jobs>").replace(cwd, "<cwd>");
    replace_ids(&replace_ids(&text, "job-", 8, "job-ID"), "pi-bash-", 16, "pi-bash-ID")
}

/// `prefix` followed by `len` lowercase hex digits becomes `id`, as the generator's regexes do.
fn replace_ids(text: &str, prefix: &str, len: usize, id: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(prefix) {
        out.push_str(&rest[..at]);
        let tail = &rest[at + prefix.len()..];
        let hex = tail
            .bytes()
            .take(len)
            .filter(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            .count();
        if hex == len {
            out.push_str(id);
            rest = &tail[len..];
        } else {
            out.push_str(prefix);
            rest = tail;
        }
    }
    out + rest
}

/// Runs `cells` through runtime.py with a Host on `guard`, in a fresh temp dir; one recorded entry per cell.
fn replay(cells: &[String], guard: Box<dyn Guard>) -> Vec<Value> {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let jobs = cwd.join(".jobs");
    let host = Arc::new(Host::new(&cwd, &jobs, &cwd, guard));
    let mut options = KernelOptions::new(&cwd, runtime());
    options.env.push(("ULTRON_RLM_SPILL_DIR".into(), String::new()));
    options.env.push(("TMPDIR".into(), cwd.to_string_lossy().into_owned()));
    let kernel = Kernel::new(options, host).unwrap();
    let (cwd_text, jobs_text) = (cwd.to_string_lossy().into_owned(), jobs.to_string_lossy().into_owned());
    let results = cells
        .iter()
        .map(|code| match kernel.execute(code, None, None) {
            Ok(result) => {
                let error = result.error.map(|error| format!("{}: {}", error.ename, error.evalue));
                let recorded = json!({
                    "status": result.status,
                    "stdout": result.stdout,
                    "result": result.result,
                    "error": error,
                });
                json!({ "code": code, "ok": serde_json::from_str::<Value>(&normalize(&recorded.to_string(), &cwd_text, &jobs_text)).unwrap() })
            }
            Err(error) => json!({ "code": code, "error": normalize(error.message(), &cwd_text, &jobs_text) }),
        })
        .collect();
    kernel.shutdown();
    results
}

#[test]
fn host_cells_match_the_typescript_host() {
    let fixture = fixture();
    let expected = fixture["results"].as_array().unwrap();
    let cells: Vec<String> = expected
        .iter()
        .map(|r| r["code"].as_str().unwrap().to_owned())
        .collect();
    let actual = replay(&cells, Box::new(AllowAll));
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(actual, expected);
    }
    assert_eq!(actual.len(), expected.len());
}

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/host.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn host_chunk_cleaning_matches_strip_ansi_and_sanitize() {
    for case in fixture()["clean"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        assert_eq!(host::text::clean(input), case["output"].as_str().unwrap(), "{input:?}");
    }
}

#[test]
fn host_truncate_tail_matches_typescript() {
    for case in fixture()["tail"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let output = host::text::truncate_tail(input);
        assert_eq!(
            output.is_some(),
            case["truncated"].as_bool().unwrap(),
            "{}",
            input.len()
        );
        assert_eq!(
            output.as_deref().unwrap_or(input),
            case["output"].as_str().unwrap(),
            "{}",
            input.len()
        );
    }
}

#[test]
fn host_streaming_decoder_matches_text_decoder() {
    for case in fixture()["decoder"].as_array().unwrap() {
        let bytes: Vec<u8> = case["bytes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b.as_u64().unwrap() as u8)
            .collect();
        let cut = case["cut"].as_u64().unwrap() as usize;
        let mut decoder = host::text::Decoder::default();
        let text =
            decoder.decode(&bytes[..cut], true) + &decoder.decode(&bytes[cut..], true) + &decoder.decode(&[], false);
        assert_eq!(text, case["text"].as_str().unwrap(), "{bytes:?} cut {cut}");
    }
}

/// Blocks writes whose content holds "FORBIDDEN" and commands starting with "rm ", adds a note to every write.
struct Strict;
impl Guard for Strict {
    fn before_shell(&self, command: &str) -> Option<String> {
        command.starts_with("rm ").then(|| "loki: no rm".to_owned())
    }
    fn before_write(&self, _path: &str, content: &str) -> Value {
        json!({ "blocked": content.contains("FORBIDDEN"), "reason": "loki: forbidden word", "notes": ["[loki] checked"] })
    }
}

/// Expectations are runtime.py's literal messages for a write hook's block and notes (`_check_write`) and the
/// session worker's shell guard ("bash command was not run: ...").
#[test]
fn host_guard_blocks_writes_edits_and_shell_commands() {
    let cells: Vec<String> = [
        "import os; os.environ['ULTRON_FILE_HOOKS'] = '1'; await write('a.txt', 'fine')",
        "await write('b.txt', 'FORBIDDEN')",
        "os.path.exists('b.txt')",
        "await edit('a.txt', 'fine', 'FORBIDDEN')",
        "open('a.txt').read()",
        "await bash('rm a.txt')",
        "str(await bash('echo ok'))",
    ]
    .map(String::from)
    .to_vec();
    let results = replay(&cells, Box::new(Strict));
    let ok = |i: usize| {
        (
            results[i]["ok"]["stdout"].clone(),
            results[i]["ok"]["result"].clone(),
            results[i]["ok"]["error"].clone(),
        )
    };
    assert_eq!(
        ok(0),
        (json!("[loki] checked\n"), json!("'Wrote a.txt (4 bytes)'"), Value::Null)
    );
    assert_eq!(
        ok(1),
        (
            json!("[loki] checked\n"),
            Value::Null,
            json!("ValueError: b.txt was not written: loki: forbidden word")
        )
    );
    assert_eq!(ok(2).1, json!("False"));
    assert_eq!(
        ok(3).2,
        json!("ValueError: a.txt was not written: loki: forbidden word")
    );
    assert_eq!(ok(4).1, json!("'fine'"));
    assert_eq!(ok(5).2, json!("RuntimeError: bash command was not run: loki: no rm"));
    assert_eq!(ok(6).1, json!("'ok'"));
}

#[test]
fn host_refuses_requests_that_need_models_or_agents() {
    let kinds = [
        "rlm.infer",
        "rlm.map",
        "rlm.spawn",
        "agents.invoke",
        "memory.prepare",
        "rlm.view_image",
        "rlm.load",
    ];
    let cells: Vec<String> = kinds
        .iter()
        .map(|kind| format!("await rlm.host_request('{kind}', {{}})"))
        .collect();
    for (kind, result) in kinds.iter().zip(replay(&cells, Box::new(AllowAll))) {
        let expected = format!("RuntimeError: Ultron RLM host request is not available in this host yet: {kind}");
        assert_eq!(result["ok"]["error"], json!(expected));
    }
}

/// bash-executor.ts writes the whole cleaned output to the spill file once the raw output passes 50 KiB or the tail is
/// truncated; the reply keeps the last 2000 lines.
#[test]
fn host_blocking_bash_spills_the_whole_output() {
    let cells = vec![
        "import os; os.environ['ULTRON_BASH_YIELD_AFTER'] = 'off'; r = await rlm.host_request('bash', {'command': 'seq 1 3000'}); (len(open(r['full_output_path']).read()), r['output'].count(chr(10)))".to_owned(),
    ];
    assert_eq!(
        replay(&cells, Box::new(AllowAll))[0]["ok"]["result"],
        json!("(13893, 1999)")
    );
}

/// shell-jobs.ts `shell.bash` and host-bash.ts: a cell stopped while `bash` waits stops the command and reports it
/// cancelled (not timed out), with no exit code and nothing left running.
#[test]
fn host_bash_stops_the_command_when_the_cell_is_aborted() {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new(dir.path(), dir.path().join("jobs"), dir.path(), Box::new(AllowAll));
    for payload in [
        json!({ "command": "sleep 5", "yield_after": 30 }),
        json!({ "command": "sleep 5" }),
    ] {
        let signal = kernel::Signal::new();
        let aborter = signal.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            aborter.abort("cell ended");
        });
        let started = std::time::Instant::now();
        let reply = kernel::HostHandler::handle(&host, "bash", payload.as_object().unwrap().clone(), &signal).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        assert_eq!(
            (
                &reply["cancelled"],
                &reply["timed_out"],
                &reply["exit_code"],
                &reply["output"]
            ),
            (&json!(true), &json!(false), &Value::Null, &json!(""))
        );
        assert_ne!(reply["running"], json!(true));
    }
}
