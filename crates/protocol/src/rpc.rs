//! RPC mode wire types (packages/coding-agent/src/modes/rpc/rpc-types.ts) and its strict JSONL framing
//! (jsonl.ts): commands arrive as JSON lines on stdin; responses and events leave as JSON lines on stdout.
//! Response payloads stay as JSON values; their typed shapes belong to the crates that produce them.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// Serialize one strict JSONL record (LF-terminated; U+2028/U+2029 stay literal).
pub fn serialize_json_line<T: Serialize>(value: &T) -> serde_json::Result<String> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    Ok(line)
}

/// LF-only line splitter over UTF-8 byte chunks; a trailing `\r` is dropped from each line.
/// Unlike readline, it never splits on other Unicode separators.
#[derive(Debug, Default)]
pub struct JsonlLineReader {
    pending: Vec<u8>,
}

impl JsonlLineReader {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(chunk);
        let mut lines = Vec::new();
        while let Some(index) = self.pending.iter().position(|&b| b == b'\n') {
            let rest = self.pending.split_off(index + 1);
            let mut line = std::mem::replace(&mut self.pending, rest);
            line.pop();
            lines.push(emit(&line));
        }
        lines
    }

    /// End of stream: an unterminated final record is still a line.
    pub fn end(&mut self) -> Option<String> {
        let line = std::mem::take(&mut self.pending);
        (!line.is_empty()).then(|| emit(&line))
    }
}

fn emit(line: &[u8]) -> String {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    String::from_utf8_lossy(line).into_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "image", rename_all = "camelCase")]
pub struct ImageContent {
    pub data: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingBehavior {
    Steer,
    FollowUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueMode {
    #[serde(rename = "all")]
    All,
    #[serde(rename = "one-at-a-time")]
    OneAtATime,
}

/// One stdin command: `{ id?, type, ...fields }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcCommand {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum Command {
    Prompt {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        images: Option<Vec<ImageContent>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        streaming_behavior: Option<StreamingBehavior>,
    },
    Steer {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        images: Option<Vec<ImageContent>>,
    },
    FollowUp {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        images: Option<Vec<ImageContent>>,
    },
    Abort,
    ClearQueue,
    NewSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_session: Option<String>,
    },
    GetState,
    SetModel {
        provider: String,
        model_id: String,
    },
    CycleModel,
    GetAvailableModels,
    SetThinkingLevel {
        level: String,
    },
    CycleThinkingLevel,
    GetAvailableThinkingLevels,
    SetSteeringMode {
        mode: QueueMode,
    },
    SetFollowUpMode {
        mode: QueueMode,
    },
    Compact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
    },
    SetAutoCompaction {
        enabled: bool,
    },
    SetAutoRetry {
        enabled: bool,
    },
    AbortRetry,
    Bash {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exclude_from_context: Option<bool>,
    },
    AbortBash,
    GetSessionStats,
    ExportHtml {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_path: Option<String>,
    },
    SwitchSession {
        session_path: String,
    },
    Fork {
        entry_id: String,
    },
    Clone,
    GetForkMessages,
    GetEntries {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<String>,
    },
    GetTree,
    GetLastAssistantText,
    SetSessionName {
        name: String,
    },
    GetMessages,
    GetCommands,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseTag {
    Response,
}

/// One stdout response: `{ id?, type: "response", command, success, data? | error }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub tag: ResponseTag,
    pub command: String,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Json>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RpcResponse {
    pub fn success(id: Option<String>, command: &str, data: Option<Json>) -> Self {
        Self {
            id,
            tag: ResponseTag::Response,
            command: command.into(),
            success: true,
            data,
            error: None,
        }
    }

    pub fn error(id: Option<String>, command: &str, message: &str) -> Self {
        Self {
            id,
            tag: ResponseTag::Response,
            command: command.into(),
            success: false,
            data: None,
            error: Some(message.into()),
        }
    }
}

/// A client's answer to an `extension_ui_request`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "extension_ui_response")]
pub struct ExtensionUiResponse {
    pub id: String,
    #[serde(flatten)]
    pub answer: ExtensionUiAnswer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExtensionUiAnswer {
    Value { value: String },
    Confirmed { confirmed: bool },
    Cancelled { cancelled: bool },
}
