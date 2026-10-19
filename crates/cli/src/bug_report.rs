//! `/bug` on the client (modes/interactive/bug-report.ts with core/bug-report.ts and core/bug-report-upload.ts):
//! the report bundle of the attached session (`report.json` metadata, the content-free `diagnostics.json`, the
//! branch as `session.jsonl` when the user shares it, the model's `summary.md` when they chose one instead), its
//! upload to the Radius gateway (`PI_RADIUS_GATEWAY`, else `https://radius.pi.dev`) or its zip export into the
//! working directory, and the `pi.bug-report` entry that records it in the session.

use crate::Env;
use crate::link::Link;
use serde_json::{Map, Value, json};
use session_core::core::Op;
use session_file::Env as _;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use telemetry::bug_report::{BugReportBundle, BugReportFile};

/// What the user chose in the dialogs.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub hint: Option<String>,
    pub include_session: bool,
    /// The model's summary, when the user asked for one.
    pub summary: Option<String>,
    /// The command line gave `--api-key` (the session's requests use it, whatever the provider).
    pub runtime_key: bool,
}

/// `getRadiusGatewayUrl()`.
pub fn gateway_url(env: &Env) -> String {
    let gateway = (env.var)("PI_RADIUS_GATEWAY").unwrap_or_else(|| ai::model_registry::DEFAULT_RADIUS_GATEWAY.into());
    ai::oauth_radius::normalize_radius_gateway_url(&gateway)
}

/// `new URL(gateway).host`: the host, with its port when it is not the scheme's default.
pub fn gateway_host(gateway: &str) -> String {
    match url::Url::parse(gateway) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_string(),
            _ => String::new(),
        },
        Err(_) => gateway.to_string(),
    }
}

/// `uuidv7()`: 48 bits of Unix milliseconds, the version and variant bits, the rest random.
fn uuid_v7(now_ms: i64) -> String {
    let random = session_file::SystemEnv.random_uuid().replace('-', "");
    let mut hex = format!("{:012x}", now_ms.max(0) as u64 & 0xffff_ffff_ffff);
    hex.push('7');
    hex.push_str(&random[13..16]);
    let variant = u8::from_str_radix(&random[16..17], 16).unwrap_or(0) & 0x3 | 0x8;
    hex.push_str(&format!("{variant:x}"));
    hex.push_str(&random[17..32]);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Node's `process.platform` and `process.arch` names for this build.
fn node_platform() -> (&'static str, &'static str) {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    (platform, arch)
}

/// The host details `collectEnvironment` reads (`os.release()`, `os.version()` from the kernel). The Rust build runs
/// no Node: the runtime names the build.
fn host() -> telemetry::bug_report::Host {
    let read = |path: &str| {
        std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let (platform, arch) = node_platform();
    telemetry::bug_report::Host {
        version: crate::version(),
        runtime: "rust".into(),
        platform: platform.into(),
        arch: arch.into(),
        os_release: read("/proc/sys/kernel/osrelease"),
        os_version: read("/proc/sys/kernel/version"),
    }
}

/// `serializeSessionBranch` with `createShareTrailingEntries`: a session header, the branch's entries chained by
/// `parentId`, and the `pi.share` entry with the system prompt and tools.
fn session_jsonl(
    session_id: &str,
    cwd: &str,
    branch: &[Value],
    share: &Value,
    timestamp: &str,
    share_id: &str,
) -> String {
    let mut lines =
        vec![json!({"type": "session", "version": 3, "id": session_id, "timestamp": timestamp, "cwd": cwd})];
    let mut parent = Value::Null;
    for entry in branch {
        let mut entry = entry.clone();
        entry["parentId"] = parent;
        parent = entry["id"].clone();
        lines.push(entry);
    }
    lines.push(json!({
        "type": "custom", "customType": "pi.share", "id": share_id, "parentId": parent, "timestamp": timestamp,
        "data": {"systemPrompt": share["systemPrompt"], "tools": share["tools"]},
    }));
    let text: Vec<String> = lines.iter().map(Value::to_string).collect();
    format!("{}\n", text.join("\n"))
}

/// The built-in providers' own `baseUrl` (each provider factory's in packages/ai/src/providers/*.ts); a provider
/// whose models carry their own (Bedrock, Vertex, Azure, Cloudflare, OpenCode, Radius) has none.
const BUILTIN_BASE_URLS: &[(&str, &str)] = &[
    ("ant-ling", "https://api.ant-ling.com/v1"),
    ("anthropic", "https://api.anthropic.com"),
    ("baseten", "https://inference.baseten.co/v1"),
    ("cerebras", "https://api.cerebras.ai/v1"),
    ("claude-code", "claude-code://cli"),
    ("deepseek", "https://api.deepseek.com"),
    ("fireworks", "https://api.fireworks.ai/inference"),
    ("github-copilot", "https://api.individual.githubcopilot.com"),
    ("google", "https://generativelanguage.googleapis.com/v1beta"),
    ("groq", "https://api.groq.com/openai/v1"),
    ("huggingface", "https://router.huggingface.co/v1"),
    ("kimi-coding", "https://api.kimi.com/coding"),
    ("meta", "https://api.meta.ai/v1"),
    ("minimax", "https://api.minimax.io/anthropic"),
    ("minimax-cn", "https://api.minimaxi.com/anthropic"),
    ("mistral", "https://api.mistral.ai"),
    ("moonshotai", "https://api.moonshot.ai/v1"),
    ("moonshotai-cn", "https://api.moonshot.cn/v1"),
    ("nvidia", "https://integrate.api.nvidia.com/v1"),
    ("openai", "https://api.openai.com/v1"),
    ("openai-codex", "https://chatgpt.com/backend-api"),
    ("openrouter", "https://openrouter.ai/api/v1"),
    (
        "qwen-token-plan",
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
    ),
    (
        "qwen-token-plan-cn",
        "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
    ),
    (
        "qwen-token-plan-individual",
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
    ),
    ("together", "https://api.together.ai/v1"),
    ("vercel-ai-gateway", "https://ai-gateway.vercel.sh"),
    ("xai", "https://api.x.ai/v1"),
    ("xiaomi", "https://api.xiaomimimo.com/v1"),
    ("xiaomi-token-plan-ams", "https://token-plan-ams.xiaomimimo.com/v1"),
    ("xiaomi-token-plan-cn", "https://token-plan-cn.xiaomimimo.com/v1"),
    ("xiaomi-token-plan-sgp", "https://token-plan-sgp.xiaomimimo.com/v1"),
    ("zai", "https://api.z.ai/api/coding/paas/v4"),
    ("zai-coding-cn", "https://open.bigmodel.cn/api/coding/paas/v4"),
];

/// `ctx.env(name)`: a set, non-empty variable.
fn env_value(env: &Env, name: &str) -> Option<String> {
    (env.var)(name).filter(|value| !value.is_empty())
}

/// The `source` of the provider's ambient credential (`provider.auth.apiKey.check`/`resolve` without a stored
/// credential): the first variable that supplies a key, or the provider's own ambient sources (AWS, gcloud
/// application default credentials, Cloudflare's account, the Claude Code CLI).
fn environment_source(env: &Env, provider: &str) -> Option<String> {
    let has = |name: &str| env_value(env, name).is_some();
    let lookup = |name: &str| env_value(env, name);
    let source = ai::env::EnvSource {
        lookup: &lookup,
        exists: &|p: &Path| p.exists(),
        home: Some(env.home.clone()),
    };
    match provider {
        "amazon-bedrock" => [
            ("AWS_BEARER_TOKEN_BEDROCK", "AWS_BEARER_TOKEN_BEDROCK"),
            ("AWS_PROFILE", "AWS_PROFILE"),
        ]
        .iter()
        .find(|(name, _)| has(name))
        .map(|(_, label)| label.to_string())
        .or_else(|| (has("AWS_ACCESS_KEY_ID") && has("AWS_SECRET_ACCESS_KEY")).then(|| "AWS access keys".into()))
        .or_else(|| {
            (has("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") || has("AWS_CONTAINER_CREDENTIALS_FULL_URI"))
                .then(|| "ECS task role".into())
        })
        .or_else(|| has("AWS_WEB_IDENTITY_TOKEN_FILE").then(|| "web identity token".into())),
        "google-vertex" => {
            if has("GOOGLE_CLOUD_API_KEY") {
                return Some("GOOGLE_CLOUD_API_KEY".into());
            }
            let adc = env_value(env, "GOOGLE_APPLICATION_CREDENTIALS").map_or_else(
                || env.home.join(".config/gcloud/application_default_credentials.json"),
                PathBuf::from,
            );
            let project = has("GOOGLE_CLOUD_PROJECT") || has("GCLOUD_PROJECT");
            (adc.exists() && project && has("GOOGLE_CLOUD_LOCATION"))
                .then(|| "gcloud application default credentials".into())
        }
        "cloudflare-workers-ai" | "cloudflare-ai-gateway" => {
            let gateway = provider == "cloudflare-workers-ai" || has("CLOUDFLARE_GATEWAY_ID");
            (has("CLOUDFLARE_API_KEY") && has("CLOUDFLARE_ACCOUNT_ID") && gateway).then(|| "CLOUDFLARE_API_KEY".into())
        }
        ai::claude_code::PROVIDER => {
            ai::claude_code::configured(&source).then(|| ai::claude_code::AUTH_SOURCE.to_string())
        }
        _ => ai::env::find_env_keys_in(provider, &source).and_then(|keys| keys.first().map(|key| key.to_string())),
    }
}

/// `configuredRequestAuthStatus` of a models.json `apiKey`: a `!command`, the variables a template names (all set,
/// else not configured), or the key itself.
fn configured_key_status(env: &Env, key: &str) -> Value {
    if ai::model_registry::is_command(key) {
        return json!({"configured": true, "source": "models_json_command"});
    }
    let names = ai::model_registry::env_names(key);
    if names.is_empty() {
        return json!({"configured": true, "source": "models_json_key"});
    }
    if names.iter().all(|name| env_value(env, name).is_some()) {
        json!({"configured": true, "source": "environment", "label": names.join(", ")})
    } else {
        json!({"configured": false})
    }
}

/// The model's provider as `describeProvider` reads it from the session's `ModelRuntime`: the provider definition
/// (a built-in's name, `baseUrl` and sign-in methods, overlaid by models.json; a models.json provider signs in with
/// a key), `getProviderAuthStatus` (the command line's `--api-key`, a stored credential, the models.json key, the
/// environment) and whether the stored credential is an OAuth login.
/// Header names stay empty: the composed provider carries no headers of its own (recorded from TypeScript).
pub fn provider_info(env: &Env, provider: &str, runtime_key: bool) -> telemetry::bug_report::ProviderInfo {
    let registry = session_core::lane::registry(env);
    let config = registry.config().provider(provider);
    let logins = interactive::selectors::provider_logins();
    let login = logins.iter().find(|p| p.id == provider).or_else(|| {
        registry
            .radius_gateway(provider)
            .and_then(|_| logins.iter().find(|p| p.id == "radius"))
    });
    let text = |key: &str| config.and_then(|c| c.get(key)).and_then(Value::as_str);
    let name = text("name")
        .map(str::to_string)
        .or_else(|| login.filter(|l| l.id == provider).map(|l| l.name.clone()))
        .unwrap_or_else(|| provider.to_string());
    let base_url = text("baseUrl").or_else(|| {
        BUILTIN_BASE_URLS
            .iter()
            .find(|(id, _)| *id == provider)
            .map(|(_, url)| *url)
    });
    let auth = match login {
        Some(login) => json!({"apiKey": login.api_key.is_some(), "oauth": login.oauth.is_some()}),
        None => json!({"apiKey": true}),
    };
    let mut definition = json!({"id": provider, "name": name, "auth": auth});
    if let Some(url) = base_url {
        definition["baseUrl"] = json!(url);
    }
    let stored = config::auth::AuthStorage::create(config::paths::AgentPaths::new(env.agent_dir()).auth())
        .list()
        .unwrap_or_default()
        .into_iter()
        .find(|(id, _)| id == provider)
        .map(|(_, kind)| kind);
    let auth_status = if runtime_key {
        json!({"configured": true, "source": "runtime"})
    } else if stored.is_some() {
        json!({"configured": true, "source": "stored"})
    } else if let Some(key) = text("apiKey") {
        configured_key_status(env, key)
    } else {
        environment_source(env, provider).map_or_else(
            || json!({"configured": false}),
            |label| json!({"configured": true, "source": "environment", "label": label}),
        )
    };
    telemetry::bug_report::ProviderInfo {
        provider: definition,
        auth_status,
        using_oauth: stored.is_some_and(|kind| kind == "oauth"),
        // Pi extensions (whose `registerProvider` this is) are not supported by ultron-rs.
        registered_by_extension: false,
    }
}

/// `buildBundle`: the report of the attached session as the user chose it.
pub fn build_bundle(link: &Link, env: &Env, request: &Request) -> Result<BugReportBundle, String> {
    let now = session_file::SystemEnv.now_ms();
    let described = link.op(&Op::Describe)?.unwrap_or_default();
    let state = link.op(&Op::State)?.unwrap_or_default();
    let session_id = described["sessionId"].as_str().unwrap_or_default().to_string();
    let cwd = env.cwd.display().to_string();
    let agent_dir = env.agent_dir();
    let trusted = config::trust::worker_project_trusted(&env.cwd, &agent_dir, &env.home);
    let settings = config::settings::SettingsManager::create(&env.cwd, &agent_dir, trusted);
    let model = described.get("model").filter(|m| m.is_object());
    let provider = model
        .and_then(|m| m["provider"].as_str())
        .map(|p| provider_info(env, p, request.runtime_key));
    let vars: BTreeMap<String, String> = std::env::vars().collect();
    let host = host();
    let id = uuid_v7(now);
    let created_at = session_file::js::iso_string(now);
    let global = Value::Object(settings.global_settings().clone());
    let project = Value::Object(settings.project_settings().clone());
    let messages = described["messages"].as_array().map_or(0, Vec::len) as u64;
    let thinking = state["thinkingLevel"].as_str().unwrap_or("off").to_string();
    let metadata = telemetry::bug_report::collect_bug_report_metadata(&telemetry::bug_report::MetadataInput {
        id: &id,
        created_at: &created_at,
        hint: request.hint.as_deref(),
        session_id: &session_id,
        cwd: &cwd,
        include_session: request.include_session,
        include_summary: request.summary.is_some(),
        message_count: messages,
        model,
        provider: provider.as_ref(),
        thinking_level: &thinking,
        // Pi extensions are not supported by ultron-rs: the report lists none.
        extensions: &[],
        extension_errors: &[],
        global_settings: &global,
        project_settings: &project,
        host: &host,
        env: &vars,
    });
    let entries = link.op(&Op::Entries { since: None })?.unwrap_or_default()["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let crashes = telemetry::crash_log::read_crash_log(&telemetry::crash_log::crash_log_path(&agent_dir));
    let diagnostics = telemetry::bug_report::collect_bug_report_diagnostics(&session_id, &entries, &crashes);
    let session = if request.include_session {
        let share = link.op(&Op::ShareContext)?.unwrap_or_default();
        let branch = described["entries"].as_array().cloned().unwrap_or_default();
        let share_id: String = session_file::SystemEnv.random_uuid().chars().take(8).collect();
        Some(session_jsonl(
            &session_id,
            &cwd,
            &branch,
            &share,
            &created_at,
            &share_id,
        ))
    } else {
        None
    };
    Ok(BugReportBundle {
        metadata,
        diagnostics,
        session_jsonl: session,
        summary: request.summary.clone(),
    })
}

/// The multipart body `FormData` sends: one file part per bundle file.
fn multipart(files: &[BugReportFile], boundary: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for file in files {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{0}\"; filename=\"{0}\"\r\nContent-Type: {1}\r\n\r\n",
                file.name, file.content_type
            )
            .as_bytes(),
        );
        body.extend_from_slice(file.data.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// `uploadBugReport`: the bundle as multipart form data to `<gateway>/v1/bug-reports`, anonymously or as the Radius
/// account (`token`). `Ok(None)` when `cancel` was set first (nothing more is reported).
pub fn upload(
    bundle: &BugReportBundle,
    gateway: &str,
    token: Option<&str>,
    cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    let url = url::Url::parse(gateway)
        .and_then(|base| base.join("/v1/bug-reports"))
        .map_err(|e| e.to_string())?;
    let boundary = format!("----ultron-bug-report-{}", session_file::SystemEnv.random_uuid());
    let body = multipart(&telemetry::bug_report::bug_report_files(bundle), &boundary);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let mut request = reqwest::Client::new()
            .post(url)
            .header("content-type", format!("multipart/form-data; boundary={boundary}"))
            .body(body);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let work = async {
            let response = request.send().await.map_err(|_| "fetch failed".to_string())?;
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            Ok::<_, String>((status, text))
        };
        let cancelled = async {
            while !cancel.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        };
        let (status, text) = tokio::select! {
            answer = work => answer?,
            () = cancelled => return Ok(None),
        };
        upload_answer(status.as_u16(), status.canonical_reason().unwrap_or(""), &text).map(Some)
    })
}

/// What `uploadBugReport` makes of the gateway's answer: the report id, or its error.
pub fn upload_answer(status: u16, status_text: &str, body: &str) -> Result<String, String> {
    let json: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let ok = (200..300).contains(&status);
    if ok
        && json["ok"] == true
        && let Some(id) = json["bug_report"]["id"].as_str()
    {
        return Ok(id.to_string());
    }
    let truthy = |v: &Value| session_file::js::truthy(Some(v));
    let detail = (json.is_object() && !truthy(&json["ok"]))
        .then(|| {
            [&json["description"], &json["error"]]
                .into_iter()
                .find(|v| truthy(v))
                .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
        })
        .flatten();
    let shown = detail.unwrap_or_else(|| {
        if status_text.is_empty() {
            status.to_string()
        } else {
            status_text.to_string()
        }
    });
    Err(format!("Bug report upload failed: {shown}"))
}

/// `exportZip`'s archive: `pi-bug-report-<id>.zip` in `cwd`, stamped with the local time now.
pub fn export(bundle: &BugReportBundle, env: &Env) -> Result<PathBuf, String> {
    let id = bundle.metadata["id"].as_str().unwrap_or_default();
    let path = env.cwd.join(telemetry::bug_report::bug_report_archive_file_name(id));
    let now = session_file::SystemEnv.now_ms();
    let zone = crate::localtime::Zone::from_env((env.var)("TZ").as_deref());
    let fields = zone.fields(now as f64);
    let offset = zone.offset(now as f64);
    let at = telemetry::zip::LocalDateTime {
        year: fields.year.clamp(0, i64::from(u16::MAX)) as u16,
        month: fields.month as u8,
        day: fields.day as u8,
        hour: fields.hour as u8,
        minute: fields.minute as u8,
        second: ((now / 1000 + offset).rem_euclid(60)) as u8,
    };
    telemetry::bug_report::write_bug_report_archive(bundle, &path, at).map_err(|e| e.to_string())?;
    Ok(path)
}

/// `recordInSession`: the `pi.bug-report` entry (`delivery`: `{delivery: "upload"}` or `{delivery: "zip", path}`),
/// and the crash log cleared once its crashes were reported.
pub fn record(
    op: &dyn Fn(Op) -> Result<Option<Value>, String>,
    env: &Env,
    bundle: &BugReportBundle,
    delivery: Value,
) -> Result<(), String> {
    let metadata = &bundle.metadata;
    let mut data = Map::new();
    data.insert("id".into(), metadata["id"].clone());
    data.insert("createdAt".into(), metadata["createdAt"].clone());
    if !metadata["hint"].is_null() {
        data.insert("hint".into(), metadata["hint"].clone());
    }
    data.insert("sessionIncluded".into(), metadata["session"]["included"].clone());
    data.insert("summaryIncluded".into(), metadata["session"]["summaryIncluded"].clone());
    for (key, value) in delivery.as_object().into_iter().flatten() {
        data.insert(key.clone(), value.clone());
    }
    op(Op::AppendCustom {
        custom_type: telemetry::bug_report::BUG_REPORT_CUSTOM_ENTRY_TYPE.into(),
        data: Value::Object(data),
    })?;
    if bundle.diagnostics["crashes"].as_array().is_some_and(|c| !c.is_empty()) {
        telemetry::crash_log::clear_crash_log(&telemetry::crash_log::crash_log_path(&env.agent_dir()));
    }
    Ok(())
}

/// `exportZip` and `recordInSession`: the archive's path and the report id, or the error to show.
fn export_and_record(
    op: &dyn Fn(Op) -> Result<Option<Value>, String>,
    env: &Env,
    bundle: &BugReportBundle,
) -> interactive::lane::BugReportOutcome {
    use interactive::lane::BugReportOutcome;
    let path = match export(bundle, env) {
        Ok(path) => path,
        Err(error) => return BugReportOutcome::Failed(format!("Failed to write bug report: {error}")),
    };
    let shown = path.display().to_string();
    // A report that was written stays reported when recording it fails (TypeScript's write cannot fail).
    let _ = record(op, env, bundle, json!({"delivery": "zip", "path": shown}));
    BugReportOutcome::Exported {
        path: shown,
        id: bundle.metadata["id"].as_str().unwrap_or_default().to_string(),
    }
}

/// The link thread's side of `/bug`: the report built for the last delivery (the failed upload's fallback exports
/// it), and the flag Esc on the upload loader sets.
#[derive(Default)]
pub struct Reporter {
    last: Arc<Mutex<Option<BugReportBundle>>>,
    pub cancel: Arc<AtomicBool>,
    runtime_key: bool,
}

impl Reporter {
    /// A reporter whose uploads stop when `cancel` is set; `runtime_key` when the command line gave `--api-key`.
    pub fn new(cancel: Arc<AtomicBool>, runtime_key: bool) -> Self {
        Reporter {
            last: Arc::default(),
            cancel,
            runtime_key,
        }
    }

    /// Build the report and deliver it: a zip at once; an upload on its own thread, which records it through `op`
    /// and answers through `answer` (nothing after a cancel).
    pub fn deliver(
        &self,
        link: &Link,
        env: &Env,
        request: interactive::lane::BugReportRequest,
        op: Arc<dyn Fn(Op) -> Result<Option<Value>, String> + Send + Sync>,
        answer: Arc<dyn Fn(interactive::lane::BugReportOutcome) + Send + Sync>,
    ) {
        use interactive::lane::BugReportOutcome;
        let built = build_bundle(
            link,
            env,
            &Request {
                hint: request.hint.clone(),
                include_session: request.include_session,
                summary: request.summary.clone(),
                runtime_key: self.runtime_key,
            },
        );
        let bundle = match built {
            Ok(bundle) => bundle,
            Err(error) => return answer(BugReportOutcome::Failed(format!("Failed to build bug report: {error}"))),
        };
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(bundle.clone());
        if !request.upload {
            return answer(export_and_record(&*op, env, &bundle));
        }
        self.cancel.store(false, Ordering::SeqCst);
        let (cancel, env) = (self.cancel.clone(), env.clone());
        std::thread::spawn(move || {
            let gateway = gateway_url(&env);
            let token = crate::radius_relay::stored_radius_token(&env).ok().flatten();
            let result = upload(&bundle, &gateway, token.as_deref(), &cancel);
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            match result {
                Ok(Some(id)) => {
                    let _ = record(&*op, &env, &bundle, json!({"delivery": "upload"}));
                    answer(BugReportOutcome::Uploaded(id));
                }
                Ok(None) => {}
                Err(failure) => answer(BugReportOutcome::UploadFailed(failure)),
            }
        });
    }

    /// The failed upload's fallback: export the report it built.
    pub fn export_last(
        &self,
        env: &Env,
        op: &dyn Fn(Op) -> Result<Option<Value>, String>,
    ) -> interactive::lane::BugReportOutcome {
        let last = self.last.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match last {
            Some(bundle) => export_and_record(op, env, &bundle),
            None => interactive::lane::BugReportOutcome::Failed("Failed to write bug report: no report".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_bug_report_uuid_v7_has_the_time_version_and_variant() {
        let id = uuid_v7(0x0123_4567_89ab);
        assert_eq!(id.len(), 36);
        assert!(id.starts_with("01234567-89ab-7"), "{id}");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"), "{id}");
    }

    #[test]
    fn cli_bug_report_gateway_host_keeps_a_non_default_port() {
        assert_eq!(gateway_host("https://radius.pi.dev"), "radius.pi.dev");
        assert_eq!(gateway_host("http://127.0.0.1:8123"), "127.0.0.1:8123");
        assert_eq!(gateway_host("https://example.com:443"), "example.com");
    }

    /// `serializeSessionBranch`: entries re-chained in branch order, the share entry last.
    #[test]
    fn cli_bug_report_session_jsonl_chains_the_branch() {
        let branch = [
            json!({"type": "message", "id": "a", "parentId": "x"}),
            json!({"type": "message", "id": "b"}),
        ];
        let share = json!({"systemPrompt": "S", "tools": [{"name": "rlm"}]});
        let text = session_jsonl("sid", "/w", &branch, &share, "T", "abcd1234");
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert!(text.ends_with('\n'));
        assert_eq!(
            lines,
            vec![
                json!({"type": "session", "version": 3, "id": "sid", "timestamp": "T", "cwd": "/w"}),
                json!({"type": "message", "id": "a", "parentId": null}),
                json!({"type": "message", "id": "b", "parentId": "a"}),
                json!({"type": "custom", "customType": "pi.share", "id": "abcd1234", "parentId": "b", "timestamp": "T",
                    "data": {"systemPrompt": "S", "tools": [{"name": "rlm"}]}}),
            ]
        );
    }
}
