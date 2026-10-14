//! The session's REPL tools for `tools.*` (kernel_apis::tool_calls): the built-in `mcp` gateway (crates/mcp
//! `gateway`, the MCP servers of `mcp.json`) and the functions extension processes register (crate::extension_tools).
//! As in TypeScript (extensions/mcp/coexistence.ts), an extension that registers its own `mcp` tool takes over
//! from the built-in gateway. None of them is a model tool: the model's only tool is `rlm`. Every call's arguments
//! are validated and coerced against the tool's schema first (`prepareToolArguments`).

use crate::extension_tools::ExtensionTools;
use kernel::Signal;
use kernel_apis::{ToolInfo, ToolRunner};
use mcp::gateway::{Gateway, MCP_GATEWAY_TOOL};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;

pub struct SessionTools {
    pub extensions: Option<ExtensionTools>,
    pub mcp: Option<Arc<Gateway>>,
}

impl SessionTools {
    fn extension_tools(&self) -> Vec<ToolInfo> {
        self.extensions.as_ref().map(ToolRunner::tools).unwrap_or_default()
    }

    /// The gateway, unless an extension registers `mcp` itself.
    fn gateway(&self, extension_tools: &[ToolInfo]) -> Option<&Arc<Gateway>> {
        if extension_tools.iter().any(|tool| tool.name == MCP_GATEWAY_TOOL) {
            return None;
        }
        self.mcp.as_ref().filter(|gateway| gateway.tool().is_some())
    }
}

impl ToolRunner for SessionTools {
    fn tools(&self) -> Vec<ToolInfo> {
        let extension_tools = self.extension_tools();
        let mut tools: Vec<ToolInfo> = self
            .gateway(&extension_tools)
            .and_then(|gateway| gateway.tool())
            .map(|tool| ToolInfo {
                name: tool.name,
                label: Some(tool.label),
                description: tool.description,
                parameters: tool.parameters,
            })
            .into_iter()
            .collect();
        tools.extend(extension_tools);
        tools
    }

    fn execute(
        &self,
        name: &str,
        call_id: &str,
        params: &Map<String, Value>,
        cancel: &Signal,
    ) -> Result<Value, String> {
        // `prepareToolArguments`: validated and coerced against the tool's schema, as the harness does for a model's
        // call; a failure fails the call with TypeBox's message.
        let tools = self.tools();
        let params = match tools.iter().find(|tool| tool.name == name) {
            Some(tool) => &mcp::validation::prepare_tool_arguments(name, &tool.parameters, params)?,
            None => params,
        };
        if name == MCP_GATEWAY_TOOL
            && let Some(gateway) = self.gateway(&self.extension_tools())
        {
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
            let result = gateway.execute(params, Some(&token));
            let _ = done_tx.send(());
            let _ = watcher.join();
            return result;
        }
        match &self.extensions {
            Some(extensions) => extensions.execute(name, call_id, params, cancel),
            None => Err(format!("Unknown extension tool \"{name}\"")),
        }
    }
}
