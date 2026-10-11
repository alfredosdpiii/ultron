//! Replays scenarios recorded from the TypeScript `InMemoryTelemetryContext` and `isInstallTelemetryEnabled`
//! (fixtures/generate.mjs) and requires identical span snapshots and results.

use serde_json::{Value, json};
use std::sync::LazyLock;
use telemetry::{Attributes, End, ErrorDetails, Span, SpanStatus, TelemetryContext, is_install_telemetry_enabled};

static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/telemetry.json")).expect("fixture parses"));

fn attrs(value: Option<&Value>) -> Attributes {
    value.map_or_else(Attributes::new, |v| {
        serde_json::from_value(v.clone()).expect("attributes")
    })
}

fn apply(context: &TelemetryContext, span: &Span, op: &Value, snapshots: &mut Vec<Value>) {
    let op = op.as_array().expect("op");
    match op[0].as_str().expect("op name") {
        "event" => span.add_event(op[1].as_str().expect("event name"), attrs(op.get(2))),
        "set" => span.set_attributes(attrs(op.get(1))),
        "status" => span.set_status(serde_json::from_value::<SpanStatus>(op[1].clone()).expect("status")),
        "child" => run(context, |name, a| span.start_child(name, a), &op[1], snapshots),
        "snapshot" => snapshots.push(json!(context.spans())),
        other => panic!("unknown op {other}"),
    }
}

fn run(
    context: &TelemetryContext,
    start: impl FnOnce(&str, Attributes) -> Span,
    scenario: &Value,
    snapshots: &mut Vec<Value>,
) {
    let span = start(
        scenario["name"].as_str().expect("name"),
        attrs(scenario.get("attributes")),
    );
    for op in scenario["ops"].as_array().into_iter().flatten() {
        apply(context, &span, op, snapshots);
    }
    let end = match &scenario["end"] {
        Value::String(s) if s == "ok" => End::Ok,
        Value::String(s) if s == "throwValue" => End::Failed(None),
        other => End::Failed(Some(
            serde_json::from_value::<ErrorDetails>(other["error"].clone()).expect("error"),
        )),
    };
    span.end(end);
    for op in scenario["late"].as_array().into_iter().flatten() {
        apply(context, &span, op, snapshots);
    }
}

#[test]
fn telemetry_in_memory_spans_match_typescript() {
    let cases = FIXTURE["spanCases"].as_array().expect("cases");
    assert_eq!(cases.len(), 7);
    for case in cases {
        let context = TelemetryContext::in_memory();
        let mut snapshots = Vec::new();
        for root in case["roots"].as_array().expect("roots") {
            run(&context, |name, a| context.start(name, a), root, &mut snapshots);
        }
        let id = &case["id"];
        assert_eq!(json!(context.spans()), case["spans"], "spans for {id}");
        assert_eq!(Value::Array(snapshots), case["snapshots"], "snapshots for {id}");
    }
}

#[test]
fn telemetry_noop_context_records_nothing_and_children_are_inert() {
    assert_eq!(FIXTURE["noopChildIsSameSpan"], json!(true));
    let context = TelemetryContext::noop();
    let span = context.start("x", Attributes::new());
    let child = span.start_child("child", Attributes::new());
    child.add_event("e", Attributes::new());
    child.end(End::Failed(None));
    span.end(End::Ok);
    assert_eq!(json!(context.spans()), json!([]));
    assert_eq!(context.dropped(), 0);
}

#[test]
fn telemetry_install_enabled_matches_typescript() {
    let cases = FIXTURE["installCases"].as_array().expect("cases");
    assert_eq!(cases.len(), 36);
    for case in cases {
        let env = case["env"].as_str();
        let setting = case["setting"].as_bool();
        assert_eq!(
            json!(is_install_telemetry_enabled(env, setting)),
            case["enabled"],
            "{case}"
        );
    }
}
