//! Replays `fixtures/github.json` (recorded by `fixtures/generate_github.mjs` from the TypeScript autoreview
//! modules): accounts, `gh api -i` parsing, pull request references, the GitHub client over scripted replies,
//! local checkouts and guide mentions, mentions, the decision to review, the acknowledgement, and claim hashes.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use review::accounts::{Account, auth_env, parse_auth_status};
use review::github::{
    Error, GitHub, PullRef, RequestOptions, parse_api_output, parse_pull_target, pull_key, pull_ref_from_url,
};
use review::local::{expand_path, guide_mentions, remote_matches, without_guide_mentions};
use review::plan::{EngineResult, PlanOptions, plan_review};
use review::reviewer::{
    DecisionInput, MAX_ACK_CHARS, ack_body, decide, latest_mention, mentions, pick_ack_line, pull_state_key, repo_key,
};
use review::runner::{RunOptions, RunResult, Runner};
use review::state::claim_hash;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/github.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("github.json")).expect("json")
}

fn json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("json")
}

fn s(value: &Value) -> &str {
    value.as_str().expect("string")
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("array")
        .iter()
        .map(|v| s(v).to_string())
        .collect()
}

fn pull_ref(value: &Value) -> PullRef {
    serde_json::from_value::<PullRefJson>(value.clone())
        .expect("ref")
        .into()
}

#[derive(serde::Deserialize)]
struct PullRefJson {
    host: String,
    owner: String,
    repo: String,
    number: u64,
}

impl From<PullRefJson> for PullRef {
    fn from(r: PullRefJson) -> Self {
        PullRef::new(&r.host, &r.owner, &r.repo, r.number)
    }
}

/// Numbers compared by value (an f64 and an integer that JSON wrote alike are equal).
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

#[test]
fn review_github_parse_auth_status_matches_typescript() {
    let data = fixture();
    for case in data["auth"].as_array().expect("auth") {
        let accounts = parse_auth_status(s(&case["stdout"]), s(&case["stderr"]));
        assert_eq!(json(&accounts), case["accounts"], "{case}");
    }
    for case in data["authEnv"].as_array().expect("authEnv") {
        let env = auth_env(&Account::new(s(&case["login"]), s(&case["host"])), "tok");
        assert_eq!(json(&env), case["env"], "{case}");
    }
}

#[test]
fn review_github_parse_api_output_matches_typescript() {
    for case in fixture()["apiOutputs"].as_array().expect("apiOutputs") {
        assert_eq!(json(&parse_api_output(s(&case["stdout"]))), case["parsed"], "{case}");
    }
}

#[test]
fn review_github_pull_references_match_typescript() {
    let data = fixture();
    for case in data["urls"].as_array().expect("urls") {
        let found = pull_ref_from_url(s(&case["url"]), s(&case["host"]));
        assert_eq!(json(&found), case["ref"], "{case}");
    }
    for case in data["targets"].as_array().expect("targets") {
        let host = case["host"].as_str().unwrap_or("github.com");
        assert_eq!(
            json(&parse_pull_target(s(&case["target"]), host)),
            case["ref"],
            "{case}"
        );
    }
    for case in data["pullKeys"].as_array().expect("pullKeys") {
        assert_eq!(pull_key(&pull_ref(&case["ref"])), s(&case["key"]));
    }
}

/// Scripted `gh api -i` replies, every call recorded.
struct Scripted {
    replies: Mutex<VecDeque<RunResult>>,
    calls: Mutex<Vec<Value>>,
}

impl Runner for Scripted {
    fn run(&self, argv: &[String], options: &RunOptions) -> RunResult {
        self.calls
            .lock()
            .expect("calls")
            .push(json!({ "argv": argv, "env": options.env, "input": options.input }));
        self.replies.lock().expect("replies").pop_front().unwrap_or(RunResult {
            code: 1,
            stdout: String::new(),
            stderr: "no reply".into(),
        })
    }
}

fn error_json(error: &Error) -> Value {
    match error {
        Error::RateLimit { message, resume_at } => {
            json!({ "name": "RateLimitError", "message": message, "status": null, "resumeAt": resume_at })
        }
        Error::GitHub { message, status } => {
            json!({ "name": "GitHubError", "message": message, "status": status, "resumeAt": null })
        }
        other => json!({ "name": "Error", "message": other.message(), "status": null, "resumeAt": null }),
    }
}

fn call(gh: &GitHub, method: &str, args: &[Value]) -> Result<Value, Error> {
    let r = || pull_ref(&args[0]);
    Ok(match method {
        "pull" => json(&gh.pull(&r())?),
        "pullDiff" => json(&gh.pull_diff(&r())?),
        "reviews" => json(&gh.reviews(&r())?),
        "issueComments" => json(&gh.issue_comments(&r())?),
        "reviewComments" => json(&gh.review_comments(&r())?),
        "lastReviewRequestAt" => json(&gh.last_review_request_at(&r())?),
        "inTeam" => json(&gh.in_team(s(&args[0]), s(&args[1]))?),
        "canPush" => json(&gh.can_push(&r())?),
        "checkSummary" => json(&gh.check_summary(&r(), s(&args[1]))?),
        "postComment" => json(&gh.post_comment(&r(), s(&args[1]))?),
        "postReview" => json(&gh.post_review(&r(), &serde_json::from_value(args[1].clone()).expect("review"))?),
        "commentsOfReview" => json(&gh.comments_of_review(&r(), args[1].as_i64().expect("id"))?),
        "postReviewComment" => json(&gh.post_review_comment(
            &r(),
            s(&args[1]),
            &serde_json::from_value(args[2].clone()).expect("comment"),
        )?),
        "graphql" => gh.graphql(s(&args[0]), args[1].clone())?,
        "reviewThreads" => json(&gh.review_threads(&r())?.into_iter().collect::<Vec<_>>()),
        "resolveThread" => json(&gh.resolve_thread(s(&args[0]))?),
        "notifications" => json(&gh.notifications(
            s(&args[0]["since"]),
            args[0]["lastModified"].as_str(),
            args[0]["etag"].as_str(),
        )?),
        "search" => json(&gh.search(s(&args[0]))?),
        "list" => json(&gh.list(s(&args[0]), args[1].as_u64().expect("pages") as u32)?),
        "requestWithHeaders" => {
            let options = RequestOptions {
                headers: args[2]["headers"]
                    .as_object()
                    .expect("headers")
                    .iter()
                    .map(|(k, v)| (k.clone(), s(v).to_string()))
                    .collect(),
                allow: args[2]["allow"]
                    .as_array()
                    .expect("allow")
                    .iter()
                    .map(|v| v.as_u64().expect("status") as u16)
                    .collect(),
                body: None,
            };
            json(&gh.request(s(&args[0]), s(&args[1]), &options)?)
        }
        other => panic!("unknown method {other}"),
    })
}

#[test]
fn review_github_client_over_scripted_replies_matches_typescript() {
    for case in fixture()["client"].as_array().expect("client") {
        let replies = case["replies"]
            .as_array()
            .expect("replies")
            .iter()
            .map(|r| RunResult {
                code: r["code"].as_i64().expect("code") as i32,
                stdout: s(&r["stdout"]).into(),
                stderr: s(&r["stderr"]).into(),
            })
            .collect();
        let runner = Arc::new(Scripted {
            replies: Mutex::new(replies),
            calls: Mutex::new(Vec::new()),
        });
        let account = Account::new(s(&case["account"]["login"]), s(&case["account"]["host"]));
        let host = account.host.clone();
        let gh = GitHub::new(
            runner.clone(),
            account,
            Arc::new(move || {
                let mut env = review::runner::Env::new();
                env.insert("GH_TOKEN".into(), "t".into());
                env.insert("GH_HOST".into(), host.clone());
                Ok(env)
            }),
            Some(Arc::new(|| 1_791_108_000_000)),
        );
        let method = s(&case["method"]);
        let outcome = call(&gh, method, case["args"].as_array().expect("args"));
        match outcome {
            Ok(result) => {
                assert!(case["error"].is_null(), "{method}: expected {}", case["error"]);
                assert!(
                    same(&result, &case["result"]),
                    "{method}: {result} != {}",
                    case["result"]
                );
            }
            Err(error) => assert!(
                same(&error_json(&error), &case["error"]),
                "{method}: {error:?} != {}",
                case["error"]
            ),
        }
        let calls = runner.calls.lock().expect("calls").clone();
        assert_eq!(Value::Array(calls), case["calls"], "{method}");
    }
}

#[test]
fn review_github_local_checkout_remotes_and_paths_match_typescript() {
    let data = fixture();
    for case in data["remotes"].as_array().expect("remotes") {
        let r = &case["ref"];
        let found = remote_matches(s(&case["url"]), s(&r["host"]), s(&r["owner"]), s(&r["repo"]));
        assert_eq!(found, case["matches"].as_bool().expect("bool"), "{case}");
    }
    for case in data["expand"].as_array().expect("expand") {
        assert_eq!(
            expand_path(s(&case["path"]), s(&case["home"])),
            s(&case["expanded"]),
            "{case}"
        );
    }
}

#[test]
fn review_github_guide_mentions_and_withheld_plans_match_typescript() {
    let data = fixture();
    for case in data["guides"].as_array().expect("guides") {
        let found = guide_mentions(s(&case["text"]), &strings(&case["names"]));
        assert_eq!(json(&found), case["found"], "{case}");
    }
    for case in data["plans"].as_array().expect("plans") {
        let result: EngineResult = serde_json::from_value(case["result"].clone()).expect("result");
        let options: PlanOptions = serde_json::from_value(case["options"].clone()).expect("options");
        let plan = plan_review(&result, &options);
        assert_eq!(json(&plan), case["plan"]);
        let (checked, withheld) = without_guide_mentions(&plan, &strings(&case["names"]));
        assert_eq!(json(&checked), case["checked"]["plan"]);
        assert_eq!(json(&withheld), case["checked"]["withheld"]);
    }
}

fn pull_request(value: &Value) -> review::github::PullRequest {
    review::github::PullRequest {
        number: value["number"].as_u64().expect("number"),
        title: s(&value["title"]).into(),
        body: s(&value["body"]).into(),
        state: s(&value["state"]).into(),
        merged: value["merged"].as_bool().expect("merged"),
        draft: value["draft"].as_bool().expect("draft"),
        author: s(&value["author"]).into(),
        author_is_bot: value["authorIsBot"].as_bool().expect("bot"),
        head_sha: s(&value["headSha"]).into(),
        base_sha: s(&value["baseSha"]).into(),
        base_ref: s(&value["baseRef"]).into(),
        url: s(&value["url"]).into(),
        created_at: s(&value["createdAt"]).into(),
        requested_reviewers: strings(&value["requestedReviewers"]),
        requested_teams: strings(&value["requestedTeams"]),
        ..Default::default()
    }
}

#[test]
fn review_github_mentions_and_latest_mention_match_typescript() {
    let data = fixture();
    for case in data["mentions"].as_array().expect("mentions") {
        assert_eq!(
            mentions(s(&case["body"]), s(&case["login"])),
            case["mentions"].as_bool().expect("bool"),
            "{case}"
        );
    }
    for case in data["latest"].as_array().expect("latest") {
        let comments: Vec<review::github::Comment> = case["comments"]
            .as_array()
            .expect("comments")
            .iter()
            .map(|c| review::github::Comment {
                id: c["id"].as_i64().expect("id"),
                user: s(&c["user"]).into(),
                body: s(&c["body"]).into(),
                created_at: s(&c["createdAt"]).into(),
                updated_at: s(&c["updatedAt"]).into(),
                path: None,
                line: None,
                review_id: None,
            })
            .collect();
        let reviews: Vec<review::github::Review> = case["reviews"]
            .as_array()
            .expect("reviews")
            .iter()
            .map(|r| review::github::Review {
                id: r["id"].as_i64().expect("id"),
                user: s(&r["user"]).into(),
                state: s(&r["state"]).into(),
                commit_id: s(&r["commitId"]).into(),
                submitted_at: s(&r["submittedAt"]).into(),
                body: s(&r["body"]).into(),
            })
            .collect();
        let latest = latest_mention(s(&case["login"]), &pull_request(&case["pull"]), &comments, &reviews);
        assert_eq!(json(&latest), case["latest"], "{case}");
    }
}

#[test]
fn review_github_decide_matches_typescript() {
    for case in fixture()["decide"].as_array().expect("decide") {
        let input = &case["input"];
        let asked = std::cell::Cell::new(0);
        let requested_at = || {
            asked.set(asked.get() + 1);
            Ok(input["requestedAt"].as_str().map(String::from))
        };
        let pull = pull_request(&input["pull"]);
        let decision = decide(&DecisionInput {
            pull: &pull,
            last_sha: input["lastSha"].as_str().map(String::from),
            last_at: input["lastAt"].as_str().map(String::from),
            last_requested_changes: input["lastRequestedChanges"].as_bool().unwrap_or(false),
            mention_at: input["mentionAt"].as_str().map(String::from),
            requested: input["requested"].as_bool().expect("requested"),
            skip_bots: input["skipBots"].as_bool().unwrap_or(false),
            requested_at: &requested_at,
        })
        .expect("decision");
        assert_eq!(json(&decision), case["decision"], "{input}");
        assert_eq!(asked.get(), case["asked"].as_u64().expect("asked"), "{input}");
    }
}

#[test]
fn review_github_ack_body_and_line_choice_match_typescript() {
    let data = fixture();
    assert_eq!(MAX_ACK_CHARS as u64, data["maxAckChars"].as_u64().expect("max"));
    for case in data["ack"].as_array().expect("ack") {
        let body = ack_body(s(&case["line"]), s(&case["sha"]), s(&case["art"]));
        assert_eq!(body, s(&case["body"]), "{}", s(&case["line"]));
    }
    for case in data["pick"].as_array().expect("pick") {
        let line = pick_ack_line(
            &strings(&case["lines"]),
            case["previous"].as_str(),
            case["random"].as_f64().expect("random"),
        );
        assert_eq!(line, s(&case["line"]), "{case}");
    }
    for case in data["keys"].as_array().expect("keys") {
        let account = Account::new(s(&case["account"]["login"]), s(&case["account"]["host"]));
        let r = pull_ref(&case["ref"]);
        assert_eq!(pull_state_key(&account, &r), s(&case["key"]));
        assert_eq!(repo_key(&PullRef::new(&r.host, "O", "R", r.number)), s(&case["repo"]));
    }
}

#[test]
fn review_github_claim_hash_matches_typescript() {
    for case in fixture()["claims"].as_array().expect("claims") {
        assert_eq!(claim_hash(s(&case["claim"])), s(&case["hash"]), "{case}");
    }
}
