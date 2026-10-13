//! A08: unsupported Python state reports non-restorable, never silently correct.
//! Ported from packages/coding-agent/test/ultron-a08-snapshot-restorability.test.ts (all six cases) with its literal
//! expectations: the real runtime.py under `crates/kernel`, snapshots signed by the host with a 32-byte profile key
//! (`kernel::sign_snapshot`), saved and restored explicitly and at startup.

mod common;

use common::{null_handler, runtime};
use kernel::{ExecutionResult, Kernel, KernelOptions, Status, sign_snapshot};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The host-held signing key; every kernel here shares it, as kernels of one profile do.
const KEY: [u8; 32] = [7; 32];

fn kernel(snapshot_path: Option<&Path>) -> Kernel {
    let mut options = KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime());
    options.snapshot_key = Some(KEY.to_vec());
    options.snapshot_path = snapshot_path.map(Path::to_path_buf);
    Kernel::new(options, null_handler()).unwrap()
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
    result.error.as_ref().map(|e| e.evalue.clone()).unwrap_or_default()
}

/// Restorable plain data, with exact types that JSON alone would silently change.
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

/// Every entry is state the snapshot cannot restore faithfully.
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

/// Compares restored values with freshly built expected ones, including every nested type (the TS `CHECK_EQUAL`).
fn check_equal() -> String {
    let expected: String = PLAIN_DATA
        .lines()
        .filter(|line| *line != "state[\"counter\"] = 3")
        .map(|line| {
            let starts_name = line.split_once(" =").is_some_and(|(name, _)| {
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            });
            if starts_name {
                format!("expected_{line}\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    let names: Vec<&str> = RESTORED_NAMES.iter().copied().filter(|n| *n != "state").collect();
    format!(
        "{expected}
def same(left, right):
    if type(left) is not type(right):
        return False
    if isinstance(left, (list, tuple)):
        return len(left) == len(right) and all(same(a, b) for a, b in zip(left, right))
    if isinstance(left, dict):
        return left.keys() == right.keys() and all(same(left[k], right[k]) for k in left)
    return left == right
[name for name in {} if not same(globals()[name], globals()[\"expected_\" + name])] + ([] if state == {{\"counter\": 3}} else [\"state\"])
",
        serde_json::to_string(&names).unwrap()
    )
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn snapshot_file() -> Fixture {
    let dir = tempfile::Builder::new().prefix("ultron-a08-").tempdir().unwrap();
    let path = dir.path().join("kernel.snapshot");
    Fixture { _dir: dir, path }
}

fn save_matrix(path: &Path) -> kernel::KernelReply {
    let source = kernel(Some(path));
    value(&source, PLAIN_DATA);
    value(&source, UNSUPPORTED);
    let reply = source.snapshot(Some(path)).unwrap();
    source.shutdown();
    reply
}

#[test]
fn acceptance_a08_snapshot_type_matrix_reports_every_unsupported_name_with_a_reason() {
    let file = snapshot_file();
    let saved = save_matrix(&file.path);
    assert_eq!(saved.result.status, Status::Ok);
    let report = saved.snapshot.unwrap();
    assert_eq!(report.saved, RESTORED_NAMES);
    assert_eq!(report.skipped, UNSUPPORTED_NAMES);
    let reason = |name: &str| report.reason(name).unwrap_or_default().to_string();
    for (name, want) in [
        ("function", "unsupported type builtins.function"),
        ("lambda_value", "unsupported type builtins.function"),
        ("generator", "unsupported type builtins.generator"),
        ("open_file", "unsupported type"),
        ("sock", "unsupported type socket.socket"),
        ("instance", "unsupported type __main__.Point"),
        ("module", "unsupported type builtins.module"),
        ("str_subclass", "unsupported type __main__.Color"),
        ("cyclic", "cyclic"),
        ("shared_b", "aliases mutable data of shared_a"),
        ("shared_a", "aliases mutable data of shared_b"),
        ("huge", "not encodable"),
    ] {
        assert!(reason(name).contains(want), "{name}: {}", reason(name));
    }
    // Nothing was written for skipped names. Line 1 is the host signature, line 2 the digest header, line 3 the body.
    let text = std::fs::read_to_string(&file.path).unwrap();
    let body: Value = serde_json::from_str(text.split('\n').nth(2).unwrap()).unwrap();
    let mut names: Vec<&String> = body["names"].as_object().unwrap().keys().collect();
    names.sort();
    assert_eq!(names, RESTORED_NAMES);
}

#[test]
fn acceptance_a08_restore_brings_back_equal_typed_values_and_reports_non_restorable_names_absent() {
    let file = snapshot_file();
    assert_eq!(save_matrix(&file.path).result.status, Status::Ok);
    let target = kernel(None);
    let restored = target.restore(Some(&file.path)).unwrap();
    assert_eq!(restored.result.status, Status::Ok);
    let report = restored.restore.unwrap();
    assert_eq!(report.restored, RESTORED_NAMES);
    assert!(!report.missing);
    assert_eq!(report.skipped, UNSUPPORTED_NAMES);
    assert!(report.reason("generator").unwrap().contains("generator"));
    assert_eq!(value(&target, &check_equal()), "[]");
    // Non-restorable names are absent, not replaced by placeholders or stale copies.
    let absent = format!(
        "[name for name in {} if name in globals()]",
        serde_json::to_string(&UNSUPPORTED_NAMES).unwrap()
    );
    assert_eq!(value(&target, &absent), "[]");
    // Restored values are live data: later cells compute with them.
    assert_eq!(
        value(
            &target,
            "mutable_blob.extend(b'd'); (bytes(mutable_blob), pair[2][0] + 1)"
        ),
        "(b'abcd', 4.0)"
    );
}

#[test]
fn acceptance_a08_startup_restore_from_a_configured_snapshot_reports_the_same_names() {
    let file = snapshot_file();
    assert_eq!(save_matrix(&file.path).result.status, Status::Ok);
    let restarted = kernel(Some(&file.path));
    assert_eq!(value(&restarted, &check_equal()), "[]");
    assert_eq!(
        value(&restarted, "'sock' in globals() or 'function' in globals()"),
        "False"
    );
}

#[test]
fn acceptance_a08_tampered_snapshot_body_is_rejected_and_the_namespace_is_untouched() {
    let file = snapshot_file();
    assert_eq!(save_matrix(&file.path).result.status, Status::Ok);
    let original = std::fs::read_to_string(&file.path).unwrap();
    let tampered = original.replacen("\"$dict\"", "\"$dicT\"", 1).replacen("-7", "-8", 1);
    assert_ne!(tampered, original);
    std::fs::write(&file.path, tampered).unwrap();
    let target = kernel(None);
    value(&target, "marker = 'kept'");
    let restored = target.restore(Some(&file.path)).unwrap();
    assert_eq!(restored.result.status, Status::Error);
    assert!(
        evalue(&restored.result).contains("integrity check failed: HMAC signature mismatch"),
        "{restored:?}"
    );
    assert!(restored.restore.is_none());
    assert_eq!(value(&target, "(marker, 'plain' in globals())"), "('kept', False)");
}

#[test]
fn acceptance_a08_unsigned_truncated_and_forged_non_data_snapshots_are_rejected() {
    let file = snapshot_file();
    let path = &file.path;
    let target = kernel(None);
    value(&target, "marker = 1");
    // Legacy/unsigned JSON and arbitrary pickle-like bytes are not loaded.
    for content in [
        r#"{"version":1,"names":{"injected":1},"skipped":[]}"#,
        "\u{80}\u{4}\u{95}cos\nsystem\n",
        "",
    ] {
        std::fs::write(path, content).unwrap();
        let refused = target.restore(Some(path)).unwrap();
        assert_eq!(refused.result.status, Status::Error);
        assert!(
            evalue(&refused.result).contains("integrity check failed"),
            "{refused:?}"
        );
    }
    // A truncated snapshot fails the signature.
    assert_eq!(target.snapshot(Some(path)).unwrap().result.status, Status::Ok);
    let text = std::fs::read_to_string(path).unwrap();
    std::fs::write(path, &text[..text.len() - 5]).unwrap();
    let truncated = target.restore(Some(path)).unwrap();
    assert_eq!(truncated.result.status, Status::Error);
    assert!(evalue(&truncated.result).contains("HMAC signature mismatch"));
    // Even a holder of the signing key cannot smuggle non-data values past validation.
    let body = r#"{"names":{"injected":{"$object":["os","system"]}},"skipped":[],"reasons":{}}"#;
    let header = format!(
        r#"{{"format":"ultron-rlm-snapshot","version":2,"sha256":"{}","bytes":{}}}"#,
        kernel::snapshot::sha256_hex(body.as_bytes()),
        body.len()
    );
    std::fs::write(path, sign_snapshot(&KEY, format!("{header}\n{body}").as_bytes())).unwrap();
    let smuggled = target.restore(Some(path)).unwrap();
    assert_eq!(smuggled.result.status, Status::Error);
    assert!(evalue(&smuggled.result).contains("unknown tag"), "{smuggled:?}");
    assert_eq!(value(&target, "('injected' in globals(), marker)"), "(False, 1)");
}

#[test]
fn acceptance_a08_a_kernel_configured_with_a_tampered_snapshot_fails_startup() {
    let file = snapshot_file();
    assert_eq!(save_matrix(&file.path).result.status, Status::Ok);
    let text = std::fs::read_to_string(&file.path).unwrap();
    std::fs::write(&file.path, text.replacen("\"counter\"", "\"countex\"", 1)).unwrap();
    let restarted = kernel(Some(&file.path));
    let failure = restarted.execute("state", None, None).unwrap_err();
    assert!(
        failure
            .message()
            .contains("startup restore failed: snapshot integrity check failed"),
        "{failure}"
    );
    // A later explicit restore attempt reports the same failure; nothing was loaded.
    let again = restarted.restore(Some(&file.path)).unwrap();
    assert_eq!(again.result.status, Status::Error);
    assert!(evalue(&again.result).contains("integrity"));
    assert_eq!(value(&restarted, "('plain' in globals(), state)"), "(False, {})");
}
