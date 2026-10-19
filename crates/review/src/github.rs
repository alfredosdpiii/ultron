//! GitHub as autoreview uses it (`autoreview/github.ts`): REST and GraphQL through `gh api -i`, authenticated per
//! account by the child's environment. Responses are requested with their headers, so conditional requests (ETag,
//! If-Modified-Since), `X-Poll-Interval` and rate limits (`Retry-After`, `X-RateLimit-Reset`) are honoured.
//!
//! Known differences from the TypeScript, outside recorded behaviour:
//! - Ids (`Number(item.id)`) that are not integers read as 0 (JavaScript kept NaN or the fraction); a pull request
//!   number past `u64` is not a pull request reference.
//! - A `null` entry in `requested_teams` or `errors` reads as empty (JavaScript threw a TypeError).
//! - Request bodies are written by serde_json (JavaScript's JSON.stringify differs only for non-integral numbers).

use std::sync::Arc;

use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::accounts::Account;
use crate::js::{head16, trim};
use crate::runner::{Env, RunOptions, Runner};

/// What a GitHub call (or the work around it) failed with.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// GitHub asked us to wait: nothing is sent for this account before `resume_at` (epoch ms).
    RateLimit {
        message: String,
        resume_at: f64,
    },
    /// Any other API failure; status 0 when `gh` produced no HTTP response.
    GitHub {
        message: String,
        status: u16,
    },
    /// The pull request's head is no longer the commit that was to be reviewed (`StaleHeadError`).
    StaleHead(String),
    /// Another `ultron autoreview run` holds the agent directory (`DaemonRunningError`).
    DaemonRunning(String),
    Other(String),
}

impl Error {
    pub fn message(&self) -> &str {
        match self {
            Error::RateLimit { message, .. } | Error::GitHub { message, .. } => message,
            Error::StaleHead(message) | Error::DaemonRunning(message) | Error::Other(message) => message,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for Error {}

// ---- JavaScript conversions the TypeScript relies on ----

/// `Number(text)`: JavaScript's string to number (whitespace trimmed, empty is 0, hex/octal/binary prefixes,
/// `Infinity`); NaN for anything else.
pub fn js_number(text: &str) -> f64 {
    let text = trim(text);
    if text.is_empty() {
        return 0.0;
    }
    match text {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = text.strip_prefix(prefix) {
            return u128::from_str_radix(digits, radix).map_or(f64::NAN, |n| n as f64);
        }
    }
    if !text
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
    {
        return f64::NAN;
    }
    text.parse::<f64>().unwrap_or(f64::NAN)
}

/// `Number(value)` of a JSON value (undefined: NaN).
pub fn js_value_number(value: Option<&Value>) -> f64 {
    match value {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => js_number(s),
        Some(Value::Array(items)) => match items.as_slice() {
            [] => 0.0,
            [one] if !one.is_object() && !one.is_array() => js_value_number(Some(one)),
            _ => f64::NAN,
        },
        Some(Value::Object(_)) => f64::NAN,
    }
}

/// An id read with `Number(...)`: integral values, else 0.
pub fn id_of(value: Option<&Value>) -> i64 {
    let n = js_value_number(value);
    if n.is_finite() && n.fract() == 0.0 { n as i64 } else { 0 }
}

/// `String(value)` for what an API field holds.
pub fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.as_f64().map_or_else(|| n.to_string(), crate::js::number),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                if item.is_null() {
                    String::new()
                } else {
                    js_string(Some(item))
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// `encodeURIComponent`.
pub fn encode_uri_component(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `new Date(ms).toISOString()` (years 0..=9999).
pub fn iso_from_ms(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rest = ms.rem_euclid(86_400_000);
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
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

/// `typeof value === "string" ? value : ""`.
pub fn text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn login(value: Option<&Value>) -> String {
    text(value.and_then(|value| value.get("login")))
}

fn items(value: Option<&Value>) -> Vec<Value> {
    match value {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    }
}

// ---- Pure parts ----

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct PullRef {
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl PullRef {
    pub fn new(host: &str, owner: &str, repo: &str, number: u64) -> Self {
        Self {
            host: host.into(),
            owner: owner.into(),
            repo: repo.into(),
            number,
        }
    }
}

pub fn pull_key(r: &PullRef) -> String {
    format!("{}/{}/{}#{}", r.host, r.owner, r.repo, r.number)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApiOutput {
    pub status: u16,
    pub headers: IndexMap<String, String>,
    pub text: String,
}

/// Split `gh api -i` output into the status, the headers and the body.
pub fn parse_api_output(stdout: &str) -> ApiOutput {
    let status_line = Regex::new(r"^HTTP/[0-9.]+ ([0-9]{3})[^\n]*\r?\n").expect("static pattern");
    let Some(found) = status_line.captures(stdout) else {
        return ApiOutput {
            status: 0,
            headers: IndexMap::new(),
            text: stdout.into(),
        };
    };
    let rest = &stdout[found[0].len()..];
    // A blank line ends the headers (at once, when there are none).
    let end = Regex::new(r"^\r?\n|\r?\n\r?\n").expect("static pattern").find(rest);
    let head = end.map_or(rest, |end| &rest[..end.start()]);
    let mut headers = IndexMap::new();
    for line in Regex::new(r"\r?\n").expect("static pattern").split(head) {
        if let Some(colon) = line.find(':').filter(|colon| *colon > 0) {
            headers.insert(
                trim(&line[..colon]).to_lowercase(),
                trim(&line[colon + 1..]).to_string(),
            );
        }
    }
    ApiOutput {
        status: found[1].parse().unwrap_or(0),
        headers,
        text: end.map_or(String::new(), |end| rest[end.end()..].to_string()),
    }
}

/// `https://api.github.com/repos/o/r/pulls/7` (or `.../issues/7`, or a web URL) as a PullRef.
pub fn pull_ref_from_url(url: &str, host: &str) -> Option<PullRef> {
    // JavaScript's `.` stops at every line terminator.
    let pattern = Regex::new(r"/([^/]+)/([^/]+)/(?:pulls|pull|issues)/([0-9]+)(?:[/?#][^\n\r\u{2028}\u{2029}]*)?$")
        .expect("static pattern");
    let found = pattern.captures(url)?;
    Some(PullRef::new(host, &found[1], &found[2], found[3].parse().ok()?))
}

/// `owner/repo#N` or a pull request URL.
pub fn parse_pull_target(target: &str, default_host: &str) -> Option<PullRef> {
    let short = Regex::new(r"^([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)#([0-9]+)$").expect("static pattern");
    if let Some(found) = short.captures(target) {
        return Some(PullRef::new(default_host, &found[1], &found[2], found[3].parse().ok()?));
    }
    let url = Regex::new(r"^https?://([^/]+)/([^/]+)/([^/]+)/pull/([0-9]+)(?:[/?#][^\n\r\u{2028}\u{2029}]*)?$")
        .expect("static pattern");
    let found = url.captures(target)?;
    Some(PullRef::new(&found[1], &found[2], &found[3], found[4].parse().ok()?))
}

pub const NOTIFICATION_REASONS: [&str; 3] = ["review_requested", "mention", "team_mention"];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApiResponse {
    pub status: u16,
    pub headers: IndexMap<String, String>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub body: String,
    /// `open` or `closed`.
    pub state: String,
    pub merged: bool,
    pub draft: bool,
    pub author: String,
    /// A GitHub App (Dependabot, Renovate, ...): `user.type` is `Bot`, or the login ends in `[bot]`.
    pub author_is_bot: bool,
    pub head_sha: String,
    pub base_sha: String,
    pub base_ref: String,
    pub url: String,
    pub created_at: String,
    pub requested_reviewers: Vec<String>,
    pub requested_teams: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub id: i64,
    pub user: String,
    pub state: String,
    pub commit_id: String,
    pub submitted_at: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    pub id: i64,
    pub user: String,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
    /// Review comments only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThread {
    pub id: String,
    pub resolved: bool,
    pub thumbs_up: u32,
    pub thumbs_down: u32,
    pub replies: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewCommentInput {
    pub path: String,
    pub line: i64,
    pub side: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_side: Option<String>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewInput {
    pub commit_id: String,
    /// `APPROVE`, `REQUEST_CHANGES` or `COMMENT`.
    pub event: String,
    pub body: String,
    pub comments: Vec<ReviewCommentInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifiedPull {
    #[serde(flatten)]
    pub pull: PullRef,
    pub reason: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationPage {
    pub not_modified: bool,
    pub pulls: Vec<NotifiedPull>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Seconds GitHub asks to wait between polls (`X-Poll-Interval`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_interval: Option<crate::js::Num>,
}

#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
    pub allow: Vec<u16>,
}

pub type EnvSource = Arc<dyn Fn() -> Result<Env, Error> + Send + Sync>;
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The wall clock in epoch milliseconds (`Date.now`).
pub fn system_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

/// One account's view of GitHub.
#[derive(Clone)]
pub struct GitHub {
    pub account: Account,
    runner: Arc<dyn Runner>,
    env: EnvSource,
    now: Clock,
}

const THREADS_QUERY: &str = "query($owner:String!,$repo:String!,$number:Int!,$after:String){repository(owner:$owner,name:$repo){pullRequest(number:$number){reviewThreads(first:100,after:$after){pageInfo{hasNextPage endCursor}nodes{id isResolved comments(first:1){totalCount nodes{databaseId reactionGroups{content reactors{totalCount}}}}}}}}}";
const RESOLVE_MUTATION: &str =
    "mutation($threadId:ID!){resolveReviewThread(input:{threadId:$threadId}){thread{id isResolved}}}";

impl GitHub {
    pub fn new(runner: Arc<dyn Runner>, account: Account, env: EnvSource, now: Option<Clock>) -> Self {
        Self {
            account,
            runner,
            env,
            now: now.unwrap_or_else(|| Arc::new(system_now)),
        }
    }

    fn base(r: &PullRef) -> String {
        format!("repos/{}/{}", r.owner, r.repo)
    }

    /// One API request. Fails with [`Error::RateLimit`] when GitHub says to wait, [`Error::GitHub`] otherwise.
    pub fn request(&self, method: &str, path: &str, options: &RequestOptions) -> Result<ApiResponse, Error> {
        let mut argv: Vec<String> = [
            "gh",
            "api",
            "-i",
            "--method",
            method,
            "--hostname",
            &self.account.host,
            path,
        ]
        .iter()
        .map(|part| part.to_string())
        .collect();
        for (name, value) in &options.headers {
            argv.push("-H".into());
            argv.push(format!("{name}: {value}"));
        }
        if options.body.is_some() {
            argv.push("--input".into());
            argv.push("-".into());
        }
        let run = RunOptions {
            env: (self.env)()?,
            input: options.body.as_ref().map(|body| body.to_string()),
            ..RunOptions::default()
        };
        let result = self.runner.run(&argv, &run);
        let parsed = parse_api_output(&result.stdout);
        if parsed.status == 0 {
            let detail = if result.stderr.is_empty() {
                &result.stdout
            } else {
                &result.stderr
            };
            return Err(Error::GitHub {
                message: format!("gh api {method} {path} failed: {}", head16(trim(detail), 300)),
                status: 0,
            });
        }
        let body = if trim(&parsed.text).is_empty() {
            None
        } else {
            serde_json::from_str::<Value>(&parsed.text).ok()
        };
        let response = ApiResponse {
            status: parsed.status,
            headers: parsed.headers,
            text: parsed.text,
            body,
        };
        if response.status < 400 || options.allow.contains(&response.status) {
            return Ok(response);
        }
        let message = Some(text(response.body.as_ref().and_then(|body| body.get("message"))))
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| format!("HTTP {}", response.status));
        if let Some(resume_at) = self.resume_at(response.status, &response.headers, &message) {
            return Err(Error::RateLimit {
                message: format!("GitHub rate limit: {message}"),
                resume_at,
            });
        }
        let full = format!("GitHub {method} {path}: {} {message}", response.status);
        Err(Error::GitHub {
            message: head16(&full, 400).to_string(),
            status: response.status,
        })
    }

    /// When a 403 or 429 is a rate limit: the time to resume at.
    fn resume_at(&self, status: u16, headers: &IndexMap<String, String>, message: &str) -> Option<f64> {
        if status != 403 && status != 429 {
            return None;
        }
        let now = (self.now)() as f64;
        let header = |name: &str| headers.get(name).map_or(f64::NAN, |value| js_number(value));
        let retry_after = header("retry-after");
        if retry_after.is_finite() && retry_after > 0.0 {
            return Some(now + retry_after * 1000.0);
        }
        if headers.get("x-ratelimit-remaining").map(String::as_str) == Some("0") {
            let reset = header("x-ratelimit-reset");
            if reset.is_finite() && reset > 0.0 {
                return Some((now + 1000.0).max(reset * 1000.0));
            }
        }
        // A secondary limit without headers: GitHub asks for at least a minute.
        let lower = message.to_lowercase();
        if status == 429 || lower.contains("rate limit") || lower.contains("abuse") {
            return Some(now + 60_000.0);
        }
        None
    }

    fn get(&self, path: &str) -> Result<ApiResponse, Error> {
        self.request("GET", path, &RequestOptions::default())
    }

    fn post(&self, path: &str, body: Value) -> Result<ApiResponse, Error> {
        self.request(
            "POST",
            path,
            &RequestOptions {
                body: Some(body),
                ..RequestOptions::default()
            },
        )
    }

    /// Every page of a list endpoint (100 per page, at most `max_pages`).
    pub fn list(&self, path: &str, max_pages: u32) -> Result<Vec<Value>, Error> {
        let mut out = Vec::new();
        let separator = if path.contains('?') { "&" } else { "?" };
        for page in 1..=max_pages {
            let response = self.get(&format!("{path}{separator}per_page=100&page={page}"))?;
            let found = items(response.body.as_ref());
            let full = found.len() >= 100;
            out.extend(found);
            if !full {
                break;
            }
        }
        Ok(out)
    }

    /// Notifications about pull requests this account was asked to review or was mentioned on.
    pub fn notifications(
        &self,
        since: &str,
        last_modified: Option<&str>,
        etag: Option<&str>,
    ) -> Result<NotificationPage, Error> {
        let mut headers = Vec::new();
        if let Some(etag) = etag.filter(|etag| !etag.is_empty()) {
            headers.push(("If-None-Match".to_string(), etag.to_string()));
        } else if let Some(last) = last_modified.filter(|last| !last.is_empty()) {
            headers.push(("If-Modified-Since".to_string(), last.to_string()));
        }
        let response = self.request(
            "GET",
            &format!(
                "notifications?all=true&per_page=50&since={}",
                encode_uri_component(since)
            ),
            &RequestOptions {
                headers,
                allow: vec![304],
                ..RequestOptions::default()
            },
        )?;
        let interval = response
            .headers
            .get("x-poll-interval")
            .map_or(f64::NAN, |v| js_number(v));
        let header = |name: &str| response.headers.get(name).filter(|value| !value.is_empty()).cloned();
        let poll_interval = (interval.is_finite() && interval > 0.0).then_some(crate::js::Num(interval));
        if response.status == 304 {
            return Ok(NotificationPage {
                not_modified: true,
                pulls: Vec::new(),
                last_modified: header("last-modified").or(last_modified.filter(|v| !v.is_empty()).map(String::from)),
                etag: header("etag").or(etag.filter(|v| !v.is_empty()).map(String::from)),
                poll_interval,
            });
        }
        let mut pulls = Vec::new();
        for item in items(response.body.as_ref()) {
            let subject = item.get("subject");
            let reason = text(item.get("reason"));
            if text(subject.and_then(|s| s.get("type"))) != "PullRequest"
                || !NOTIFICATION_REASONS.contains(&reason.as_str())
            {
                continue;
            }
            if let Some(pull) = pull_ref_from_url(&text(subject.and_then(|s| s.get("url"))), &self.account.host) {
                pulls.push(NotifiedPull {
                    pull,
                    reason,
                    updated_at: text(item.get("updated_at")),
                });
            }
        }
        Ok(NotificationPage {
            not_modified: false,
            pulls,
            last_modified: header("last-modified"),
            etag: header("etag"),
            poll_interval,
        })
    }

    /// Pull requests matching a search query.
    pub fn search(&self, query: &str) -> Result<Vec<PullRef>, Error> {
        let response = self.get(&format!(
            "search/issues?per_page=50&sort=updated&order=desc&q={}",
            encode_uri_component(query)
        ))?;
        let mut out = Vec::new();
        for item in items(response.body.as_ref().and_then(|body| body.get("items"))) {
            let url = format!(
                "{}/pulls/{}",
                text(item.get("repository_url")),
                js_string(item.get("number"))
            );
            if let Some(found) = pull_ref_from_url(&url, &self.account.host)
                && item.get("pull_request").is_some()
            {
                out.push(found);
            }
        }
        Ok(out)
    }

    pub fn pull(&self, r: &PullRef) -> Result<PullRequest, Error> {
        let response = self.get(&format!("{}/pulls/{}", Self::base(r), r.number))?;
        let item = response.body.unwrap_or(json!({}));
        let head = item.get("head");
        let base = item.get("base");
        let author = login(item.get("user"));
        let bot = Regex::new(r"(?i)\[bot\]$").expect("static pattern");
        Ok(PullRequest {
            number: r.number,
            title: text(item.get("title")),
            body: text(item.get("body")),
            state: if item.get("state") == Some(&json!("open")) {
                "open"
            } else {
                "closed"
            }
            .into(),
            merged: item.get("merged") == Some(&Value::Bool(true))
                || matches!(item.get("merged_at"), Some(Value::String(_))),
            draft: item.get("draft") == Some(&Value::Bool(true)),
            author_is_bot: item.get("user").and_then(|user| user.get("type")) == Some(&json!("Bot"))
                || bot.is_match(&author),
            author,
            head_sha: text(head.and_then(|h| h.get("sha"))),
            base_sha: text(base.and_then(|b| b.get("sha"))),
            base_ref: text(base.and_then(|b| b.get("ref"))),
            url: text(item.get("html_url")),
            created_at: text(item.get("created_at")),
            requested_reviewers: items(item.get("requested_reviewers"))
                .iter()
                .map(|v| login(Some(v)))
                .collect(),
            requested_teams: items(item.get("requested_teams"))
                .iter()
                .map(|team| text(team.get("slug")))
                .collect(),
        })
    }

    /// The pull request's diff as GitHub computes it (merge base to head).
    pub fn pull_diff(&self, r: &PullRef) -> Result<String, Error> {
        let response = self.request(
            "GET",
            &format!("{}/pulls/{}", Self::base(r), r.number),
            &RequestOptions {
                headers: vec![("Accept".into(), "application/vnd.github.v3.diff".into())],
                ..RequestOptions::default()
            },
        )?;
        Ok(response.text)
    }

    pub fn reviews(&self, r: &PullRef) -> Result<Vec<Review>, Error> {
        let found = self.list(&format!("{}/pulls/{}/reviews", Self::base(r), r.number), 5)?;
        Ok(found
            .iter()
            .map(|item| Review {
                id: id_of(item.get("id")),
                user: login(item.get("user")),
                state: text(item.get("state")),
                commit_id: text(item.get("commit_id")),
                submitted_at: text(item.get("submitted_at")),
                body: text(item.get("body")),
            })
            .collect())
    }

    fn comment(item: &Value) -> Comment {
        let number = |key: &str| match item.get(key) {
            Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
            _ => None,
        };
        let created = text(item.get("created_at"));
        let updated = text(item.get("updated_at"));
        Comment {
            id: id_of(item.get("id")),
            user: login(item.get("user")),
            body: text(item.get("body")),
            updated_at: if updated.is_empty() { created.clone() } else { updated },
            created_at: created,
            path: match item.get("path") {
                Some(Value::String(path)) => Some(path.clone()),
                _ => None,
            },
            line: number("line").or_else(|| number("original_line")),
            review_id: number("pull_request_review_id"),
        }
    }

    /// Comments on the pull request's conversation.
    pub fn issue_comments(&self, r: &PullRef) -> Result<Vec<Comment>, Error> {
        let found = self.list(&format!("{}/issues/{}/comments", Self::base(r), r.number), 5)?;
        Ok(found.iter().map(Self::comment).collect())
    }

    /// Inline review comments on the pull request.
    pub fn review_comments(&self, r: &PullRef) -> Result<Vec<Comment>, Error> {
        let found = self.list(&format!("{}/pulls/{}/comments", Self::base(r), r.number), 5)?;
        Ok(found.iter().map(Self::comment).collect())
    }

    /// When this account was last asked to review the pull request (ISO time), from the issue's events.
    pub fn last_review_request_at(&self, r: &PullRef) -> Result<Option<String>, Error> {
        let events = self.list(&format!("{}/issues/{}/events", Self::base(r), r.number), 5)?;
        let me = self.account.login.to_lowercase();
        let mut latest: Option<String> = None;
        for event in events {
            if event.get("event") != Some(&json!("review_requested")) {
                continue;
            }
            if login(event.get("requested_reviewer")).to_lowercase() != me {
                continue;
            }
            let at = text(event.get("created_at"));
            if latest
                .as_ref()
                .is_none_or(|latest| crate::js::cmp16(&at, latest).is_gt())
            {
                latest = Some(at);
            }
        }
        Ok(latest)
    }

    /// Whether this account is an active member of a team of the repository's organization; `None` when the
    /// membership cannot be read (the token lacks `read:org`, or the team is not visible).
    pub fn in_team(&self, org: &str, slug: &str) -> Result<Option<bool>, Error> {
        let path = format!("orgs/{org}/teams/{slug}/memberships/{}", self.account.login);
        let options = RequestOptions {
            allow: vec![404],
            ..RequestOptions::default()
        };
        match self.request("GET", &path, &options) {
            Ok(response) if response.status == 404 => Ok(Some(false)),
            Ok(response) => Ok(Some(
                response.body.as_ref().and_then(|body| body.get("state")) == Some(&json!("active")),
            )),
            Err(error @ Error::RateLimit { .. }) => Err(error),
            Err(_) => Ok(None),
        }
    }

    /// Whether this account may push to the repository (false when that cannot be read).
    pub fn can_push(&self, r: &PullRef) -> Result<bool, Error> {
        match self.get(&Self::base(r)) {
            Ok(response) => {
                let permissions = response.body.as_ref().and_then(|body| body.get("permissions"));
                let set = |key: &str| permissions.and_then(|p| p.get(key)) == Some(&Value::Bool(true));
                Ok(set("push") || set("admin") || set("maintain"))
            }
            Err(error @ Error::RateLimit { .. }) => Err(error),
            Err(_) => Ok(false),
        }
    }

    /// One line on the head commit's CI checks, or `None` when there are none or they cannot be read.
    pub fn check_summary(&self, r: &PullRef, sha: &str) -> Result<Option<String>, Error> {
        let response = match self.get(&format!("{}/commits/{sha}/check-runs?per_page=100", Self::base(r))) {
            Ok(response) => response,
            Err(error @ Error::RateLimit { .. }) => return Err(error),
            Err(_) => return Ok(None),
        };
        let runs = items(response.body.as_ref().and_then(|body| body.get("check_runs")));
        if runs.is_empty() {
            return Ok(None);
        }
        let (mut passed, mut pending) = (0, 0);
        let mut failed: Vec<String> = Vec::new();
        for run in runs {
            if run.get("status") != Some(&json!("completed")) {
                pending += 1;
            } else if ["success", "neutral", "skipped"].contains(&text(run.get("conclusion")).as_str()) {
                passed += 1;
            } else {
                failed.push(text(run.get("name")));
            }
        }
        let mut parts = vec![format!("{passed} passed")];
        if !failed.is_empty() {
            let shown: Vec<&str> = failed.iter().take(5).map(String::as_str).collect();
            parts.push(format!("{} failed ({})", failed.len(), shown.join(", ")));
        }
        if pending > 0 {
            parts.push(format!("{pending} pending"));
        }
        Ok(Some(parts.join(", ")))
    }

    /// Post a comment on the pull request's conversation; returns its id.
    pub fn post_comment(&self, r: &PullRef, body: &str) -> Result<i64, Error> {
        let response = self.post(
            &format!("{}/issues/{}/comments", Self::base(r), r.number),
            json!({ "body": body }),
        )?;
        Ok(id_of(response.body.as_ref().and_then(|body| body.get("id"))))
    }

    /// Post a review in one request; returns its id.
    pub fn post_review(&self, r: &PullRef, review: &ReviewInput) -> Result<i64, Error> {
        let body = serde_json::to_value(review).unwrap_or(Value::Null);
        let response = self.post(&format!("{}/pulls/{}/reviews", Self::base(r), r.number), body)?;
        Ok(id_of(response.body.as_ref().and_then(|body| body.get("id"))))
    }

    /// The inline comments GitHub kept of a posted review.
    pub fn comments_of_review(&self, r: &PullRef, review_id: i64) -> Result<Vec<Comment>, Error> {
        let path = format!("{}/pulls/{}/reviews/{review_id}/comments", Self::base(r), r.number);
        Ok(self.list(&path, 5)?.iter().map(Self::comment).collect())
    }

    /// Post one inline comment outside a review; returns its id.
    pub fn post_review_comment(
        &self,
        r: &PullRef,
        commit_id: &str,
        comment: &ReviewCommentInput,
    ) -> Result<i64, Error> {
        let mut body = json!({ "commit_id": commit_id });
        if let (Value::Object(target), Ok(Value::Object(fields))) = (&mut body, serde_json::to_value(comment)) {
            target.extend(fields);
        }
        let response = self.post(&format!("{}/pulls/{}/comments", Self::base(r), r.number), body)?;
        Ok(id_of(response.body.as_ref().and_then(|body| body.get("id"))))
    }

    pub fn graphql(&self, query: &str, variables: Value) -> Result<Value, Error> {
        let response = self.post("graphql", json!({ "query": query, "variables": variables }))?;
        let body = response.body.unwrap_or(json!({}));
        if let Some(Value::Array(errors)) = body.get("errors")
            && let Some(first) = errors.first()
        {
            let message = format!("GitHub GraphQL: {}", text(first.get("message")));
            return Err(Error::GitHub {
                message: head16(&message, 300).to_string(),
                status: 200,
            });
        }
        Ok(match body.get("data") {
            None | Some(Value::Null) => json!({}),
            Some(data) => data.clone(),
        })
    }

    /// Review threads by the database id of their first comment: id, resolved, the reactions on that comment and
    /// the replies.
    pub fn review_threads(&self, r: &PullRef) -> Result<IndexMap<i64, ReviewThread>, Error> {
        let mut out = IndexMap::new();
        let mut after = Value::Null;
        for _ in 0..10 {
            let data = self.graphql(
                THREADS_QUERY,
                json!({ "owner": r.owner, "repo": r.repo, "number": r.number, "after": after }),
            )?;
            let threads = data
                .get("repository")
                .and_then(|repo| repo.get("pullRequest"))
                .and_then(|pull| pull.get("reviewThreads"))
                .filter(|threads| !threads.is_null())
                .cloned()
                .unwrap_or(json!({}));
            for node in items(threads.get("nodes")) {
                let comments = node
                    .get("comments")
                    .filter(|c| !c.is_null())
                    .cloned()
                    .unwrap_or(json!({}));
                let first = comments.get("nodes").and_then(|nodes| nodes.get(0)).cloned();
                let (Some(first), Some(Value::String(id))) = (first, node.get("id")) else {
                    continue;
                };
                let Some(Value::Number(database_id)) = first.get("databaseId") else {
                    continue;
                };
                let reactions = |content: &str| -> u32 {
                    items(first.get("reactionGroups"))
                        .iter()
                        .find(|group| group.get("content") == Some(&json!(content)))
                        .and_then(|group| group.get("reactors"))
                        .and_then(|reactors| reactors.get("totalCount"))
                        .and_then(Value::as_f64)
                        .map_or(0, |count| count as u32)
                };
                let replies = comments
                    .get("totalCount")
                    .and_then(Value::as_f64)
                    .map_or(0, |total| (total - 1.0).max(0.0) as u32);
                let key = database_id
                    .as_i64()
                    .unwrap_or_else(|| database_id.as_f64().unwrap_or(0.0) as i64);
                out.insert(
                    key,
                    ReviewThread {
                        id: id.clone(),
                        resolved: node.get("isResolved") == Some(&Value::Bool(true)),
                        thumbs_up: reactions("THUMBS_UP"),
                        thumbs_down: reactions("THUMBS_DOWN"),
                        replies,
                    },
                );
            }
            let page = threads.get("pageInfo");
            match (
                page.and_then(|p| p.get("hasNextPage")),
                page.and_then(|p| p.get("endCursor")),
            ) {
                (Some(Value::Bool(true)), Some(Value::String(cursor))) => after = json!(cursor),
                _ => break,
            }
        }
        Ok(out)
    }

    pub fn resolve_thread(&self, thread_id: &str) -> Result<(), Error> {
        self.graphql(RESOLVE_MUTATION, json!({ "threadId": thread_id }))?;
        Ok(())
    }
}
