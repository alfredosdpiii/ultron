//! A26 for the Rust build: the acceptance instrument lock and the regression-mutation runner.
//!
//! - [`compute_lock`] / [`check_lock`] / [`describe_lock_check`] are `scripts/acceptance-lock.mjs`: sha256 of the
//!   manifest, the instrument files and every evidence test file, written as the same tab-indented JSON. With the
//!   [`Profile::node`] paths the output is byte for byte what the Node script writes (`tests/gate.rs` runs both).
//! - [`apply_edits`], the in-flight record and the report are `scripts/acceptance-mutation.mjs`: each mutation's
//!   find/replace edits are applied, the named evidence tests run (`cargo test` instead of vitest), and the original
//!   bytes are restored whatever the run did; a mutation is killed only when evidence tests fail, a build failure or
//!   a runner crash is an `error`, never a kill.
//!
//! The Rust profile ([`Profile::rust`]) locks `crates/acceptance` (the Rust evidence map `COVERAGE.md`, every file
//! under `crates/acceptance/tests`, the Rust mutation list) plus the shared row contract `acceptance/manifest.json`.

use base64::Engine;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Which files a lock covers and where the gate writes.
#[derive(Clone, Debug)]
pub struct Profile {
    /// Hashed as the manifest; when it is JSON with `rows`, their `evidence[].file` are evidence files.
    pub manifest: String,
    pub mutations: String,
    pub lock: String,
    /// Files that define how rows are judged, in lock order.
    pub instrument: Vec<String>,
    /// Directories whose files (recursively) are all evidence.
    pub evidence_dirs: Vec<String>,
    /// The mutation runner's report and in-flight record.
    pub result: String,
    pub inflight: String,
}

impl Profile {
    /// `scripts/acceptance-lock.mjs`'s constants.
    pub fn node() -> Profile {
        Profile {
            manifest: "acceptance/manifest.json".into(),
            mutations: "acceptance/mutations.json".into(),
            lock: "acceptance/instrument.lock".into(),
            instrument: [
                "acceptance/mutations.json",
                "scripts/acceptance-lock.mjs",
                "scripts/acceptance-report.mjs",
                "scripts/acceptance-mutation.mjs",
            ]
            .map(String::from)
            .to_vec(),
            evidence_dirs: Vec::new(),
            result: "acceptance/mutation.json".into(),
            inflight: "acceptance/.mutation-inflight.json".into(),
        }
    }

    /// The Rust build's gate.
    pub fn rust() -> Profile {
        Profile {
            manifest: "crates/acceptance/COVERAGE.md".into(),
            mutations: "crates/acceptance/mutations.json".into(),
            lock: "crates/acceptance/instrument.lock".into(),
            instrument: [
                "crates/acceptance/mutations.json",
                "acceptance/manifest.json",
                "crates/acceptance-gate/src/lib.rs",
                "crates/acceptance-gate/src/main.rs",
            ]
            .map(String::from)
            .to_vec(),
            evidence_dirs: vec!["crates/acceptance/tests".into()],
            result: "crates/acceptance/mutation.json".into(),
            inflight: "crates/acceptance/.mutation-inflight.json".into(),
        }
    }
}

pub fn sha256_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let digest = Sha256::digest(bytes);
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// JavaScript's default `sort()`: by UTF-16 code units.
fn js_sort(items: &mut [String]) {
    items.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
}

/// `JSON.stringify(value, null, "\t")`.
pub fn stringify_tabs(value: &Value) -> String {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    serde::Serialize::serialize(value, &mut serializer).expect("JSON values serialize");
    String::from_utf8(out).expect("serde_json writes UTF-8")
}

fn walk(root: &Path, dir: &str, files: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = format!("{dir}/{}", entry.file_name().to_string_lossy());
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk(root, &name, files),
            Ok(_) => {
                files.insert(name);
            }
            Err(_) => {}
        }
    }
}

/// Every evidence file: the manifest rows' `evidence[].file`, every file under the profile's evidence directories
/// and every mutation's `tests`, sorted and unique (`evidenceFiles`).
pub fn evidence_files(root: &Path, profile: &Profile, manifest: Option<&Value>, mutations: &Value) -> Vec<String> {
    let mut files = BTreeSet::new();
    let list = |value: Option<&Value>| value.and_then(Value::as_array).cloned().unwrap_or_default();
    for row in list(manifest.and_then(|m| m.get("rows"))) {
        for evidence in list(row.get("evidence")) {
            if let Some(file) = evidence.get("file").and_then(Value::as_str) {
                files.insert(file.to_string());
            }
        }
    }
    for dir in &profile.evidence_dirs {
        walk(root, dir, &mut files);
    }
    for mutation in list(mutations.get("mutations")) {
        for file in list(mutation.get("tests")) {
            if let Some(file) = file.as_str() {
                files.insert(file.to_string());
            }
        }
    }
    let mut files: Vec<String> = files.into_iter().collect();
    js_sort(&mut files);
    files
}

fn hash_value(path: &Path) -> Value {
    sha256_file(path).map_or(Value::Null, Value::String)
}

/// `computeLock`.
pub fn compute_lock(root: &Path, profile: &Profile) -> Result<Value, String> {
    let manifest_path = root.join(&profile.manifest);
    let manifest = if profile.manifest.ends_with(".json") {
        Some(read_json(&manifest_path)?)
    } else if manifest_path.exists() {
        None
    } else {
        return Err(format!("{}: not found", manifest_path.display()));
    };
    let mutations_path = root.join(&profile.mutations);
    let mutations = if mutations_path.exists() {
        read_json(&mutations_path)?
    } else {
        json!({"mutations": []})
    };
    let mut instrument = Map::new();
    for path in &profile.instrument {
        instrument.insert(path.clone(), hash_value(&root.join(path)));
    }
    let mut evidence = Map::new();
    for path in evidence_files(root, profile, manifest.as_ref(), &mutations) {
        let hash = hash_value(&root.join(&path));
        evidence.insert(path, hash);
    }
    Ok(json!({
        "version": 1,
        "algorithm": "sha256",
        "manifest": hash_value(&manifest_path),
        "instrument": instrument,
        "evidence": evidence,
    }))
}

/// `checkLock`'s result.
#[derive(Clone, Debug, PartialEq)]
pub struct LockCheck {
    pub ok: bool,
    pub lock_missing: bool,
    pub manifest_changed: bool,
    pub instrument_changed: Vec<String>,
    pub evidence_changed: Vec<String>,
}

fn differ(recorded: Option<&Value>, current: &Value) -> Vec<String> {
    let empty = Map::new();
    let a = recorded.and_then(Value::as_object).unwrap_or(&empty);
    let b = current.as_object().unwrap_or(&empty);
    let mut keys: Vec<String> = Vec::new();
    for key in a.keys().chain(b.keys()) {
        if !keys.contains(key) {
            keys.push(key.clone());
        }
    }
    let mut changed: Vec<String> = keys.into_iter().filter(|k| a.get(k) != b.get(k)).collect();
    js_sort(&mut changed);
    changed
}

/// `checkLock`: what changed since the recorded lock.
pub fn check_lock(root: &Path, profile: &Profile) -> Result<LockCheck, String> {
    let current = compute_lock(root, profile)?;
    let missing = LockCheck {
        ok: false,
        lock_missing: true,
        manifest_changed: true,
        instrument_changed: Vec::new(),
        evidence_changed: Vec::new(),
    };
    let Ok(recorded) = read_json(&root.join(&profile.lock)) else {
        return Ok(missing);
    };
    let manifest_changed = recorded.get("manifest") != current.get("manifest");
    let instrument_changed = differ(recorded.get("instrument"), &current["instrument"]);
    let evidence_changed = differ(recorded.get("evidence"), &current["evidence"]);
    Ok(LockCheck {
        ok: !manifest_changed && instrument_changed.is_empty() && evidence_changed.is_empty(),
        lock_missing: false,
        manifest_changed,
        instrument_changed,
        evidence_changed,
    })
}

/// `describeLockCheck`.
pub fn describe_lock_check(check: &LockCheck) -> String {
    if check.ok {
        return "instrument lock matches".into();
    }
    if check.lock_missing {
        return "instrument lock is missing or unreadable".into();
    }
    let mut parts = Vec::new();
    if check.manifest_changed {
        parts.push("manifest changed".to_string());
    }
    if !check.instrument_changed.is_empty() {
        parts.push(format!("instrument changed: {}", check.instrument_changed.join(", ")));
    }
    if !check.evidence_changed.is_empty() {
        parts.push(format!("evidence changed: {}", check.evidence_changed.join(", ")));
    }
    parts.join("; ")
}

/// `--write`: (exit code, stdout, stderr).
pub fn write_lock(root: &Path, profile: &Profile) -> Result<(i32, String, String), String> {
    let lock = compute_lock(root, profile)?;
    let missing: Vec<&String> = ["instrument", "evidence"]
        .iter()
        .flat_map(|part| lock[*part].as_object().into_iter().flatten())
        .filter(|(_, hash)| hash.is_null())
        .map(|(path, _)| path)
        .collect();
    if !missing.is_empty() {
        let names: Vec<&str> = missing.iter().map(|s| s.as_str()).collect();
        return Ok((
            1,
            String::new(),
            format!("Cannot lock missing files: {}\n", names.join(", ")),
        ));
    }
    std::fs::write(root.join(&profile.lock), format!("{}\n", stringify_tabs(&lock))).map_err(|e| e.to_string())?;
    let count = lock["evidence"].as_object().map_or(0, Map::len);
    Ok((
        0,
        format!("Wrote {} ({count} evidence files)\n", profile.lock),
        String::new(),
    ))
}

// ------------------------------------------------------------------------------------------------ mutation runner

/// `applyEdits`: each `find` must occur exactly once in the text as edited so far.
pub fn apply_edits(source: &str, edits: &[(String, String)]) -> Result<String, String> {
    let mut text = source.to_string();
    for (find, replace) in edits {
        let Some(first) = text.find(find.as_str()) else {
            return Err(format!("find string not present: {find}"));
        };
        if text[first + find.len()..].contains(find.as_str()) {
            return Err(format!("find string is not unique: {find}"));
        }
        text = format!("{}{replace}{}", &text[..first], &text[first + find.len()..]);
    }
    Ok(text)
}

/// Restore a file a previous run left mutated (it was interrupted): the in-flight record holds its original bytes.
/// Returns the restored path.
pub fn restore_inflight(root: &Path, profile: &Profile) -> Result<Option<String>, String> {
    let path = root.join(&profile.inflight);
    if !path.exists() {
        return Ok(None);
    }
    let record = read_json(&path)?;
    let file = record["file"].as_str().ok_or("in-flight record without a file")?;
    let original = base64::engine::general_purpose::STANDARD
        .decode(record["original"].as_str().unwrap_or_default())
        .map_err(|e| e.to_string())?;
    std::fs::write(root.join(file), original).map_err(|e| e.to_string())?;
    std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    Ok(Some(file.to_string()))
}

/// Puts the original bytes back and drops the in-flight record when dropped, so a runner error or a panic between
/// mutating and restoring still restores the file.
struct Restore<'a> {
    path: PathBuf,
    original: &'a [u8],
    inflight: PathBuf,
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.path, self.original);
        let _ = std::fs::remove_file(&self.inflight);
    }
}

/// One run of the evidence tests.
#[derive(Debug, Default)]
pub struct Run {
    pub infra_error: Option<String>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// The cargo package of a test file `crates/<dir>/tests/<stem>.rs` (its Cargo.toml's `[package] name`) and the
/// test target `<stem>`.
pub fn test_target(root: &Path, file: &str) -> Result<(String, String), String> {
    let path = Path::new(file);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|_| path.extension().is_some_and(|e| e == "rs"))
        .ok_or_else(|| format!("not a Rust test file: {file}"))?;
    let tests = path.parent().filter(|p| p.ends_with("tests"));
    let crate_dir = tests
        .and_then(Path::parent)
        .ok_or_else(|| format!("not under a crate's tests/: {file}"))?;
    let manifest = std::fs::read_to_string(root.join(crate_dir).join("Cargo.toml"))
        .map_err(|e| format!("{}: {e}", crate_dir.display()))?;
    let name = manifest
        .lines()
        .skip_while(|l| l.trim() != "[package]")
        .find_map(|l| {
            let (key, value) = l.split_once('=')?;
            (key.trim() == "name").then(|| value.trim().trim_matches('"').to_string())
        })
        .ok_or_else(|| format!("no package name in {}/Cargo.toml", crate_dir.display()))?;
    Ok((name, stem.to_string()))
}

/// `<runner> test --locked --no-fail-fast -p <pkg>... --test <stem>...` in `root`, killed (its whole process group)
/// after `timeout`.
pub fn run_tests(root: &Path, runner: &[String], files: &[String], timeout: Duration) -> Run {
    let mut args: Vec<String> = vec!["test".into(), "--locked".into(), "--no-fail-fast".into()];
    let mut packages = Vec::new();
    let mut targets = Vec::new();
    for file in files {
        match test_target(root, file) {
            Ok((package, target)) => {
                if !packages.contains(&package) {
                    packages.push(package);
                }
                targets.push(target);
            }
            Err(e) => {
                return Run {
                    infra_error: Some(e),
                    ..Run::default()
                };
            }
        }
    }
    for package in packages {
        args.extend(["-p".into(), package]);
    }
    for target in targets {
        args.extend(["--test".into(), target]);
    }
    let Some((program, prefix)) = runner.split_first() else {
        return Run {
            infra_error: Some("no test runner".into()),
            ..Run::default()
        };
    };
    let mut command = Command::new(program);
    command
        .args(prefix)
        .args(&args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CI", "1");
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return Run {
                infra_error: Some(format!("test runner failed: {e}")),
                ..Run::default()
            };
        }
    };
    let pid = child.id();
    let pipe = |mut reader: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = reader.read_to_string(&mut text);
            text
        })
    };
    let stdout = pipe(Box::new(child.stdout.take().expect("piped")));
    let stderr = pipe(Box::new(child.stderr.take().expect("piped")));
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let status = child.wait();
        let _ = tx.send(());
        status
    });
    let timed_out = rx.recv_timeout(timeout).is_err();
    if timed_out {
        let _ = Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).status();
    }
    let status = waiter.join().ok().and_then(Result::ok);
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if timed_out {
        return Run {
            infra_error: Some(format!("test runner timed out after {} ms", timeout.as_millis())),
            stdout,
            stderr,
            ..Run::default()
        };
    }
    let Some(status) = status else {
        return Run {
            infra_error: Some("test runner could not be waited for".into()),
            stdout,
            stderr,
            ..Run::default()
        };
    };
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return Run {
            infra_error: Some(format!("test runner killed by signal {signal}")),
            stdout,
            stderr,
            ..Run::default()
        };
    }
    Run {
        infra_error: None,
        exit_code: status.code(),
        stdout,
        stderr,
    }
}

/// `classifyRun` over libtest output: failed tests kill; a non-zero exit without a failed test (a build error, a
/// crash) is an infrastructure `error`; a green run survived.
pub fn classify_run(run: &Run) -> Value {
    if let Some(error) = &run.infra_error {
        return json!({"outcome": "error", "detail": error});
    }
    let failed: Vec<&str> = run
        .stdout
        .lines()
        .filter_map(|line| line.strip_prefix("test ")?.strip_suffix(" ... FAILED"))
        .collect();
    if !failed.is_empty() {
        let first: Vec<&str> = failed.into_iter().take(20).collect();
        return json!({"outcome": "killed", "failing_tests": first});
    }
    if run.exit_code != Some(0) {
        let first = run
            .stderr
            .lines()
            .find(|l| l.starts_with("error"))
            .or_else(|| run.stderr.lines().rfind(|l| !l.trim().is_empty()))
            .unwrap_or_default();
        return json!({
            "outcome": "error",
            "detail": format!("suite failed outside its tests (exit {}): {first}", run.exit_code.map_or("none".into(), |c| c.to_string())),
        });
    }
    json!({"outcome": "survived"})
}

/// `Date.prototype.toISOString` of `time`.
pub fn iso_time(time: SystemTime) -> String {
    let ms = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis()) as i64;
    let (days, rest) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

/// Mutation-runner options.
pub struct MutationOptions {
    pub only: Option<Vec<String>>,
    pub timeout: Duration,
    /// The cargo command (`["cargo"]`, or the machine-lock wrapper).
    pub runner: Vec<String>,
}

fn edits_of(mutation: &Value) -> Vec<(String, String)> {
    mutation["edits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            (
                e["find"].as_str().unwrap_or_default().to_string(),
                e["replace"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// `acceptance-mutation.mjs`'s main: baseline, each mutation applied, run and restored, the report written. Returns
/// (exit code, log lines).
pub fn run_mutations(root: &Path, profile: &Profile, options: &MutationOptions) -> Result<(i32, Vec<String>), String> {
    let mut log = Vec::new();
    if let Some(file) = restore_inflight(root, profile)? {
        log.push(format!("Restored {file} from an interrupted mutation run"));
    }
    let config = read_json(&root.join(&profile.mutations))?;
    let mutations: Vec<Value> = config["mutations"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|m| {
            options
                .only
                .as_ref()
                .is_none_or(|only| only.iter().any(|id| m["id"].as_str() == Some(id)))
        })
        .collect();

    // Baseline: only tests that pass on the unmutated source can kill a mutation.
    let mut baseline: Vec<(String, bool)> = Vec::new();
    for mutation in &mutations {
        for file in strings(&mutation["tests"]) {
            if baseline.iter().any(|(f, _)| *f == file) {
                continue;
            }
            let verdict = classify_run(&run_tests(
                root,
                &options.runner,
                std::slice::from_ref(&file),
                options.timeout,
            ));
            let green = verdict["outcome"] == "survived";
            log.push(format!(
                "baseline {file}: {}",
                if green {
                    "green".to_string()
                } else {
                    format!("not green ({})", verdict["outcome"].as_str().unwrap_or_default())
                }
            ));
            baseline.push((file, green));
        }
    }

    let mut results = Vec::new();
    for mutation in &mutations {
        let mut result = json!({
            "id": mutation["id"],
            "row": mutation["row"],
            "file": mutation["file"],
            "guarantee": mutation["guarantee"],
        });
        let tests = strings(&mutation["tests"]);
        let not_green: Vec<&str> = tests
            .iter()
            .filter(|f| !baseline.iter().any(|(b, green)| b == *f && *green))
            .map(String::as_str)
            .collect();
        if !not_green.is_empty() {
            result["outcome"] = "error".into();
            result["detail"] = format!("baseline not green: {}", not_green.join(", ")).into();
            results.push(result);
            continue;
        }
        let file = mutation["file"].as_str().ok_or("mutation without a file")?;
        let path = root.join(file);
        let original = std::fs::read(&path).map_err(|e| format!("{file}: {e}"))?;
        let original_hash = sha256_file(&path);
        let mutated = match apply_edits(&String::from_utf8_lossy(&original), &edits_of(mutation)) {
            Ok(text) => text,
            Err(error) => {
                result["outcome"] = "invalid".into();
                result["detail"] = error.into();
                results.push(result);
                continue;
            }
        };
        let inflight = root.join(&profile.inflight);
        let record = json!({"file": file, "original": base64::engine::general_purpose::STANDARD.encode(&original)});
        std::fs::write(&inflight, record.to_string()).map_err(|e| e.to_string())?;
        let verdict = {
            let _restore = Restore {
                path: path.clone(),
                original: &original,
                inflight,
            };
            std::fs::write(&path, mutated).map_err(|e| e.to_string())?;
            classify_run(&run_tests(root, &options.runner, &tests, options.timeout))
        };
        if sha256_file(&path) != original_hash {
            return Err(format!("Failed to restore {file}"));
        }
        result["tests"] = json!(tests);
        if let Value::Object(fields) = &verdict {
            for (k, v) in fields {
                result[k] = v.clone();
            }
        }
        log.push(format!(
            "{} ({}): {}",
            mutation["id"].as_str().unwrap_or_default(),
            mutation["row"].as_str().unwrap_or_default(),
            verdict["outcome"].as_str().unwrap_or_default()
        ));
        results.push(result);
    }

    let mut summary = Map::new();
    for key in ["total", "killed", "survived", "error", "invalid"] {
        summary.insert(key.into(), 0.into());
    }
    summary["total"] = results.len().into();
    for result in &results {
        let outcome = result["outcome"].as_str().unwrap_or("error");
        summary[outcome] = (summary[outcome].as_u64().unwrap_or(0) + 1).into();
    }
    let findings: Vec<Value> = results
        .iter()
        .filter(|r| r["outcome"] != "killed")
        .map(|r| r["id"].clone())
        .collect();
    let report = json!({
        "generated_at": iso_time(SystemTime::now()),
        "mutations_sha256": hash_value(&root.join(&profile.mutations)),
        "summary": summary,
        "findings": findings,
        "results": results,
    });
    std::fs::write(root.join(&profile.result), format!("{}\n", stringify_tabs(&report))).map_err(|e| e.to_string())?;
    let n = |k: &str| summary[k].as_u64().unwrap_or(0);
    log.push(format!(
        "Mutation slice: {}/{} killed, {} survived, {} error, {} invalid. Wrote {}",
        n("killed"),
        n("total"),
        n("survived"),
        n("error"),
        n("invalid"),
        profile.result
    ));
    Ok((if n("killed") == n("total") { 0 } else { 1 }, log))
}
