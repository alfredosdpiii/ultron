//! `api/google-vertex.ts`: Gemini on Vertex AI in API-key mode, sharing the stream of `google.rs`. ADC mode (project
//! and location with Google application credentials) is not ported: it needs google-auth-library's token exchange.

use crate::Transport;
use crate::env::provider_env_value;
use crate::google::{Endpoint, GoogleOptions, GoogleThinking, simple_options, stream_endpoint};
use crate::options::{SimpleStreamOptions, StreamOptions};
use crate::types::{AssistantMessage, EventSink, Model, TranscriptContext};
use serde::Deserialize;

const API_VERSION: &str = "v1";
const GCP_VERTEX_CREDENTIALS_MARKER: &str = "gcp-vertex-credentials";
const GLOBAL_BASE_URL: &str = "https://aiplatform.googleapis.com";

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleVertexOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    pub tool_choice: Option<String>,
    pub thinking: Option<GoogleThinking>,
    pub project: Option<String>,
    pub location: Option<String>,
}

/// `resolveApiKey`: a trimmed key that is neither the ADC marker nor a `<placeholder>`.
fn resolve_api_key(options: &StreamOptions) -> Option<String> {
    let key = options.api_key.as_deref()?.trim();
    let placeholder =
        key.len() > 2 && key.starts_with('<') && key.ends_with('>') && !key[1..key.len() - 1].contains('>');
    (!key.is_empty() && key != GCP_VERTEX_CREDENTIALS_MARKER && !placeholder).then(|| key.to_string())
}

/// `baseUrlIncludesApiVersion`: a path segment like `v1`, `v1beta` or `v1beta1`.
fn includes_api_version(base_url: &str) -> bool {
    let path = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest.split_once('/').map_or("", |(_, p)| p));
    path.split(['?', '#'])
        .next()
        .unwrap_or_default()
        .split('/')
        .any(|part| {
            let Some(rest) = part.strip_prefix('v') else {
                return false;
            };
            let digits = rest.trim_start_matches(|c: char| c.is_ascii_digit());
            digits.len() < rest.len()
                && (digits.is_empty()
                    || digits
                        .strip_prefix("beta")
                        .is_some_and(|d| d.chars().all(|c| c.is_ascii_digit())))
        })
}

fn endpoint(model: &Model, options: &GoogleVertexOptions) -> Result<Endpoint, String> {
    let Some(api_key) = resolve_api_key(&options.base) else {
        let env = options.base.env.as_ref();
        let project = options
            .project
            .clone()
            .filter(|p| !p.is_empty())
            .or_else(|| provider_env_value("GOOGLE_CLOUD_PROJECT", env))
            .or_else(|| provider_env_value("GCLOUD_PROJECT", env));
        if project.is_none() {
            return Err(
                "Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or pass project in options."
                    .into(),
            );
        }
        let location = options
            .location
            .clone()
            .filter(|l| !l.is_empty())
            .or_else(|| provider_env_value("GOOGLE_CLOUD_LOCATION", env));
        if location.is_none() {
            return Err("Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION or pass location in options.".into());
        }
        return Err(
            "Vertex AI application-default credentials are not supported by the Rust port; pass an API key".into(),
        );
    };
    let custom = model.base_url.trim();
    let base = if custom.is_empty() || custom.contains("{location}") {
        format!("{GLOBAL_BASE_URL}/{API_VERSION}")
    } else if includes_api_version(custom) {
        custom.trim_end_matches('/').to_string()
    } else {
        format!("{}/{API_VERSION}", custom.trim_end_matches('/'))
    };
    Ok(Endpoint {
        base,
        api_key,
        vertex: true,
    })
}

/// Stream one Vertex response; failures end with an `error` event.
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &GoogleVertexOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let google = GoogleOptions {
        base: options.base.clone(),
        tool_choice: options.tool_choice.clone(),
        thinking: options.thinking.clone(),
    };
    let endpoint = endpoint(model, options);
    stream_endpoint(model, context, &google, endpoint, "Google Vertex", transport, sink).await
}

/// `streamSimple`: as the Gemini API's, without its up-front API key check or 2.5-flash-lite budgets.
pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> Result<AssistantMessage, String> {
    let google = simple_options(model, context, options, false)?;
    let vertex = GoogleVertexOptions {
        base: google.base,
        tool_choice: google.tool_choice,
        thinking: google.thinking,
        ..Default::default()
    };
    Ok(stream(model, context, &vertex, transport, sink).await)
}
