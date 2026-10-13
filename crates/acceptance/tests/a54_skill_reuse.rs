//! A54: on a repeated task the second run uses the saved skill (tool use and skill import observed) and costs less
//! than the first. Ported from packages/coding-agent/test/ultron-a54-skill-reuse.test.ts: the `ultron` binary in
//! RPC mode (a fresh server and session worker per run; only the profile carries over) against a scripted
//! OpenAI-compatible provider whose root script derives the procedure over several cells and saves it as a code
//! skill, or, once the rlm tool description lists the skill, imports it and answers in one cell.

mod common;

use common::{Install, Provider, Reply, Request, rpc_tool_results};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

const SKILL: &str = r#""""Most frequent word in a text (ties broken alphabetically)."""
import collections
import re

def top_word(text: str) -> str:
    """Return the most frequent lowercase word; ties go to the alphabetically first."""
    counts = collections.Counter(re.findall(r"[a-z']+", text.lower()))
    return min(counts, key=lambda word: (-counts[word], word))
"#;
const SKILL_TEST: &str = r#"from code_skills import top_word
def test_counts_and_ties():
    assert top_word.top_word("b a b a c") == "a"
    assert top_word.top_word("The cat. the DOG, the end") == "the"
"#;

fn derive() -> Vec<String> {
    vec![
        "text = open('corpus.txt').read()\nprint('WORDS', len(text.split()))".into(),
        "import re, collections\ncounts = collections.Counter(re.findall(r\"[a-z']+\", text.lower()))\nprint('TOP3', counts.most_common(3))".into(),
        "best = min(counts, key=lambda w: (-counts[w], w))\nprint('BEST', best)".into(),
        format!(
            "r = await skills.propose_code(\"top_word\", {}, {}, {{\"task\": \"top word of corpus.txt\", \"cells\": 3}})\nprint('PROPOSED', r['status'], r['version'])",
            serde_json::to_string(SKILL).unwrap(),
            serde_json::to_string(SKILL_TEST).unwrap()
        ),
    ]
}

const REUSE: &str = "from code_skills import top_word\nprint('SKILL', top_word.__skill_version__, top_word.top_word(open('corpus.txt').read()))";

fn tool_description(request: &Request) -> String {
    request.body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|tool| tool["function"]["name"] == "rlm")
        .and_then(|tool| tool["function"]["description"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// The `(?:SKILL \d+|BEST) (\w+)` answer over every tool message.
fn answer_of(tool_messages: &[String]) -> Option<String> {
    for line in tool_messages.join("\n").lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["BEST", word, ..] => return Some(word.to_string()),
            ["SKILL", version, word, ..] if version.chars().all(|c| c.is_ascii_digit()) => {
                return Some(word.to_string());
            }
            _ => {}
        }
    }
    None
}

fn script(request: &Request) -> Reply {
    // The task's user message (the request may open with a clock line).
    if !request.users().iter().any(|u| u.contains("TASK: which word")) {
        return Reply::Text("ok".into());
    }
    let turn = request.tool_results().len();
    if tool_description(request).contains("- top_word v1:") {
        if turn == 0 {
            return Reply::Rlm(REUSE.into());
        }
    } else if let Some(code) = derive().get(turn) {
        return Reply::Rlm(code.clone());
    }
    let answer = answer_of(&request.tool_results()).unwrap_or_else(|| "unknown".into());
    Reply::Text(format!("TOP {answer}"))
}

struct Run {
    answer: Option<String>,
    tool_output: String,
    calls: usize,
    prompt_tokens: usize,
    tools_by_name: BTreeMap<String, usize>,
    requests: Vec<Request>,
}

fn run_task(provider: &Provider, install: &Install, server: &str) -> Run {
    let first = provider.requests().len();
    // A fresh server and worker for every run: nothing but the profile carries over.
    let server_dir = install.root.path().join(server).display().to_string();
    let mut client = install.rpc(
        &["--no-session"],
        &[("ULTRON_SERVER_DIR", &server_dir), ("ULTRON_CODE_SKILLS_DIR", "")],
    );
    client
        .prompt_and_wait(
            "TASK: which word occurs most often in corpus.txt?",
            Duration::from_secs(120),
        )
        .unwrap_or_else(|e| panic!("{e}"));
    let answer = client.get_last_assistant_text().unwrap();
    let tool_output = rpc_tool_results(&client.get_messages().unwrap()).join("\n");
    client.stop();
    let requests: Vec<Request> = provider.requests()[first..].to_vec();
    let mut tools_by_name = BTreeMap::new();
    for message in requests.last().unwrap().body["messages"].as_array().unwrap() {
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            *tools_by_name
                .entry(call["function"]["name"].as_str().unwrap().to_string())
                .or_insert(0) += 1;
        }
    }
    // Prompt tokens as request bytes / 4 (the scripted provider's measure): the run's input cost.
    let prompt_tokens = requests.iter().map(|r| r.raw.len().div_ceil(4)).sum();
    Run {
        answer,
        tool_output,
        calls: requests.len(),
        prompt_tokens,
        tools_by_name,
        requests,
    }
}

#[test]
fn acceptance_a54_the_second_run_imports_the_saved_skill_and_makes_fewer_cheaper_calls() {
    let provider = Provider::start(script);
    let install = Install::new(&provider);
    let words = [
        "delta", "alpha", "beta", "alpha", "gamma", "beta", "alpha", "delta", "beta",
    ];
    let corpus: Vec<&str> = (0..400).map(|i| words[i % words.len()]).collect();
    std::fs::write(install.project().join("corpus.txt"), format!("{}\n", corpus.join(" "))).unwrap();

    let first = run_task(&provider, &install, "server-1");
    assert_eq!(first.answer.as_deref(), Some("TOP alpha"));
    assert!(first.tool_output.contains("PROPOSED active 1"), "{}", first.tool_output);
    assert_eq!(first.tools_by_name, BTreeMap::from([("rlm".to_string(), 4)]));
    assert!(!tool_description(&first.requests[0]).contains("- top_word v1:"));
    // The saved skill is on disk in the profile, tested and active.
    let history: Value =
        serde_json::from_str(&std::fs::read_to_string(install.agent().join("skills/top_word/.history.json")).unwrap())
            .unwrap();
    assert_eq!(history["active"], 1);
    assert_eq!(history["versions"].as_array().unwrap().len(), 1);
    assert_eq!(history["versions"][0]["state"], "active");
    assert_eq!(history["versions"][0]["test"]["passed"], true);

    let second = run_task(&provider, &install, "server-2");
    assert_eq!(second.answer.as_deref(), Some("TOP alpha"));
    // The new worker lists the skill's docstring in the rlm tool description ...
    assert!(
        tool_description(&second.requests[0]).contains(
            "- top_word v1: Most frequent word in a text (ties broken alphabetically). [top_word(text: str) -> str]"
        ),
        "{}",
        tool_description(&second.requests[0])
    );
    // ... and its kernel imported the tested version.
    assert!(second.tool_output.contains("SKILL 1 alpha"), "{}", second.tool_output);
    assert_eq!(second.tools_by_name, BTreeMap::from([("rlm".to_string(), 1)]));
    assert!(second.calls < first.calls, "{} < {}", second.calls, first.calls);
    assert!(second.prompt_tokens < first.prompt_tokens);
}
