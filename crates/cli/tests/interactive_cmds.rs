//! The interactive TUI's commands and keys after the first batch, in a real terminal (tmux): `!`/`!!` bash,
//! /compact, /thinking and Shift+Tab, Ctrl+P, Ctrl+T, Ctrl+O, /fork, /clone, /export and /usage. Each test replays
//! a scenario of `fixtures/interactive_cmds.json`, recorded from the TypeScript client TUI by
//! `fixtures/interactive_cmds.mjs` (same keystrokes, same scripted Anthropic SSE server, private HOME /
//! ULTRON_CODING_AGENT_DIR / ULTRON_SERVER_DIR), and every stable screen (and file the scenario names) must equal
//! the TypeScript one after the same normalization.

mod tui_harness;

use serde_json::Value;
use tui_harness::{Mock, TempDir, Tui, normalize, replies};

fn fixture() -> &'static Value {
    static FIXTURE: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    FIXTURE.get_or_init(|| serde_json::from_str(include_str!("../fixtures/interactive_cmds.json")).unwrap())
}

/// `normalizeArtifact` of the generator: the screen normalization, then entry times and short entry ids.
fn normalize_artifact(content: &str, tmp: &str) -> String {
    let text = normalize(content, tmp);
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(at) = rest.find("\"timestamp\":") {
        out.push_str(&rest[..at]);
        let after = &rest[at + "\"timestamp\":".len()..];
        if let Some(quoted) = after.strip_prefix('"') {
            let end = quoted.find('"').unwrap();
            out.push_str("\"timestamp\":\"<T>\"");
            rest = &quoted[end + 1..];
        } else {
            let end = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
            if end == 0 {
                out.push_str("\"timestamp\":");
            } else {
                out.push_str("\"timestamp\":0");
            }
            rest = &after[end..];
        }
    }
    out.push_str(rest);
    // "id"/"parentId" of 8 hex digits.
    let mut result = String::new();
    let mut rest = out.as_str();
    loop {
        let next = ["\"id\":\"", "\"parentId\":\""]
            .iter()
            .filter_map(|k| rest.find(k).map(|i| (i, *k)))
            .min();
        let Some((i, key)) = next else { break };
        let value_start = i + key.len();
        result.push_str(&rest[..value_start]);
        let value = &rest[value_start..];
        let hex = value.len() > 8
            && value.as_bytes()[8] == b'"'
            && value[..8]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if hex {
            result.push_str("<E>");
            rest = &value[8..];
        } else {
            rest = value;
        }
    }
    result.push_str(rest);
    result
}

/// JSONL compared by value: each line's JSON (the Rust session keeps message fields in its own key order).
fn jsonl_values(text: &str) -> Vec<Value> {
    text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn replay(name: &str) {
    replay_with(name, &[]);
}

fn replay_with(name: &str, divergences: &[(&str, &str)]) {
    replay_checked(name, divergences, None);
}

/// Replay `name`, comparing each screen with `compare(got, want)` instead of exactly.
fn replay_compare(name: &str, compare: &dyn Fn(&str, &str)) {
    replay_full(name, &[], None, Some(compare));
}

/// Replay `name`; `divergences` are whole rows whose recorded text is replaced by the Rust one before comparing;
/// `last`, when given, checks the final screen instead of comparing it (a step whose outcome is Rust's own).
fn replay_checked(name: &str, divergences: &[(&str, &str)], last: Option<&dyn Fn(&str)>) {
    replay_full(name, divergences, last, None);
}

/// A screen comparison `(got, want)`.
type Compare<'a> = dyn Fn(&str, &str) + 'a;

fn replay_full(name: &str, divergences: &[(&str, &str)], last: Option<&dyn Fn(&str)>, compare: Option<&Compare<'_>>) {
    let scenario = fixture()["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("scenario {name} is not in the fixture"));
    let mock = Mock::start(replies(scenario), None);
    let mut env = vec![
        ("ULTRON_AUTO_NAME", "off"),
        ("ULTRON_TUI_PARITY", "1"),
        ("ULTRON_SPLASH", "off"),
    ];
    for (k, v) in scenario["env"].as_object().into_iter().flatten() {
        env.push((k.as_str(), v.as_str().unwrap()));
    }
    // Files the scenario starts with (its `files`, relative to the temp root).
    let tmp = TempDir::new();
    for (path, content) in scenario["files"].as_object().into_iter().flatten() {
        let file = tmp.0.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content.as_str().unwrap()).unwrap();
    }
    let tui = Tui::start_at(
        &tmp.0,
        mock.port,
        scenario["cols"].as_u64().unwrap(),
        scenario["rows"].as_u64().unwrap(),
        &["--model", "anthropic/claude-sonnet-4-5"],
        &env,
    );
    let captures = scenario["captures"].as_array().unwrap();
    let steps = scenario["steps"].as_array().unwrap();
    for (i, (step, want)) in steps.iter().zip(captures).enumerate() {
        if step["release"] == true {
            mock.release();
        }
        if let Some(text) = step["type"].as_str() {
            tui.send_text(text);
        }
        if let Some(keys) = step["keys"].as_array() {
            tui.send_keys(&keys.iter().map(|k| k.as_str().unwrap()).collect::<Vec<_>>());
        }
        let got = tui.wait(step["wait"].as_str().unwrap());
        if let Some(check) = last
            && i + 1 == steps.len()
        {
            check(&got);
            continue;
        }
        let want = want
            .as_str()
            .unwrap()
            .split('\n')
            .map(|row| {
                divergences
                    .iter()
                    .find(|(ts, _)| *ts == row)
                    .map_or_else(|| row.to_string(), |(_, rust)| (*rust).to_string())
            })
            .collect::<Vec<_>>()
            .join("\n");
        let want = want.as_str();
        if let Some(compare) = compare {
            compare(&got, want);
            continue;
        }
        let got = tui.wait_screen(got, want);
        if got != want {
            let diff: Vec<String> = got
                .lines()
                .zip(want.lines())
                .enumerate()
                .filter(|(_, (g, w))| g != w)
                .map(|(row, (g, w))| format!("row {row}:\n  got  {g:?}\n  want {w:?}"))
                .collect();
            panic!(
                "{name} step {i} differs from TypeScript:\n{}\n--- got\n{got}\n--- want\n{want}",
                diff.join("\n")
            );
        }
    }
    let root = tui.root().display().to_string();
    for (path, want) in scenario["artifacts"].as_object().into_iter().flatten() {
        let got = std::fs::read_to_string(tui.root().join(path))
            .ok()
            .map(|c| normalize_artifact(&c, &root));
        let got = got.unwrap_or_else(|| panic!("{name}: {path} was not written"));
        if path.ends_with(".jsonl") {
            assert_eq!(
                jsonl_values(&got),
                jsonl_values(want.as_str().unwrap()),
                "{name}: {path} differs from TypeScript"
            );
        } else {
            // A JSON document (auth.json) is compared byte for byte.
            assert_eq!(got, want.as_str().unwrap(), "{name}: {path} differs from TypeScript");
        }
    }
}

macro_rules! parity {
    ($($test:ident => $name:literal),* $(,)?) => {
        $(
            #[test]
            fn $test() {
                replay($name);
            }
        )*

        #[test]
        fn cli_interactive_cmds_fixture_has_a_test_per_scenario() {
            let tested = [$($name,)* "usage", "logout", "login-api-key", "rlm-panel"];
            for scenario in fixture()["scenarios"].as_array().unwrap() {
                let name = scenario["name"].as_str().unwrap();
                assert!(tested.contains(&name), "scenario {name} has no parity test");
            }
        }
    };
}

parity! {
    cli_interactive_bash_output_matches_ts => "bash-output",
    cli_interactive_bash_exit_code_matches_ts => "bash-exit-code",
    cli_interactive_bash_cancel_matches_ts => "bash-cancel",
    cli_interactive_bash_output_reaches_the_model_like_ts => "bash-in-context",
    cli_interactive_bare_bang_is_a_prompt_like_ts => "bash-empty",
    cli_interactive_compact_matches_ts => "compact",
    cli_interactive_compact_then_prompt_matches_ts => "compact-then-prompt",
    cli_interactive_compact_empty_session_matches_ts => "compact-empty",
    cli_interactive_thinking_selector_matches_ts => "thinking-selector",
    cli_interactive_thinking_argument_matches_ts => "thinking-arg",
    cli_interactive_thinking_cycle_key_matches_ts => "thinking-cycle",
    cli_interactive_model_cycle_key_matches_ts => "model-cycle",
    cli_interactive_thinking_block_toggle_matches_ts => "thinking-toggle",
    cli_interactive_tools_expand_status_matches_ts => "tools-expand",
    cli_interactive_fork_matches_ts => "fork",
    cli_interactive_fork_without_messages_matches_ts => "fork-empty",
    cli_interactive_clone_matches_ts => "clone",
    cli_interactive_clone_empty_matches_ts => "clone-empty",
    cli_interactive_export_jsonl_matches_ts => "export-jsonl",
    cli_interactive_export_html_matches_ts => "export-html",
    cli_interactive_history_search_matches_ts => "history-search",
    cli_interactive_tree_navigates_like_ts => "tree",
    cli_interactive_tree_branches_and_search_like_ts => "tree-branches",
    cli_interactive_tree_filters_like_ts => "tree-filters",
    cli_interactive_tree_empty_like_ts => "tree-empty",
    cli_interactive_tree_summarize_like_ts => "tree-summarize",
    cli_interactive_resume_selector_like_ts => "resume-selector",
    cli_interactive_settings_like_ts => "settings",
    cli_interactive_review_expands_like_ts => "review",
    cli_interactive_logout_without_credentials_like_ts => "logout-empty",
    cli_interactive_share_without_gh_login_like_ts => "share-no-gh",
    cli_interactive_changelog_like_ts => "changelog",
    cli_interactive_external_editor_like_ts => "external-editor",
    cli_interactive_external_editor_missing_like_ts => "external-editor-missing",
    cli_interactive_external_editor_failure_keeps_text_like_ts => "external-editor-fails",
    cli_interactive_settings_models_like_ts => "settings-models",
    cli_interactive_settings_model_thinking_like_ts => "settings-thinking",
    cli_interactive_resume_switch_like_ts => "resume-switch",
    cli_interactive_resume_after_compaction_like_ts => "compact-resume",
    cli_interactive_history_search_empty_matches_ts => "history-search-empty",
}

/// `/logout` removes a stored key; TypeScript then refreshes its model catalog over the network, which (offline)
/// leaves claude-sonnet-4-5 with a 200k window; the Rust TUI keeps the catalog it has.
#[test]
fn cli_interactive_logout_like_ts() {
    replay_with(
        "logout",
        &[(
            "0.0%/200k (auto)                                                          claude-sonnet-4-5 • medium",
            "0.0%/1.0M (auto)                                                          claude-sonnet-4-5 • medium",
        )],
    );
}

/// `/login` with an API key: method, provider search, the masked prompt and the saved `auth.json` match
/// TypeScript. TypeScript's last screen still says it is reloading credentials (its worker's catalog refresh goes
/// to the network); the Rust session reads credentials per request, so it reports the saved key with Pi's model
/// hint at once.
#[test]
fn cli_interactive_login_api_key_like_ts() {
    replay_checked(
        "login-api-key",
        &[],
        Some(&|screen: &str| {
            let text = screen.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                text.contains(
                    "Saved API key for Groq. Credentials saved to <TMP>/agent/auth.json. The Session still uses anthropic/claude-sonnet-4-5; /model switches to Groq's models"
                ),
                "{screen}"
            );
        }),
    );
}

/// The RLM panel (Ctrl+R), `/rlm focus` and `/rlm pane` (the graph below 120 columns) on an idle session. The
/// `tasks`/`tokens` gauges come from `agents.status {graph: true}` and the `kernels` strip from the inspected
/// `rlm.pool`, as in TypeScript. The Rust session opens the root lane's kernel at once (TypeScript's starts on the
/// first cell), so the strip says `1/16` live where TypeScript's says `0/16`: it is compared by its counters after
/// the live count. Every other row matches TypeScript (blank rows aside).
#[test]
fn cli_interactive_rlm_views_like_ts() {
    let kernels = |row: &str| row.trim_start().starts_with("kernels ");
    replay_compare("rlm-panel", &|got: &str, want: &str| {
        let rows = |screen: &str| -> Vec<String> {
            screen
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| match l.split_once("/16 · ") {
                    Some((_, counters)) if kernels(l) => format!("kernels <live>/16 · {counters}"),
                    _ => l.to_string(),
                })
                .collect()
        };
        assert_eq!(rows(got), rows(want), "--- got\n{got}\n--- want\n{want}");
    });
}

/// `/usage` prints the session report, the rows backed by the session's runtime counters (workflow runs, Loki
/// checks, usage-limit blocks) included, as in TypeScript.
#[test]
fn cli_interactive_usage_matches_ts() {
    replay("usage");
}
