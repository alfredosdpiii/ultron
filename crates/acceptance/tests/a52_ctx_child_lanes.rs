//! A52/A38: `ctx` on a subagent lane, as the TypeScript worker serves it. One `ContextControl` answers `ctx.*` from
//! every lane's kernel, and each lane edits its own branch (`#lane(caller.lane)` in ultron/context-control.ts), so
//! the edit reaches that lane's next request only (the TS test's extension observer of `ultron:context_edit` is left
//! out: Pi extensions are not supported by ultron-rs).
//! "history, summarize, guards, pin, note, and state follow the branch" of
//! packages/coding-agent/test/ultron-context-control.test.ts, run on an `rlm.spawn` child's lane through the `ultron`
//! binary over RPC with the scripted provider, with that test's literal expectations: a child conversation has one
//! user message (its brief, the current turn), so the earlier exchange it summarizes is a cell call and its result
//! instead of a question and its answer. The root lane's branch and requests stay untouched. Not ported: `ctx.state`
//! (the `/rlm` panel, which reads the root lane in TypeScript too).

mod common;

use common::{HANG, Install, Provider, Reply, Request, wait_until};
use serde_json::{Value, json};

const SECRET: &str = "SECRET_ONE";
const SUMMARY: &str = "[Summary of 2 earlier items (ctx.summarize)]\nThey asked one thing.";
const NOTE: &str = "Note (ctx.note): REMEMBER_THIS";
const STUB: &str = "[Tool result removed from context.]";

/// The child's third cell: the TS test's `ctx` calls (the secret and the note are built by concatenation, so the
/// cell's own code never carries them).
const CTX_CELL: &str = "import json
items = (await ctx.history(limit=50))['items']
print('KINDS', json.dumps([i['kind'] for i in items]))
print('PROTECTED', json.dumps([i['protected'] for i in items]))
user, a1, r1, a2, r2 = [i['id'] for i in items[:5]]
try:
    await ctx.history(kinds=['bogus'])
except Exception as e:
    print('ERR_KINDS', e)
try:
    await ctx.forget([a1, user], 'x')
    print('CURRENT_FORGOTTEN')
except Exception as e:
    print('ERR_CURRENT', 'current user turn' in str(e))
try:
    await ctx.forget(['nope'], 'x')
except Exception as e:
    print('ERR_UNKNOWN', \"not in the model's context\" in str(e))
print('ALL_VISIBLE', all(i['state'] == 'visible' for i in (await ctx.history(limit=50))['items']))
r = await ctx.pin(r2)
print('PIN', r['pinned'], r['changed'])
try:
    await ctx.summarize([r2], 'x')
    print('PINNED_SUMMARIZED')
except Exception as e:
    print('ERR_PINNED', 'pinned' in str(e))
s = await ctx.summarize([a1], 'They asked one thing.')
print('SUMMARIZED', s['summarized'] == [a1, r1])
await ctx.note('REMEMBER_' + 'THIS')
after = (await ctx.history(limit=50))['items']
state = {i['id']: i['state'] for i in after}
print('STATES', state[a1], state[r1])
print('LAST', after[-1]['kind'], after[-1]['preview'])
print('GET', ('SECRET_' + 'ONE') in (await ctx.get(r1))['text'], 'They asked one thing.' in (await ctx.get(a1))['visible_text'])
print('CTX_DONE')";

/// The child's fourth cell: unpinning makes the result editable again.
const UNPIN_CELL: &str = "items = (await ctx.history(limit=50))['items']
r2 = [i for i in items if i['kind'] == 'tool' and ('answer' + ' 1') in i['preview']][0]['id']
u = await ctx.unpin(r2)
print('UNPIN', u['pinned'], u['changed'])
f = await ctx.forget([r2], 'done')
print('FORGOT', f['forgotten'] == [r2])
n = await ctx.note('TEMP_' + 'NOTE')
g = await ctx.forget([n['id']], 'not needed')
state = {i['id']: i['state'] for i in (await ctx.history(limit=50))['items']}
print('NOTE_FORGOTTEN', g['forgotten'] == [n['id']], state[n['id']])
print('UNPIN_DONE')";

const ROOT_CELL: &str = "import json
items = (await ctx.history(limit=50))['items']
print('ROOT_KINDS', json.dumps(sorted(set(i['kind'] for i in items))))
print('ROOT_EDITED', any(i['state'] != 'visible' or i['pinned'] for i in items))";

fn prompts(request: &Request) -> Vec<String> {
    request
        .users()
        .into_iter()
        .filter(|u| !u.starts_with("[clock] "))
        .collect()
}

fn is_child(request: &Request) -> bool {
    prompts(request).first().is_some_and(|u| u.starts_with("Child task"))
}

fn last_result(request: &Request) -> String {
    request.tool_results().last().cloned().unwrap_or_default()
}

fn script(request: &Request) -> Reply {
    if is_child(request) {
        let last = last_result(request);
        return if request.tool_results().is_empty() {
            Reply::RlmCall("c1".into(), "print('SECRET_' + 'ONE first output')".into())
        } else if last.contains("first output") {
            Reply::RlmCall("c2".into(), "print('answer' + ' 1')".into())
        } else if last.starts_with("answer 1") {
            Reply::RlmCall("c3".into(), CTX_CELL.into())
        } else if last.contains("CTX_DONE") {
            Reply::RlmCall("c4".into(), UNPIN_CELL.into())
        } else {
            Reply::Text("CHILD_DONE".into())
        };
    }
    let prompts = prompts(request);
    let tool_last = request.body["messages"].as_array().unwrap().last().unwrap()["role"] == "tool";
    if prompts.iter().any(|p| p == "Root check") {
        return if tool_last {
            Reply::Text("ROOT_DONE".into())
        } else {
            Reply::RlmCall("root_check".into(), ROOT_CELL.into())
        };
    }
    if prompts.last().is_some_and(|p| p == "Spawn the helper") && !tool_last {
        return Reply::Rlm(
            "h = await rlm.spawn('Child task: tidy your own context', name='helper')\nprint('SPAWNED')".into(),
        );
    }
    Reply::Text("root acknowledged".into())
}

/// The request's messages as `(role, text)`, tool calls as `call:<id>`.
fn roles(request: &Request) -> Vec<(String, String)> {
    request.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let role = m["role"].as_str().unwrap_or_default().to_string();
            let text = match &m["content"] {
                Value::String(text) => text.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            let calls: Vec<String> = m["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| format!("call:{}", c["id"].as_str().unwrap_or_default()))
                .collect();
            (role, if calls.is_empty() { text } else { calls.join(",") })
        })
        .collect()
}

#[test]
fn acceptance_a52_ctx_on_a_subagent_lane_edits_only_that_lane() {
    let provider = Provider::start(script);
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[]);
    client
        .prompt_and_wait("Remember the root marker ROOT_MARK_77", HANG)
        .unwrap();
    client.prompt_and_wait("Spawn the helper", HANG).unwrap();
    let child_done = || {
        provider
            .requests()
            .iter()
            .any(|r| is_child(r) && last_result(r).contains("UNPIN_DONE"))
    };
    wait_until("the child's last request", child_done);
    // The child's end re-invokes the root (child_done); wait until that run settled.
    wait_until("the child_done run", || {
        client.get_messages().is_ok_and(|m| {
            m.iter().any(|m| m["customType"] == "ultron-runtime-event")
                && m.last().is_some_and(|m| m["role"] == "assistant")
        }) && client.get_state().is_ok_and(|s| s["isStreaming"] == false)
    });
    client.prompt_and_wait("Root check", HANG).unwrap();
    assert_eq!(client.get_last_assistant_text().unwrap().as_deref(), Some("ROOT_DONE"));

    let requests = provider.requests();
    let children: Vec<&Request> = requests.iter().filter(|r| is_child(r)).collect();
    let step = |marker: &str| -> &Request {
        children
            .iter()
            .find(|r| last_result(r).contains(marker))
            .unwrap_or_else(|| panic!("the child request after {marker}"))
    };

    // The cell ran the TS test's calls on the child's own branch.
    let output = last_result(step("CTX_DONE"));
    for line in [
        r#"KINDS ["user", "assistant", "tool", "assistant", "tool", "assistant"]"#,
        "PROTECTED [true, false, false, false, false, true]",
        "ERR_KINDS kinds must be a list of: user, assistant, tool, custom, note, summary, compaction",
        "ERR_CURRENT True",
        "ERR_UNKNOWN True",
        "ALL_VISIBLE True",
        "PIN True True",
        "ERR_PINNED True",
        "SUMMARIZED True",
        "STATES summarized summarized",
        "LAST note REMEMBER_THIS",
        "GET True True",
    ] {
        assert!(output.lines().any(|l| l == line), "{line}:\n{output}");
    }
    assert!(!output.contains("CURRENT_FORGOTTEN") && !output.contains("PINNED_SUMMARIZED"));

    // The child's next request: the summary replaces the first call and its result, the note follows the cell's
    // result (it waited for the turn's boundary), the pinned result stays.
    let next = step("CTX_DONE");
    assert!(!next.raw.contains(SECRET), "{}", next.raw);
    let seen = roles(next);
    let at = |role: &str, text: &str| seen.iter().position(|(r, t)| r == role && t == text);
    let ctx_result = seen
        .iter()
        .position(|(r, t)| r == "tool" && t.contains("CTX_DONE"))
        .unwrap();
    assert_eq!(at("user", NOTE), Some(ctx_result + 1), "{seen:?}");
    assert!(at("assistant", SUMMARY).is_some(), "{seen:?}");
    assert!(at("assistant", "call:c1").is_none(), "{seen:?}");
    assert!(
        seen.iter().any(|(r, t)| r == "tool" && t.starts_with("answer 1")),
        "{seen:?}"
    );
    // The clock line (a subagent lane's, clock-context.ts `clockLane`), the brief, the summary, the second call and
    // its result, the edit cell and its result, the note.
    let without_system: Vec<&(String, String)> = seen.iter().filter(|(r, _)| r != "system").collect();
    assert_eq!(without_system.len(), 8, "{seen:?}");

    // After unpinning, forgetting the result leaves a stub, keeping its call paired.
    let last = step("UNPIN_DONE");
    let unpinned = last_result(last);
    assert!(
        unpinned.contains("UNPIN False True") && unpinned.contains("FORGOT True"),
        "{unpinned}"
    );
    let seen = roles(last);
    let call = seen
        .iter()
        .position(|(r, t)| r == "assistant" && t == "call:c2")
        .unwrap();
    assert_eq!(seen[call + 1], ("tool".to_string(), STUB.to_string()), "{seen:?}");
    assert!(!last.raw.contains(SECRET));
    // A note forgotten while it still waited for the boundary never reaches the request either.
    assert!(unpinned.contains("NOTE_FORGOTTEN True forgotten"), "{unpinned}");
    assert!(!last.raw.contains("TEMP_NOTE"), "{}", last.raw);

    // Nothing leaked to the root lane: its branch has no edit, pin or note, and its requests never carry the
    // child's note or summary.
    let roots: Vec<&Request> = requests.iter().filter(|r| !is_child(r)).collect();
    let check = roots.last().unwrap();
    let root_output = last_result(check);
    assert!(root_output.contains("ROOT_EDITED False"), "{root_output}");
    assert!(!root_output.contains("note"), "{root_output}");
    for request in &roots {
        assert!(!request.raw.contains("REMEMBER_THIS") && !request.raw.contains("REMEMBER_' + 'THIS"));
        assert!(!request.raw.contains("Note (ctx.note)"));
        assert!(!request.raw.contains("They asked one thing"));
    }
    assert!(check.raw.contains("ROOT_MARK_77"));
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));
}

/// A subagent lane compacts at its threshold as a TypeScript lane does, with `ctx`'s compaction hooks
/// (ultron-context-control.test.ts "trim before compaction" and the A52 notes-and-pins case, on a child lane): an
/// answered large result is trimmed instead of a threshold compaction; when trimming cannot get under it the lane
/// compacts, and its next request carries the summary and, after it, the notes the compaction cut off.
#[test]
fn acceptance_a52_a_subagent_lane_trims_then_compacts_and_keeps_its_notes() {
    // A 30,000-token window: blocking threshold 30,000 - 16,384 = 13,616 tokens. About 10,000 tokens with markers at
    // the start, middle and end.
    const BIG_CELL: &str = "await ctx.note('KEEP_' + 'NOTE')\nprint('HEAD_' + 'MARK' + 'x' * 20000 + 'MIDDLE_' + 'MARK' + 'y' * 20000 + 'TAIL_' + 'MARK')";
    // After its compaction the child's conversation starts with the summary instead of its brief.
    fn child(request: &Request) -> bool {
        is_child(request) || request.raw.contains("CHILD_SUMMARY_TEXT") && !request.system().contains("summarization")
    }
    let script = |request: &Request| -> Reply {
        if request.system().contains("context summarization assistant") {
            return Reply::Text("CHILD_SUMMARY_TEXT".into());
        }
        if child(request) {
            let results = request.tool_results();
            let last = last_result(request);
            return if results.is_empty() {
                Reply::RlmCall("k1".into(), BIG_CELL.into())
            } else if last.contains("MIDDLE_MARK") {
                // About 15,000 tokens after the newest answer: over the threshold; trimming the answered first
                // result gets under it.
                Reply::RlmCall("k2".into(), "print('w' * 60000)".into())
            } else if last.starts_with("www") {
                // The whole output budget (about 32,800 tokens): trimming the second result cannot get under it.
                Reply::RlmCall("k3".into(), "print('THIRD' + 'z' * 200000)".into())
            } else {
                Reply::Text("CHILD_DONE".into())
            };
        }
        let prompts = prompts(request);
        let tool_last = request.body["messages"].as_array().unwrap().last().unwrap()["role"] == "tool";
        if prompts.last().is_some_and(|p| p == "Spawn the helper") && !tool_last {
            return Reply::Rlm("h = await rlm.spawn('Child task: big outputs', name='big')\nprint('SPAWNED')".into());
        }
        Reply::Text("root acknowledged".into())
    };
    let provider = Provider::start(script);
    let install = Install::new(&provider);
    std::fs::write(
        install.agent().join("models.json"),
        json!({"providers": {"groq": {"baseUrl": format!("http://127.0.0.1:{}/v1", provider.port),
            "modelOverrides": {"llama-3.3-70b-versatile": {"contextWindow": 30000}}}}})
        .to_string(),
    )
    .unwrap();
    // Cell output up to its maximum budget (128 KiB), so the results are as large as printed.
    let mut client = install.rpc(&["--no-session"], &[("ULTRON_RLM_OUTPUT_BYTES", "131072")]);
    client.prompt_and_wait("Spawn the helper", HANG).unwrap();
    wait_until("the child's last request", || {
        provider
            .requests()
            .iter()
            .any(|r| child(r) && last_result(r).contains("THIRD"))
    });
    let requests = provider.requests();
    let children: Vec<&Request> = requests
        .iter()
        .filter(|r| child(r) && !r.system().contains("context summarization assistant"))
        .collect();
    let summarizers: Vec<&Request> = requests
        .iter()
        .filter(|r| r.system().contains("context summarization assistant"))
        .collect();

    // Over the threshold with the first result answered: trimmed instead of compacted.
    let trimmed = children
        .iter()
        .find(|r| last_result(r).starts_with("www"))
        .expect("the request after the second cell");
    assert!(trimmed.raw.contains("HEAD_MARK") && trimmed.raw.contains("TAIL_MARK"));
    assert!(
        !trimmed.raw.contains("MIDDLE_MARK"),
        "the answered result was not trimmed"
    );
    assert!(!trimmed.raw.contains("CHILD_SUMMARY_TEXT"));

    // Over it again with only an unanswered result: compacted once, and the cut-off note comes back after the
    // summary.
    assert_eq!(summarizers.len(), 1, "one compaction");
    let compacted = children.iter().find(|r| last_result(r).contains("THIRD")).unwrap();
    let seen = roles(compacted);
    let summary = seen
        .iter()
        .position(|(r, t)| r == "user" && t.contains("CHILD_SUMMARY_TEXT"))
        .expect("the summary");
    let (role, kept) = &seen[summary + 1];
    assert_eq!(role, "user");
    assert!(
        kept.starts_with("Notes and pinned items kept across compaction (ctx.note / ctx.pin):")
            && kept.contains("KEEP_NOTE"),
        "{seen:?}"
    );
    // The root lane neither compacted nor sees any of it.
    for request in requests
        .iter()
        .filter(|r| !child(r) && !r.system().contains("context summarization"))
    {
        assert!(!request.raw.contains("KEEP_NOTE") && !request.raw.contains("CHILD_SUMMARY_TEXT"));
    }
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));
}
