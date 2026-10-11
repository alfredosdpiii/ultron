//! Session naming by one small model call. The deterministic name from the first prompt
//! (`session_journal::names::auto_name`) is written at once; this asks a cheap model for a better title, which
//! replaces it only while the name is still the automatic one. Settings: `sessionNaming.enabled` (default true)
//! and `sessionNaming.model` (`provider/id`, or an id of the session's provider). The model defaults to the
//! session provider's cheapest catalog model, else the session model.

use ai::{
    AssistantContent, AssistantMessage, Message, Model, SimpleStreamOptions, StopReason, StreamOptions, TextContent,
    TranscriptContext, UserContent, UserMessage, UserMessageContent,
};
use serde_json::Value;
use std::time::Duration;

pub const SYSTEM: &str = "Name this coding session in 3-6 words. Reply with the title only.";
/// How much of the first prompt the title request carries, in characters.
pub const PROMPT_CHARS: usize = 2_000;
pub const MAX_TOKENS: u64 = 24;
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// What the naming request needs from the settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NamingSettings {
    pub enabled: bool,
    pub model: Option<String>,
}

/// `sessionNaming` of the merged settings.
pub fn naming_settings(settings: &Value) -> NamingSettings {
    let section = &settings["sessionNaming"];
    NamingSettings {
        enabled: section["enabled"].as_bool() != Some(false),
        model: section["model"]
            .as_str()
            .map(str::to_string)
            .filter(|m| !m.trim().is_empty()),
    }
}

fn price(model: &Model) -> f64 {
    model.cost.input + model.cost.output
}

/// The naming model: the setting, else the provider's cheapest priced text model whose API this build streams
/// (non-reasoning models first; never one that always thinks), else `session`.
pub fn naming_model(setting: Option<&str>, session: &Model, supported: &dyn Fn(&str) -> bool) -> Model {
    if let Some(spec) = setting {
        let (provider, id) = spec.split_once('/').unwrap_or((session.provider.as_str(), spec));
        if let Some(model) = ai::models::builtin_model(provider, id) {
            let mut model = model.clone();
            if model.provider == session.provider {
                model.base_url = session.base_url.clone();
            }
            return model;
        }
    }
    // Thinking stays off: models that always think (managed effort) are not candidates; non-reasoning ones first.
    let cheapest = ai::models::builtin_models(&session.provider)
        .filter(|m| supported(&m.api) && m.input.iter().any(|i| i == "text") && price(m) > 0.0)
        .filter(|m| {
            m.compat.as_ref().and_then(|c| c.get("supportsMidConvoEffort")) != Some(&serde_json::Value::Bool(true))
        })
        .min_by(|l, r| {
            l.reasoning
                .cmp(&r.reasoning)
                .then(price(l).total_cmp(&price(r)))
                .then_with(|| l.id.cmp(&r.id))
        });
    match cheapest {
        Some(model) => {
            let mut model = model.clone();
            model.base_url = session.base_url.clone();
            model
        }
        None => session.clone(),
    }
}

/// The request: the system line and the first prompt, cut to [`PROMPT_CHARS`].
pub fn request(prompt: &str) -> TranscriptContext {
    let text: String = prompt.chars().take(PROMPT_CHARS).collect();
    let mut messages = Vec::new();
    if let Some(system) = ai::transcript::create_initial_system_message(Some(SYSTEM), None) {
        messages.push(Message::System(system));
    }
    messages.push(Message::User(UserMessage {
        content: UserMessageContent::Blocks(vec![UserContent::Text(TextContent {
            text,
            text_signature: None,
        })]),
        timestamp: 0,
    }));
    TranscriptContext { messages }
}

pub fn options(api_key: Option<String>) -> SimpleStreamOptions {
    SimpleStreamOptions {
        base: StreamOptions {
            api_key,
            max_tokens: Some(MAX_TOKENS),
            max_retries: Some(0),
            ..Default::default()
        },
        reasoning: None,
        ..Default::default()
    }
}

/// The title of a finished response, when it has one.
pub fn title_of(message: &AssistantMessage) -> Option<String> {
    if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
        return None;
    }
    let text: String = message
        .content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    session_journal::names::clean_title(&text)
}
