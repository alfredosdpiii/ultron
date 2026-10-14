//! A fake GitHub for the autoreview tests (`test/ultron-autoreview-fixtures.ts`): an in-memory model of accounts,
//! pull requests, notifications and search behind the injected Runner (`gh auth`, `gh api -i`, and the `git`
//! commands of the checkout). Every call is recorded with its argv, environment and stdin. No process is run.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Value, json};

use review::accounts::{Account, TokenStore};
use review::checkout::CheckoutManager;
use review::github::{PullRef, iso_from_ms};
use review::ledger::parse_iso_ms;
use review::reviewer::{Candidate, ReviewEngine, ReviewerDeps};
use review::runner::{Env, RunOptions, RunResult, Runner};
use review::settings::{AutoreviewSettings, ModelFallbacks, autoreview_paths, resolve_config};
use review::state::StateStore;

pub const BOT: &str = "ultron-bot";
pub const DIFF: &str = "diff --git a/calc.py b/calc.py
index 1111111..2222222 100644
--- a/calc.py
+++ b/calc.py
@@ -1,6 +1,6 @@
 def total(items):
     \"\"\"Sum of item prices.\"\"\"
     result = 0
-    for i in range(len(items)):
+    for i in range(len(items) - 1):
         result += items[i][\"price\"]
     return result
";

pub fn head() -> String {
    "a".repeat(40)
}
pub fn old() -> String {
    "c".repeat(40)
}
pub fn new() -> String {
    "d".repeat(40)
}
pub fn reference() -> PullRef {
    PullRef::new("github.com", "o", "r", 1)
}

#[derive(Debug, Clone, Default)]
pub struct FakeComment {
    pub id: i64,
    pub user: String,
    pub body: String,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub pull_request_review_id: Option<i64>,
}

impl FakeComment {
    pub fn new(id: i64, user: &str, body: &str, created_at: &str) -> Self {
        Self {
            id,
            user: user.into(),
            body: body.into(),
            created_at: created_at.into(),
            ..Self::default()
        }
    }

    fn json(&self) -> Value {
        let mut item =
            json!({ "id": self.id, "user": { "login": self.user }, "body": self.body, "created_at": self.created_at });
        let map = item.as_object_mut().expect("object");
        if let Some(updated) = &self.updated_at {
            map.insert("updated_at".into(), json!(updated));
        }
        if let Some(path) = &self.path {
            map.insert("path".into(), json!(path));
        }
        if let Some(line) = self.line {
            map.insert("line".into(), json!(line));
        }
        if let Some(id) = self.pull_request_review_id {
            map.insert("pull_request_review_id".into(), json!(id));
        }
        item
    }
}

#[derive(Debug, Clone)]
pub struct FakeReview {
    pub id: i64,
    pub user: String,
    pub state: String,
    pub commit_id: String,
    pub submitted_at: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct FakePull {
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub merged: bool,
    pub author: String,
    pub head_sha: String,
    pub base_sha: String,
    pub base_ref: String,
    pub created_at: String,
    pub requested_reviewers: Vec<String>,
    pub requested_teams: Vec<String>,
    pub draft: bool,
    pub reviews: Vec<FakeReview>,
    pub issue_comments: Vec<FakeComment>,
    pub review_comments: Vec<FakeComment>,
    pub events: Vec<Value>,
    pub check_runs: Vec<Value>,
    pub diff: String,
    pub diffs: HashMap<String, String>,
    pub ancestors: Vec<String>,
    /// Review thread id by the id of its first comment, and which threads are resolved (in order).
    pub threads: IndexMap<i64, String>,
    pub resolved: Vec<String>,
    /// Thumbs up and down on a comment, by comment id.
    pub reactions: HashMap<i64, (u32, u32)>,
}

impl FakePull {
    pub fn new(owner: &str, repo: &str, number: u64) -> Self {
        Self {
            owner: owner.into(),
            repo: repo.into(),
            number,
            title: "Tweak total".into(),
            body: "Sums prices.".into(),
            state: "open".into(),
            merged: false,
            author: "alice".into(),
            head_sha: head(),
            base_sha: "b".repeat(40),
            base_ref: "main".into(),
            created_at: "2026-10-04T08:00:00Z".into(),
            requested_reviewers: Vec::new(),
            requested_teams: Vec::new(),
            draft: false,
            reviews: Vec::new(),
            issue_comments: Vec::new(),
            review_comments: Vec::new(),
            events: Vec::new(),
            check_runs: Vec::new(),
            diff: DIFF.into(),
            diffs: HashMap::new(),
            ancestors: Vec::new(),
            threads: IndexMap::new(),
            resolved: Vec::new(),
            reactions: HashMap::new(),
        }
    }

    pub fn requested(mut self, logins: &[&str]) -> Self {
        self.requested_reviewers = logins.iter().map(|l| l.to_string()).collect();
        self
    }

    fn json(&self) -> Value {
        json!({
            "number": self.number, "title": self.title, "body": self.body, "state": self.state, "merged": self.merged,
            "draft": self.draft,
            "user": { "login": self.author, "type": if self.author.ends_with("[bot]") { "Bot" } else { "User" } },
            "head": { "sha": self.head_sha }, "base": { "sha": self.base_sha, "ref": self.base_ref },
            "html_url": format!("https://github.com/{}/{}/pull/{}", self.owner, self.repo, self.number),
            "created_at": self.created_at,
            "requested_reviewers": self.requested_reviewers.iter().map(|login| json!({ "login": login })).collect::<Vec<_>>(),
            "requested_teams": self.requested_teams.iter().map(|slug| json!({ "slug": slug })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Call {
    pub argv: Vec<String>,
    pub env: Env,
    pub input: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Failure {
    pub matches: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub message: String,
    pub times: u32,
}

#[derive(Debug, Clone)]
pub struct ApiCall {
    pub method: String,
    pub path: String,
    pub body: Option<Value>,
    pub env: Env,
}

pub struct Notification {
    pub reason: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
    /// The subject type (`PullRequest` when absent).
    pub kind: Option<String>,
    /// `updated_at` (the hub's time when absent).
    pub updated_at: Option<String>,
}

impl Notification {
    /// A notification about `o/r#<number>`.
    pub fn new(reason: &str, number: u64) -> Self {
        Self {
            reason: reason.into(),
            owner: "o".into(),
            repo: "r".into(),
            number,
            kind: None,
            updated_at: None,
        }
    }
}

type KeepComment = Box<dyn Fn(&str, i64, &str) -> bool + Send>;

/// A search query substring and the pull requests (owner, repo, number) it finds.
pub type SearchEntry = (String, Vec<(String, String, u64)>);

pub struct HubState {
    pub accounts: Vec<(String, String, String, bool)>,
    pub pulls: IndexMap<String, FakePull>,
    pub notifications: Vec<Notification>,
    pub remotes: HashMap<String, String>,
    pub can_push: bool,
    pub teams: HashMap<String, Vec<String>>,
    pub calls: Vec<Call>,
    pub now: i64,
    pub failures: Vec<Failure>,
    pub clone_fails: bool,
    pub keep_comment: Option<KeepComment>,
    /// Search results by query substring (`review-requested:`, `mentions:`), first match wins.
    pub search: Vec<SearchEntry>,
    /// `X-Poll-Interval` on notification replies.
    pub poll_interval: Option<u64>,
    /// Answer a conditional notifications request with 304.
    pub not_modified: bool,
    ids: i64,
}

pub struct FakeHub {
    state: Mutex<HubState>,
}

const REVIEW_STATES: [(&str, &str); 3] = [
    ("APPROVE", "APPROVED"),
    ("REQUEST_CHANGES", "CHANGES_REQUESTED"),
    ("COMMENT", "COMMENTED"),
];

fn respond(status: u16, body: Option<Value>, headers: &[(String, String)]) -> RunResult {
    let mut head = vec![format!(
        "HTTP/2.0 {status} {}",
        if status < 400 { "OK" } else { "Error" }
    )];
    head.extend(headers.iter().map(|(name, value)| format!("{name}: {value}")));
    let text = match body {
        Some(Value::String(text)) => text,
        Some(body) => body.to_string(),
        None => String::new(),
    };
    RunResult {
        code: if status < 400 { 0 } else { 1 },
        stdout: format!("{}\r\n\r\n{text}", head.join("\r\n")),
        stderr: if status < 400 {
            String::new()
        } else {
            format!("gh: HTTP {status}")
        },
    }
}

impl FakeHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(HubState {
                accounts: Vec::new(),
                pulls: IndexMap::new(),
                notifications: Vec::new(),
                remotes: HashMap::new(),
                can_push: false,
                teams: HashMap::new(),
                calls: Vec::new(),
                now: parse_iso_ms("2026-10-04T10:00:00Z").expect("date"),
                failures: Vec::new(),
                clone_fails: false,
                keep_comment: None,
                search: Vec::new(),
                poll_interval: None,
                not_modified: false,
                ids: 1000,
            }),
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, HubState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn now(&self) -> i64 {
        self.lock().now
    }

    pub fn advance(&self, ms: i64) {
        self.lock().now += ms;
    }

    /// Search results for queries containing `name`: pull requests `o/r#<n>`.
    pub fn set_search(&self, entries: &[(&str, &[u64])]) {
        self.lock().search = entries
            .iter()
            .map(|(name, numbers)| {
                (
                    name.to_string(),
                    numbers.iter().map(|n| ("o".to_string(), "r".to_string(), *n)).collect(),
                )
            })
            .collect();
    }

    /// Change the pull request `o/r#<number>`.
    pub fn with_pull_n<T>(&self, number: u64, change: impl FnOnce(&mut FakePull) -> T) -> T {
        let mut state = self.lock();
        change(state.pulls.get_mut(&format!("o/r#{number}")).expect("pull"))
    }

    pub fn set_now(&self, iso: &str) {
        self.lock().now = parse_iso_ms(iso).expect("date");
    }

    pub fn add_account(&self, login: &str, host: &str, active: bool) -> String {
        let token = format!(
            "ghp_{}SECRETTOKEN0123456789abcdef",
            login.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>()
        );
        self.lock()
            .accounts
            .push((login.into(), host.into(), token.clone(), active));
        token
    }

    pub fn add_pull(&self, pull: FakePull) {
        let key = format!("{}/{}#{}", pull.owner, pull.repo, pull.number);
        self.lock().pulls.insert(key, pull);
    }

    /// Change the pull request `o/r#1`.
    pub fn with_pull<T>(&self, change: impl FnOnce(&mut FakePull) -> T) -> T {
        let mut state = self.lock();
        change(state.pulls.get_mut("o/r#1").expect("pull o/r#1"))
    }

    pub fn pull(&self) -> FakePull {
        self.with_pull(|pull| pull.clone())
    }

    pub fn fail(&self, matches: &str, status: u16, headers: &[(&str, &str)], message: &str) {
        self.lock().failures.push(Failure {
            matches: matches.into(),
            status,
            headers: headers.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect(),
            message: message.into(),
            times: 1,
        });
    }

    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    /// API calls (method and path) recorded so far whose `METHOD path` matches.
    pub fn api(&self, pattern: &str) -> Vec<ApiCall> {
        let pattern = Regex::new(pattern).expect("pattern");
        let mut out = Vec::new();
        for call in self.calls() {
            if call.argv.first().map(String::as_str) != Some("gh")
                || call.argv.get(1).map(String::as_str) != Some("api")
            {
                continue;
            }
            let at = |flag: &str, offset: usize| {
                let index = call.argv.iter().position(|arg| arg == flag).expect("flag");
                call.argv[index + offset].clone()
            };
            let (method, path) = (at("--method", 1), at("--hostname", 2));
            if !pattern.is_match(&format!("{method} {path}")) {
                continue;
            }
            out.push(ApiCall {
                method,
                path,
                body: call
                    .input
                    .as_ref()
                    .map(|input| serde_json::from_str(input).expect("json input")),
                env: call.env.clone(),
            });
        }
        out
    }

    fn auth(state: &HubState, argv: &[String]) -> RunResult {
        let ok = |stdout: String| RunResult {
            code: 0,
            stdout,
            stderr: String::new(),
        };
        match argv.get(2).map(String::as_str) {
            Some("status") => {
                let mut hosts: IndexMap<String, Vec<Value>> = IndexMap::new();
                for (login, host, _, active) in &state.accounts {
                    hosts
                        .entry(host.clone())
                        .or_default()
                        .push(json!({ "login": login, "host": host, "state": "success", "active": active }));
                }
                ok(json!({ "hosts": hosts }).to_string())
            }
            Some("token") => {
                let after = |flag: &str| {
                    argv.iter()
                        .position(|a| a == flag)
                        .and_then(|i| argv.get(i + 1))
                        .cloned()
                };
                let (login, host) = (after("--user"), after("--hostname"));
                match state
                    .accounts
                    .iter()
                    .find(|a| Some(&a.0) == login.as_ref() && Some(&a.1) == host.as_ref())
                {
                    Some(account) => ok(format!("{}\n", account.2)),
                    None => RunResult {
                        code: 1,
                        stdout: String::new(),
                        stderr: "no token".into(),
                    },
                }
            }
            _ => RunResult {
                code: 1,
                stdout: String::new(),
                stderr: "unknown gh auth command".into(),
            },
        }
    }

    fn api_call(state: &mut HubState, argv: &[String], options: &RunOptions) -> RunResult {
        let method = argv[argv.iter().position(|a| a == "--method").expect("method") + 1].clone();
        let full = argv[argv.iter().position(|a| a == "--hostname").expect("hostname") + 2].clone();
        let (path, query) = full.split_once('?').unwrap_or((&full, ""));
        let mut headers: HashMap<String, String> = HashMap::new();
        for (index, arg) in argv.iter().enumerate() {
            if arg == "-H" {
                let (name, rest) = argv[index + 1].split_once(':').unwrap_or((&argv[index + 1], ""));
                headers.insert(name.to_lowercase(), rest.trim().to_string());
            }
        }
        let token = options.env.get("GH_TOKEN").or(options.env.get("GH_ENTERPRISE_TOKEN"));
        let Some(account) = state.accounts.iter().find(|a| Some(&a.2) == token).map(|a| a.0.clone()) else {
            return respond(401, Some(json!({ "message": "Bad credentials" })), &[]);
        };
        let probe = format!("{method} {path}");
        if let Some(failure) = state
            .failures
            .iter_mut()
            .find(|f| f.times > 0 && probe.contains(&f.matches))
        {
            failure.times -= 1;
            return respond(
                failure.status,
                Some(json!({ "message": failure.message })),
                &failure.headers,
            );
        }
        let body: Value = options
            .input
            .as_ref()
            .map_or(json!({}), |input| serde_json::from_str(input).expect("json input"));
        let page: u32 = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("page="))
            .and_then(|page| page.parse().ok())
            .unwrap_or(1);
        let list =
            |items: Vec<Value>| respond(200, Some(Value::Array(if page == 1 { items } else { Vec::new() })), &[]);
        let stamp = iso_from_ms(state.now);

        if path == "notifications" {
            let mut extra = vec![("Last-Modified".to_string(), "Sun, 04 Oct 2026 10:00:00 GMT".to_string())];
            if let Some(interval) = state.poll_interval {
                extra.push(("X-Poll-Interval".into(), interval.to_string()));
            }
            if state.not_modified && headers.contains_key("if-modified-since") {
                return respond(304, None, &extra);
            }
            let items: Vec<Value> = state
                .notifications
                .iter()
                .map(|n| {
                    json!({ "reason": n.reason, "updated_at": n.updated_at.clone().unwrap_or_else(|| stamp.clone()),
                        "subject": { "type": n.kind.clone().unwrap_or_else(|| "PullRequest".into()),
                        "url": format!("https://api.github.com/repos/{}/{}/pulls/{}", n.owner, n.repo, n.number) },
                        "repository": { "full_name": format!("{}/{}", n.owner, n.repo) } })
                })
                .collect();
            return respond(200, Some(Value::Array(items)), &extra);
        }
        if path == "search/issues" {
            let q = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("q="))
                .map(percent_decode)
                .unwrap_or_default();
            let items: Vec<Value> = state
                .search
                .iter()
                .find(|(name, _)| q.contains(name.as_str()))
                .map_or_else(Vec::new, |(_, found)| {
                    found
                        .iter()
                        .map(|(owner, repo, number)| {
                            json!({ "number": number,
                                "repository_url": format!("https://api.github.com/repos/{owner}/{repo}"),
                                "pull_request": {} })
                        })
                        .collect()
                });
            return respond(200, Some(json!({ "items": items })), &[]);
        }
        if path == "graphql" {
            let variables = body.get("variables").cloned().unwrap_or(json!({}));
            if body
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("")
                .contains("resolveReviewThread")
            {
                let id = variables
                    .get("threadId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                for pull in state.pulls.values_mut() {
                    if pull.threads.values().any(|t| *t == id) && !pull.resolved.contains(&id) {
                        pull.resolved.push(id.clone());
                    }
                }
                return respond(
                    200,
                    Some(json!({ "data": { "resolveReviewThread": { "thread": { "id": id, "isResolved": true } } } })),
                    &[],
                );
            }
            let key = format!(
                "{}/{}#{}",
                variables["owner"].as_str().unwrap_or(""),
                variables["repo"].as_str().unwrap_or(""),
                variables["number"]
            );
            let nodes: Vec<Value> = state.pulls.get(&key).map_or_else(Vec::new, |pull| {
                pull.threads
                    .iter()
                    .map(|(comment, id)| {
                        let (up, down) = pull.reactions.get(comment).copied().unwrap_or((0, 0));
                        json!({ "id": id, "isResolved": pull.resolved.contains(id), "comments": { "totalCount": 1,
                            "nodes": [{ "databaseId": comment, "reactionGroups": [
                                { "content": "THUMBS_UP", "reactors": { "totalCount": up } },
                                { "content": "THUMBS_DOWN", "reactors": { "totalCount": down } }] }] } })
                    })
                    .collect()
            });
            return respond(
                200,
                Some(json!({ "data": { "repository": { "pullRequest": { "reviewThreads": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": nodes } } } } })),
                &[],
            );
        }
        let repository = Regex::new(r"^repos/([^/]+)/([^/]+)$").expect("pattern");
        if let Some(found) = repository.captures(path) {
            return respond(
                200,
                Some(json!({ "full_name": format!("{}/{}", &found[1], &found[2]),
                    "permissions": { "pull": true, "push": state.can_push, "admin": false } })),
                &[],
            );
        }
        let membership = Regex::new(r"^orgs/([^/]+)/teams/([^/]+)/memberships/([^/]+)$").expect("pattern");
        if let Some(found) = membership.captures(path) {
            return match state.teams.get(&format!("{}/{}", &found[1], &found[2])) {
                None => respond(
                    403,
                    Some(json!({ "message": "Must have admin rights or read:org" })),
                    &[],
                ),
                Some(members) if members.iter().any(|m| *m == found[3]) => {
                    respond(200, Some(json!({ "state": "active", "role": "member" })), &[])
                }
                Some(_) => respond(404, Some(json!({ "message": "Not Found" })), &[]),
            };
        }
        let resource =
            Regex::new(r"^repos/([^/]+)/([^/]+)/(pulls|issues|commits)/([^/]+)(?:/(.*))?$").expect("pattern");
        let Some(found) = resource.captures(path) else {
            return respond(404, Some(json!({ "message": "Not Found" })), &[]);
        };
        let (owner, repo, kind, id) = (&found[1], &found[2], &found[3], &found[4]);
        let rest = found.get(5).map_or("", |m| m.as_str());
        if kind == "commits" {
            let runs = state
                .pulls
                .values()
                .find(|p| p.owner == owner && p.repo == repo && p.head_sha == id)
                .map_or_else(Vec::new, |p| p.check_runs.clone());
            return respond(200, Some(json!({ "check_runs": runs })), &[]);
        }
        let key = format!("{owner}/{repo}#{id}");
        if !state.pulls.contains_key(&key) {
            return respond(404, Some(json!({ "message": "Not Found" })), &[]);
        }
        let mut ids = state.ids;
        let keep = state.keep_comment.take();
        let can_keep = |path: &str, line: i64, body: &str| keep.as_ref().is_none_or(|keep| keep(path, line, body));
        let pull = state.pulls.get_mut(&key).expect("pull");
        let result = if kind == "issues" {
            match (rest, method.as_str()) {
                ("events", _) => list(pull.events.clone()),
                ("comments", "GET") => list(pull.issue_comments.iter().map(FakeComment::json).collect()),
                ("comments", "POST") => {
                    ids += 1;
                    let new_id = ids;
                    pull.issue_comments.push(FakeComment::new(
                        new_id,
                        &account,
                        body["body"].as_str().unwrap_or(""),
                        &stamp,
                    ));
                    respond(201, Some(json!({ "id": new_id })), &[])
                }
                _ => respond(404, Some(json!({ "message": "Not Found" })), &[]),
            }
        } else if rest.is_empty() && method == "GET" {
            if headers.get("accept").is_some_and(|accept| accept.contains("diff")) {
                respond(200, Some(Value::String(pull.diff.clone())), &[])
            } else {
                respond(200, Some(pull.json()), &[])
            }
        } else if rest == "reviews" && method == "GET" {
            list(
                pull.reviews
                    .iter()
                    .map(|r| {
                        json!({ "id": r.id, "user": { "login": r.user }, "state": r.state, "commit_id": r.commit_id,
                        "submitted_at": r.submitted_at, "body": r.body })
                    })
                    .collect(),
            )
        } else if rest == "reviews" && method == "POST" {
            let event = body["event"].as_str().unwrap_or("").to_string();
            ids += 1;
            let review_id = ids;
            pull.reviews.push(FakeReview {
                id: review_id,
                user: account.clone(),
                state: REVIEW_STATES
                    .iter()
                    .find(|(e, _)| *e == event)
                    .map_or(event.clone(), |(_, s)| s.to_string()),
                commit_id: body["commit_id"].as_str().unwrap_or("").into(),
                submitted_at: stamp.clone(),
                body: body["body"].as_str().unwrap_or("").into(),
            });
            pull.requested_reviewers.retain(|login| *login != account);
            for item in body["comments"].as_array().cloned().unwrap_or_default() {
                let (path, line, text) = (
                    item["path"].as_str().unwrap_or(""),
                    item["line"].as_i64().unwrap_or(0),
                    item["body"].as_str().unwrap_or(""),
                );
                if !can_keep(path, line, text) {
                    continue;
                }
                ids += 1;
                let next = ids;
                pull.review_comments.push(FakeComment {
                    id: next,
                    user: account.clone(),
                    body: text.into(),
                    created_at: stamp.clone(),
                    path: Some(path.into()),
                    line: Some(line),
                    pull_request_review_id: Some(review_id),
                    updated_at: None,
                });
                pull.threads.insert(next, format!("PRRT_{next}"));
            }
            respond(200, Some(json!({ "id": review_id })), &[])
        } else if let Some(review) = rest.strip_prefix("reviews/").and_then(|r| r.strip_suffix("/comments")) {
            let review: i64 = review.parse().unwrap_or(-1);
            list(
                pull.review_comments
                    .iter()
                    .filter(|c| c.pull_request_review_id == Some(review))
                    .map(FakeComment::json)
                    .collect(),
            )
        } else if rest == "comments" && method == "GET" {
            list(pull.review_comments.iter().map(FakeComment::json).collect())
        } else if rest == "comments" && method == "POST" {
            ids += 1;
            let new_id = ids;
            pull.review_comments.push(FakeComment {
                id: new_id,
                user: account.clone(),
                body: body["body"].as_str().unwrap_or("").into(),
                created_at: stamp.clone(),
                path: body["path"].as_str().map(String::from),
                line: body["line"].as_i64(),
                ..FakeComment::default()
            });
            pull.threads.insert(new_id, format!("PRRT_{new_id}"));
            respond(201, Some(json!({ "id": new_id })), &[])
        } else {
            respond(404, Some(json!({ "message": "Not Found" })), &[])
        };
        state.ids = ids;
        state.keep_comment = keep;
        result
    }

    fn git(state: &HubState, argv: &[String]) -> RunResult {
        let ok = |stdout: String| RunResult {
            code: 0,
            stdout,
            stderr: String::new(),
        };
        let mut index = 1;
        while matches!(argv.get(index).map(String::as_str), Some("-c" | "-C")) {
            index += 2;
        }
        let Some(command) = argv.get(index) else {
            return ok(String::new());
        };
        let args = &argv[index + 1..];
        let pull_of = |number: Option<&str>| {
            state
                .pulls
                .values()
                .find(|p| Some(p.number.to_string().as_str()) == number)
        };
        match command.as_str() {
            "config" => ok(state.remotes.get(&argv[2]).cloned().unwrap_or_default()),
            "clone" => {
                if state.clone_fails {
                    return RunResult {
                        code: 128,
                        stdout: String::new(),
                        stderr: "fatal: repository not found".into(),
                    };
                }
                let target = args.last().expect("target");
                std::fs::create_dir_all(target).expect("clone dir");
                std::fs::write(Path::new(target).join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
                ok(String::new())
            }
            "rev-parse" => {
                let number = args.first().and_then(|a| a.rsplit_once("pull/")).map(|(_, n)| n);
                ok(format!("{}\n", pull_of(number).map_or("", |p| p.head_sha.as_str())))
            }
            "merge-base" if args.first().map(String::as_str) == Some("--is-ancestor") => {
                let pull = state.pulls.values().find(|p| p.head_sha == args[2]);
                if pull.is_some_and(|p| p.ancestors.contains(&args[1])) {
                    ok(String::new())
                } else {
                    RunResult {
                        code: 1,
                        stdout: String::new(),
                        stderr: String::new(),
                    }
                }
            }
            "merge-base" => ok(format!(
                "{}\n",
                state
                    .pulls
                    .values()
                    .find(|p| p.head_sha == args[1])
                    .map_or("", |p| p.base_sha.as_str())
            )),
            "worktree" => {
                if args.first().map(String::as_str) == Some("add") {
                    std::fs::create_dir_all(&args[args.len() - 2]).expect("worktree");
                }
                ok(String::new())
            }
            "diff" => {
                let sha = Regex::new("^[0-9a-f]{40}$").expect("pattern");
                let shas: Vec<&String> = args.iter().filter(|a| sha.is_match(a)).collect();
                let (from, to) = (shas[0], shas[1]);
                let pull = state.pulls.values().find(|p| p.head_sha == *to);
                ok(pull.map_or(String::new(), |p| {
                    p.diffs
                        .get(&format!("{from}..{to}"))
                        .cloned()
                        .unwrap_or_else(|| p.diff.clone())
                }))
            }
            _ => ok(String::new()),
        }
    }
}

impl Runner for FakeHub {
    fn run(&self, argv: &[String], options: &RunOptions) -> RunResult {
        let mut state = self.lock();
        state.calls.push(Call {
            argv: argv.to_vec(),
            env: options.env.clone(),
            input: options.input.clone(),
        });
        match (argv.first().map(String::as_str), argv.get(1).map(String::as_str)) {
            (Some("gh"), Some("auth")) => Self::auth(&state, argv),
            (Some("gh"), Some("api")) => Self::api_call(&mut state, argv, options),
            (Some("git"), _) => Self::git(&state, argv),
            _ => RunResult {
                code: 127,
                stdout: String::new(),
                stderr: format!("{}: not found", argv.first().map_or("", String::as_str)),
            },
        }
    }
}

/// `decodeURIComponent` (and `+` as a space, as URLSearchParams reads a query).
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 3 <= bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The TypeScript `engineResult(partial)`.
pub fn engine_result(partial: Value) -> Value {
    let mut base = json!({
        "complete": true, "label": "o/r#1", "files": 1, "added": 1, "removed": 1, "findings": [], "alsoRaised": [],
        "earlier": [], "dropped": { "rejected": 0, "duplicates": 0 },
        "timing": { "totalMs": 42000, "scopeMs": 100, "findMs": 30000, "verifyMs": 11000 },
        "usage": { "inputTokens": 90000, "outputTokens": 4000, "costUsd": 0.31, "frames": 12, "tokens": 94000, "budget": 300000 },
        "model": "stub/frames", "verifyModel": "stub/frames", "notChecked": [], "incomplete": [],
        "diffLines": { "calc.py": [[1, 6]] },
    });
    if let (Value::Object(base), Value::Object(partial)) = (&mut base, partial) {
        base.extend(partial);
    }
    base
}

pub fn major() -> Value {
    json!({ "file": "calc.py", "line": 4, "severity": "major", "category": "correctness",
        "claim": "total() skips the last item.", "why": "range(len(items) - 1) stops one short.",
        "suggestedFix": "Use range(len(items)).", "replacement": "    for i in range(len(items)):",
        "verification": "confirmed", "confidence": 0.9 })
}

/// `{ ...MAJOR, ...extra }`.
pub fn major_with(extra: Value) -> Value {
    let mut finding = major();
    if let (Value::Object(finding), Value::Object(extra)) = (&mut finding, extra) {
        for (key, value) in extra {
            if value.is_null() {
                finding.remove(&key);
            } else {
                finding.insert(key, value);
            }
        }
    }
    finding
}

type Hook = Box<dyn Fn() + Send + Sync>;

/// A scripted engine: each review takes the next result (the last one repeats) and records its spec, with the
/// diff and earlier diff it was given read back.
pub struct FakeEngine {
    pub specs: Mutex<Vec<Value>>,
    pub results: Mutex<Vec<Result<Value, String>>>,
    pub on_review: Mutex<Option<Hook>>,
}

impl FakeEngine {
    pub fn new(results: Vec<Result<Value, String>>) -> Arc<Self> {
        Arc::new(Self {
            specs: Mutex::new(Vec::new()),
            results: Mutex::new(if results.is_empty() {
                vec![Ok(engine_result(json!({})))]
            } else {
                results
            }),
            on_review: Mutex::new(None),
        })
    }

    pub fn specs(&self) -> Vec<Value> {
        self.specs.lock().expect("specs").clone()
    }
}

impl ReviewEngine for FakeEngine {
    fn review(&self, spec: &Value, _signal: Option<&kernel::Signal>) -> Result<Value, String> {
        let mut recorded = spec.clone();
        let read = |key: &str| {
            spec.get(key)
                .and_then(Value::as_str)
                .map(|p| std::fs::read_to_string(p).expect("diff"))
        };
        recorded["diff"] = json!(read("diffPath").unwrap_or_default());
        if let Some(earlier) = read("earlierDiffPath") {
            recorded["earlierDiff"] = json!(earlier);
        }
        self.specs.lock().expect("specs").push(recorded);
        if let Some(hook) = self.on_review.lock().expect("hook").as_ref() {
            hook();
        }
        let mut results = self.results.lock().expect("results");
        if results.len() > 1 {
            results.remove(0)
        } else {
            results[0].clone()
        }
    }
}

pub struct Context {
    pub dir: tempfile::TempDir,
    pub hub: Arc<FakeHub>,
    pub token: String,
    pub engine: Arc<FakeEngine>,
    pub deps: ReviewerDeps,
    pub logs: Arc<Mutex<Vec<String>>>,
    pub account: Account,
}

impl Context {
    pub fn candidate(&self, reasons: &[&str]) -> Candidate {
        Candidate {
            account: self.account.clone(),
            pull: reference(),
            reasons: reasons.iter().map(|r| r.to_string()).collect(),
            picked_at: self.hub.now() - 30_000,
            notified_at: None,
        }
    }

    pub fn requested(&self) -> Candidate {
        self.candidate(&["review_requested"])
    }

    pub fn logs(&self) -> String {
        self.logs.lock().expect("logs").join("\n")
    }

    pub fn pull_state(&self) -> review::state::PullState {
        let key = review::reviewer::pull_state_key(&self.account, &reference());
        self.deps.store.read().pulls.get(&key).cloned().expect("pull state")
    }
}

/// The recorded Ultron logo (`fixtures/settings.json`).
pub fn logo() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/settings.json");
    let settings: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("settings.json")).expect("json");
    settings["logo"].as_str().expect("logo").to_string()
}

/// The TypeScript test's `setup()`: a fake hub with the reviewing account, a scripted engine, state in a temp dir.
pub fn setup(settings: Value, engine: Option<Arc<FakeEngine>>) -> Context {
    let dir = tempfile::tempdir().expect("tempdir");
    let hub = FakeHub::new();
    let token = hub.add_account(BOT, "github.com", true);
    let engine = engine.unwrap_or_else(|| FakeEngine::new(Vec::new()));
    let root = dir.path().to_string_lossy().into_owned();
    let mut env = HashMap::new();
    env.insert("ULTRON_AUTOREVIEW_CACHE_DIR".to_string(), format!("{root}/cache"));
    let paths = autoreview_paths(&format!("{root}/agent"), &env, "/home/u");
    let mut merged = json!({ "ackArt": "none" });
    if let (Value::Object(merged), Value::Object(settings)) = (&mut merged, settings) {
        for (key, value) in settings {
            if value.is_null() {
                merged.remove(&key);
            } else {
                merged.insert(key, value);
            }
        }
    }
    let settings: AutoreviewSettings = serde_json::from_value(merged).expect("settings");
    let config = resolve_config(&settings, &ModelFallbacks::default(), &logo());
    let logs = Arc::new(Mutex::new(Vec::new()));
    let sink = logs.clone();
    let clock = hub.clone();
    let runner: Arc<dyn Runner> = hub.clone();
    let deps = ReviewerDeps {
        runner: runner.clone(),
        tokens: Arc::new(TokenStore::new(runner.clone())),
        engine: engine.clone(),
        store: Arc::new(StateStore::new(&paths.state)),
        checkouts: Arc::new(CheckoutManager::new(runner, &paths.cache)),
        config,
        paths,
        log: Arc::new(move |line: &str| sink.lock().expect("logs").push(line.into())),
        now: Arc::new(move || clock.now()),
        random: Arc::new(|| 0.0),
        lessons: None,
        environments: None,
        home: "/home/u".into(),
    };
    Context {
        dir,
        hub,
        token,
        engine,
        deps,
        logs,
        account: Account::new(BOT, "github.com"),
    }
}
