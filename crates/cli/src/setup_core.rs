//! The pure parts of `ultron setup` (cli/setup/core.ts): environment checks, the models.json entry for a custom
//! endpoint, the Hindsight Docker command and health probe, and when an interactive start should offer setup.
//! Nothing here draws to the terminal; `setup_wizard` asks the questions and `setup_tui` draws them.
//!
//! The Rust build runs without Node.js, so the environment check has no Node.js row; python3 is the interpreter the
//! REPL kernel runs (`ULTRON_PYTHON`, else /usr/bin/python3 when it exists, else `python3`).

use serde_json::{Map, Value, json};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------------------------- environment

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentCheck {
    pub label: String,
    pub ok: bool,
    pub detail: String,
    /// What to do about it, when not ok.
    pub fix: Option<String>,
    /// A failed optional check is a warning, not a blocker.
    pub optional: bool,
}

/// Run `command args` and return its first output line, or `None` when it is missing or fails.
pub type ProbeCommand = Arc<dyn Fn(&str, &[&str]) -> Option<String> + Send + Sync>;

/// `probeCommand`: `command args` on `path` (the run's PATH), stdout then stderr, first line trimmed; at most 5 s.
pub fn probe_command(path: Option<&str>, command: &str, args: &[&str]) -> Option<String> {
    let mut child = std::process::Command::new(command);
    child
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match path {
        Some(path) => child.env("PATH", path),
        None => child.env_remove("PATH"),
    };
    let mut child = child.spawn().ok()?;
    let (mut stdout, mut stderr) = (child.stdout.take()?, child.stderr.take()?);
    let out = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let (out, err) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    if !status?.success() {
        return None;
    }
    let text = format!("{out}{err}");
    Some(text.trim().split('\n').next().unwrap_or("").trim().to_string())
}

/// The interpreter the REPL kernel runs: `ULTRON_PYTHON`, else /usr/bin/python3 when it exists, else `python3`.
pub fn python_command(var: &dyn Fn(&str) -> Option<String>) -> String {
    if let Some(python) = var("ULTRON_PYTHON").filter(|p| !p.is_empty()) {
        return python;
    }
    if Path::new("/usr/bin/python3").exists() {
        "/usr/bin/python3".into()
    } else {
        "python3".into()
    }
}

/// python3 (the REPL runs on it) and Docker (only for installing Hindsight). `python` is the interpreter to probe.
pub fn check_environment(probe: &ProbeCommand, python: &str) -> Vec<EnvironmentCheck> {
    let python = probe(python, &["--version"]);
    let docker = probe("docker", &["--version"]);
    vec![
        EnvironmentCheck {
            label: "python3".into(),
            ok: python.is_some(),
            detail: match &python {
                None => "not found on PATH".into(),
                Some(line) if line.is_empty() => "found".into(),
                Some(line) => line.clone(),
            },
            fix: python.is_none().then(|| {
                "The REPL runs on the system python3. Install Python 3 (e.g. `brew install python`, `sudo apt install python3`) and make sure `python3` is on your PATH.".into()
            }),
            optional: false,
        },
        EnvironmentCheck {
            label: "Docker".into(),
            ok: docker.is_some(),
            detail: match &docker {
                None => "not found (only needed to install Hindsight)".into(),
                Some(line) if line.is_empty() => "found".into(),
                Some(line) => line.clone(),
            },
            fix: None,
            optional: true,
        },
    ]
}

// ------------------------------------------------------------------------------------------------------------- Loki

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LokiAnalyzerCheck {
    pub name: &'static str,
    /// What Loki uses it for.
    pub language: &'static str,
    pub found: bool,
    pub hint: &'static str,
}

/// `LOKI_ANALYZERS`: name, language, command, args, hint.
pub const LOKI_ANALYZERS: [(&str, &str, &str, &[&str], &str); 8] = [
    (
        "ruff",
        "Python",
        "ruff",
        &["--version"],
        "uv tool install ruff (or pipx install ruff)",
    ),
    (
        "mypy",
        "Python types",
        "mypy",
        &["--version"],
        "uv tool install mypy (or pipx install mypy)",
    ),
    (
        "tsc",
        "TypeScript types",
        "tsc",
        &["--version"],
        "npm i -D typescript in the project (node_modules/.bin/tsc is used when present)",
    ),
    (
        "oxlint",
        "JavaScript/TypeScript",
        "oxlint",
        &["--version"],
        "npm i -D oxlint @oxlint/plugins in the project (node_modules/.bin/oxlint is used when present)",
    ),
    (
        "golangci-lint",
        "Go",
        "golangci-lint",
        &["--version"],
        "https://golangci-lint.run/welcome/install/",
    ),
    (
        "clippy",
        "Rust",
        "cargo",
        &["clippy", "--version"],
        "rustup component add clippy",
    ),
    (
        "credo",
        "Elixir",
        "mix",
        &["--version"],
        "install Elixir, then add {:credo, only: [:dev, :test]} to the project's mix.exs deps",
    ),
    (
        "sobelow",
        "Phoenix security",
        "mix",
        &["--version"],
        "install Elixir, then add {:sobelow, only: [:dev, :test]} to the project's mix.exs deps",
    ),
];

/// Which of Loki's analyzers are on PATH (one probe per distinct command line).
pub fn check_loki_analyzers(probe: &ProbeCommand) -> Vec<LokiAnalyzerCheck> {
    let mut cache: Vec<(String, bool)> = Vec::new();
    LOKI_ANALYZERS
        .iter()
        .map(|(name, language, command, args, hint)| {
            let key = format!("{command} {}", args.join(" "));
            let found = match cache.iter().find(|(k, _)| *k == key) {
                Some((_, found)) => *found,
                None => {
                    let found = probe(command, args).is_some();
                    cache.push((key, found));
                    found
                }
            };
            LokiAnalyzerCheck {
                name,
                language,
                found,
                hint,
            }
        })
        .collect()
}

/// `bundledLokiVersion` of the pinned engine this build carries (its VERSION.json): `<version> (<commit 12>)`.
pub fn bundled_loki_version_from(pin: &str) -> Option<String> {
    let pin: Value = serde_json::from_str(pin).ok()?;
    let version = pin["version"].as_str()?;
    let commit = pin["source"]["commit"]
        .as_str()
        .map(|c| format!(" ({})", c.chars().take(12).collect::<String>()))
        .unwrap_or_default();
    Some(format!("{version}{commit}"))
}

/// The Loki engine version bundled with this build.
pub fn bundled_loki_version() -> Option<String> {
    bundled_loki_version_from(include_str!(
        "../../../packages/coding-agent/src/ultron/loki-engine/VERSION.json"
    ))
}

// ----------------------------------------------------------------------------------------------------- secret files

/// `writePrivateFile`: readable only by the user (0600), in a directory only the user can list (0700 when created).
pub fn write_private_file(path: &Path, content: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let temporary = format!("{}.{}.tmp", path.display(), std::process::id());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(content.as_bytes())?;
    drop(file);
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&temporary, path)
}

// --------------------------------------------------------------------------------------------------- custom endpoint

/// `validateProviderId`: `/^[a-z0-9][a-z0-9._-]*$/`.
pub fn validate_provider_id(value: &str) -> Option<String> {
    let mut chars = value.chars();
    let ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    (!ok).then(|| "Use lowercase letters, digits, '.', '_' or '-' (e.g. my-proxy)".into())
}

/// A WHATWG URL's parts as `new URL()` gives them, for the cases setup needs: the scheme (lowercase), and for
/// http(s) the hostname (lowercase) and explicit port. `None` when `new URL()` throws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UrlParts {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    /// Path and query (`/` when empty), for requests.
    pub path: String,
}

pub fn parse_url(value: &str) -> Option<UrlParts> {
    let value = value.trim_matches(|c: char| c <= ' ');
    let colon = value.find(':')?;
    let scheme = &value[..colon];
    let mut chars = scheme.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    let rest = &value[colon + 1..];
    if !matches!(scheme.as_str(), "http" | "https" | "ws" | "wss" | "ftp" | "file") {
        // A non-special scheme: any text after it is an opaque path.
        return Some(UrlParts {
            scheme,
            host: String::new(),
            port: None,
            path: rest.to_string(),
        });
    }
    let rest = rest.trim_start_matches(['/', '\\']);
    let end = rest.find(['/', '\\', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = (&rest[..end], &rest[end..]);
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let close = stripped.find(']')?;
        let host = &authority[..close + 2];
        let after = &stripped[close + 1..];
        (host, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if scheme == "file" {
        return Some(UrlParts {
            scheme,
            host: host.to_ascii_lowercase(),
            port: None,
            path: path.to_string(),
        });
    }
    if host.is_empty() || host.chars().any(|c| c <= ' ' || "<>^|%".contains(c)) {
        return None;
    }
    let port = match port {
        None | Some("") => None,
        Some(p) => {
            if !p.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let n: u32 = p.parse().ok()?;
            if n > 65535 {
                return None;
            }
            let default = match scheme.as_str() {
                "http" | "ws" => 80,
                "https" | "wss" => 443,
                _ => 21,
            };
            (n != default).then_some(n as u16)
        }
    };
    let path = path.split('#').next().unwrap_or("");
    Some(UrlParts {
        scheme,
        host: host.to_ascii_lowercase(),
        port,
        path: if path.is_empty() || path.starts_with('?') {
            format!("/{path}")
        } else {
            path.replace('\\', "/")
        },
    })
}

/// `validateBaseUrl`.
pub fn validate_base_url(value: &str) -> Option<String> {
    if value.split("://").count() > 2 {
        return Some("That looks like two URLs run together; enter one".into());
    }
    match parse_url(value.trim()) {
        Some(url) if url.scheme == "http" || url.scheme == "https" => None,
        Some(_) => Some("The URL must start with http:// or https://".into()),
        None => Some("Not a valid URL (e.g. http://localhost:11434/v1)".into()),
    }
}

/// `parseModelIds`: split on whitespace and commas, unique, in order.
pub fn parse_model_ids(value: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for id in value.split(|c: char| crate::js::is_space(c) || c == ',') {
        let id = crate::js::trim(id);
        if !id.is_empty() && !ids.iter().any(|existing| existing == id) {
            ids.push(id.to_string());
        }
    }
    ids
}

/// The key written when the endpoint needs none: models.json makes a provider available only with a key.
pub const NO_KEY_PLACEHOLDER: &str = "none";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomEndpoint {
    pub provider_id: String,
    pub base_url: String,
    pub api_key: Option<String>,
    /// `openai-completions` or `openai-responses`.
    pub api: String,
    pub model_ids: Vec<String>,
}

/// `mergeCustomEndpoint`: models.json with `endpoint` added as a provider (replacing one of the same id), other
/// providers and keys as they are; the new text and whether a provider was replaced. A file that is not a JSON
/// object is an error, so a broken file is never overwritten.
pub fn merge_custom_endpoint(existing: Option<&str>, endpoint: &CustomEndpoint) -> Result<(String, bool), String> {
    let mut config = Map::new();
    if let Some(existing) = existing.filter(|text| !crate::js::trim(text).is_empty()) {
        let text = existing.strip_prefix('\u{feff}').unwrap_or(existing);
        let parsed: Value = serde_json::from_str(&ai::model_config::strip_json_comments(text))
            .map_err(|error| format!("models.json is not valid JSON: {error}"))?;
        match parsed {
            Value::Object(map) => config = map,
            _ => return Err("models.json must contain a JSON object".into()),
        }
    }
    let mut providers = match config.get("providers") {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    let replaced = providers.contains_key(&endpoint.provider_id);
    let key = endpoint
        .api_key
        .as_deref()
        .map(crate::js::trim)
        .filter(|k| !k.is_empty())
        .unwrap_or(NO_KEY_PLACEHOLDER);
    providers.insert(
        endpoint.provider_id.clone(),
        json!({
            "baseUrl": endpoint.base_url.trim_end_matches('/'),
            "api": endpoint.api,
            "apiKey": key,
            "models": endpoint.model_ids.iter().map(|id| json!({"id": id})).collect::<Vec<_>>(),
        }),
    );
    config.insert("providers".into(), Value::Object(providers));
    Ok((
        format!("{}\n", config::json::stringify_pretty(&Value::Object(config))),
        replaced,
    ))
}

/// `readTextFile`: the file's text, `None` when it does not exist.
pub fn read_text_file(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// `customProviderExists`.
pub fn custom_provider_exists(models_json: &Path, provider_id: &str) -> bool {
    let Ok(Some(text)) = read_text_file(models_json) else {
        return false;
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    serde_json::from_str::<Value>(&ai::model_config::strip_json_comments(text))
        .ok()
        .and_then(|v| v.get("providers").and_then(Value::as_object).cloned())
        .is_some_and(|providers| providers.contains_key(provider_id))
}

// ------------------------------------------------------------------------------------------------------------ fetch

/// One GET as setup makes it (`fetch` with `redirect: "error"` and a timeout).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchResponse {
    pub status: u16,
    pub body: String,
}

/// A GET; `Err` is why no response came, as `describeFetchError` words it.
pub type Fetch = Arc<dyn Fn(&FetchRequest) -> Result<FetchResponse, String> + Send + Sync>;

/// The real fetch: plain HTTP over a socket (so a refused connection reads as Node's errno does), HTTPS through the
/// provider transport.
pub fn http_fetch() -> Fetch {
    Arc::new(|request: &FetchRequest| {
        let url = parse_url(&request.url).ok_or_else(|| format!("Failed to parse URL from {}", request.url))?;
        match url.scheme.as_str() {
            "http" => plain_get(&url, request),
            "https" => tls_get(request),
            _ => Err("fetch failed".into()),
        }
    })
}

fn plain_get(url: &UrlParts, request: &FetchRequest) -> Result<FetchResponse, String> {
    use std::net::ToSocketAddrs;
    let deadline = Instant::now() + request.timeout;
    let host = url.host.trim_start_matches('[').trim_end_matches(']');
    let port = url.port.unwrap_or(80);
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|_| "host not found".to_string())?
        .collect();
    let mut stream = None;
    let mut refused = false;
    for addr in &addrs {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("timed out".into());
        }
        match std::net::TcpStream::connect_timeout(addr, left) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => refused = true,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return Err("timed out".into()),
            Err(e) => return Err(e.to_string()),
        }
    }
    let Some(mut stream) = stream else {
        return Err(if refused || addrs.is_empty() {
            "connection refused".into()
        } else {
            "fetch failed".into()
        });
    };
    let left = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1));
    let _ = stream.set_read_timeout(Some(left));
    let _ = stream.set_write_timeout(Some(left));
    let host_header = match url.port {
        Some(port) => format!("{}:{port}", url.host),
        None => url.host.clone(),
    };
    let mut head = format!(
        "GET {} HTTP/1.1\r\nhost: {host_header}\r\nconnection: close\r\naccept: */*\r\n",
        url.path
    );
    for (name, value) in &request.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let timed = |e: std::io::Error| {
        if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) {
            "timed out".to_string()
        } else {
            e.to_string()
        }
    };
    stream.write_all(head.as_bytes()).map_err(timed)?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(timed)?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("fetch failed")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut body = raw[split + 4..].to_vec();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("fetch failed")?;
    if (300..400).contains(&status) && head.to_ascii_lowercase().contains("\nlocation:") {
        // `redirect: "error"`.
        return Err("unexpected redirect".into());
    }
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        body = dechunk(&body);
    }
    Ok(FetchResponse {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line_end) = data.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(String::from_utf8_lossy(&data[..line_end]).trim(), 16).unwrap_or(0);
        data = &data[line_end + 2..];
        if size == 0 || data.len() < size {
            break;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[(size + 2).min(data.len())..];
    }
    out
}

fn tls_get(request: &FetchRequest) -> Result<FetchResponse, String> {
    use ai::http::Transport;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let transport = ai::http::ReqwestTransport::new(Some(request.timeout));
    runtime.block_on(async {
        let response = transport
            .send(ai::http::HttpRequest {
                method: "GET".into(),
                url: request.url.clone(),
                headers: request.headers.iter().cloned().collect(),
                body: String::new(),
            })
            .await?;
        let mut body = response.body;
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next_chunk().await? {
            bytes.extend(chunk);
        }
        Ok(FetchResponse {
            status: response.status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        })
    })
}

/// `listEndpointModels`: the ids `GET <baseUrl>/models` lists, sorted; `None` when it does not answer with a list.
pub fn list_endpoint_models(base_url: &str, api_key: Option<&str>, fetch: &Fetch) -> Option<Vec<String>> {
    let key = api_key.map(crate::js::trim).filter(|k| !k.is_empty());
    let response = fetch(&FetchRequest {
        url: format!("{}/models", base_url.trim_end_matches('/')),
        headers: key
            .map(|k| vec![("authorization".to_string(), format!("Bearer {k}"))])
            .unwrap_or_default(),
        timeout: Duration::from_secs(5),
    })
    .ok()?;
    if !(200..300).contains(&response.status) {
        return None;
    }
    let body: Value = serde_json::from_str(&response.body).ok()?;
    let mut ids: Vec<String> = body
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    ids.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    Some(ids)
}

// --------------------------------------------------------------------------------------------------------- Hindsight

pub const HINDSIGHT_IMAGE: &str = "ghcr.io/vectorize-io/hindsight:latest";
pub const HINDSIGHT_CONTAINER: &str = "hindsight";
pub const HINDSIGHT_VOLUME: &str = "hindsight-data";
pub const HINDSIGHT_DOCS: &str = "https://hindsight.vectorize.io/developer/installation";
/// `DEFAULT_HINDSIGHT_URL`.
pub const DEFAULT_HINDSIGHT_URL: &str = "http://localhost:8888";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HindsightLlmChoice {
    /// `HINDSIGHT_API_LLM_PROVIDER`.
    pub provider: &'static str,
    pub label: &'static str,
    pub needs_key: bool,
    pub needs_base_url: bool,
    pub default_base_url: Option<&'static str>,
}

const fn llm(provider: &'static str, label: &'static str) -> HindsightLlmChoice {
    HindsightLlmChoice {
        provider,
        label,
        needs_key: true,
        needs_base_url: false,
        default_base_url: None,
    }
}

/// `HINDSIGHT_LLM_PROVIDERS`.
pub const HINDSIGHT_LLM_PROVIDERS: [HindsightLlmChoice; 8] = [
    llm("openai", "OpenAI (Hindsight's default)"),
    llm("anthropic", "Anthropic"),
    llm("gemini", "Google Gemini"),
    llm("groq", "Groq"),
    llm("openrouter", "OpenRouter"),
    llm("deepseek", "DeepSeek"),
    HindsightLlmChoice {
        provider: "ollama",
        label: "Ollama (local)",
        needs_key: false,
        needs_base_url: true,
        default_base_url: Some("http://host.docker.internal:11434/v1"),
    },
    HindsightLlmChoice {
        provider: "openai",
        label: "Other OpenAI-compatible endpoint",
        needs_key: true,
        needs_base_url: true,
        default_base_url: None,
    },
];

#[derive(Clone, Debug, Default)]
pub struct HindsightDockerOptions {
    pub port: u16,
    pub provider: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
    /// Passed in the child's environment, never on the command line.
    pub has_api_key: bool,
}

/// `hindsightDockerArgs`: the documented single-container install; the key is named without a value.
pub fn hindsight_docker_args(options: &HindsightDockerOptions) -> Vec<String> {
    let mut args: Vec<String> = [
        "run",
        "-d",
        "--name",
        HINDSIGHT_CONTAINER,
        "--restart",
        "unless-stopped",
        "-p",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(format!("{}:8888", options.port));
    args.push("-e".into());
    args.push(format!("HINDSIGHT_API_LLM_PROVIDER={}", options.provider));
    if options.has_api_key {
        args.extend(["-e".into(), "HINDSIGHT_API_LLM_API_KEY".into()]);
    }
    if let Some(model) = options.model.as_deref().map(crate::js::trim).filter(|m| !m.is_empty()) {
        args.extend(["-e".into(), format!("HINDSIGHT_API_LLM_MODEL={model}")]);
    }
    let base_url = options
        .base_url
        .as_deref()
        .map(crate::js::trim)
        .filter(|b| !b.is_empty());
    if let Some(base_url) = base_url {
        args.extend(["-e".into(), format!("HINDSIGHT_API_LLM_BASE_URL={base_url}")]);
    }
    if options
        .base_url
        .as_deref()
        .is_some_and(|b| b.contains("host.docker.internal"))
    {
        args.extend(["--add-host".into(), "host.docker.internal:host-gateway".into()]);
    }
    args.extend([
        "-v".into(),
        format!("{HINDSIGHT_VOLUME}:/home/hindsight/.pg0"),
        HINDSIGHT_IMAGE.into(),
    ]);
    args
}

/// `formatCommand`: arguments that are not plain words are single-quoted.
pub fn format_command(command: &str, args: &[String]) -> String {
    std::iter::once(command.to_string())
        .chain(args.iter().cloned())
        .map(|part| {
            let plain = !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
            if plain {
                part
            } else {
                format!("'{}'", part.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `hindsightPort`: the URL's port, else its scheme's default; 8888 for a URL that does not parse.
pub fn hindsight_port(url: &str) -> u16 {
    match parse_url(url) {
        Some(parts) => parts.port.unwrap_or(if parts.scheme == "https" { 443 } else { 80 }),
        None => 8888,
    }
}

/// `isLocalUrl`.
pub fn is_local_url(url: &str) -> bool {
    parse_url(url).is_some_and(|parts| ["localhost", "127.0.0.1", "::1", "[::1]"].contains(&parts.host.as_str()))
}

/// Where the effective Hindsight URL came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrlSource {
    Env,
    Setting,
    Default,
}

/// `effectiveHindsightUrl`: ULTRON_HINDSIGHT_URL, then the `hindsightUrl` setting, then localhost:8888; `None`
/// when memory is turned off.
pub fn effective_hindsight_url(env_value: Option<&str>, setting: Option<&str>) -> (Option<String>, UrlSource) {
    let from_env = env_value.map(crate::js::trim).filter(|v| !v.is_empty());
    let setting = setting.map(crate::js::trim).filter(|v| !v.is_empty());
    let source = if from_env.is_some() {
        UrlSource::Env
    } else if setting.is_some() {
        UrlSource::Setting
    } else {
        UrlSource::Default
    };
    match from_env.or(setting) {
        None => (Some(DEFAULT_HINDSIGHT_URL.into()), source),
        Some(value) => {
            let off = ["off", "none", "0", "false"].contains(&value.to_lowercase().as_str());
            ((!off).then(|| value.to_string()), source)
        }
    }
}

/// `probeHindsight`: GET `<url>/health`; the reason it is not reachable.
pub fn probe_hindsight(url: &str, fetch: &Fetch) -> Result<(), String> {
    let response = fetch(&FetchRequest {
        url: format!("{}/health", url.trim_end_matches('/')),
        headers: Vec::new(),
        timeout: Duration::from_secs(2),
    })?;
    if (200..300).contains(&response.status) {
        Ok(())
    } else {
        Err(format!("HTTP {}", response.status))
    }
}

/// `waitForHindsight`: poll until Hindsight answers or `timeout` passes.
pub fn wait_for_hindsight(url: &str, timeout: Duration, interval: Duration, fetch: &Fetch) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if probe_hindsight(url, fetch).is_ok() {
            return true;
        }
        std::thread::sleep(interval);
    }
    false
}

/// `hindsightManualInstructions`.
pub fn hindsight_manual_instructions(url: &str) -> Vec<String> {
    let port = hindsight_port(url);
    vec![
        "Docker (recommended; the full image is several GB):".into(),
        "  export OPENAI_API_KEY=sk-...".into(),
        format!("  docker run -d --name {HINDSIGHT_CONTAINER} --restart unless-stopped -p {port}:8888 \\"),
        "    -e HINDSIGHT_API_LLM_API_KEY=$OPENAI_API_KEY \\".into(),
        format!("    -v {HINDSIGHT_VOLUME}:/home/hindsight/.pg0 {HINDSIGHT_IMAGE}"),
        String::new(),
        "Without Docker (Python 3.11 or newer):".into(),
        "  pip install hindsight-api".into(),
        "  export HINDSIGHT_API_LLM_PROVIDER=openai   # or anthropic, gemini, groq, ollama, ...".into(),
        "  export HINDSIGHT_API_LLM_API_KEY=sk-...".into(),
        format!(
            "  hindsight-api{}",
            if port == 8888 {
                String::new()
            } else {
                format!(" --port {port}")
            }
        ),
        String::new(),
        format!("Other providers and options: {HINDSIGHT_DOCS}"),
        "Then run `ultron setup` again to check the connection.".into(),
    ]
}

// --------------------------------------------------------------------------------------------------------- first run

pub struct SetupOfferInput<'a> {
    pub stdin_is_tty: bool,
    pub stdout_is_tty: bool,
    /// The start runs the TUI: no `-p`, `--mode`, prompt, piped stdin or `@file`.
    pub interactive: bool,
    /// A model or credential was named on the command line, or extensions were loaded with `-e`.
    pub explicit_model: bool,
    pub var: &'a dyn Fn(&str) -> Option<String>,
    /// The user chose "Don't ask again".
    pub dismissed: bool,
    /// Checked last, and only when everything else allows the offer (it loads the model catalog).
    pub has_usable_model: &'a dyn Fn() -> bool,
}

/// `shouldOfferSetup`.
pub fn should_offer_setup(input: &SetupOfferInput) -> bool {
    if !input.stdin_is_tty || !input.stdout_is_tty || !input.interactive {
        return false;
    }
    if input.explicit_model || input.dismissed {
        return false;
    }
    if let Some(skip) = (input.var)("ULTRON_SKIP_SETUP").map(|s| crate::js::trim(&s).to_lowercase())
        && !skip.is_empty()
        && !["0", "false", "no"].contains(&skip.as_str())
    {
        return false;
    }
    if (input.var)("CI").is_some_and(|ci| !ci.is_empty()) {
        return false;
    }
    !(input.has_usable_model)()
}
