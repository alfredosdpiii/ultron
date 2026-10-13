//! Snapshot and continuity process tests with the real runtime.py.
//!
//! `kernel_snapshot_*`: ported from packages/coding-agent/test/ultron-a08-snapshot-restorability.test.ts and
//! ultron-snapshot-auth.test.ts with their literal expectations. `kernel_continuity_*`: the Rust-only continuity
//! policy (no TypeScript counterpart): state survives SIGKILL of the kernel, a refused checkpoint is reported, and
//! names that cannot be restored are reported lost, never silently replaced.

use kernel::{
    ContinuityPolicy, ContinuityReport, ExecutionResult, Kernel, KernelOptions, LostName, Signal, Status,
    load_or_create_snapshot_key, sign_snapshot, snapshot::SNAPSHOT_KEY_FILE,
};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

fn options(key: &[u8], snapshot_path: Option<&Path>) -> KernelOptions {
    let mut options = KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime());
    options.snapshot_key = Some(key.to_vec());
    options.snapshot_path = snapshot_path.map(Path::to_path_buf);
    options
}

fn kernel_with(options: KernelOptions) -> Kernel {
    Kernel::new(
        options,
        Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null)),
    )
    .unwrap()
}

fn kernel(key: &[u8], snapshot_path: Option<&Path>) -> Kernel {
    kernel_with(options(key, snapshot_path))
}

fn run(kernel: &Kernel, code: &str) -> ExecutionResult {
    kernel
        .execute(code, None, None)
        .unwrap_or_else(|error| panic!("{code}: {error}"))
}

fn value(kernel: &Kernel, code: &str) -> String {
    let result = run(kernel, code);
    assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
    result.result.unwrap_or_default()
}

fn evalue(result: &ExecutionResult) -> String {
    result
        .error
        .as_ref()
        .map(|error| error.evalue.clone())
        .unwrap_or_default()
}

const KEY: [u8; 32] = [7; 32];

const PLAIN_DATA: &str = r#"
plain = {
    "none": None, "flag": True, "count": -7, "ratio": 0.25, "text": "héllo ☃",
    "items": [1, [2, (3, 4)], {"k": b"raw"}],
}
pair = (1, "two", (3.0, None))
tags = {"a", "b"}
frozen = frozenset({1, 2})
blob = b"\x00\xffbytes"
mutable_blob = bytearray(b"abc")
by_number = {1: "one", (2, 3): "tuple key", None: "none key"}
state["counter"] = 3
"#;

const UNSUPPORTED: &str = r#"
import math, socket, tempfile, enum
def function(x):
    return x + 1
lambda_value = lambda x: x * 2
generator = (n for n in range(3))
open_file = tempfile.TemporaryFile()
sock = socket.socket()
class Point:
    def __init__(self, x):
        self.x = x
instance = Point(1)
module = math
class Color(str, enum.Enum):
    RED = "red"
str_subclass = Color.RED
cyclic = [1]
cyclic.append(cyclic)
shared_a = [1, 2]
shared_b = shared_a
huge = 10 ** 5000
"#;

const UNSUPPORTED_NAMES: [&str; 18] = [
    "Color",
    "Point",
    "cyclic",
    "enum",
    "function",
    "generator",
    "huge",
    "instance",
    "lambda_value",
    "math",
    "module",
    "open_file",
    "shared_a",
    "shared_b",
    "sock",
    "socket",
    "str_subclass",
    "tempfile",
];

const RESTORED_NAMES: [&str; 8] = [
    "blob",
    "by_number",
    "frozen",
    "mutable_blob",
    "pair",
    "plain",
    "state",
    "tags",
];

#[test]
fn kernel_snapshot_type_matrix_saves_plain_data_and_reports_every_unsupported_name() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("kernel.snapshot");
    let source = kernel(&KEY, Some(&path));
    value(&source, PLAIN_DATA);
    value(&source, UNSUPPORTED);
    let saved = source.snapshot(None).unwrap();
    assert_eq!(saved.result.status, Status::Ok);
    let report = saved.snapshot.unwrap();
    assert_eq!(report.saved, RESTORED_NAMES);
    assert_eq!(report.skipped, UNSUPPORTED_NAMES);
    let reason = |name: &str| report.reason(name).unwrap().to_string();
    assert!(reason("function").contains("unsupported type builtins.function"));
    assert!(reason("lambda_value").contains("unsupported type builtins.function"));
    assert!(reason("generator").contains("unsupported type builtins.generator"));
    assert!(reason("open_file").contains("unsupported type"));
    assert!(reason("sock").contains("unsupported type socket.socket"));
    assert!(reason("instance").contains("unsupported type __main__.Point"));
    assert!(reason("module").contains("unsupported type builtins.module"));
    assert!(reason("str_subclass").contains("unsupported type __main__.Color"));
    assert!(reason("cyclic").contains("cyclic"));
    assert!(reason("shared_b").contains("aliases mutable data of shared_a"));
    assert!(reason("shared_a").contains("aliases mutable data of shared_b"));
    assert!(reason("huge").contains("not encodable"));
    // Line 1 is the host signature, line 2 the kernel's digest header, line 3 the body.
    let text = std::fs::read_to_string(&path).unwrap();
    let body: Value = serde_json::from_str(text.split('\n').nth(2).unwrap()).unwrap();
    let mut names: Vec<&String> = body["names"].as_object().unwrap().keys().collect();
    names.sort();
    assert_eq!(names, RESTORED_NAMES);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    // No unsigned or temporary file is left beside it.
    let leftovers: Vec<_> = std::fs::read_dir(directory.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(leftovers, ["kernel.snapshot"]);
}

#[test]
fn kernel_snapshot_forged_digests_truncation_and_non_data_values_are_refused() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("kernel.snapshot");
    let target = kernel(&KEY, None);
    value(&target, "marker = 1");
    for content in [
        r#"{"version":1,"names":{"injected":1},"skipped":[]}"#,
        "\u{80}\u{4}\u{95}cos\nsystem\n",
        "",
    ] {
        std::fs::write(&path, content).unwrap();
        let refused = target.restore(Some(&path)).unwrap();
        assert_eq!(refused.result.status, Status::Error);
        assert!(
            evalue(&refused.result).contains("integrity check failed"),
            "{refused:?}"
        );
    }
    // A truncated snapshot fails the signature.
    assert_eq!(target.snapshot(Some(&path)).unwrap().result.status, Status::Ok);
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, &text[..text.len() - 5]).unwrap();
    assert!(evalue(&target.restore(Some(&path)).unwrap().result).contains("HMAC signature mismatch"));
    // Even a holder of the signing key cannot smuggle non-data values past validation.
    let body = r#"{"names":{"injected":{"$object":["os","system"]}},"skipped":[],"reasons":{}}"#;
    let digest: String = Sha256::digest(body).iter().map(|byte| format!("{byte:02x}")).collect();
    let header = format!(
        r#"{{"format":"ultron-rlm-snapshot","version":2,"sha256":"{digest}","bytes":{}}}"#,
        body.len()
    );
    std::fs::write(&path, sign_snapshot(&KEY, format!("{header}\n{body}").as_bytes())).unwrap();
    assert!(evalue(&target.restore(Some(&path)).unwrap().result).contains("unknown tag"));
    assert_eq!(value(&target, "('injected' in globals(), marker)"), "(False, 1)");
}

#[test]
fn kernel_snapshot_forged_body_with_recomputed_sha256_is_refused_and_unsigned_files_too() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lane.snapshot");
    let source = kernel(&KEY, None);
    value(&source, "state['n'] = 1\nbalance = 100");
    assert_eq!(source.snapshot(Some(&path)).unwrap().result.status, Status::Ok);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.split('\n')
            .next()
            .unwrap()
            .starts_with(r#"{"format":"ultron-rlm-snapshot-signature""#)
    );
    let lines: Vec<&str> = text.split('\n').collect();
    let forged = lines[2].replace("100", "999999");
    assert_ne!(forged, lines[2]);
    let digest: String = Sha256::digest(&forged)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let header = format!(
        r#"{{"format":"ultron-rlm-snapshot","version":2,"sha256":"{digest}","bytes":{}}}"#,
        forged.len()
    );
    let original = text.clone();
    std::fs::write(&path, format!("{}\n{header}\n{forged}", lines[0])).unwrap();
    let target = kernel(&KEY, None);
    value(&target, "marker = 'kept'");
    let refused = target.restore(Some(&path)).unwrap();
    let error = refused.result.error.clone().unwrap();
    assert_eq!(error.ename, "SnapshotIntegrityError");
    assert!(error.evalue.contains("HMAC signature mismatch"));
    assert!(refused.restore.is_none());
    assert_eq!(
        value(&target, "(marker, 'balance' in globals(), state)"),
        "('kept', False, {})"
    );

    // Unsigned (pre-signing v2) files are refused, explicitly and at startup.
    std::fs::write(&path, original.split('\n').skip(1).collect::<Vec<_>>().join("\n")).unwrap();
    let message = evalue(&target.restore(Some(&path)).unwrap().result);
    assert!(
        message.contains("missing host signature (unsigned snapshots"),
        "{message}"
    );
    assert!(message.contains("are refused)"), "{message}");
    let configured = kernel(&KEY, Some(&path));
    let failure = configured.execute("1", None, None).unwrap_err();
    assert!(
        failure.message().starts_with("RLM kernel startup restore failed: "),
        "{failure}"
    );
    assert!(failure.message().contains("missing host signature"), "{failure}");
}

#[test]
fn kernel_snapshot_valid_restore_and_foreign_key_refusal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lane.snapshot");
    let key = load_or_create_snapshot_key(directory.path()).unwrap();
    let source = kernel(&key, None);
    value(&source, "state['n'] = 1\nbalance = 100");
    assert_eq!(source.snapshot(Some(&path)).unwrap().result.status, Status::Ok);
    let target = kernel(&key, None);
    let restored = target.restore(Some(&path)).unwrap();
    assert_eq!(restored.result.status, Status::Ok);
    assert_eq!(restored.restore.unwrap().restored, ["balance", "state"]);
    assert_eq!(value(&target, "(balance, state)"), "(100, {'n': 1})");
    let stranger_dir = tempfile::tempdir().unwrap();
    let stranger = kernel(&load_or_create_snapshot_key(stranger_dir.path()).unwrap(), None);
    assert!(evalue(&stranger.restore(Some(&path)).unwrap().result).contains("HMAC signature mismatch"));
}

#[test]
fn kernel_snapshot_profile_key_is_created_once_private_and_stable() {
    let directory = tempfile::tempdir().unwrap();
    let key = load_or_create_snapshot_key(directory.path()).unwrap();
    let path = directory.path().join(SNAPSHOT_KEY_FILE);
    assert_eq!(key.len(), 32);
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.len(), 65);
    assert!(
        text.ends_with('\n')
            && text[..64]
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(load_or_create_snapshot_key(directory.path()).unwrap(), key);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(load_or_create_snapshot_key(directory.path()).unwrap(), key);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let other = tempfile::tempdir().unwrap();
    assert_ne!(load_or_create_snapshot_key(other.path()).unwrap(), key);
    std::fs::write(&path, "not a key").unwrap();
    let error = load_or_create_snapshot_key(directory.path()).unwrap_err();
    assert!(error.contains("malformed"), "{error}");
}

#[test]
fn kernel_snapshot_key_never_reaches_the_kernel_or_the_file() {
    let directory = tempfile::tempdir().unwrap();
    let key = load_or_create_snapshot_key(directory.path()).unwrap();
    let hex: String = key.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = directory.path().join("lane.snapshot");
    let source = kernel(&key, None);
    value(&source, "state['n'] = 1\nbalance = 100");
    assert_eq!(source.snapshot(Some(&path)).unwrap().result.status, Status::Ok);
    let target = kernel(&key, None);
    assert_eq!(target.restore(Some(&path)).unwrap().result.status, Status::Ok);
    assert_eq!(target.snapshot(Some(&path)).unwrap().result.status, Status::Ok);
    let probe = [
        "import gc, os, sys".to_string(),
        format!("needle_hex = {:?} + {:?}", &hex[..16], &hex[16..]),
        "needle_raw = bytes.fromhex(needle_hex)".into(),
        "def found(text):".into(),
        "    return needle_hex in text or needle_hex.upper() in text".into(),
        "hits = []".into(),
        "if any(found(f'{k}={v}') for k, v in os.environ.items()): hits.append('environ')".into(),
        "for name in ('environ', 'cmdline'):".into(),
        "    try:".into(),
        "        raw = open(f'/proc/self/{name}', 'rb').read()".into(),
        "        if needle_hex.encode() in raw or needle_raw in raw: hits.append(name)".into(),
        "    except OSError:".into(),
        "        pass".into(),
        "if any(found(repr(v)) for k, v in list(globals().items()) if k not in ('needle_hex', 'needle_raw')): hits.append('namespace')".into(),
        "for obj in gc.get_objects():".into(),
        "    if obj is needle_hex or obj is needle_raw: continue".into(),
        "    if isinstance(obj, (bytes, bytearray)) and needle_raw in obj: hits.append('object'); break".into(),
        "    if isinstance(obj, str) and len(obj) < 1_000_000 and obj is not needle_hex and found(obj) and obj != needle_hex: hits.append('object'); break".into(),
        "hits".into(),
    ]
    .join("\n");
    assert_eq!(value(&target, &probe), "[]");
    let file = std::fs::read(&path).unwrap();
    assert!(!file.windows(hex.len()).any(|window| window == hex.as_bytes()));
    assert!(!file.windows(key.len()).any(|window| window == key.as_slice()));
}

#[test]
fn kernel_snapshot_path_must_be_configured_and_continuity_requires_one() {
    let unconfigured = kernel(&KEY, None);
    assert_eq!(
        unconfigured.snapshot(None).unwrap_err().message(),
        "RLM snapshot path is not configured"
    );
    assert_eq!(
        unconfigured.restore(None).unwrap_err().message(),
        "RLM snapshot path is not configured"
    );
    let mut options = options(&KEY, None);
    options.continuity = Some(ContinuityPolicy::default());
    let error = Kernel::new(options, Arc::new(kernel::EchoHandler)).err().unwrap();
    assert_eq!(error.message(), "RLM kernel continuity requires a snapshot path");
}

#[test]
fn kernel_snapshot_without_continuity_a_started_kernel_that_dies_is_not_restored() {
    // kernel.ts: once user code could have run, an older checkpoint is never silently resurrected.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("main.snapshot");
    let writer = kernel(&KEY, None);
    value(&writer, "saved = 'old'");
    assert_eq!(writer.snapshot(Some(&path)).unwrap().result.status, Status::Ok);
    let configured = kernel(&KEY, Some(&path));
    assert_eq!(value(&configured, "saved"), "'old'");
    let pid = configured.pid().unwrap();
    kill(pid);
    wait_until(|| !configured.is_running());
    assert_eq!(value(&configured, "'saved' in globals()"), "False");
    assert!(configured.last_recovery().is_none());
}

// ---- Continuity (Rust-only) ----

fn kill(pid: u32) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
}

/// A hang guard, never a timing expectation: every wait in these tests is for a condition.
const HANG_GUARD: Duration = Duration::from_secs(120);

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !ready() {
        assert!(Instant::now() < deadline, "condition not reached in {HANG_GUARD:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn continuous(path: &Path, min_interval: Duration) -> Kernel {
    let mut options = options(&KEY, Some(path));
    options.continuity = Some(ContinuityPolicy { min_interval });
    kernel_with(options)
}

/// Waits until the latest checkpoint saved or skipped `name`.
fn checkpointed(kernel: &Kernel, name: &str) {
    wait_until(|| {
        kernel.checkpoint().is_some_and(|checkpoint| {
            checkpoint
                .saved
                .iter()
                .chain(&checkpoint.skipped)
                .any(|saved| saved == name)
        })
    });
}

/// Waits until a new kernel process replaced `old` and finished its startup.
fn restarted(kernel: &Kernel, old: u32) {
    wait_until(|| kernel.is_running() && kernel.pid().is_some_and(|pid| pid != old));
}

fn lost(name: &str, reason: &str) -> LostName {
    LostName {
        name: name.into(),
        reason: reason.into(),
    }
}

#[test]
fn kernel_continuity_state_survives_sigkill_of_an_idle_kernel_and_unsupported_names_are_reported_lost() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("main.snapshot");
    let kernel = continuous(&path, Duration::ZERO);
    value(
        &kernel,
        "total = 41\nrows = [{'id': 1}, {'id': 2}]\nstate['phase'] = 'collect'",
    );
    value(&kernel, "helper = lambda x: x + 1\nimport fractions");
    checkpointed(&kernel, "helper");
    let old = kernel.pid().unwrap();
    kill(old);
    restarted(&kernel, old);

    let reply = kernel
        .execute_cell(
            "(total + 1, rows[1]['id'], state, 'helper' in globals(), 'fractions' in globals())",
            None,
            None,
        )
        .unwrap();
    assert_eq!(reply.result.status, Status::Ok);
    assert_eq!(
        reply.result.result.as_deref(),
        Some("(42, 2, {'phase': 'collect'}, False, False)")
    );
    let report = reply.recovery.clone().expect("recovery report");
    // The reason is the error the dead kernel's cells got, with its stderr tail.
    let reason = report.reason.clone();
    assert!(reason.starts_with("RLM kernel exited (SIGKILL); the system out-of-memory killer may have stopped it"));
    assert_eq!(
        report,
        ContinuityReport {
            reason: reason.clone(),
            snapshot_path: path.display().to_string(),
            restored: vec!["rows".into(), "state".into(), "total".into()],
            lost: vec![
                lost("fractions", "unsupported type builtins.module"),
                lost("helper", "unsupported type builtins.function"),
            ],
            interrupted_cell: false,
            unsaved_cells: 0,
            restore_error: None,
        }
    );
    assert!(
        reply.result.stderr.starts_with(&report.render()),
        "{}",
        reply.result.stderr
    );
    assert_eq!(
        report.render(),
        format!(
            "[RLM kernel restarted after: {reason}]\n\
         [Restored: rows, state, total]\n\
         [Lost: fractions (unsupported type builtins.module); helper (unsupported type builtins.function)]\n"
        )
    );
    assert_eq!(kernel.last_recovery(), Some(report));
    // Reported once: the next cell carries no notice.
    let next = kernel.execute_cell("total", None, None).unwrap();
    assert!(next.recovery.is_none());
    assert_eq!(next.result.stderr, "");
}

#[test]
fn kernel_continuity_sigkill_during_a_cell_keeps_the_state_before_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("main.snapshot");
    let kernel = Arc::new(continuous(&path, Duration::ZERO));
    value(&kernel, "before = 'kept'");
    checkpointed(&kernel, "before");
    let old = kernel.pid().unwrap();
    let started = directory.path().join("started");
    let running = {
        let kernel = kernel.clone();
        let code = format!(
            "during = 'gone'\nopen({:?}, 'w').close()\nimport time\ntime.sleep(3600)",
            started.display().to_string()
        );
        std::thread::spawn(move || kernel.execute(&code, None, None))
    };
    wait_until(|| started.exists());
    kill(old);
    let failure = running.join().unwrap().unwrap_err();
    assert!(
        failure.message().starts_with("RLM kernel exited (SIGKILL)"),
        "{failure}"
    );
    restarted(&kernel, old);
    let reply = kernel
        .execute_cell("(before, 'during' in globals())", None, None)
        .unwrap();
    assert_eq!(reply.result.result.as_deref(), Some("('kept', False)"));
    let report = reply.recovery.unwrap();
    assert!(report.interrupted_cell);
    // The declared `state` dict is always saved.
    assert_eq!(report.restored, ["before", "state"]);
    assert_eq!(report.lost, []);
    assert!(
        report
            .render()
            .contains("[The cell running at the failure did not finish; its changes to Python state are lost]\n")
    );
}

#[test]
fn kernel_continuity_a_tampered_checkpoint_is_refused_and_reported_and_the_kernel_keeps_serving() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("main.snapshot");
    let kernel = continuous(&path, Duration::ZERO);
    value(&kernel, "balance = 100\nf = lambda: 1");
    checkpointed(&kernel, "balance");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("100", "999")).unwrap();
    let old = kernel.pid().unwrap();
    kill(old);
    restarted(&kernel, old);
    let reply = kernel
        .execute_cell("('balance' in globals(), 1 + 1)", None, None)
        .unwrap();
    assert_eq!(reply.result.result.as_deref(), Some("(False, 2)"));
    let report = reply.recovery.unwrap();
    let refusal = "snapshot integrity check failed: HMAC signature mismatch (the snapshot was modified or signed with another profile's key)";
    assert_eq!(report.restore_error.as_deref(), Some(refusal));
    assert_eq!(report.restored, Vec::<String>::new());
    assert_eq!(
        report.lost,
        [
            lost("balance", &format!("snapshot not restored: {refusal}")),
            lost("f", "unsupported type builtins.function"),
            lost("state", &format!("snapshot not restored: {refusal}")),
        ]
    );
    assert!(report.render().contains(&format!(
        "[State not restored ({refusal}); the namespace started empty]\n"
    )));
    // The next checkpoint replaces the refused one, and later recoveries restore again.
    value(&kernel, "balance = 7");
    checkpointed(&kernel, "balance");
    let old = kernel.pid().unwrap();
    kill(old);
    restarted(&kernel, old);
    let reply = kernel.execute_cell("balance", None, None).unwrap();
    assert_eq!(reply.result.result.as_deref(), Some("7"));
    // `_` holds the previous cell's value.
    assert_eq!(reply.recovery.unwrap().restored, ["_", "balance", "state"]);
}

#[test]
fn kernel_continuity_debounce_reports_unsaved_cells() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("main.snapshot");
    // An interval no test step can outlast: "b" is never checkpointed before the crash, however slow the machine.
    let kernel = continuous(&path, Duration::from_secs(3600));
    value(&kernel, "a = 1");
    checkpointed(&kernel, "a");
    // Inside the interval: no checkpoint yet, so a crash now loses this cell, and the report says so.
    value(&kernel, "b = 2");
    assert!(!kernel.checkpoint().unwrap().saved.contains(&"b".to_string()));
    let old = kernel.pid().unwrap();
    kill(old);
    restarted(&kernel, old);
    let reply = kernel.execute_cell("(a, 'b' in globals())", None, None).unwrap();
    assert_eq!(reply.result.result.as_deref(), Some("(1, False)"));
    let report = reply.recovery.unwrap();
    assert_eq!(report.unsaved_cells, 1);
    assert!(
        report
            .render()
            .contains("[1 completed cell(s) after the last checkpoint are lost; restored values predate them]\n")
    );
}

#[test]
fn kernel_continuity_debounce_flushes_when_idle() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("main.snapshot");
    let kernel = continuous(&path, Duration::from_secs(1));
    value(&kernel, "a = 1");
    checkpointed(&kernel, "a");
    // Within the interval (or past it on a slow machine, when the cell checkpoints at once): once idle, the pending
    // cell is checkpointed without another call.
    value(&kernel, "b = 2");
    checkpointed(&kernel, "b");
}

#[test]
fn kernel_continuity_first_start_reports_what_it_restored_from_an_earlier_process() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("main.snapshot");
    {
        let first = continuous(&path, Duration::ZERO);
        value(&first, "kept = [1, 2]\nsock = __import__('socket').socket()");
        checkpointed(&first, "kept");
    }
    let second = continuous(&path, Duration::ZERO);
    let reply = second.execute_cell("kept", None, None).unwrap();
    assert_eq!(reply.result.result.as_deref(), Some("[1, 2]"));
    let report = reply.recovery.unwrap();
    assert_eq!(report.reason, "kernel started from its checkpoint");
    assert_eq!(report.restored, ["kept", "state"]);
    assert_eq!(report.lost, [lost("sock", "unsupported type socket.socket")]);
}

#[test]
fn kernel_continuity_shutdown_does_not_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("main.snapshot");
    let kernel = continuous(&path, Duration::ZERO);
    value(&kernel, "x = 1");
    checkpointed(&kernel, "x");
    let pid = kernel.pid().unwrap();
    kernel.shutdown();
    // Settle, then check no restart happened: a late restart could only let this pass wrongly, never fail.
    std::thread::sleep(Duration::from_millis(300));
    assert!(kernel.pid().is_none());
    assert!(!kernel.is_running());
    assert_eq!(
        kernel.execute("x", None, None).unwrap_err().message(),
        "RLM kernel is shut down"
    );
    wait_until(|| {
        !std::path::Path::new(&format!("/proc/{pid}")).exists() || {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z')
        }
    });
}

#[test]
fn kernel_snapshot_changed_after_the_kernel_wrote_it_is_not_signed_and_the_previous_file_stays() {
    // Model code can rewrite the unsigned file between the kernel's write and the host's signature; the host signs
    // only bytes whose sha256 the kernel reported (kernel.ts signedSnapshot).
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lane.snapshot");
    let kernel = kernel(&KEY, None);
    value(&kernel, "balance = 100");
    assert_eq!(kernel.snapshot(Some(&path)).unwrap().result.status, Status::Ok);
    let previous = std::fs::read(&path).unwrap();
    value(
        &kernel,
        "import sys\nruntime = sys.modules['__main__']\noriginal = runtime.save_snapshot\n\
         def tampering(path):\n    report = original(path)\n    open(path, 'ab').write(b' ')\n    return report\n\
         runtime.save_snapshot = tampering\nbalance = 1_000_000",
    );
    let refused = kernel.snapshot(Some(&path)).unwrap();
    let error = refused.result.error.unwrap();
    assert_eq!(error.ename, "SnapshotIntegrityError");
    assert_eq!(
        error.evalue,
        "snapshot changed after the kernel wrote it; refusing to sign it"
    );
    assert_eq!(std::fs::read(&path).unwrap(), previous);
    let names: Vec<_> = std::fs::read_dir(directory.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(names, ["lane.snapshot"]);
}
