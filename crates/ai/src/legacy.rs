//! `legacy-api-aliases.ts`: the deprecated per-API stream function names (`streamAnthropic`,
//! `streamSimpleAzureOpenAIResponses`, ...). Each alias is the API module's own `stream`/`stream_simple`;
//! [`LEGACY_API_ALIASES`] lists every upstream name with its API and variant, and whether this crate implements that
//! API (all of them now).

pub use crate::anthropic::{stream as stream_anthropic, stream_simple as stream_simple_anthropic};
pub use crate::azure_openai_responses::{
    stream as stream_azure_openai_responses, stream_simple as stream_simple_azure_openai_responses,
};
pub use crate::google::{stream as stream_google, stream_simple as stream_simple_google};
pub use crate::google_vertex::{stream as stream_google_vertex, stream_simple as stream_simple_google_vertex};
pub use crate::mistral::{stream as stream_mistral, stream_simple as stream_simple_mistral};
pub use crate::openai_codex_responses::{
    stream as stream_openai_codex_responses, stream_simple as stream_simple_openai_codex_responses,
};
pub use crate::openai_completions::{
    stream as stream_openai_completions, stream_simple as stream_simple_openai_completions,
};
pub use crate::openai_responses::{stream as stream_openai_responses, stream_simple as stream_simple_openai_responses};

/// One deprecated export: its upstream name, the API it streams, and whether it is the simple-options variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegacyApiAlias {
    pub name: &'static str,
    pub api: &'static str,
    pub simple: bool,
    /// Whether this crate implements the API (and exports the alias above).
    pub implemented: bool,
}

const fn alias(name: &'static str, api: &'static str, simple: bool, implemented: bool) -> LegacyApiAlias {
    LegacyApiAlias {
        name,
        api,
        simple,
        implemented,
    }
}

pub const LEGACY_API_ALIASES: [LegacyApiAlias; 16] = [
    alias("streamAnthropic", "anthropic-messages", false, true),
    alias("streamSimpleAnthropic", "anthropic-messages", true, true),
    alias("streamAzureOpenAIResponses", "azure-openai-responses", false, true),
    alias("streamSimpleAzureOpenAIResponses", "azure-openai-responses", true, true),
    alias("streamGoogle", "google-generative-ai", false, true),
    alias("streamSimpleGoogle", "google-generative-ai", true, true),
    alias("streamGoogleVertex", "google-vertex", false, true),
    alias("streamSimpleGoogleVertex", "google-vertex", true, true),
    alias("streamMistral", "mistral-conversations", false, true),
    alias("streamSimpleMistral", "mistral-conversations", true, true),
    alias("streamOpenAICodexResponses", "openai-codex-responses", false, true),
    alias("streamSimpleOpenAICodexResponses", "openai-codex-responses", true, true),
    alias("streamOpenAICompletions", "openai-completions", false, true),
    alias("streamSimpleOpenAICompletions", "openai-completions", true, true),
    alias("streamOpenAIResponses", "openai-responses", false, true),
    alias("streamSimpleOpenAIResponses", "openai-responses", true, true),
];
