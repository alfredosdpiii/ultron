//! What the frame path adds to a provider payload (inference.ts `beforePayload`): the contract as the Claude Code
//! CLI's `json_schema` (`withContractSchema`), and the output cap at the request's tranche (`capOutputTokens`),
//! whatever the API's payload shape.

use serde_json::{Map, Value, json};

/// `withContractSchema`: hand a frame's contract to a provider that enforces JSON schemas natively. Only the Claude
/// Code CLI (`claude -p --json-schema`) takes it, through its payload's `json_schema`.
pub fn with_contract_schema(mut payload: Value, contract: Option<&Value>, api: &str) -> Value {
    if api != "claude-code-cli" {
        return payload;
    }
    if let (Some(contract), Value::Object(body)) = (contract, &mut payload) {
        body.insert("json_schema".into(), contract.clone());
    }
    payload
}

/// A JavaScript number as the payload holds it (`typeof x === "number"`).
fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64)
}

/// A whole number back as an integer, so the payload serializes as JavaScript would.
fn js_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

/// Lower `holder[key]` to `limit` (never under `floor`) when it is a number; whether it was.
fn cap_field(holder: &mut Map<String, Value>, key: &str, limit: f64, floor: f64) -> bool {
    let Some(current) = number(holder.get(key)) else {
        return false;
    };
    holder.insert(key.into(), js_number(floor.max(current.min(limit))));
    true
}

/// `capOutputTokens`: cap a provider payload's output tokens at `limit`. Known fields are lowered in place; an
/// OpenAI-compatible payload without one gets one, since an uncapped reply could overrun the hold.
pub fn cap_output_tokens(mut payload: Value, limit: u64, api: &str, provider: &str) -> Value {
    let Value::Object(body) = &mut payload else {
        return payload;
    };
    let limit_f = limit as f64;
    let mut capped = false;
    capped |= cap_field(body, "max_tokens", limit_f, 1.0);
    capped |= cap_field(body, "max_completion_tokens", limit_f, 1.0);
    capped |= cap_field(body, "max_output_tokens", limit_f, 16.0);
    capped |= cap_field(body, "maxTokens", limit_f, 1.0);
    for nested in ["generationConfig", "inferenceConfig", "config"] {
        if let Some(Value::Object(holder)) = body.get_mut(nested) {
            capped |= cap_field(holder, "maxOutputTokens", limit_f, 1.0);
            capped |= cap_field(holder, "maxTokens", limit_f, 1.0);
        }
    }
    // Anthropic requires max_tokens above the thinking budget (itself at least 1024).
    let max_tokens = number(body.get("max_tokens"));
    if let (Some(Value::Object(thinking)), Some(max_tokens)) = (body.get_mut("thinking"), max_tokens)
        && thinking.get("type").and_then(Value::as_str) == Some("enabled")
        && let Some(budget) = number(thinking.get("budget_tokens"))
    {
        let adjusted = 1024f64.max(budget.min(max_tokens - 1.0));
        thinking.insert("budget_tokens".into(), js_number(adjusted));
        body.insert("max_tokens".into(), js_number(max_tokens.max(adjusted + 1.0)));
    }
    if !capped && api == "openai-completions" {
        let key = if provider == "openai" || provider == "azure-openai" {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        body.insert(key.into(), json!(limit));
    }
    payload
}

/// The payload hook of one frame request: the contract schema, then the cap at `tranche`.
pub(crate) fn hook(contract: Option<Value>, tranche: u64) -> ai::options::PayloadHook {
    std::sync::Arc::new(move |payload: &Value, model: &ai::Model| {
        let payload = with_contract_schema(payload.clone(), contract.as_ref(), &model.api);
        Some(cap_output_tokens(payload, tranche, &model.api, &model.provider))
    })
}
