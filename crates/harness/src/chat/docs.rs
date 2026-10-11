//! The built-in conversation documents (spec §6, §8.2, §8.6): `pi.live` (run control and presentation), `pi.inbox`
//! (queued submissions), `pi.usage` (spend ledger), `pi.agent` (model and thinking level) and `pi.provider` (the
//! provider-facing session identity). Every Harness conversation gets them in its creating commit.

use std::sync::LazyLock;

use durable::{Doc, DocDefinition, DocSemantics, DocumentFork, DocumentHistory};
use serde_json::{Value, json};

fn conversation(history: DocumentHistory, fork: DocumentFork) -> DocSemantics {
    DocSemantics::Conversation { history, fork }
}

fn define(definition: DocDefinition) -> Doc {
    definition.define().expect("built-in document definitions are valid")
}

fn running(slots: Option<&Value>) -> bool {
    slots
        .and_then(Value::as_array)
        .is_some_and(|slots| slots.iter().any(|slot| slot["status"] == "running"))
}

/// `pi.live`: run control (`run`), the current generation attempt, the current tool round, nested calls and
/// compaction statuses. A complete base whenever nothing runs: no `generation` and no running slot.
pub static LIVE: LazyLock<Doc> = LazyLock::new(|| {
    define(
        DocDefinition::new(
            "pi.live",
            1,
            conversation(DocumentHistory::Latest, DocumentFork::Initial),
            |_| json!({}),
        )
        .checkpoint_when(|_, value, _, _| {
            Ok(value.get("generation").is_none() && !running(value.get("tools")) && !running(value.get("nestedTools")))
        }),
    )
});

/// `pi.inbox`: the conversation's queued submissions in ID order; a complete base whenever it is empty.
pub static INBOX: LazyLock<Doc> = LazyLock::new(|| {
    define(
        DocDefinition::new(
            "pi.inbox",
            1,
            conversation(DocumentHistory::Latest, DocumentFork::Initial),
            |_| json!({ "items": [] }),
        )
        .checkpoint_when(|_, value, _, _| Ok(value["items"].as_array().is_none_or(|items| items.is_empty()))),
    )
});

/// `pi.usage`: assistant spend per `provider/model` and tool spend per tool name; a base on every change.
pub static USAGE: LazyLock<Doc> = LazyLock::new(|| {
    define(
        DocDefinition::new(
            "pi.usage",
            1,
            conversation(DocumentHistory::Latest, DocumentFork::Initial),
            |_| json!({ "models": {}, "tools": {} }),
        )
        .checkpoint_when(|_, _, _, _| Ok(true)),
    )
});

/// `pi.agent`: the conversation's model (`{provider, modelId}`) and thinking level; rewindable so a fork starts from
/// the agent at its fork entry.
pub static AGENT: LazyLock<Doc> = LazyLock::new(|| {
    define(
        DocDefinition::new(
            "pi.agent",
            1,
            conversation(DocumentHistory::Rewindable, DocumentFork::AsOf),
            |_| json!({}),
        )
        .checkpoint_when(|_, _, _, _| Ok(true)),
    )
});

/// `pi.provider`: the stable provider session ID; every fork starts with a fresh one.
pub static PROVIDER: LazyLock<Doc> = LazyLock::new(|| {
    define(
        DocDefinition::new(
            "pi.provider",
            1,
            conversation(DocumentHistory::Latest, DocumentFork::Initial),
            |_| json!({ "sessionId": uuid_v7() }),
        )
        .checkpoint_when(|_, _, _, _| Ok(true)),
    )
});

/// A UUIDv7: 48 bits of Unix milliseconds, then random bits.
pub fn uuid_v7() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS randomness is unavailable");
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}
