//! Rust port of the provider layer of Ultron's `packages/ai`: the message model, stream events, every API
//! implementation (Anthropic Messages, OpenAI Chat Completions/Responses/Codex Responses, Azure OpenAI Responses,
//! Google, Vertex, Mistral, Bedrock Converse, pi-messages, Claude Code CLI, Claude subscription), provider auth with every OAuth flow, the
//! `Models` runtime, the images API, the built-in model catalog, user `models.json`, and env API keys.
//!
//! Evidence: the `fixtures/generate*.mjs` scripts run the TypeScript sources against canned HTTP/SSE responses through
//! a stubbed `fetch` and record requests, events and results; the tests replay them here.

pub mod anthropic;
pub mod auth;
pub mod aws;
pub mod aws_credentials;
pub mod azure_openai_responses;
pub mod bedrock;
pub mod claude_code_cli;
pub mod claude_subscription;
pub mod cloudflare;
pub mod constrained;
pub mod dispatch;
pub mod env;
pub use model_scope::glob;
pub mod google;
pub mod google_auth;
pub mod google_shared;
pub mod google_vertex;
pub mod http;
pub mod images;
pub mod json;
pub mod jsutil;
pub mod legacy;
pub mod mistral;
pub mod model_config;
pub mod model_refresh;
pub mod model_registry;
pub mod model_resolver;
pub mod models;
pub mod oauth;
pub mod oauth_flow;
pub mod oauth_kimi;
pub mod oauth_login;
pub mod oauth_meta;
pub mod oauth_openrouter;
pub mod oauth_radius;
pub mod oauth_xai;
pub mod openai_codex_responses;
pub mod openai_completions;
pub mod openai_prompt_cache;
pub mod openai_responses;
pub mod options;
pub mod pi_messages;
pub mod sse;
pub mod transcript;
pub mod types;

pub use http::{AbortSignal, Body, HttpRequest, HttpResponse, ReqwestTransport, Transport};
pub use options::{SimpleStreamOptions, StreamOptions, ThinkingBudgets};
pub use types::*;
