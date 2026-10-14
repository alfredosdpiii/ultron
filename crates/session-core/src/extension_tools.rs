//! The session's extension tools for `tools.*` (kernel_apis::tool_calls): the functions extension processes register
//! (TypeScript `registerTool`, crates/extensions `functions`), called through `functions/call`. A tool is named by
//! its function name; when two extensions register the same name the first loaded wins (`ext.<extension>.<name>`
//! still reaches either). They are never model tools: the model's only tool is `rlm`.

use extensions::{CancelToken, ExtensionHost};
use kernel::Signal;
use kernel_apis::{ToolInfo, ToolRunner};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;

pub struct ExtensionTools {
    host: Arc<ExtensionHost>,
}

impl ExtensionTools {
    pub fn new(host: Arc<ExtensionHost>) -> Self {
        Self { host }
    }

    /// The extension that registered function `name` first.
    fn owner(&self, name: &str) -> Option<String> {
        self.host
            .functions()
            .into_iter()
            .find(|(_, f)| f.name == name)
            .map(|(extension, _)| extension)
    }
}

impl ToolRunner for ExtensionTools {
    fn tools(&self) -> Vec<ToolInfo> {
        let mut tools: Vec<ToolInfo> = Vec::new();
        for (_, function) in self.host.functions() {
            if tools.iter().any(|t| t.name == function.name) {
                continue;
            }
            tools.push(ToolInfo {
                name: function.name,
                label: None,
                description: function.description,
                parameters: function.parameters,
            });
        }
        tools
    }

    fn execute(
        &self,
        name: &str,
        _call_id: &str,
        params: &Map<String, Value>,
        cancel: &Signal,
    ) -> Result<Value, String> {
        let extension = self
            .owner(name)
            .ok_or_else(|| format!("Unknown extension tool \"{name}\""))?;
        let token = CancelToken::new();
        let (watched, signal) = (token.clone(), cancel.clone());
        // The call's cancel signal reaches the extension as `notifications/cancelled`.
        let watcher = std::thread::spawn(move || {
            while !signal.wait(Duration::from_millis(100)) {
                if watched.is_cancelled() {
                    return;
                }
            }
            watched.cancel(Some(&signal.reason().unwrap_or_else(|| "tool call cancelled".into())));
        });
        let result = self
            .host
            .call_function(&extension, name, Value::Object(params.clone()), None, Some(&token))
            .map_err(|e| e.to_string());
        if !token.is_cancelled() {
            token.cancel(None);
        }
        let _ = watcher.join();
        let result = result?;
        // `isError`: the tool failed; its text is the error (`AgentToolResult` of a throwing tool).
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            let text = kernel_apis::tool_calls::result_text(&result);
            return Err(if text.is_empty() {
                format!("{name} failed")
            } else {
                text
            });
        }
        Ok(result)
    }
}
