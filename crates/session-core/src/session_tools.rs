//! The session's REPL tools for `tools.*` (kernel_apis::tool_calls): the built-in `mcp` gateway (crates/mcp
//! `gateway`, the MCP servers of `mcp.json`). It is not a model tool: the model's only tool is `rlm`. Every call's
//! arguments are validated and coerced against the tool's schema first (`prepareToolArguments`). Pi extensions are
//! not supported by ultron-rs, so no other tool is listed.

use kernel::Signal;
use kernel_apis::tool_calls::ToolUpdate;
use kernel_apis::{ToolInfo, ToolRunner};
use mcp::gateway::{Gateway, MCP_GATEWAY_TOOL};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;

pub struct SessionTools {
    pub mcp: Arc<Gateway>,
}

impl ToolRunner for SessionTools {
    fn tools(&self) -> Vec<ToolInfo> {
        self.mcp
            .tool()
            .map(|tool| ToolInfo {
                name: tool.name,
                label: Some(tool.label),
                description: tool.description,
                parameters: tool.parameters,
                execution_mode: None,
            })
            .into_iter()
            .collect()
    }

    fn execute(
        &self,
        name: &str,
        call_id: &str,
        params: &Map<String, Value>,
        cancel: &Signal,
    ) -> Result<Value, String> {
        self.execute_with_updates(name, call_id, params, cancel, &|_: &Value| {})
    }

    fn execute_with_updates(
        &self,
        name: &str,
        _call_id: &str,
        params: &Map<String, Value>,
        cancel: &Signal,
        _on_update: ToolUpdate<'_>,
    ) -> Result<Value, String> {
        let Some(tool) = self
            .tools()
            .into_iter()
            .find(|tool| tool.name == name && name == MCP_GATEWAY_TOOL)
        else {
            return Err(format!("Unknown tool \"{name}\""));
        };
        // `prepareToolArguments`: validated and coerced against the tool's schema, as the harness does for a model's
        // call; a failure fails the call with TypeBox's message.
        let params = mcp::validation::prepare_tool_arguments(name, &tool.parameters, params)?;
        let token = mcp::CancelToken::new();
        let (watched, signal) = (token.clone(), cancel.clone());
        let (done_tx, done) = std::sync::mpsc::channel::<()>();
        // The call's cancel signal reaches the server as `notifications/cancelled`.
        let watcher = std::thread::spawn(move || {
            loop {
                if signal.wait(Duration::from_millis(100)) {
                    watched.cancel(Some(&signal.reason().unwrap_or_else(|| "tool call cancelled".into())));
                    return;
                }
                if done.try_recv().is_ok() {
                    return;
                }
            }
        });
        let result = self.mcp.execute(&params, Some(&token));
        let _ = done_tx.send(());
        let _ = watcher.join();
        result
    }
}
