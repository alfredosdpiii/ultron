//! The REPL's shared world (`swarm_api.py`: `swarm`, `board`, `notes`, `rlm.spawn(..., keep=True)`, `rlm.ask`) works
//! in the product's session: the root's cells reach the durable runtime's records (`harness::chat::host_ops`) as the
//! root conversation (`root`), and a persistent child answers its first prompt, then a follow-up through its inbox.

mod p_session_support;

use p_session_support::{Setup, actor, text, tool};
use serde_json::{Value, json};

#[test]
fn session_core_swarm_board_notes_and_persistent_children_work_from_the_root_repl() {
    let code = [
        "import json",
        "print('append', json.dumps(await notes.append('first line')))",
        "print('read', json.dumps(await notes.read()))",
        "t = await board.add('task one')",
        "print('claim', json.dumps(await board.claim(t['id'])))",
        "print('list', json.dumps([(x['title'], x['status'], x.get('owner')) for x in await board.list()]))",
        "print('agents', json.dumps(sorted(await swarm.agents())))",
        "print('send', json.dumps(await swarm.send('root', 'hello me')))",
        "print('inbox', json.dumps([(m['from'], m['text']) for m in await swarm.inbox()]))",
        "h = await rlm.spawn('KEEP-CHILD: remember 42', name='keeper', keep=True)",
        "print('collect', json.dumps([r['result'] for r in await rlm.collect([h])]))",
        "print('ask', json.dumps(await rlm.ask(h, 'KEEP-QUESTION: what number?')))",
        "print('registered', json.dumps(sorted(await swarm.agents())))",
    ]
    .join("\n");
    let script = Box::new(move |body: &Value, _| {
        let messages = body["messages"].to_string();
        if messages.contains("swarm test") {
            if messages.contains("tool_result") {
                text("root done")
            } else {
                tool("toolu_root", &code)
            }
        } else if messages.contains("KEEP-QUESTION") {
            text("The number is 42")
        } else {
            text("Remembered 42")
        }
    });
    let a = actor(script, Setup::default());
    let ended = a.prompt("swarm test");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let results = a
        .requests()
        .iter()
        .flat_map(p_session_support::tool_results)
        .map(|r| match r {
            Value::String(text) => text,
            other => other.to_string(),
        })
        .collect::<Vec<_>>();
    let output = results.join("\n");
    let line = |prefix: &str| -> Value {
        let found = output
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{prefix} ")))
            .unwrap_or_else(|| panic!("no {prefix} line in {output}"));
        serde_json::from_str(found).unwrap()
    };
    assert_eq!(
        line("read"),
        json!({"text": "[root] first line\n", "version": 1}),
        "{output}"
    );
    assert_eq!(line("claim")["owner"], "root", "{output}");
    assert_eq!(line("list"), json!([["task one", "claimed", "root"]]), "{output}");
    assert_eq!(line("inbox"), json!([["root", "hello me"]]), "{output}");
    assert_eq!(
        line("collect"),
        json!([{"status": "succeeded", "verification": "unverified", "value": "Remembered 42"}]),
        "{output}"
    );
    let asked = line("ask");
    assert_eq!(
        (&asked["status"], &asked["value"]),
        (&json!("succeeded"), &json!("The number is 42")),
        "{output}"
    );
    assert_eq!(line("agents"), json!([]), "{output}");
    // The persistent child registered itself when it started.
    assert!(
        line("registered").as_array().unwrap().iter().any(|a| a == "keeper"),
        "{output}"
    );
}
