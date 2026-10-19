//! A46: the combined system performs the full demonstration without manual state repair. Every case of
//! packages/coding-agent/test/ultron-a46-demo.test.ts with its literal expectations:
//! - "aborting mid-invoke cancels the child and the session keeps working";
//! - "fix, review, retain, and improve with real runtime effects": diagnose (full-data computation in the kernel, a
//!   predict call and an `rlm-child` investigator), an isolated fix with host-run checks, a gate and a typed review,
//!   a retained reviewer continued with a correction, a slow background child stopped while other work completes,
//!   a refinement that reaches only the activated held-out review, and the records read back through RPC `inspect`;
//! - "failure variants never become verified completion".
//!
//! The real binary runs in RPC mode against a scripted provider, so every model decision is fixed while every
//! runtime effect is real: the kernel, the sandboxed shell, the typed-agent host and its lanes, retained instances,
//! background jobs, the release gate, artifacts, refinements, experiments and the inspector of the shipped
//! session stack. The TS scripted reply `{delayMs: 20000}` of a slow child is [`Reply::Hang`]: the response is held
//! until the request is cancelled. The TS abort case waits 3 s before aborting; here it waits for the slow child's
//! request to reach the provider.

mod common;

use cli::rpc::client::RpcClient;
use common::{HANG, Install, Provider, Reply, Request};
use serde_json::{Map, Value, json};

const REFINEMENT: &str = "Always check the empty input case.";
const ROWS: usize = 200_000;

struct Diagnostics {
    csv: String,
    fails: usize,
    top: String,
    top_count: usize,
}

/// The diagnostics log and its counts, computed here from the rows themselves (`diagnostics()` of the TS test).
fn diagnostics() -> Diagnostics {
    let locations = ["calc.py:average", "calc.py:total", "io.py:read", "calc.py:average"];
    let mut csv = String::from("id,status,location\n");
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for index in 0..ROWS {
        let fail = index % 7 == 0;
        let location = locations[index % locations.len()];
        csv.push_str(&format!(
            "row-{index},{},{location}\n",
            if fail { "fail" } else { "ok" }
        ));
        if fail {
            match counts.iter_mut().find(|(known, _)| *known == location) {
                Some((_, count)) => *count += 1,
                None => counts.push((location, 1)),
            }
        }
    }
    let &(top, top_count) = counts.iter().max_by_key(|(_, count)| *count).unwrap();
    Diagnostics {
        fails: counts.iter().map(|(_, count)| count).sum(),
        top: top.into(),
        top_count,
        csv,
    }
}

const CODE_DIAGNOSE: &str = r#"import csv, json
rows = list(csv.DictReader(open('diagnostics.csv')))
fails = [r for r in rows if r['status'] == 'fail']
counts = {}
for r in fails:
    counts[r['location']] = counts.get(r['location'], 0) + 1
top = max(counts, key=counts.get)
state['diagnosis'] = {'rows': len(rows), 'fails': len(fails), 'top': top, 'top_count': counts[top]}
await agents.register({'id': 'classify-severity', 'version': '1', 'strategy': 'predict', 'instructions': 'Classify the defect severity.', 'inputSchema': {'type': 'object', 'required': ['report'], 'properties': {'report': {'type': 'string'}}}, 'outputSchema': {'type': 'object', 'required': ['severity'], 'additionalProperties': False, 'properties': {'severity': {'enum': ['low', 'high']}}}, 'maxRepairs': 1, 'inputDescription': 'report', 'outputDescription': '{severity}'})
severity = await agents.invoke('classify-severity@1', {'report': top})
investigation = await agents.invoke('rlm-child@1', {'prompt': 'INVESTIGATE ' + top})
print('RESULT ' + json.dumps({'diagnosis': state['diagnosis'], 'severity': severity, 'investigation': investigation}))"#;

const CODE_FIX_AND_REVIEW: &str = r#"import json, shutil, tempfile, hashlib, pathlib
work = tempfile.mkdtemp(prefix='ultron-candidate-')
shutil.copytree('repo', work + '/repo')
calc = pathlib.Path(work + '/repo/calc.py')
calc.write_text(calc.read_text().replace('len(values) - 1', 'len(values)'))
base = await bash('cd repo && python3 test_calc.py')
cand = await bash('cd ' + work + '/repo && python3 test_calc.py')
fixture = hashlib.sha256(open('repo/test_calc.py', 'rb').read()).hexdigest()
await gates.define('demo-unit', [{'name': 'unit', 'required': True}], fixture)
verdict = lambda r: 'passed' if r['exit_code'] == 0 else 'failed'
decision = await gates.compare('demo-unit', {'variant': 'baseline', 'fixture_hash': fixture, 'results': {'unit': verdict(base)}}, {'variant': 'candidate', 'fixture_hash': fixture, 'results': {'unit': verdict(cand)}})
diff = (await bash('diff -u repo/calc.py ' + work + '/repo/calc.py'))['output']
review = await agents.spawn('correctness-reviewer@1', {'request': 'Review this change:\n' + diff})
review_result = await review.result()
state['review_task'] = review.id
state['review_result'] = review_result
art = await rlm.host_request('artifacts.put', {'text': diff, 'options': {'mediaType': 'text/x-diff', 'label': 'candidate fix'}})
print('RESULT ' + json.dumps({'base': verdict(base), 'candidate': verdict(cand), 'gate': decision['decision'], 'review': review_result, 'review_task': review.id, 'artifact': art['id']}))"#;

fn code_retain_and_improve() -> String {
    format!(
        r#"import json
inst = await instances.retain(state['review_task'])
follow = await instances.invoke(inst['id'], {{'request': 'CORRECTION: check the empty input case'}})
follow_result = await agents.result(follow['task_id'])
original_after = await agents.result(state['review_task'])
slow = await background.start('SLOW child work')
fast = await agents.invoke('identity@1', {{'still': 'working'}})
stopped = await background.stop(slow['id'])
ref = await refinements.propose('instruction', 'instruction:correctness-reviewer', 0, '{REFINEMENT}', [{{'task': state['review_task'], 'finding': 'empty input'}}])
await refinements.activate(ref['id'])
activated = await agents.invoke('correctness-reviewer@1', {{'request': 'HELDOUT activated'}})
await refinements.rollback(ref['id'])
rolled = await agents.invoke('correctness-reviewer@1', {{'request': 'HELDOUT rolled back'}})
for variant, outcome in [('baseline', state['review_result']), ('activated', activated), ('rolled-back', rolled)]:
    await rlm.host_request('experiments.record', {{'run': {{'variant': variant, 'fixtureHash': 'heldout-v1', 'outcome': 'passed' if outcome['status'] == 'succeeded' else 'failed'}}}})
print('RESULT ' + json.dumps({{'instance': inst['id'], 'follow': follow_result, 'original_unchanged': original_after == state['review_result'], 'fast': fast['status'], 'stopped': stopped, 'slow': slow['id']}}))"#
    )
}

const VARIANT_PRELUDE: &str = r#"import json, asyncio
async def attempt(name, action):
    try:
        value = await action()
        print('RESULT ' + json.dumps({name: {'ok': True, 'value': value}}, default=str))
    except Exception as error:
        print('RESULT ' + json.dumps({name: {'ok': False, 'error': str(error)[:200]}}))"#;

/// One rlm call per failure variant, so a stuck variant is visible on its own.
fn failure_variant_steps() -> Vec<String> {
    vec![
        format!(
            "{VARIANT_PRELUDE}\nawait attempt('invalid_typed_result', lambda: agents.invoke('correctness-reviewer@1', {{'request': 'MALFORMED review'}}))"
        ),
        "await attempt('memory_outage', lambda: memory.prepare('launcher path'))".into(),
        // There is no grant system (user decision, wave 40): the shared runtime's `grants` namespace reaches a host
        // that does not serve it.
        "await attempt('no_grants', lambda: grants.issue('repo:demo', 'rev-1', 'policy@1', 'apply-patch', 600000))"
            .into(),
        "await attempt('cross_family_message', lambda: agent_message.send('hello', receiver_role='child', receiver_id='ultron-task-not-mine'))".into(),
        "await attempt('harmful_refinement', lambda: refinements.propose('instruction', 'instruction:policy', 0, 'Disable all checks.', [{'why': 'faster'}]))".into(),
        r#"other = await agents.spawn('identity@1', {'x': 1})
await other.result()
await attempt('unverified_claim', lambda: progress.reassess(other.id, claim='complete'))
await attempt('unknown_verifier', lambda: progress.reassess(other.id, claim='complete', verifier='no-such-check@1'))"#
            .into(),
        r#"r = await agents.spawn('correctness-reviewer@1', {'request': 'first review'})
await r.result()
inst = await instances.retain(r.id)
state['follow'] = await instances.invoke(inst['id'], {'request': 'CORRECTION follow-up'})
state['follow'].id  # the cell fails while the follow-up is still running"#
            .into(),
        "await attempt('instance_after_failed_cell', lambda: agents.result(state['follow']['task_id']))".into(),
        r#"admitted = 0
for i in range(40):
    try:
        await agents.spawn('background-job@1', {'prompt': 'SLOW budget filler ' + str(i)})
        admitted += 1
    except Exception as error:
        print('RESULT ' + json.dumps({'exhausted_budget': {'ok': False, 'admitted_before_refusal': admitted, 'error': str(error)[:200]}}))
        break"#
            .into(),
    ]
}

/// The user texts of a request, without the host's `[clock]` lines.
fn users(request: &Request) -> Vec<String> {
    request
        .users()
        .into_iter()
        .filter(|t| !t.starts_with("[clock]"))
        .collect()
}

/// The first and last user text of a request (`firstUser` / `lastUser` of the TS scripted provider).
fn first_user(request: &Request) -> String {
    users(request).first().cloned().unwrap_or_default()
}

fn last_user(request: &Request) -> String {
    users(request).last().cloned().unwrap_or_default()
}

fn is_review(request: &Request) -> bool {
    first_user(request).starts_with("Review the supplied change for correctness")
}

/// `rootScript(steps)`: the scripted model of every request the demonstration makes.
fn root_script(steps: Vec<String>) -> impl Fn(&Request) -> Reply + Send + Sync + 'static {
    move |request| {
        if request.system().contains("You are a typed function") {
            return Reply::Text(r#"{"severity": "high"}"#.into());
        }
        let first = first_user(request);
        if first.starts_with("INVESTIGATE") {
            return Reply::Text("The average divides by len(values) - 1.".into());
        }
        if first.starts_with("SLOW") {
            return Reply::Hang("slow work finished".into());
        }
        if is_review(request) {
            let last = last_user(request);
            if last.contains("MALFORMED") {
                return Reply::Text("looks fine to me".into());
            }
            if last.contains("CORRECTION") {
                return Reply::Text(
                    json!({
                        "outcome": "findings",
                        "findings": [
                            { "file": "calc.py", "line": 2, "severity": "medium", "explanation": "average([]) divides by zero" },
                        ],
                    })
                    .to_string(),
                );
            }
            return Reply::Text(json!({ "outcome": "no_findings", "findings": [] }).to_string());
        }
        if first.starts_with("DEMO") {
            // `turn`: the assistant messages the conversation already holds.
            return match steps.get(request.messages("assistant").len()) {
                Some(code) => Reply::Rlm(code.clone()),
                None => Reply::Text("DEMO COMPLETE".into()),
            };
        }
        panic!("Unscripted request: {}", first.chars().take(80).collect::<String>());
    }
}

/// `start(steps, files)`: private agent, server and project directories, the files, and an RPC client with an
/// unreachable memory server (a Hindsight outage).
fn start(steps: Vec<String>, files: &[(&str, &str)]) -> (Provider, Install, RpcClient) {
    let provider = Provider::start(root_script(steps));
    let install = Install::new(&provider);
    std::fs::create_dir_all(install.project().join("repo")).unwrap();
    for (name, content) in files {
        std::fs::write(install.project().join(name), content).unwrap();
    }
    let client = install.rpc(&["--no-session"], &[("ULTRON_HINDSIGHT_URL", "http://127.0.0.1:9")]);
    (provider, install, client)
}

fn inspect(client: &mut RpcClient, request: &str, payload: Value) -> Value {
    let response = client
        .send(json!({ "type": "inspect", "request": request, "payload": payload }))
        .unwrap();
    RpcClient::data(&response).unwrap_or_else(|error| panic!("inspect {request} failed: {error}"))
}

/// Every `RESULT {...}` line printed by the scripted rlm cells, in order.
fn results(client: &mut RpcClient) -> Vec<Value> {
    common::rpc_tool_results(&client.get_messages().unwrap())
        .iter()
        .flat_map(|text| text.lines().map(str::to_string).collect::<Vec<_>>())
        .filter_map(|line| {
            line.strip_prefix("RESULT ")
                .map(|json| serde_json::from_str(json).unwrap())
        })
        .collect()
}

#[test]
fn acceptance_a46_aborting_mid_invoke_cancels_the_child_and_the_session_keeps_working() {
    let (provider, _install, mut client) = start(
        vec!["await agents.invoke('rlm-child@1', {'prompt': 'SLOW child under invoke'})".into()],
        &[],
    );
    client.mark();
    let accepted = client
        .send(json!({ "type": "prompt", "message": "DEMO: invoke a slow child" }))
        .unwrap();
    RpcClient::data(&accepted).unwrap();
    common::wait_until("the slow child's model request", || {
        provider.requests().iter().any(|r| first_user(r).starts_with("SLOW"))
    });
    RpcClient::data(&client.send(json!({ "type": "abort" })).unwrap()).unwrap();
    client.wait_for(&|line| line["type"] == "agent_settled", HANG).unwrap();
    client.mark();
    // The worker survived: a new prompt is answered (the conversation already holds an assistant turn, so the
    // scripted model has no further cell).
    let before = provider.requests().len();
    client.prompt_and_wait("DEMO: are you still there?", HANG).unwrap();
    assert!(provider.requests().len() > before);
    assert_eq!(
        client.get_last_assistant_text().unwrap().as_deref(),
        Some("DEMO COMPLETE")
    );
    // The child was cancelled with the cell, not left running.
    let status = inspect(&mut client, "agents.status", json!({}));
    let children: Vec<&Value> = status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|task| task["definition"] == "rlm-child@1")
        .collect();
    assert_eq!(children.len(), 1, "{status}");
    assert_eq!(children[0]["state"], "cancelled", "{status}");
    client.stop();
}

#[test]
fn acceptance_a46_fix_review_retain_and_improve_with_real_runtime_effects() {
    let data = diagnostics();
    let (provider, install, mut client) = start(
        vec![
            CODE_DIAGNOSE.into(),
            CODE_FIX_AND_REVIEW.into(),
            code_retain_and_improve(),
        ],
        &[
            ("diagnostics.csv", &data.csv),
            (
                "repo/calc.py",
                "def average(values):\n    return sum(values) / (len(values) - 1)\n",
            ),
            (
                "repo/test_calc.py",
                "from calc import average\nassert average([2, 4, 6]) == 4, average([2, 4, 6])\nprint('ok')\n",
            ),
        ],
    );
    client
        .prompt_and_wait("DEMO: fix the failing average in repo/ using diagnostics.csv", HANG)
        .unwrap();
    assert_eq!(
        client.get_last_assistant_text().unwrap().as_deref(),
        Some("DEMO COMPLETE")
    );
    let results = results(&mut client);
    assert_eq!(results.len(), 3, "{results:?}");
    let (diagnose, fix, improve) = (&results[0], &results[1], &results[2]);

    // 1-2. Full-data computation in the kernel, a predict call, and an RLM investigator.
    assert_eq!(
        diagnose["diagnosis"],
        json!({ "rows": ROWS, "fails": data.fails, "top": data.top, "top_count": data.top_count })
    );
    assert_eq!(diagnose["severity"]["status"], "succeeded", "{diagnose}");
    assert_eq!(diagnose["severity"]["value"], json!({ "severity": "high" }));
    assert_eq!(diagnose["investigation"]["status"], "succeeded", "{diagnose}");
    // The dataset stays in the kernel: no model request carries its rows.
    let requests = provider.requests();
    for request in &requests {
        assert!(!request.raw.contains("row-199999"));
        assert!(request.raw.len() < 200_000);
    }
    assert!(requests.iter().any(|r| r.system().contains("You are a typed function")));
    assert!(
        requests
            .iter()
            .any(|r| first_user(r).starts_with("INVESTIGATE calc.py:average"))
    );

    // 3-4. Isolated candidate, host-run baseline and candidate checks, gate, and review.
    assert_eq!(fix["base"], "failed");
    assert_eq!(fix["candidate"], "passed");
    assert_eq!(fix["gate"], "passed");
    assert_eq!(fix["review"]["status"], "succeeded");
    assert_eq!(fix["review"]["value"]["outcome"], "no_findings");
    assert_eq!(fix["review"]["verification"], "unverified");
    assert_eq!(fix["artifact"].as_str().unwrap().len(), 64);
    assert!(
        std::fs::read_to_string(install.project().join("repo/calc.py"))
            .unwrap()
            .contains("len(values) - 1")
    );

    // 5-6. The retained reviewer continues with a correction; the first result is unchanged; a slow child is
    // stopped while other work completes.
    assert_eq!(improve["follow"]["status"], "succeeded", "{improve}");
    assert_eq!(improve["follow"]["value"]["outcome"], "findings", "{improve}");
    assert_eq!(improve["original_unchanged"], true);
    assert_eq!(improve["fast"], "succeeded");
    assert_eq!(improve["stopped"], json!({ "cancelled": true }));
    // The follow-up ran on the reviewer's own lane: its conversation holds the first review request.
    let follow_ups: Vec<&Request> = requests
        .iter()
        .filter(|r| is_review(r) && last_user(r).contains("CORRECTION"))
        .collect();
    assert_eq!(follow_ups.len(), 1);
    assert!(users(follow_ups[0]).iter().any(|u| u.contains("Review this change")));

    // 8. The refinement reached the activated held-out run only.
    let reviewer_prompts: Vec<String> = requests.iter().filter(|r| is_review(r)).map(last_user).collect();
    let find = |text: &str| {
        reviewer_prompts
            .iter()
            .find(|p| p.contains(text))
            .unwrap_or_else(|| panic!("no review of {text}: {reviewer_prompts:?}"))
            .clone()
    };
    assert!(find("HELDOUT activated").contains(REFINEMENT));
    assert!(!find("HELDOUT rolled back").contains(REFINEMENT));

    // 9. Decisions and costs reconstruct from the same records through the inspector.
    let status = inspect(&mut client, "agents.status", json!({}));
    let state_of = |id: &Value| {
        status["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| &task["id"] == id)
            .map(|task| task["state"].clone())
    };
    assert_eq!(state_of(&fix["review_task"]), Some(json!("completed")));
    assert_eq!(state_of(&improve["slow"]), Some(json!("cancelled")));
    let controls = status["controls"].as_object().unwrap();
    assert!(
        !controls.is_empty() && controls.values().all(|enabled| enabled == false),
        "{controls:?}"
    );
    let instances = inspect(&mut client, "instances.list", json!({}));
    let instances = instances.as_array().unwrap();
    assert_eq!(instances.len(), 1, "{instances:?}");
    assert_eq!(instances[0]["id"], improve["instance"]);
    assert_eq!(instances[0]["task_id"], fix["review_task"]);
    assert_eq!(instances[0]["invocations"].as_array().unwrap().len(), 1);
    assert!(instances[0]["invocations"][0].is_object());
    let history = inspect(&mut client, "gates.history", json!({ "gate_id": "demo-unit" }));
    let decisions: Vec<&Value> = history["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| &a["decision"])
        .collect();
    assert_eq!(decisions, [&json!("passed")]);
    let experiments = inspect(&mut client, "experiments.list", json!({}));
    let variants: Vec<&Value> = experiments
        .as_array()
        .unwrap()
        .iter()
        .map(|run| &run["variant"])
        .collect();
    assert_eq!(
        variants,
        [&json!("baseline"), &json!("activated"), &json!("rolled-back")]
    );
    client.stop();
}

fn ok_value(variants: &Map<String, Value>, name: &str) -> Value {
    let variant = &variants[name];
    assert_eq!(variant["ok"], true, "{name}: {variant}");
    variant["value"].clone()
}

fn refused(variants: &Map<String, Value>, name: &str) {
    assert_eq!(variants[name]["ok"], false, "{name}: {}", variants[name]);
}

#[test]
fn acceptance_a46_failure_variants_never_become_verified_completion() {
    let (_provider, _install, mut client) = start(failure_variant_steps(), &[]);
    client.prompt_and_wait("DEMO: exercise failure variants", HANG).unwrap();
    let mut variants = Map::new();
    for result in results(&mut client) {
        variants.extend(result.as_object().unwrap().clone());
    }
    // Invalid typed result: the reviewer's prose is rejected, never reported as a clean review.
    let invalid = ok_value(&variants, "invalid_typed_result");
    assert_eq!(invalid["status"], "failed", "{invalid}");
    assert_eq!(invalid["verification"], "unverified");
    // Optional-service outage is explicit rather than an empty success.
    refused(&variants, "memory_outage");
    refused(&variants, "no_grants");
    assert!(
        variants["no_grants"]["error"]
            .as_str()
            .unwrap()
            .contains("grants.issue"),
        "{variants:?}"
    );
    refused(&variants, "cross_family_message");
    assert_eq!(
        variants["cross_family_message"]["error"],
        "Receiver is not a direct child of the sender"
    );
    refused(&variants, "harmful_refinement");
    assert_eq!(variants["harmful_refinement"]["error"], "Forbidden refinement target");
    for claim in ["unverified_claim", "unknown_verifier"] {
        assert!(variants.contains_key(claim), "{variants:?}");
        assert!(
            !variants[claim].to_string().contains("\"verified\""),
            "{claim}: {}",
            variants[claim]
        );
    }
    refused(&variants, "exhausted_budget");
    let exhausted = &variants["exhausted_budget"];
    assert!(
        exhausted["error"]
            .as_str()
            .unwrap()
            .contains("admitted-task limit exceeded"),
        "{exhausted}"
    );
    assert!(
        exhausted["admitted_before_refusal"].as_u64().unwrap() < 40,
        "{exhausted}"
    );
    // Work started through a host module outlives a cell that fails right after starting it.
    let follow = ok_value(&variants, "instance_after_failed_cell");
    assert_eq!(follow["status"], "succeeded", "{follow}");
    assert_eq!(follow["value"]["outcome"], "findings", "{follow}");
    client.stop();
}
