//! The `claude-subscription` provider: the official `claude` CLI driven as a request-scoped model client on the
//! user's Claude Pro/Max login, so Ultron's own harness keeps the agent loop (durable generation, compaction,
//! budgets, sandboxed `rlm` cells).
//!
//! One model call is one `claude -p` process (`--max-turns 1`, stream-json in and out, no native tools, settings
//! sources, slash commands or session persistence; `--permission-mode dontAsk`), in a stable private working
//! directory with a private per-request directory for the system prompt, settings (the generation fields as
//! `CLAUDE_CODE_EXTRA_BODY`) and the inert MCP manifest. The model's tool list is exactly `[rlm]`: it is advertised
//! through an inert MCP inventory server (`ultron __internal inert-mcp`) that never executes anything, a request
//! declaring any other tool is refused, and a response calling any other tool fails. A loopback admission relay
//! (`ANTHROPIC_BASE_URL`) forwards exactly the first Messages request upstream and captures the answer.
//! Canonical history replays as native frames; an unchanged assistant message replays its signed native blocks
//! from the carrier in `providerMetadata`.
//!
//! Ported from Nous Research's MIT-licensed Hermes plugin `claude-subscription-directsdk`
//! (hermes-official-plugins; Copyright (c) 2026 Nous Research and contributors; MIT License: permission is hereby
//! granted, free of charge, to any person obtaining a copy of this software and associated documentation files, to
//! deal in the Software without restriction, subject to including the copyright notice and this permission notice
//! in all copies or substantial portions of the Software; THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY
//! KIND). Evidence: `tests/claude_subscription*.rs` port the plugin's `tests/test_directsdk*.py` and
//! `tests/test_admission_*.py` with their literal expectations against a fake `claude`
//! (`fixtures/claude-subscription/claude.py`) and a fake upstream on loopback.

pub mod admission;
pub mod catalog;
pub mod fallback;
pub mod history;
pub mod inert_mcp;
pub mod limit;
pub mod process;
pub mod rewrite;
pub mod setup;

use crate::options::{SimpleStreamOptions, now_ms};
use crate::transcript::{current_tools, system_message_text, transform_messages};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, DiagnosticErrorInfo,
    EventSink, Message, Model, StopReason, TextContent, ThinkingContent, ToolCall, TranscriptContext,
};
use history::{BodyParams, CARRIER_KEY, Reasoning};
use process::{Delta, Finish, Handle, NativeResponse, Prepared};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

pub use catalog::{API, PROVIDER};

pub type Env = BTreeMap<String, String>;

pub const INSTALL_HINT: &str = "Claude Code is not installed (no `claude` on PATH or in the usual install directories). Install it with `npm install -g @anthropic-ai/claude-code` or set ULTRON_CLAUDE_COMMAND to the binary.";
pub const LOGIN_HINT: &str =
    "Claude Code is installed but not logged in. Run `claude auth login`, then select this provider again.";
pub const LOGGED_OUT_HINT: &str = "Claude Code is installed but has no usable login in the environment Ultron runs it in. Run `claude auth login` as the user Ultron runs as, set CLAUDE_CODE_OAUTH_TOKEN (from `claude setup-token`) in Ultron's environment, or point CLAUDE_CONFIG_DIR at a logged-in config directory, then try again.";
/// Read-idle limit of one call when the request names none; reset by every native output line.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
/// `ULTRON_CLAUDE_TIMEOUT_MS`: the read-idle limit.
pub const TIMEOUT_ENV: &str = "ULTRON_CLAUDE_TIMEOUT_MS";
pub const USAGE_DIAGNOSTIC: &str = "claude_subscription_usage";
pub const ERROR_DIAGNOSTIC: &str = "claude_subscription_error";

/// A failed call. The status is what retry and fallback route on (`status_code` in the plugin).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaudeError {
    /// A request this provider refuses before starting anything.
    Invalid(String),
    /// Claude Code is not installed (503).
    Missing(String),
    /// Claude Code has no usable login (401).
    LoggedOut(String),
    /// The upstream or native answered with an error.
    Api {
        message: String,
        status: Option<u16>,
    },
    /// The plan's usage limit (429): the subscription is out of usage until `resets_at` (epoch ms) when reported.
    /// [`limit`] decides it; [`fallback`] may route the call to API credits.
    UsageLimit {
        message: String,
        resets_at: Option<i64>,
    },
    /// Malformed native output or another process failure.
    Failed(String),
    Cancelled,
    Timeout,
}

impl ClaudeError {
    pub fn status(&self) -> Option<u16> {
        match self {
            ClaudeError::Missing(_) => Some(503),
            ClaudeError::LoggedOut(_) => Some(401),
            ClaudeError::Api { status, .. } => *status,
            ClaudeError::UsageLimit { .. } => Some(429),
            _ => None,
        }
    }
}

impl std::fmt::Display for ClaudeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaudeError::Invalid(m) | ClaudeError::Missing(m) | ClaudeError::LoggedOut(m) | ClaudeError::Failed(m) => {
                f.write_str(m)
            }
            ClaudeError::Api { message, .. } | ClaudeError::UsageLimit { message, .. } => f.write_str(message),
            ClaudeError::Cancelled => f.write_str("Claude request cancelled"),
            ClaudeError::Timeout => f.write_str("Claude request timed out"),
        }
    }
}

/// How a call runs. [`Config::from_env`] is what the dispatch path uses; tests inject the rest.
#[derive(Clone, Debug)]
pub struct Config {
    /// argv prefix of Claude Code; else `ULTRON_CLAUDE_COMMAND`, else `claude` (PATH, then install dirs).
    pub command: Option<Vec<String>>,
    /// The child's whole environment; `None` is the process environment.
    pub env: Option<Env>,
    /// argv of the inert MCP server (the manifest path is appended); else `<this executable> __internal inert-mcp`.
    pub inert_mcp: Option<Vec<String>>,
    /// Read-idle limit of the call.
    pub timeout: Duration,
    /// Upstream byte-silence limit after the response headers.
    pub idle: Duration,
    pub telemetry: Option<bool>,
    /// Where the private directories go; else the system temp dir.
    pub temp_root: Option<PathBuf>,
    /// Where the admitted request goes: `https://api.anthropic.com`, or a loopback `http://` fixture.
    pub upstream: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            command: None,
            env: None,
            inert_mcp: None,
            timeout: DEFAULT_TIMEOUT,
            idle: admission::UPSTREAM_IDLE,
            telemetry: None,
            temp_root: None,
            upstream: catalog::UPSTREAM.into(),
        }
    }
}

/// The model's upstream: Anthropic itself, or a loopback HTTP fixture. Any other host would receive the
/// subscription's bearer token, so it is refused.
pub fn upstream_for(model: &Model) -> Result<String, ClaudeError> {
    let base = model.base_url.trim_end_matches('/');
    if base.is_empty() || base == catalog::UPSTREAM {
        return Ok(catalog::UPSTREAM.into());
    }
    let url = admission::check_upstream(base).map_err(ClaudeError::Invalid)?;
    if url.scheme() == "http" {
        return Ok(base.to_string());
    }
    Err(ClaudeError::Invalid(format!(
        "claude-subscription only sends the subscription's requests to {}; models.json baseUrl {base} is refused",
        catalog::UPSTREAM
    )))
}

/// The Ultron request as native input: system prompt, frames and extra body.
pub fn prepare(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
) -> Result<Prepared, ClaudeError> {
    let leading = context
        .messages
        .iter()
        .take_while(|m| matches!(m, Message::System(_)))
        .count();
    let system = crate::transcript::current_system_message(&context.messages[..leading])
        .map(|s| system_message_text(&s))
        .unwrap_or_default();
    let tools = current_tools(&context.messages);
    let reasoning = match options.reasoning.as_deref() {
        Some(level) => Reasoning {
            enabled: Some(true),
            effort: Some(level.to_string()),
        },
        None => Reasoning {
            enabled: Some(false),
            effort: None,
        },
    };
    let body = history::request_body(&BodyParams {
        model: &model.id,
        tools: &tools,
        max_tokens: options.base.max_tokens,
        reasoning: Some(reasoning),
        json_schema: None,
        stop_sequences: None,
    })?;
    let normalize = |id: &str, _: &AssistantMessage| -> String {
        id.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(64)
            .collect()
    };
    let transformed = transform_messages(&context.messages[leading..], model, Some(&normalize), now_ms());
    let frames = history::prepare_history(&transformed, &body.names)?;
    Ok(Prepared {
        model: model.id.clone(),
        system,
        frames,
        body,
    })
}

/// The config of a dispatched call: the process environment with the request's overlay, the request timeout.
pub fn config_for(model: &Model, options: &SimpleStreamOptions) -> Result<(Config, Env), ClaudeError> {
    let mut config = Config {
        upstream: upstream_for(model)?,
        ..Config::default()
    };
    let env = process::base_env(&config, options.base.env.as_ref());
    let timeout_ms = options.base.timeout_ms.or_else(|| {
        env.get(TIMEOUT_ENV)
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
    });
    if let Some(ms) = timeout_ms {
        config.timeout = Duration::from_millis(ms);
    }
    Ok((config, env))
}

fn diagnostic(kind: &str, details: Option<Map<String, Value>>, error: Option<String>) -> AssistantMessageDiagnostic {
    AssistantMessageDiagnostic {
        kind: kind.into(),
        timestamp: now_ms(),
        error: error.map(|message| DiagnosticErrorInfo {
            name: None,
            message,
            stack: None,
            code: None,
        }),
        details,
    }
}

fn number(value: &Value) -> u64 {
    value
        .as_u64()
        .unwrap_or_else(|| value.as_f64().map_or(0, |f| f.max(0.0) as u64))
}

/// Emits events into the in-progress message.
struct Emitter<'s, 'e> {
    output: AssistantMessage,
    sink: &'s mut EventSink<'e>,
    /// Native block index (or arrival order) -> content index, kind.
    streamed: Vec<(Option<u64>, &'static str, usize)>,
}

impl Emitter<'_, '_> {
    fn emit(&mut self, make: impl FnOnce(AssistantMessage) -> AssistantMessageEvent) {
        let event = make(self.output.clone());
        (self.sink)(&event);
    }

    fn delta(&mut self, delta: Delta) {
        let (index, kind, text) = match delta {
            Delta::Text(index, text) => (index, "text", text),
            Delta::Thinking(index, text) => (index, "thinking", text),
        };
        let existing = match index {
            Some(_) => self.streamed.iter().find(|(i, k, _)| *i == index && *k == kind),
            None => self.streamed.last().filter(|(i, k, _)| i.is_none() && *k == kind),
        };
        let at = match existing {
            Some((_, _, at)) => *at,
            None => {
                let at = self.output.content.len();
                self.output.content.push(if kind == "text" {
                    AssistantContent::Text(TextContent {
                        text: String::new(),
                        text_signature: None,
                    })
                } else {
                    AssistantContent::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    })
                });
                self.streamed.push((index, kind, at));
                if kind == "text" {
                    self.emit(|partial| AssistantMessageEvent::TextStart {
                        content_index: at,
                        partial,
                    });
                } else {
                    self.emit(|partial| AssistantMessageEvent::ThinkingStart {
                        content_index: at,
                        partial,
                    });
                }
                at
            }
        };
        match &mut self.output.content[at] {
            AssistantContent::Text(t) => t.text.push_str(&text),
            AssistantContent::Thinking(t) => t.thinking.push_str(&text),
            AssistantContent::ToolCall(_) => {}
        }
        if kind == "text" {
            self.emit(|partial| AssistantMessageEvent::TextDelta {
                content_index: at,
                delta: text,
                partial,
            });
        } else {
            self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                content_index: at,
                delta: text,
                partial,
            });
        }
    }

    /// Replace the streamed content with the final native blocks, closing every block.
    fn finish(&mut self, response: &NativeResponse) {
        let streamed: Vec<(usize, String)> = self
            .output
            .content
            .iter()
            .enumerate()
            .map(|(i, block)| match block {
                AssistantContent::Text(t) => (i, t.text.clone()),
                AssistantContent::Thinking(t) => (i, t.thinking.clone()),
                AssistantContent::ToolCall(_) => (i, String::new()),
            })
            .collect();
        let mut content = Vec::new();
        for block in &response.blocks {
            match block["type"].as_str() {
                Some("text") => content.push(AssistantContent::Text(TextContent {
                    text: block["text"].as_str().unwrap_or_default().to_string(),
                    text_signature: None,
                })),
                Some("thinking") => content.push(AssistantContent::Thinking(ThinkingContent {
                    thinking: block["thinking"].as_str().unwrap_or_default().to_string(),
                    thinking_signature: block["signature"].as_str().map(str::to_string),
                    redacted: None,
                })),
                Some("redacted_thinking") => content.push(AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: block["data"].as_str().map(str::to_string),
                    redacted: Some(true),
                })),
                Some("tool_use") => {
                    let name = block["name"].as_str().unwrap_or_default();
                    let host = name.strip_prefix(history::PREFIX).unwrap_or(name);
                    if response
                        .calls
                        .iter()
                        .any(|c| c.id == block["id"].as_str().unwrap_or_default())
                    {
                        content.push(AssistantContent::ToolCall(ToolCall {
                            id: block["id"].as_str().unwrap_or_default().to_string(),
                            name: host.to_string(),
                            arguments: block.get("input").cloned().unwrap_or(json!({})),
                            thought_signature: None,
                            namespace: None,
                        }));
                    }
                }
                _ => {}
            }
        }
        self.output.content = content;
        for index in 0..self.output.content.len() {
            let before = streamed.iter().find(|(i, _)| *i == index).map(|(_, t)| t.clone());
            match self.output.content[index].clone() {
                AssistantContent::Text(t) => {
                    match before {
                        Some(prefix) if t.text.starts_with(&prefix) => {
                            let rest = t.text[prefix.len()..].to_string();
                            if !rest.is_empty() {
                                self.emit(|partial| AssistantMessageEvent::TextDelta {
                                    content_index: index,
                                    delta: rest,
                                    partial,
                                });
                            }
                        }
                        _ => {
                            self.emit(|partial| AssistantMessageEvent::TextStart {
                                content_index: index,
                                partial,
                            });
                            let delta = t.text.clone();
                            self.emit(|partial| AssistantMessageEvent::TextDelta {
                                content_index: index,
                                delta,
                                partial,
                            });
                        }
                    }
                    self.emit(|partial| AssistantMessageEvent::TextEnd {
                        content_index: index,
                        content: t.text,
                        partial,
                    });
                }
                AssistantContent::Thinking(t) => {
                    match before {
                        Some(prefix) if t.thinking.starts_with(&prefix) => {
                            let rest = t.thinking[prefix.len()..].to_string();
                            if !rest.is_empty() {
                                self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                                    content_index: index,
                                    delta: rest,
                                    partial,
                                });
                            }
                        }
                        _ => {
                            self.emit(|partial| AssistantMessageEvent::ThinkingStart {
                                content_index: index,
                                partial,
                            });
                            let delta = t.thinking.clone();
                            self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                                content_index: index,
                                delta,
                                partial,
                            });
                        }
                    }
                    self.emit(|partial| AssistantMessageEvent::ThinkingEnd {
                        content_index: index,
                        content: t.thinking,
                        partial,
                    });
                }
                AssistantContent::ToolCall(call) => {
                    self.emit(|partial| AssistantMessageEvent::ToolCallStart {
                        content_index: index,
                        partial,
                    });
                    self.emit(|partial| AssistantMessageEvent::ToolCallEnd {
                        content_index: index,
                        tool_call: call,
                        partial,
                    });
                }
            }
        }
    }
}

/// Fill usage, stop reason, carrier and diagnostics from the completed turn.
fn complete(output: &mut AssistantMessage, response: &NativeResponse) {
    let usage = &response.usage;
    output.usage.input = number(&usage["input_tokens"]);
    output.usage.output = number(&usage["output_tokens"]);
    output.usage.cache_read = number(&usage["cache_read_input_tokens"]);
    output.usage.cache_write = number(&usage["cache_creation_input_tokens"]);
    output.usage.cache_write_1h = usage["cache_creation"]["ephemeral_1h_input_tokens"].as_u64();
    output.usage.reasoning = usage["output_tokens_details"]["thinking_tokens"].as_u64();
    output.usage.total_tokens =
        output.usage.input + output.usage.output + output.usage.cache_read + output.usage.cache_write;
    output.response_id = response
        .assistants
        .last()
        .and_then(|a| a["id"].as_str())
        .map(str::to_string);
    output.response_model = response
        .assistants
        .last()
        .and_then(|a| a["model"].as_str())
        .map(str::to_string);
    output.raw_stop_reason = response
        .assistants
        .last()
        .and_then(|a| a["stop_reason"].as_str())
        .map(str::to_string);
    output.stop_reason = match response.finish {
        Finish::ToolCalls => StopReason::ToolUse,
        Finish::Length => StopReason::Length,
        Finish::Stop => StopReason::Stop,
        Finish::ContentFilter => StopReason::Error,
    };
    if response.finish == Finish::ContentFilter {
        output.error_message = Some(
            response
                .refusal
                .clone()
                .unwrap_or_else(|| "The model refused to complete the request".into()),
        );
    }
    let carrier = history::carrier(response.assistants.clone(), history::projection(output));
    output.provider_metadata = Some([(CARRIER_KEY.to_string(), carrier)].into_iter().collect());
    let mut details = Map::new();
    details.insert("native_usage".into(), usage.clone());
    details.insert("native_cost".into(), response.cost.clone());
    details.insert("native_admission".into(), response.admission.clone());
    let mut diagnostics = vec![diagnostic(USAGE_DIAGNOSTIC, Some(details), None)];
    for warning in &response.warnings {
        diagnostics.push(diagnostic("claude_subscription_warning", None, Some(warning.clone())));
    }
    output.diagnostics = Some(diagnostics);
}

/// One model call as an assistant-message stream. Errors end the stream with an `error` event, never a panic.
pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let prepared = prepare(model, context, options);
    let setup = config_for(model, options);
    run_with(model, prepared, setup, options, sink).await
}

/// [`stream_simple`] with an explicit config and environment (tests and embedders).
pub async fn stream_with(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    config: Config,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let prepared = prepare(model, context, options);
    let env = process::base_env(&config, options.base.env.as_ref());
    run_with(model, prepared, Ok((config, env)), options, sink).await
}

async fn run_with(
    model: &Model,
    prepared: Result<Prepared, ClaudeError>,
    setup: Result<(Config, Env), ClaudeError>,
    options: &SimpleStreamOptions,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let mut output = AssistantMessage::pending(model, now_ms());
    output.stop_reason = StopReason::Stop;
    let mut emitter = Emitter {
        output,
        sink,
        streamed: Vec::new(),
    };
    emitter.emit(|partial| AssistantMessageEvent::Start { partial });
    let signal = options.base.signal.clone();
    let result = match (prepared, setup) {
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(_), Ok(_)) if signal.as_ref().is_some_and(crate::http::AbortSignal::aborted) => Err(ClaudeError::Cancelled),
        (Ok(prepared), Ok((config, env))) => {
            let handle = Handle::new();
            let (deltas, mut received) = tokio::sync::mpsc::unbounded_channel::<Delta>();
            let (done, mut finished) = tokio::sync::oneshot::channel();
            let worker = handle.clone();
            std::thread::spawn(move || {
                let mut forward = |delta: Delta| {
                    let _ = deltas.send(delta);
                };
                let _ = done.send(process::run(&config, env, &prepared, &worker, &mut forward));
            });
            loop {
                tokio::select! {
                    biased;
                    Some(delta) = received.recv() => emitter.delta(delta),
                    result = &mut finished => {
                        while let Ok(delta) = received.try_recv() {
                            emitter.delta(delta);
                        }
                        break result.unwrap_or(Err(ClaudeError::Failed("Native response missing".into())));
                    }
                    () = crate::http::cancelled(signal.as_ref()) => handle.cancel(),
                }
            }
        }
    };
    match result {
        Ok(response) => {
            emitter.finish(&response);
            complete(&mut emitter.output, &response);
            if emitter.output.stop_reason == StopReason::Error {
                let reason = StopReason::Error;
                emitter.emit(|error| AssistantMessageEvent::Error { reason, error });
            } else {
                let reason = emitter.output.stop_reason;
                emitter.emit(|message| AssistantMessageEvent::Done { reason, message });
            }
        }
        Err(error) => {
            let aborted =
                error == ClaudeError::Cancelled && signal.as_ref().is_some_and(crate::http::AbortSignal::aborted);
            let mut message = error.to_string();
            if message.to_ascii_lowercase().contains("does not support this model")
                && let Some(hint) = catalog::version_hint(&model.id)
            {
                message.push_str(&format!(" ({hint})"));
            }
            let output = &mut emitter.output;
            output.stop_reason = if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            output.error_message = Some(message.clone());
            let mut details = Map::new();
            details.insert("status".into(), error.status().map_or(Value::Null, Value::from));
            if let ClaudeError::UsageLimit { resets_at, .. } = &error {
                details.insert("usage_limit".into(), Value::Bool(true));
                details.insert("resets_at".into(), resets_at.map_or(Value::Null, Value::from));
            }
            let mut diagnostics = vec![diagnostic(ERROR_DIAGNOSTIC, Some(details), Some(message.clone()))];
            if error.status() == Some(429) {
                diagnostics.push(diagnostic(
                    crate::claude_code_cli::USAGE_LIMIT_DIAGNOSTIC,
                    None,
                    Some(message),
                ));
            }
            output.diagnostics = Some(diagnostics);
            let reason = output.stop_reason;
            emitter.emit(|error| AssistantMessageEvent::Error { reason, error });
        }
    }
    emitter.output
}
