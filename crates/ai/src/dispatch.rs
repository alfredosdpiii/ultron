//! `streamSimple` by `model.api` over every ported API implementation (the API registry of `providers/all.ts`), with
//! Cloudflare placeholders materialized from the request env first. `Err` is a setup failure that upstream throws
//! synchronously (for example a missing API key in `streamSimple`).

use crate::claude_code_cli::{self, ClaudeCodeOptions};
use crate::cloudflare::resolve_cloudflare_model;
use crate::http::Transport;
use crate::options::SimpleStreamOptions;
use crate::types::{AssistantMessage, EventSink, Model, TranscriptContext};
use crate::{
    anthropic, azure_openai_responses, bedrock, google, google_vertex, mistral, openai_codex_responses,
    openai_completions, openai_responses, pi_messages,
};

/// The API ids this crate implements.
pub const APIS: &[&str] = &[
    "anthropic-messages",
    "openai-completions",
    "openai-responses",
    "azure-openai-responses",
    "openai-codex-responses",
    "google-generative-ai",
    "google-vertex",
    "mistral-conversations",
    "bedrock-converse-stream",
    "pi-messages",
    "claude-code-cli",
    "claude-subscription",
];

pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> Result<AssistantMessage, String> {
    let resolved;
    let model = if model.provider.starts_with("cloudflare-") {
        resolved = resolve_cloudflare_model(model, options.base.env.as_ref());
        &resolved
    } else {
        model
    };
    match model.api.as_str() {
        "anthropic-messages" => anthropic::stream_simple(model, context, options, transport, sink).await,
        "openai-completions" => openai_completions::stream_simple(model, context, options, transport, sink).await,
        "openai-responses" => openai_responses::stream_simple(model, context, options, transport, sink).await,
        "azure-openai-responses" => {
            azure_openai_responses::stream_simple(model, context, options, transport, sink).await
        }
        "openai-codex-responses" => {
            openai_codex_responses::stream_simple(model, context, options, transport, sink).await
        }
        "google-generative-ai" => google::stream_simple(model, context, options, transport, sink).await,
        "google-vertex" => google_vertex::stream_simple(model, context, options, transport, sink).await,
        "mistral-conversations" => mistral::stream_simple(model, context, options, transport, sink).await,
        "bedrock-converse-stream" => Ok(bedrock::stream_simple(model, context, options, transport, sink).await),
        "pi-messages" => Ok(pi_messages::stream_simple(model, context, options, transport, sink).await),
        "claude-code-cli" => {
            let options = ClaudeCodeOptions {
                base: options.clone(),
                json_schema: None,
            };
            Ok(claude_code_cli::stream_simple(model, context, &options, sink).await)
        }
        // Subscription first, API credits when the plan is out of usage (`claude_subscription::fallback`).
        "claude-subscription" => {
            Ok(crate::claude_subscription::fallback::stream_simple(model, context, options, transport, sink).await)
        }
        other => Err(format!("No API implementation for \"{other}\"")),
    }
}
