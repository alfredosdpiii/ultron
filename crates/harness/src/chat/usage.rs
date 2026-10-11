//! The `pi.usage` ledger (spec §8.6): every writer of an assistant entry or a result with usage adds it in the same
//! commit. Port of upstream `harness/usage.ts`.

use chord_delta::Seg;
use durable::{ConversationId, Tx};
use serde_json::{Map, Number, Value, json};

use super::docs::USAGE;
use super::util::{d, owner};
use crate::scheduler::Result;

/// Ledger bucket: assistant spend by `provider/model`, or tool spend by tool name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bucket {
    Models,
    Tools,
}

impl Bucket {
    fn key(self) -> &'static str {
        match self {
            Bucket::Models => "models",
            Bucket::Tools => "tools",
        }
    }
}

const COUNTERS: [&str; 5] = ["input", "output", "cacheRead", "cacheWrite", "totalTokens"];
const OPTIONAL: [&str; 2] = ["cacheWrite1h", "reasoning"];
const COSTS: [&str; 5] = ["input", "output", "cacheRead", "cacheWrite", "total"];

/// `a + b` keeping integers integral.
fn sum(a: Option<&Value>, b: Option<&Value>) -> Value {
    let (a, b) = (a.cloned().unwrap_or(json!(0)), b.cloned().unwrap_or(json!(0)));
    match (a.as_u64(), b.as_u64()) {
        (Some(a), Some(b)) => json!(a + b),
        _ => Number::from_f64(a.as_f64().unwrap_or(0.0) + b.as_f64().unwrap_or(0.0))
            .map(Value::Number)
            .unwrap_or(json!(0)),
    }
}

/// Add `usage` to one bucket of the conversation's `pi.usage`, in the commit that records the response. A first entry
/// is set whole; later ones add counter by counter, so each commit writes only numeric sets.
pub fn record_usage(
    tx: &mut Tx<'_>,
    conversation_id: ConversationId,
    bucket: Bucket,
    key: &str,
    usage: &Value,
) -> Result<()> {
    let ledger = tx.doc(&USAGE, owner(conversation_id))?;
    let path = |leaf: &[&str]| -> Vec<Seg> {
        let mut path = vec![Seg::from(bucket.key()), Seg::from(key)];
        path.extend(leaf.iter().map(|segment| Seg::from(*segment)));
        path
    };
    let Some(total) = d(ledger.get(&path(&[])))? else {
        return d(ledger.set(&path(&[]), super::util::strict(usage.clone())));
    };
    for counter in COUNTERS {
        d(ledger.set(&path(&[counter]), sum(total.get(counter), usage.get(counter))))?;
    }
    for counter in OPTIONAL {
        if usage.get(counter).is_some_and(|value| !value.is_null()) {
            d(ledger.set(&path(&[counter]), sum(total.get(counter), usage.get(counter))))?;
        }
    }
    for cost in COSTS {
        d(ledger.set(
            &path(&["cost", cost]),
            sum(total["cost"].get(cost), usage["cost"].get(cost)),
        ))?;
    }
    Ok(())
}

/// Add every counter of `usage` into `total` (plain values).
pub fn add_usage(total: &mut Value, usage: &Value) {
    for counter in COUNTERS {
        total[counter] = sum(total.get(counter), usage.get(counter));
    }
    for counter in OPTIONAL {
        if usage.get(counter).is_some_and(|value| !value.is_null()) {
            total[counter] = sum(total.get(counter), usage.get(counter));
        }
    }
    if !total["cost"].is_object() {
        total["cost"] = json!({});
    }
    for cost in COSTS {
        total["cost"][cost] = sum(total["cost"].get(cost), usage["cost"].get(cost));
    }
}

/// Add every bucket of `state` into `sum`.
pub fn add_usage_state(total: &mut Value, state: &Value) {
    for bucket in ["models", "tools"] {
        if !total[bucket].is_object() {
            total[bucket] = Value::Object(Map::new());
        }
        let Some(entries) = state[bucket].as_object() else {
            continue;
        };
        for (key, usage) in entries {
            match total[bucket].get_mut(key) {
                Some(existing) => add_usage(existing, usage),
                None => {
                    total[bucket][key] = usage.clone();
                }
            }
        }
    }
}
