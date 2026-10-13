//! A52 (partial): `ctx.forget` never removes the current user turn or pinned items, an extension can observe the
//! edit, and compaction keeps notes and pins.
//! Ported from "history, summarize, guards, pin, note, and state follow the branch" of
//! packages/coding-agent/test/ultron-context-control.test.ts: the `ctx.*` calls run as cells in the real runtime.py
//! kernel (crates/kernel) against `kernel_apis::ContextControl`, over a branch that stands in for the lane (prompts
//! append the user and assistant entries a lane would), and the next request's context is the branch through
//! `apply_context_edits`. Edit events are the module's `entry_added` observations.
//! Not ported: the note projection into the next request ("Note (ctx.note): ...") and the `/rlm` context panel, and
//! all of ultron-a52-context-guards.test.ts: `ContextControl` is not wired into the session's lanes
//! (crates/session-core/src/compose.rs), extensions have no `pi.events` bus, and compaction does not call
//! `kept_across_compaction`.

mod common;

use common::{kernel_in, null_handler, run};
use kernel::Status;
use kernel_apis::KernelApis;
use kernel_apis::context_control::{
    CONTEXT_OMITTED_CUSTOM_TYPE, ContextBranches, ContextControl, apply_context_edits, message_text,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// The lane's committed branch.
#[derive(Default)]
struct Lane {
    entries: Mutex<Vec<Value>>,
}

impl Lane {
    fn push(&self, entry: Value) -> String {
        let mut entries = self.entries.lock().unwrap();
        let seq = entries.len() as i64 + 1;
        let id = format!("e{seq}");
        let mut entry = entry;
        entry["id"] = id.clone().into();
        entry["parentId"] = entries.last().map_or(Value::Null, |e| e["id"].clone());
        entry["seq"] = seq.into();
        entry["timestamp"] = seq.into();
        entries.push(entry);
        id
    }

    fn all(&self) -> Vec<Value> {
        self.entries.lock().unwrap().clone()
    }

    /// `lane.prompt`: the user message and the scripted answer.
    fn prompt(&self, text: &str, answer: &str) {
        self.push(json!({"type": "message", "message": {"role": "user", "content": text, "timestamp": 0}}));
        self.push(json!({"type": "message", "message": {"role": "assistant",
            "content": [{"type": "text", "text": answer}], "stopReason": "stop", "timestamp": 0}}));
    }

    /// The message texts the next request carries (context edits applied, omitted entries dropped).
    fn context(&self) -> Vec<String> {
        apply_context_edits(&self.all())
            .into_iter()
            .map(|applied| applied.entry)
            .filter(|e| e["type"] == "message" && e["customType"] != CONTEXT_OMITTED_CUSTOM_TYPE)
            .map(|e| message_text(&e["message"]))
            .collect()
    }
}

impl ContextBranches for Lane {
    fn entries(&self, _lane: &str) -> Result<Vec<Value>, String> {
        Ok(self.all())
    }
    fn append_custom(&self, _lane: &str, custom_type: &str, data: &Value) -> Result<String, String> {
        Ok(self.push(json!({"type": "custom", "customType": custom_type, "data": data})))
    }
    fn tip_id(&self, _lane: &str) -> Result<Option<String>, String> {
        Ok(self.all().last().and_then(|e| e["id"].as_str().map(str::to_string)))
    }
}

#[test]
fn acceptance_a52_ctx_history_summarize_guards_pin_note_and_state_follow_the_branch_ported_part() {
    let lane = Arc::new(Lane::default());
    let control = Arc::new(ContextControl::new(lane.clone()));
    let handler = KernelApis::new(null_handler()).with(control.clone());
    let dir = tempfile::tempdir().unwrap();
    let kernel = kernel_in(dir.path(), Arc::new(handler));
    let events: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = Arc::new(Mutex::new(0usize));
    // The harness reports each committed entry to the module; context edits come back as observable events.
    let observe = || {
        let entries = lane.all();
        let mut seen = seen.lock().unwrap();
        for entry in &entries[*seen..] {
            if let Some(event) = control.entry_added("main", entry) {
                events.lock().unwrap().push(event);
            }
        }
        *seen = entries.len();
    };
    let cell = |code: &str| {
        let out = run(&kernel, code);
        assert_eq!(out.status, Status::Ok, "{code}: {out:?}");
        observe();
        out.stdout
    };

    lane.prompt("first question SECRET_ONE", "first answer");
    lane.prompt("second question", "answer 1");
    observe();
    let items: Value = serde_json::from_str(&cell(
        "import json\nh = await ctx.history(limit=50)\nitems = h['items']\nprint(json.dumps(items))",
    ))
    .unwrap();
    let kinds: Vec<&str> = items
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["user", "assistant", "user", "assistant"]);
    let id = |n: usize| items[n]["id"].as_str().unwrap().to_string();
    let (first_q, first_a, second_q, second_a) = (id(0), id(1), id(2), id(3));
    assert_eq!(items[2]["protected"], true);
    assert_eq!(
        cell("try:\n    await ctx.history(kinds=['bogus'])\nexcept Exception as e:\n    print('ERR', e)"),
        "ERR kinds must be a list of: user, assistant, tool, custom, note, summary, compaction\n"
    );

    // The current user message is refused; unknown ids too, all or nothing.
    let refused = cell(&format!(
        "try:\n    await ctx.forget(['{first_q}', '{second_q}'], 'x')\nexcept Exception as e:\n    print('ERR', e)"
    ));
    assert!(
        refused.starts_with("ERR ") && refused.contains("current user turn"),
        "{refused}"
    );
    let unknown = cell("try:\n    await ctx.forget(['nope'], 'x')\nexcept Exception as e:\n    print('ERR', e)");
    assert!(unknown.contains("not in the model's context"), "{unknown}");
    assert_eq!(
        cell("print(all(i['state'] == 'visible' for i in (await ctx.history(limit=50))['items']))"),
        "True\n"
    );

    // Pin the second answer: summarize and forget refuse it.
    assert_eq!(
        cell(&format!(
            "r = await ctx.pin('{second_a}')\nprint(r['pinned'], r['changed'])"
        )),
        "True True\n"
    );
    let pinned = cell(&format!(
        "try:\n    await ctx.summarize(['{second_a}'], 'x')\nexcept Exception as e:\n    print('ERR', e)"
    ));
    assert!(pinned.contains("pinned"), "{pinned}");

    let summary: Value = serde_json::from_str(&cell(&format!(
        "print(json.dumps((await ctx.summarize(['{first_q}', '{first_a}'], 'They asked one thing.'))['summarized']))"
    )))
    .unwrap();
    assert_eq!(summary, json!([first_q, first_a]));
    cell("await ctx.note('REMEMBER_THIS')");
    let items: Value =
        serde_json::from_str(&cell("print(json.dumps((await ctx.history(limit=50))['items']))")).unwrap();
    let state_of = |id: &str| items.as_array().unwrap().iter().find(|i| i["id"] == id).unwrap()["state"].clone();
    assert_eq!(state_of(&first_q), "summarized");
    assert_eq!(state_of(&first_a), "summarized");
    let last = items.as_array().unwrap().last().unwrap();
    assert_eq!(
        (&last["kind"], &last["preview"]),
        (&json!("note"), &json!("REMEMBER_THIS"))
    );
    let got: Value = serde_json::from_str(&cell(&format!("print(json.dumps(await ctx.get('{first_q}')))"))).unwrap();
    assert!(got["text"].as_str().unwrap().contains("SECRET_ONE"));
    assert!(got["visible_text"].as_str().unwrap().contains("They asked one thing."));

    lane.prompt("third question", "answer 2");
    let third = lane.context();
    let joined = third.join("\n");
    assert!(!joined.contains("SECRET_ONE"));
    assert!(!joined.contains("first answer"));
    assert!(third[0].contains("They asked one thing."));
    assert!(third.contains(&"answer 1".to_string()));
    let events = events.lock().unwrap().clone();
    let sources: Vec<&str> = events.iter().map(|e| e["source"].as_str().unwrap()).collect();
    assert_eq!(sources, ["ctx.summarize"]);
    assert_eq!(
        events[0]["edits"],
        json!([{"targetId": first_q, "action": "replace"}, {"targetId": first_a, "action": "omit"}])
    );
    assert_eq!(events[0]["lane"], "main");

    // `ctx.state` has no Python wrapper (the TS test, too, asks the module): the `/rlm` panel's read.
    let state = control
        .handle_request("ctx.state", &serde_json::Map::new(), "main")
        .unwrap();
    assert_eq!(state["forgottenCount"], 2);
    assert_eq!(state["pinnedCount"], 1);
    assert_eq!(state["notes"], 1);
    assert_eq!(state["collapsedCount"], 0);

    // Unpinning makes it editable again.
    assert_eq!(
        cell(&format!(
            "r = await ctx.unpin('{second_a}')\nprint(r['pinned'], r['changed'])"
        )),
        "False True\n"
    );
    assert_eq!(
        cell(&format!(
            "print(json.dumps((await ctx.forget(['{second_a}'], 'done'))['forgotten']))"
        )),
        format!("[\"{second_a}\"]\n")
    );
}
