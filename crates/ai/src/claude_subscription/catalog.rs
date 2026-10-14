//! Pinned native routes (`model_catalog.py`). Behind the loopback relay Claude Code runs a plain model id inside its
//! 200K gateway default and selects the 1M window only for an explicit `[1m]` route, so Ultron never budgets past
//! the window the CLI enforces: plain ids are 200K, `[1m]` is appended only for ids pinned at 1M, and an unpinned id
//! is sent as typed and budgeted at 200K.

use crate::types::{Model, ModelCost};
use indexmap::IndexMap;
use serde_json::Map;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

pub const PROVIDER: &str = "claude-subscription";
pub const API: &str = "claude-subscription";
/// The only upstream a request may reach (a loopback `http://` URL is accepted as a test fixture).
pub const UPSTREAM: &str = "https://api.anthropic.com";
pub const LONG: &str = "[1m]";
pub const PLAIN_WINDOW: u64 = 200_000;
pub const LONG_WINDOW: u64 = 1_000_000;

/// Canonical ids (as the CLI spells them) and their qualified windows, in catalog order.
pub const CONTEXT_WINDOWS: &[(&str, u64)] = &[
    ("claude-sonnet-5-5", LONG_WINDOW),
    ("claude-sonnet-5", LONG_WINDOW),
    ("claude-haiku-5-5", LONG_WINDOW),
    ("claude-haiku-4-5-20251001", PLAIN_WINDOW),
    ("claude-opus-5-5", LONG_WINDOW),
    ("claude-opus-5", LONG_WINDOW),
    ("claude-opus-4-8", LONG_WINDOW),
    ("claude-fable-5-1", LONG_WINDOW),
];

/// Family aliases and the undated Haiku 4.5 id.
pub const ALIASES: &[(&str, &str)] = &[
    ("sonnet", "claude-sonnet-5-5"),
    ("haiku", "claude-haiku-5-5"),
    ("claude-haiku-4-5", "claude-haiku-4-5-20251001"),
    ("opus", "claude-opus-5-5"),
    ("fable", "claude-fable-5-1"),
];

/// Families that answer `thinking: {"type": "disabled"}` with a 400: a caller's disable is omitted for them.
pub const MANDATORY_THINKING: &[&str] = &[
    "claude-fable",
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
];

/// Routes that 400 on `thinking: {"type": "adaptive"}`; their effort signal still applies.
pub const NO_ADAPTIVE_THINKING: &[&str] = &["claude-haiku-4-5-20251001"];

/// The oldest Claude Code that serves each model; an older CLI answers `400 ... does not support this model`.
pub const MIN_CLI_VERSIONS: &[(&str, &str)] = &[
    ("claude-haiku-5-5", "2.1.293"),
    ("claude-sonnet-5-5", "2.1.284"),
    ("claude-opus-5-5", "2.1.280"),
];

fn alias(base: &str) -> &str {
    ALIASES
        .iter()
        .find(|(name, _)| *name == base)
        .map_or(base, |(_, target)| target)
}

/// The canonical id of `model`: `[1m]` dropped, aliases resolved.
pub fn canonical(model: &str) -> &str {
    alias(model.strip_suffix(LONG).unwrap_or(model))
}

pub fn pinned_window(canonical: &str) -> Option<u64> {
    CONTEXT_WINDOWS.iter().find(|(id, _)| *id == canonical).map(|(_, w)| *w)
}

/// The `--model` the CLI receives: pinned 1M ids gain `[1m]`, pinned 200K ids stay plain, others pass through.
pub fn native_model(model: &str) -> Result<String, String> {
    let canonical = canonical(model);
    match pinned_window(canonical) {
        Some(LONG_WINDOW) => Ok(format!("{canonical}{LONG}")),
        Some(_) if model.ends_with(LONG) => Err("Haiku 4.5 does not support a 1M context window".into()),
        Some(_) => Ok(canonical.to_string()),
        None => Ok(model.to_string()),
    }
}

pub fn accepts_thinking_disable(model: &str) -> bool {
    let canonical = canonical(model);
    !MANDATORY_THINKING.iter().any(|prefix| canonical.starts_with(prefix))
}

pub fn supports_adaptive_thinking(model: &str) -> bool {
    !NO_ADAPTIVE_THINKING.contains(&canonical(model))
}

/// The window Ultron budgets for `model`: the pinned window, else 200K (an unpinned `[1m]` id is not promised 1M).
pub fn context_window(model: &str) -> u64 {
    pinned_window(canonical(model)).unwrap_or(PLAIN_WINDOW)
}

/// `claude-opus-4-8` -> `Opus 4.8`; a trailing snapshot date is dropped.
pub fn catalog_label(model: &str) -> String {
    let mut parts: Vec<&str> = model.strip_prefix("claude-").unwrap_or(model).split('-').collect();
    if parts
        .last()
        .is_some_and(|p| p.len() == 8 && p.bytes().all(|b| b.is_ascii_digit()))
    {
        parts.pop();
    }
    let Some((family, rest)) = parts.split_first() else {
        return String::new();
    };
    let mut family = family.to_string();
    if let Some(first) = family.get(..1) {
        family = first.to_uppercase() + &family[1..];
    }
    let version: Vec<&str> = rest
        .iter()
        .copied()
        .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .collect();
    [family, version.join(".")]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every pinned route in catalog order, with its canonical model and window (`MODEL_METADATA`).
pub fn pinned_routes() -> Vec<(String, &'static str, u64)> {
    CONTEXT_WINDOWS
        .iter()
        .map(|(id, window)| (native_model(id).unwrap_or_else(|_| id.to_string()), *id, *window))
        .collect()
}

/// `Sonnet 5.5 needs Claude Code 2.1.284 or newer`, for a model with a known minimum.
pub fn version_hint(model: &str) -> Option<String> {
    let canonical = canonical(model);
    MIN_CLI_VERSIONS
        .iter()
        .find(|(id, _)| *id == canonical)
        .map(|(id, version)| {
            format!(
                "{} needs Claude Code {version} or newer; run `claude update`",
                catalog_label(id)
            )
        })
}

/// A model id this provider accepts: an alias, a pinned id or route, or an unpinned lowercase id.
pub fn valid_id(id: &str) -> bool {
    let base = id.strip_suffix(LONG).unwrap_or(id);
    !base.is_empty()
        && base.len() <= 64
        && base
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        && base.as_bytes()[0].is_ascii_alphanumeric()
        && native_model(id).is_ok()
}

/// The `Model` for `claude-subscription/<id>`: the id as selected, the window Ultron may budget, no per-token cost
/// (the subscription is billed by the account, not per request).
pub fn model(id: &str) -> Option<Model> {
    if !valid_id(id) {
        return None;
    }
    let canonical = canonical(id);
    let pinned = pinned_window(canonical).is_some();
    let name = if pinned {
        format!("{} (Claude subscription)", catalog_label(canonical))
    } else {
        format!("{id} (Claude subscription, unpinned)")
    };
    let thinking: IndexMap<String, Option<String>> = [("minimal", "low"), ("xhigh", "xhigh"), ("max", "max")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), Some(v.to_string())))
        .collect();
    Some(Model {
        id: id.to_string(),
        name,
        api: API.into(),
        provider: PROVIDER.into(),
        base_url: UPSTREAM.into(),
        reasoning: true,
        thinking_level_map: Some(thinking),
        input: vec!["text".into(), "image".into()],
        input_limits: None,
        cost: ModelCost::default(),
        prompt_cache: None,
        context_window: context_window(id),
        max_tokens: 64_000,
        sampling_params: None,
        headers: None,
        compat: None,
        request: None,
        extra: Map::new(),
    })
}

static INTERNED: LazyLock<Mutex<HashMap<String, &'static Model>>> = LazyLock::new(Mutex::default);

/// [`model`] with a `'static` lifetime for the built-in catalog lookup; each distinct valid id is built once.
pub fn interned(id: &str) -> Option<&'static Model> {
    let mut table = INTERNED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(found) = table.get(id) {
        return Some(found);
    }
    let built: &'static Model = Box::leak(Box::new(model(id)?));
    table.insert(id.to_string(), built);
    Some(built)
}
