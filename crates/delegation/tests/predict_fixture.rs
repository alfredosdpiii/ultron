//! Replays fixtures/predict.json (recorded by fixtures/generate-predict.mjs from predict-adapter.ts): for each call the
//! production predict adapter must send the same system prompt and user message (instructions, input, output schema,
//! contract, repair note) and return the same value or error for the model's reply.

use delegation::agents::{AdapterRequest, Completion, PredictCall, predict_adapter};
use kernel::Signal;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[test]
fn delegation_predict_adapter_matches_create_predict_adapter() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/predict.json");
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    for case in fixture["results"].as_array().unwrap() {
        let sent: Arc<Mutex<Option<Value>>> = Arc::default();
        let (seen, reply) = (sent.clone(), case.clone());
        let adapter = predict_adapter(Arc::new(move |call: PredictCall<'_>| {
            *seen.lock().unwrap() = Some(json!({ "system": call.system, "user": call.user }));
            Ok(Completion {
                text: reply["reply"].as_str().unwrap().into(),
                stop_reason: reply["stop"].as_str().unwrap_or("stop").into(),
                error_message: reply["message"].as_str().map(String::from),
            })
        }));
        let signal = Signal::new();
        let got = adapter(AdapterRequest {
            definition: &fixture["definition"],
            input: &case["input"],
            signal: &signal,
            repair: case.get("repair").cloned(),
        });
        assert_eq!(sent.lock().unwrap().clone().unwrap(), case["sent"], "{case}");
        match got {
            Ok(value) => assert_eq!(value, case["value"], "{case}"),
            Err(error) => assert_eq!(json!(error), case["error"], "{case}"),
        }
    }
}
