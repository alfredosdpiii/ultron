//! A56 (and A51 in the live root lane): collapse on return rewrites only the newest cell output, so the cached
//! prefix before it survives. Ported from the ultron-async-cli.test.ts case of that name against the `ultron` binary
//! in RPC mode with a scripted provider: the root invokes `rlm-child@1`, whose 2,000-character answer the cell
//! prints; the model's next answer is the boundary after which the request carries that cell output collapsed
//! (`kernel_apis::context_control` wired into the root lane by `session_core::core::context_lane`), while every other
//! request extends the previous one byte for byte.

mod common;

use common::{HANG, Install, Provider, Reply, Request};

const ROOT: &str = "COLLAPSE: invoke a child and continue";

fn first_user(request: &Request) -> String {
    request
        .users()
        .into_iter()
        .find(|u| !u.starts_with("[clock]"))
        .unwrap_or_default()
}

#[test]
fn acceptance_a56_collapse_on_return_rewrites_only_the_newest_cell_output() {
    let provider = Provider::start(|request: &Request| {
        let first = first_user(request);
        if first.starts_with("CHILD") {
            return Reply::Text("V".repeat(2000));
        }
        if first != ROOT {
            return Reply::Text("unexpected".into());
        }
        match request.messages("assistant").len() {
            0 => Reply::Rlm("r = await agents.invoke(\"rlm-child@1\", {\"prompt\": \"CHILD big\"})\nprint(r)".into()),
            1 => Reply::Rlm("print('second cell')".into()),
            2 => Reply::Rlm("print('third cell')".into()),
            _ => Reply::Text("DONE".into()),
        }
    });
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[]);
    client.prompt_and_wait(ROOT, HANG).unwrap();
    assert_eq!(client.get_last_assistant_text().unwrap().as_deref(), Some("DONE"));
    let bodies: Vec<Vec<String>> = provider
        .requests()
        .iter()
        .filter(|r| first_user(r) == ROOT)
        .map(|r| {
            r.body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m.to_string())
                .collect()
        })
        .collect();
    assert_eq!(bodies.len(), 4);
    // The cell output that returned the task: the first tool result (after the system message, the clock line, the
    // prompt and the assistant's call; TypeScript has no clock line there, so its index is 3).
    let target = bodies[1].iter().position(|m| m.contains("\"role\":\"tool\"")).unwrap();
    assert_eq!(target, 4);
    assert!(bodies[1][target].contains(&"V".repeat(1000)), "{}", bodies[1][target]);
    let collapsed_at = bodies
        .iter()
        .position(|body| body.get(target).is_some_and(|m| m.contains("collapsed on return")))
        .expect("a request carries the collapsed cell output");
    assert!(collapsed_at > 1, "{collapsed_at}");
    assert!(!bodies[collapsed_at][target].contains(&"V".repeat(1000)));
    for index in 1..bodies.len() {
        let (previous, current) = (&bodies[index - 1], &bodies[index]);
        if index == collapsed_at {
            // The one rewrite: everything before the collapsed output is the same, and the output was among the
            // previous request's newest messages (the edit lands at the first boundary after the model's answer).
            assert_eq!(current[..target], previous[..target]);
            assert!(previous.len() - target <= 3, "{}", previous.len());
        } else {
            assert_eq!(current[..previous.len()], previous[..], "request {index}");
        }
    }
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));
}
