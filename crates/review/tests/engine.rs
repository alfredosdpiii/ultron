//! The review engine (`autoreview/engine.ts`): the cell it runs, the errors it reports, the defaults it fills, session
//! pruning, and a whole review through a real Python kernel whose `rlm.map` frames go to a scripted chat-completions
//! transport, routed by the model each frame names. The command-level differential test against the TypeScript is
//! crates/cli/tests/autoreview.rs.

use ai::{Body, HttpRequest, HttpResponse, Model, Transport};
use kernel::CellError;
use review::engine::{
    EngineOptions, KernelEngine, ModelResolver, RetryPolicy, RetryingTransport, ReviewEngine, SESSION_RETENTION,
    finish_result, is_retryable_failure, pipeline_error, prune_sessions, review_cell,
};
use serde_json::{Value, json};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[test]
fn review_engine_cell_runs_run_file_with_json_quoted_paths() {
    assert_eq!(
        review_cell("/tmp/a b/spec.json", "/tmp/x\"y/result.json"),
        "import autoreview_api\nawait autoreview_api.run_file(rlm, \"/tmp/a b/spec.json\", \"/tmp/x\\\"y/result.json\")"
    );
}

#[test]
fn review_engine_pipeline_error_names_the_exception_and_the_traceback_end() {
    let traceback: Vec<String> = (1..=8).map(|n| format!("frame {n}")).collect();
    let error = CellError {
        ename: "ReviewError".into(),
        evalue: "base 'nope' is not a commit".into(),
        traceback,
    };
    assert_eq!(
        pipeline_error(Some(&error)),
        "the review pipeline failed: ReviewError: base 'nope' is not a commit\nframe 3\nframe 4\nframe 5\nframe 6\nframe 7\nframe 8"
    );
    // No error object: the TypeScript's `?? "error"` and `?? ""`, trimmed.
    assert_eq!(pipeline_error(None), "the review pipeline failed: error:");
}

#[test]
fn review_engine_result_error_and_model_defaults() {
    assert_eq!(
        finish_result(json!({"error": "base 'nope' is not a commit in /r"}), "p/m"),
        Err("base 'nope' is not a commit in /r".into())
    );
    // model ?? runtime.model; verifyModel ?? model ?? runtime.model.
    assert_eq!(
        finish_result(json!({"complete": true, "model": null, "verifyModel": null}), "p/m").unwrap(),
        json!({"complete": true, "model": "p/m", "verifyModel": "p/m"})
    );
    assert_eq!(
        finish_result(json!({"model": "a/b"}), "p/m").unwrap(),
        json!({"model": "a/b", "verifyModel": "a/b"})
    );
    assert_eq!(
        finish_result(json!({"model": "a/b", "verifyModel": "c/d"}), "p/m").unwrap(),
        json!({"model": "a/b", "verifyModel": "c/d"})
    );
    assert!(finish_result(json!([1]), "p/m").is_err());
}

fn touch(path: &Path, age: Duration) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(path).unwrap();
    file.set_modified(SystemTime::now() - age).unwrap();
}

#[test]
fn review_engine_prunes_only_old_session_files() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path();
    let old = SESSION_RETENTION + Duration::from_secs(60);
    let fresh = SESSION_RETENTION - Duration::from_secs(60);
    touch(&sessions.join("g1/old.jsonl"), old);
    touch(&sessions.join("g1/new.jsonl"), fresh);
    touch(&sessions.join("g1/old.txt"), old);
    touch(&sessions.join("top.jsonl"), old);
    std::fs::create_dir_all(sessions.join("g2/nested.jsonl")).unwrap();
    prune_sessions(sessions, SystemTime::now());
    assert!(!sessions.join("g1/old.jsonl").exists());
    assert!(sessions.join("g1/new.jsonl").exists());
    assert!(sessions.join("g1/old.txt").exists());
    assert!(sessions.join("top.jsonl").exists());
    assert!(sessions.join("g2/nested.jsonl").is_dir());
    // A missing directory is not an error.
    prune_sessions(&sessions.join("missing"), SystemTime::now());
}

struct Chunks(std::vec::IntoIter<Vec<u8>>);

impl Body for Chunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.next();
        Box::pin(async move { Ok(next) })
    }
}

/// A chat-completions provider: the finder of correctness reports the off-by-one, every verifier confirms it, other
/// frames find nothing. Records each request's model.
#[derive(Default)]
struct Provider {
    models: Mutex<Vec<String>>,
}

impl Transport for Provider {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        let body: Value = serde_json::from_str(&request.body).unwrap();
        self.models
            .lock()
            .unwrap()
            .push(body["model"].as_str().unwrap().to_string());
        let text = body["messages"].to_string().replace("\\n", "\n").replace("\\\"", "\"");
        let content = if text.contains("You check several findings") {
            json!([{"verdict": "confirmed", "evidence": "`for i in range(len(items) - 1):` stops early.", "corrected_line": null, "severity": "major", "scenario_holds": true, "finding": 1}]).to_string()
        } else if text.contains("You check one finding") {
            json!({"verdict": "confirmed", "evidence": "`for i in range(len(items) - 1):` stops early.", "corrected_line": null, "severity": "major", "scenario_holds": true}).to_string()
        } else if text.contains("Your specialty: Correctness") && text.contains("File: calc.py") {
            json!([{"file": "calc.py", "line": 4, "severity": "major", "category": "correctness", "claim": "total() skips the last item.", "why": "range(len(items) - 1) stops one short.", "suggested_fix": "Use range(len(items)).", "confidence": 0.9, "scenario": "total([1, 2]) returns 1; it should return 3."}]).to_string()
        } else {
            "[]".to_string()
        };
        let events = [
            json!({"id": "c", "object": "chat.completion.chunk", "model": "m", "choices": [{"index": 0, "delta": {"role": "assistant", "content": content}, "finish_reason": null}]}),
            json!({"id": "c", "object": "chat.completion.chunk", "model": "m", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 50, "completion_tokens": 10}}),
        ];
        let mut chunks: Vec<Vec<u8>> = events.iter().map(|e| format!("data: {e}\n\n").into_bytes()).collect();
        chunks.push(b"data: [DONE]\n\n".to_vec());
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: [("content-type".to_string(), "text/event-stream".to_string())].into(),
                body: Box::new(Chunks(chunks.into_iter())),
            })
        })
    }
}

fn model(provider: &str, id: &str) -> Model {
    serde_json::from_value(json!({
        "id": id, "name": id, "api": "openai-completions", "provider": provider, "baseUrl": "http://fake.invalid/v1",
        "reasoning": false, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 128000, "maxTokens": 8192,
    }))
    .unwrap()
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", "2026-10-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-10-01T00:00:00Z")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

const CALC: &str = "def total(items):\n    \"\"\"Sum of item prices.\"\"\"\n    result = 0\n    for i in range(len(items)):\n        result += items[i][\"price\"]\n    return result\n";

#[test]
fn review_engine_reviews_a_local_diff_through_a_kernel_with_frames_routed_by_model() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("calc.py"), CALC).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "calc"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(
        repo.join("calc.py"),
        CALC.replace("range(len(items))", "range(len(items) - 1)"),
    )
    .unwrap();
    git(&repo, &["commit", "-qam", "tweak"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);

    let provider = Arc::new(Provider::default());
    let resolved = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = resolved.clone();
    let resolve: ModelResolver = Arc::new(move |reference: &str| {
        seen.lock().unwrap().push(reference.to_string());
        let (provider, id) = reference.split_once('/').unwrap();
        if provider == "nowhere" {
            return Err(format!("Model \"{reference}\" not found"));
        }
        Ok((model(provider, id), Some("key".into())))
    });
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let logged = lines.clone();
    let engine = KernelEngine::open(EngineOptions {
        dir: root.join("agent/autoreview"),
        runtime_path: runtime(),
        python: None,
        model: model("stub", "frames"),
        api_key: Some("key".into()),
        resolve,
        transport: provider.clone(),
        retry: RetryPolicy::default(),
        frame_env: Default::default(),
        settings: Default::default(),
        kernel_env: Vec::new(),
        log: Some(Arc::new(move |line: &str| {
            logged.lock().unwrap().push(line.to_string())
        })),
    })
    .unwrap();
    assert_eq!(engine.default_model(), "stub/frames");
    assert!(lines.lock().unwrap()[0].starts_with("engine ready in "));
    assert!(lines.lock().unwrap()[0].ends_with(" ms (model stub/frames)"));
    for sub in ["work", "sessions"] {
        let meta = std::fs::metadata(root.join("agent/autoreview").join(sub)).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    }

    let spec = json!({"repoDir": repo, "base": base, "head": head, "mode": "fast", "model": "stub/frames",
        "verifyModel": "other/verify", "thinking": "low", "verifyThinking": "low"});
    let result = engine.review(&spec, None).unwrap();
    assert_eq!(result["complete"], true, "{result}");
    assert_eq!(result["model"], "stub/frames");
    assert_eq!(result["verifyModel"], "other/verify");
    let findings = result["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["file"], "calc.py");
    assert_eq!(findings[0]["line"], 4);
    assert_eq!(findings[0]["verification"], "confirmed");
    // Finders ran on the engine's model, the verifier on the one the spec named (resolved once).
    let models = provider.models.lock().unwrap().clone();
    assert_eq!(models.iter().filter(|m| *m == "frames").count(), 3);
    assert_eq!(models.iter().filter(|m| *m == "verify").count(), 1);
    assert_eq!(*resolved.lock().unwrap(), ["other/verify"]);

    // A verifier model the engine cannot resolve fails those frames, not the review: the finding stays unverified.
    let spec = json!({"repoDir": repo, "base": base, "head": head, "mode": "fast", "model": "stub/frames",
        "verifyModel": "nowhere/x", "retryBaseSeconds": 0});
    let result = engine.review(&spec, None).unwrap();
    assert_eq!(result["verifyModel"], "nowhere/x");
    assert_eq!(result["complete"], false, "{result}");

    // A pipeline error (a base that is not a commit) is the review's error.
    let spec = json!({"repoDir": repo, "base": "nope", "head": head, "mode": "fast"});
    let error = engine.review(&spec, None).unwrap_err();
    assert!(error.contains("base 'nope' is not a commit"), "{error}");
    // Each review's scratch directory is gone.
    let leftovers = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!("ultron-autoreview-{}-", std::process::id()))
        })
        .count();
    assert_eq!(leftovers, 0);
}

/// Replies with the scripted statuses in order (then 200), recording each request.
struct Statuses {
    replies: Mutex<Vec<(u16, &'static str)>>,
    sent: Mutex<u32>,
}

impl Transport for Statuses {
    fn send(&self, _request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        *self.sent.lock().unwrap() += 1;
        let next = {
            let mut replies = self.replies.lock().unwrap();
            (!replies.is_empty()).then(|| replies.remove(0))
        };
        Box::pin(async move {
            let (status, body) = match next {
                Some((0, message)) => return Err(message.to_string()),
                Some(reply) => reply,
                None => (200, "ok"),
            };
            Ok(HttpResponse {
                status,
                headers: Default::default(),
                body: Box::new(Chunks(vec![body.as_bytes().to_vec()].into_iter())),
            })
        })
    }
}

fn send(replies: Vec<(u16, &'static str)>, policy: RetryPolicy) -> (Result<(u16, String), String>, u32) {
    let inner = Arc::new(Statuses {
        replies: Mutex::new(replies),
        sent: Mutex::new(0),
    });
    let transport = RetryingTransport {
        inner: inner.clone(),
        policy,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        let request = HttpRequest {
            method: "POST".into(),
            url: "http://x/v1".into(),
            headers: Default::default(),
            body: "{}".into(),
        };
        let mut response = transport.send(request).await?;
        let mut body = Vec::new();
        while let Some(chunk) = response.body.next_chunk().await? {
            body.extend(chunk);
        }
        Ok((response.status, String::from_utf8(body).unwrap()))
    });
    let sent = *inner.sent.lock().unwrap();
    (result, sent)
}

#[test]
fn review_engine_retries_transient_provider_failures_like_the_frame_harness() {
    let quick = RetryPolicy {
        base_delay_ms: 1.0,
        ..RetryPolicy::default()
    };
    // A rate limit, then success: the frame sees only the success.
    assert_eq!(
        send(vec![(429, "{\"error\":\"Rate limit reached\"}")], quick),
        (Ok((200, "ok".into())), 2)
    );
    // Server errors and connection failures are retried up to maxRetries (3) more times, then the last is returned.
    assert_eq!(send(vec![(503, "busy"); 5], quick), (Ok((503, "busy".into())), 4));
    assert_eq!(
        send(vec![(0, "connection refused"), (0, "socket hang up")], quick),
        (Ok((200, "ok".into())), 3)
    );
    assert_eq!(
        send(vec![(0, "invalid url"), (0, "x")], quick),
        (Err("invalid url".into()), 1)
    );
    // Quota exhaustion is not transient; a client error is not either. Their bodies reach the frame intact.
    assert_eq!(
        send(vec![(429, "insufficient_quota: billing")], quick),
        (Ok((429, "insufficient_quota: billing".into())), 1)
    );
    assert_eq!(
        send(vec![(400, "bad request")], quick),
        (Ok((400, "bad request".into())), 1)
    );
    // retry.enabled: false sends once.
    let off = RetryPolicy {
        enabled: false,
        ..quick
    };
    assert_eq!(
        send(vec![(429, "rate limit")], off),
        (Ok((429, "rate limit".into())), 1)
    );
    // The delays: baseDelayMs * 2^(attempt-1), capped by maxAgentDelayMs.
    let policy = RetryPolicy::default();
    assert_eq!(
        (1..=7).map(|a| policy.delay_ms(a)).collect::<Vec<_>>(),
        [2000.0, 4000.0, 8000.0, 16000.0, 32000.0, 60000.0, 60000.0]
    );
    assert!(is_retryable_failure("429 Too Many Requests"));
    assert!(is_retryable_failure("upstream connect error"));
    assert!(!is_retryable_failure("429 GoUsageLimitError"));
    assert!(!is_retryable_failure(""));
}
