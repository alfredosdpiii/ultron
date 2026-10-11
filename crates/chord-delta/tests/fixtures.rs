//! Replays fixtures generated from the pinned upstream Chord source (see fixtures/generate.mjs).

use chord_delta::{
    Decoder, DeltaError, Encoder, Op, apply, apply_immutable, apply_immutable_batches, error_json, validate_wire_op,
};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/delta.json")).expect("fixture parses")
}

fn outcome(result: Result<Value, DeltaError>) -> Value {
    match result {
        Ok(value) => json!({ "ok": value }),
        Err(error) => json!({ "error": error_json(&error) }),
    }
}

fn parse_ops(ops: &Value) -> Result<Vec<Op>, DeltaError> {
    ops.as_array().expect("ops array").iter().map(Op::from_json).collect()
}

#[test]
fn tracker_streams_replay_encode_and_decode_like_upstream() {
    let fixture = fixture();
    let streams = fixture["streams"].as_array().unwrap();
    assert_eq!(streams.len(), 40);
    for stream in streams {
        let seed = &stream["seed"];
        let base = stream["base"].clone();
        let (mut encoder, mut decoder) = (Encoder::default(), Decoder::default());
        let (mut mutable, mut immutable) = (base.clone(), base.clone());
        let mut all = Vec::new();
        for (step, batch) in stream["batches"].as_array().unwrap().iter().enumerate() {
            let at = format!("seed {seed} batch {step}");
            let ops = parse_ops(&batch["ops"]).unwrap_or_else(|e| panic!("{at}: {e}"));
            let wire = batch["wire"].as_array().unwrap();
            assert_eq!(encoder.encode(&ops), *wire, "{at}: encode");
            assert_eq!(
                decoder.decode(wire).unwrap_or_else(|e| panic!("{at}: {e}")),
                ops,
                "{at}: decode"
            );

            let before = immutable.clone();
            let next = apply_immutable(&immutable, &ops).unwrap_or_else(|e| panic!("{at}: {e}"));
            assert_eq!(immutable, before, "{at}: input unchanged");
            immutable = next;
            mutable = apply(mutable, ops.clone()).unwrap_or_else(|e| panic!("{at}: {e}"));
            // Serialized comparison also checks key order (the generator uses no integer-like keys).
            assert_eq!(mutable.to_string(), batch["value"].to_string(), "{at}: apply");
            assert_eq!(
                immutable.to_string(),
                batch["value"].to_string(),
                "{at}: apply_immutable"
            );
            all.push(ops);
        }
        let last = &stream["batches"].as_array().unwrap().last().unwrap()["value"];
        let replayed = apply_immutable_batches(&base, all.iter().map(Vec::as_slice)).unwrap();
        assert_eq!(replayed, *last, "seed {seed}: apply_immutable_batches");
    }
}

#[test]
fn apply_edge_and_error_cases_match_upstream() {
    for (i, case) in fixture()["apply"].as_array().unwrap().iter().enumerate() {
        let (mutable, immutable) = match parse_ops(&case["ops"]) {
            Err(error) => (outcome(Err(error.clone())), outcome(Err(error))),
            Ok(ops) => {
                let target = case["target"].clone();
                let immutable = outcome(apply_immutable(&target, &ops));
                assert_eq!(target, case["target"], "case {i}: input unchanged");
                (outcome(apply(target, ops)), immutable)
            }
        };
        assert_eq!(mutable, case["apply"], "case {i} apply: {}", case["ops"]);
        assert_eq!(
            immutable, case["applyImmutable"],
            "case {i} apply_immutable: {}",
            case["ops"]
        );
    }
}

#[test]
fn op_validation_matches_upstream() {
    for case in fixture()["validate"].as_array().unwrap() {
        let result = outcome(Op::from_json(&case["op"]).map(|_| json!(true)));
        assert_eq!(result, case["result"], "op {}", case["op"]);
    }
}

#[test]
fn wire_validation_matches_upstream() {
    for case in fixture()["validateWire"].as_array().unwrap() {
        let result = outcome(validate_wire_op(&case["op"]).map(|_| json!(true)));
        assert_eq!(result, case["result"], "wire op {}", case["op"]);
    }
}

#[test]
fn decoder_streams_match_upstream() {
    for case in fixture()["decode"].as_array().unwrap() {
        let mut decoder = Decoder::default();
        let batches = case["batches"].as_array().unwrap();
        for (batch, expected) in batches.iter().zip(case["results"].as_array().unwrap()) {
            let decoded = decoder
                .decode(batch.as_array().unwrap())
                .map(|ops| Value::Array(ops.iter().map(Op::to_json).collect()));
            assert_eq!(outcome(decoded), *expected, "batch {batch}");
        }
    }
}

#[test]
fn trim_never_splits_a_surrogate_pair() {
    let target = json!({ "s": "😀b" });
    let ops = vec![Op::Trim(vec![chord_delta::Seg::Key("s".into())], 1)];
    let error = apply(target, ops).unwrap_err();
    assert_eq!(error.to_string(), r#"trim splits a surrogate pair: ["s"]"#);
}
