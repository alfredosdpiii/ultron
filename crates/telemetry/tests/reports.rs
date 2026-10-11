//! Replays fixtures/reports.json, recorded from the TypeScript bug-report, crash-log and provider-attribution modules
//! by fixtures/generate-reports.mjs.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::LazyLock;
use telemetry::attribution::{ProviderHeaders, merge_provider_attribution_headers};
use telemetry::bug_report::{
    BugReportBundle, ExtensionError, Host, MetadataInput, ProviderInfo, bug_report_archive_file_name, bug_report_files,
    collect_bug_report_diagnostics, collect_bug_report_metadata, redact_json_value, redact_url,
    write_bug_report_archive,
};
use telemetry::crash_log::{
    Crash, CrashError, CrashKind, CrashRecord, Extension, clear_crash_log, find_extension_stack_matches,
    read_crash_log, record_crash, take_unnotified_crash,
};
use telemetry::zip::LocalDateTime;
use telemetry::{is_install_telemetry_enabled, iso_timestamp, parse_iso_millis};

static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/reports.json")).expect("fixture"));

fn str_of(value: &Value) -> &str {
    value.as_str().expect("string")
}

fn host() -> Host {
    let h = &FIXTURE["host"];
    Host {
        version: str_of(&h["version"]).into(),
        runtime: str_of(&h["runtime"]).into(),
        platform: str_of(&h["platform"]).into(),
        arch: str_of(&h["arch"]).into(),
        os_release: str_of(&h["osRelease"]).into(),
        os_version: str_of(&h["osVersion"]).into(),
    }
}

fn extensions(value: &Value) -> Vec<Extension> {
    serde_json::from_value(value.clone()).expect("extensions")
}

fn records(value: &Value) -> Vec<CrashRecord> {
    value
        .as_array()
        .expect("records")
        .iter()
        .map(|r| CrashRecord(r.as_object().expect("record").clone()))
        .collect()
}

fn records_json(records: &[CrashRecord]) -> Value {
    Value::Array(records.iter().map(|r| Value::Object(r.0.clone())).collect())
}

#[test]
fn telemetry_redact_url_matches_typescript() {
    let cases = FIXTURE["redactUrlCases"].as_array().expect("cases");
    assert_eq!(cases.len(), 26);
    for case in cases {
        assert_eq!(redact_url(str_of(&case["input"])), str_of(&case["output"]), "{case}");
    }
}

#[test]
fn telemetry_redact_url_ported_literals() {
    // bug-report.test.ts "removes URL credentials and secret query parameters".
    assert_eq!(
        redact_url("https://user:pass@proxy.example.com:8080/"),
        "https://proxy.example.com:8080/"
    );
    assert_eq!(
        redact_url("git:https://pat@github.com/org/repo"),
        "git:https://github.com/org/repo"
    );
    assert_eq!(
        redact_url("https://api.example/v1?api-key=abc&model=x"),
        "https://api.example/v1?api-key=%3Credacted%3E&model=x"
    );
}

#[test]
fn telemetry_redact_json_value_matches_typescript() {
    let cases = FIXTURE["redactJsonCases"].as_array().expect("cases");
    assert_eq!(cases.len(), 6);
    for case in cases {
        assert_eq!(redact_json_value(&case["input"]), case["output"], "{case}");
    }
    // bug-report.test.ts "redacts nested secret values without hiding token counts".
    assert_eq!(
        redact_json_value(&json!({
            "apiKey": "sk-123",
            "headers": { "Authorization": "Bearer x", "X-Trace": "1" },
            "compaction": { "reserveTokens": 16_384, "keepRecentTokens": 20_000 },
            "baseUrl": "https://me:secret@example.com/",
        })),
        json!({
            "apiKey": "<redacted>",
            "headers": { "Authorization": "<redacted>", "X-Trace": "1" },
            "compaction": { "reserveTokens": 16_384, "keepRecentTokens": 20_000 },
            "baseUrl": "https://example.com/",
        })
    );
}

#[test]
fn telemetry_bug_report_metadata_matches_typescript() {
    let host = host();
    let created_at = str_of(&FIXTURE["host"]["createdAt"]);
    let exts = extensions(&FIXTURE["extensions"]);
    let errors = [ExtensionError {
        path: "/ext/bad.ts".into(),
        error: "SyntaxError: nope".into(),
    }];
    let cases = FIXTURE["metadataCases"].as_array().expect("cases");
    assert_eq!(cases.len(), 4);
    for case in cases {
        let env: BTreeMap<String, String> = serde_json::from_value(case["env"].clone()).expect("env");
        let options = &case["options"];
        let provider = (!case["provider"].is_null()).then(|| ProviderInfo {
            provider: case["provider"].clone(),
            auth_status: case["status"].clone(),
            using_oauth: case["usingOAuth"].as_bool().expect("oauth"),
            registered_by_extension: case["registered"].as_bool().expect("registered"),
        });
        let input = MetadataInput {
            id: str_of(&options["id"]),
            created_at,
            hint: options["hint"].as_str(),
            session_id: str_of(&options["sessionId"]),
            cwd: str_of(&options["cwd"]),
            include_session: options["includeSession"].as_bool().expect("bool"),
            include_summary: options["includeSummary"].as_bool().expect("bool"),
            message_count: options["messageCount"].as_u64().expect("count"),
            model: options.get("model"),
            provider: provider.as_ref(),
            thinking_level: str_of(&options["thinkingLevel"]),
            extensions: &exts,
            extension_errors: &errors,
            global_settings: &FIXTURE["globalSettings"],
            project_settings: &FIXTURE["projectSettings"],
            host: &host,
            env: &env,
        };
        assert_eq!(collect_bug_report_metadata(&input), case["output"], "{}", options["id"]);
    }
}

#[test]
fn telemetry_bug_report_diagnostics_match_typescript() {
    let cases = FIXTURE["diagnosticsCases"].as_array().expect("cases");
    let first = &cases[0];
    let entries = first["entries"].as_array().expect("entries");
    assert_eq!(
        collect_bug_report_diagnostics("session-9", entries, &records(&first["crashes"])),
        first["output"]
    );
    assert_eq!(collect_bug_report_diagnostics("empty", &[], &[]), cases[1]["output"]);
}

fn bundle(value: &Value) -> BugReportBundle {
    BugReportBundle {
        metadata: value["metadata"].clone(),
        diagnostics: value["diagnostics"].clone(),
        session_jsonl: value["sessionJsonl"].as_str().map(Into::into),
        summary: value["summary"].as_str().map(Into::into),
    }
}

#[test]
fn telemetry_bug_report_files_match_typescript() {
    for case in FIXTURE["fileCases"].as_array().expect("cases") {
        assert_eq!(json!(bug_report_files(&bundle(&case["bundle"]))), case["files"]);
    }
    assert_eq!(bug_report_archive_file_name("abc"), str_of(&FIXTURE["archive"]["name"]));
}

fn base64(text: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let digits: Vec<u8> = text.bytes().filter(|&c| c != b'=').map(value).collect();
    digits
        .chunks(4)
        .flat_map(|chunk| {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, &d)| n | u32::from(d) << (18 - 6 * i));
            let bytes = n.to_be_bytes();
            bytes[1..chunk.len()].to_vec()
        })
        .collect()
}

fn u16_at(bytes: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("u32")) as usize
}

/// (local header without the compressed size, central header without sizes/offset, name, inflated data) per entry,
/// plus the end record without the directory offset and size.
type ZipView = (Vec<(Vec<u8>, Vec<u8>, String, Vec<u8>)>, Vec<u8>);

fn read_zip(bytes: &[u8]) -> ZipView {
    let end = bytes.len() - 22;
    let count = u16_at(bytes, end + 10);
    let mut central = u32_at(bytes, end + 16);
    let mut entries = Vec::new();
    for _ in 0..count {
        let local = u32_at(bytes, central + 42);
        let name_len = u16_at(bytes, local + 26);
        let compressed_len = u32_at(bytes, local + 18);
        let name = String::from_utf8(bytes[local + 30..local + 30 + name_len].to_vec()).expect("name");
        let data_at = local + 30 + name_len;
        let mut inflated = Vec::new();
        flate2::read::DeflateDecoder::new(&bytes[data_at..data_at + compressed_len])
            .read_to_end(&mut inflated)
            .expect("inflate");
        assert_eq!(u32_at(bytes, local + 22), inflated.len());
        let mut local_header = bytes[local..local + 30].to_vec();
        local_header[18..22].fill(0);
        let mut central_header = bytes[central..central + 46].to_vec();
        central_header[20..24].fill(0);
        central_header[42..46].fill(0);
        entries.push((local_header, central_header, name, inflated));
        central += 46 + name_len;
    }
    let mut end_record = bytes[end..].to_vec();
    end_record[12..20].fill(0);
    (entries, end_record)
}

#[test]
fn telemetry_bug_report_archive_matches_typescript_entries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("report.zip");
    // The generator pins the clock at 2026-04-05T14:07:09.321Z with TZ=UTC.
    let at = LocalDateTime {
        year: 2026,
        month: 4,
        day: 5,
        hour: 14,
        minute: 7,
        second: 9,
    };
    write_bug_report_archive(&bundle(&FIXTURE["archive"]["bundle"]), &path, at).expect("write");
    let ours = std::fs::read(&path).expect("read");
    let theirs = base64(str_of(&FIXTURE["archive"]["base64"]));
    // Deflate streams differ between zlib and miniz; every header field and every inflated byte must not.
    let (our_entries, our_end) = read_zip(&ours);
    let (their_entries, their_end) = read_zip(&theirs);
    assert_eq!(our_entries.len(), 4);
    assert_eq!(our_entries, their_entries);
    assert_eq!(our_end, their_end);
}

fn crash_error(spec: &Value) -> CrashError {
    match spec.get("value") {
        Some(value) => CrashError::Value(str_of(value).into()),
        None => CrashError::Error {
            name: str_of(&spec["name"]).into(),
            message: str_of(&spec["message"]).into(),
            stack: spec["stack"].as_str().map(Into::into),
        },
    }
}

#[test]
fn telemetry_crash_log_steps_match_typescript() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested/dir/crashes.json");
    let version = str_of(&FIXTURE["host"]["version"]);
    let steps = FIXTURE["crashSteps"].as_array().expect("steps");
    assert_eq!(steps.len(), 28);
    for (index, step) in steps.iter().enumerate() {
        let op = &step["op"];
        let result = match str_of(&op[0]) {
            "take" => json!(take_unnotified_crash(&path, step["takeAt"].as_i64().expect("now")).map(|r| r.0)),
            "record" => {
                let spec = &op[1];
                let kind = match str_of(&spec["kind"]) {
                    "fatal_error" => CrashKind::FatalError,
                    _ => CrashKind::UncaughtException,
                };
                let error = crash_error(&spec["error"]);
                let crash = Crash {
                    kind,
                    error: &error,
                    session_file: spec["sessionFile"].as_str(),
                    cwd: str_of(&spec["cwd"]),
                };
                json!(record_crash(&crash, version, step["now"].as_i64().expect("now"), &path).map(|r| r.0))
            }
            "read" => records_json(&read_crash_log(&path)),
            "clear" => {
                clear_crash_log(&path);
                Value::Null
            }
            "raw" => {
                std::fs::write(&path, str_of(&op[1])).expect("raw");
                Value::Null
            }
            other => panic!("unknown op {other}"),
        };
        assert_eq!(result, step["result"], "step {index} {op}");
        let file = std::fs::read_to_string(&path).ok();
        assert_eq!(json!(file), step["file"], "file after step {index} {op}");
    }
}

#[test]
fn telemetry_record_crash_is_best_effort() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The parent "directory" is a file: nothing can be written, and nothing panics.
    let blocker = dir.path().join("file");
    std::fs::write(&blocker, "x").expect("write");
    let error = CrashError::Value("v".into());
    let crash = Crash {
        kind: CrashKind::FatalError,
        error: &error,
        session_file: None,
        cwd: "/",
    };
    assert_eq!(record_crash(&crash, "1", 0, &blocker.join("crashes.json")), None);
    assert_eq!(take_unnotified_crash(&blocker.join("crashes.json"), 0), None);
    clear_crash_log(dir.path());
    assert!(dir.path().exists());
}

#[test]
fn telemetry_extension_stack_matches_match_typescript() {
    let cases = FIXTURE["stackCases"].as_array().expect("cases");
    assert_eq!(cases.len(), 14);
    for case in cases {
        let exts = extensions(&case["extensions"]);
        assert_eq!(
            json!(find_extension_stack_matches(case["stack"].as_str(), &exts)),
            case["matches"],
            "{}",
            case["stack"]
        );
    }
    assert!(find_extension_stack_matches(None, &extensions(&cases[0]["extensions"])).is_empty());
}

#[test]
fn telemetry_attribution_headers_match_typescript() {
    let cases = FIXTURE["attributionCases"].as_array().expect("cases");
    assert_eq!(cases.len(), 168);
    for case in cases {
        let sources: Vec<Option<ProviderHeaders>> = case["headerSources"]
            .as_array()
            .expect("sources")
            .iter()
            .map(|s| serde_json::from_value(s.clone()).expect("headers"))
            .collect();
        let refs: Vec<Option<&ProviderHeaders>> = sources.iter().map(Option::as_ref).collect();
        let model = &case["model"];
        let enabled = is_install_telemetry_enabled(None, case["enabled"].as_bool());
        let output = merge_provider_attribution_headers(
            str_of(&model["provider"]),
            str_of(&model["baseUrl"]),
            enabled,
            case["sessionId"].as_str(),
            &refs,
        );
        assert_eq!(json!(output), case["output"], "{case}");
    }
    // PI_TELEMETRY=0 overrides an enabled setting.
    let enabled = is_install_telemetry_enabled(Some("0"), Some(true));
    assert_eq!(
        json!(merge_provider_attribution_headers(
            "openrouter",
            "x",
            enabled,
            None,
            &[]
        )),
        FIXTURE["envOverride"]
    );
}

#[test]
fn telemetry_crash_timestamps_round_trip() {
    let now = 1_775_398_029_321;
    assert_eq!(parse_iso_millis(&iso_timestamp(now)), Some(now));
}
