//! Subagents in the typed-agent API (native-host.ts keeps `rlm-child@1` tasks in the one task journal):
//! `agents.inspect`, `agents.result` and `agents.cancel` accept a subagent's id; `agents.status` carries the shell
//! jobs and REPL tool calls the caller may see (session-worker.ts `statusExtras`); and a root-level subagent's end is
//! attributed to the cell that returns it, which collapse on return then rewrites with the task's line
//! (`onTaskEnd` -> `contextControl.taskEnded`).

mod p_session_support;

use p_session_support::{Setup, actor, text, tool, tool_results, user_texts};
use serde_json::Value;

fn first_user(request: &Value) -> String {
    user_texts(request)
        .iter()
        .filter_map(Value::as_str)
        .find(|text| !text.starts_with("[clock]"))
        .unwrap_or_default()
        .to_string()
}

const CELL: &str = "import asyncio, json\nh = await rlm.spawn(\"CHILD: answer\", name=\"c\")\nfor _ in range(12000):\n    try:\n        r = (await rlm.collect([h]))[0]\n        break\n    except RuntimeError:\n        await asyncio.sleep(0.01)\ni = await agents.inspect(h.id)\nprint(\"INSPECT\", i[\"id\"] == h.id, i[\"definition\"], i[\"state\"])\nres = await agents.result(h.id)\nprint(\"RESULT\", res[\"status\"], len(res[\"value\"]))\nprint(\"CANCEL\", json.dumps(await agents.cancel(h.id)))\nj = await bash(\"echo extras\", yield_after=0)\nj = await j.result(wait=60)\ns = await agents.status()\nprint(\"JOBS\", json.dumps([[x[\"id\"] == j.id, x[\"lane\"], x[\"command\"], x[\"status\"], x[\"tail\"]] for x in s[\"jobs\"]]))\nprint(\"TOOLS\", json.dumps(s[\"toolCalls\"]))\nprint(\"ID\", h.id)\nprint(res[\"value\"])";

#[test]
fn session_core_agents_api_answers_for_subagents_and_status_carries_jobs() {
    let a = actor(
        Box::new(|request, _| {
            if first_user(request).starts_with("CHILD") {
                return text(&"V".repeat(2000));
            }
            match tool_results(request).len() {
                0 => tool("toolu_1", CELL),
                1 => tool("toolu_2", "print('second cell')"),
                _ => text("DONE"),
            }
        }),
        Setup {
            vars: vec![("ULTRON_TOOL_ROUNDS_NUDGE", "0".into())],
            ..Setup::default()
        },
    );
    a.prompt("ROOT: delegate");
    let requests = a.requests();
    let roots: Vec<&Value> = requests
        .iter()
        .filter(|r| !first_user(r).starts_with("CHILD"))
        .collect();
    let first = tool_results(roots[1])[0].to_string();
    assert!(first.contains("INSPECT True rlm-child@1 completed"), "{first}");
    assert!(first.contains("RESULT succeeded 2000"), "{first}");
    assert!(first.contains(r#"CANCEL {\"cancelled\": false}"#), "{first}");
    assert!(
        first.contains(r#"JOBS [[true, \"main\", \"echo extras\", \"completed\", \"extras\\n\"]]"#),
        "{first}"
    );
    assert!(first.contains("TOOLS []"), "{first}");
    let start = first.find("ID ").unwrap() + 3;
    let id: String = first[start..].chars().take_while(|c| *c != '\\' && *c != '"').collect();
    // The answer after the cell is the boundary: the request after it carries the cell collapsed with the task's line.
    let last = roots.last().unwrap().to_string();
    assert!(last.contains(&format!("↳ task {id} rlm-child@1")), "{last}");
    // The child's one response: 10 input and 5 output tokens of claude-sonnet-4-5 ($0.000105).
    assert!(
        last.contains("succeeded cost=$0.0001 · full result: await agents.result("),
        "{last}"
    );
}
