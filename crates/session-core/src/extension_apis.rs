//! `extensions.list` / `extensions.run` from a cell (worker-services.ts `handle`): the extensions' slash commands as
//! the legacy extension command service lists them (`{name, description?, sourceInfo?}`), and running one
//! (`{notifications: []}` once its handler returned).

use extensions::ExtensionHost;
use kernel_apis::{ApiModule, Request};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

pub struct ExtensionCommandsApi {
    extensions: Option<Arc<ExtensionHost>>,
    cwd: PathBuf,
    agent_dir: PathBuf,
    home: PathBuf,
}

impl ExtensionCommandsApi {
    pub fn new(extensions: Option<Arc<ExtensionHost>>, cwd: PathBuf, agent_dir: PathBuf, home: PathBuf) -> Self {
        ExtensionCommandsApi {
            extensions,
            cwd,
            agent_dir,
            home,
        }
    }

    /// `legacyExtensions.commands`: every registered command by invocation name, with its Pi `SourceInfo`.
    pub fn list(&self) -> Value {
        let Some(host) = &self.extensions else { return json!([]) };
        let trusted = crate::extension_commands::worker_project_trusted(&self.cwd, &self.agent_dir, &self.home);
        let settings = config::settings::SettingsManager::create(&self.cwd, &self.agent_dir, trusted);
        Value::Array(
            host.router()
                .registered_commands()
                .into_iter()
                .map(|command| {
                    crate::extension_commands::command_info(&crate::extension_commands::ExtensionCommand {
                        source_info: crate::extension_commands::source_info(
                            std::path::Path::new(&command.extension_path),
                            &settings,
                            &self.cwd,
                            &self.agent_dir,
                            &self.home,
                        ),
                        name: command.invocation_name,
                        description: command.description,
                    })
                })
                .collect(),
        )
    }
}

impl ApiModule for ExtensionCommandsApi {
    fn prefixes(&self) -> &[&str] {
        &["extensions."]
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        match request.kind {
            "extensions.list" => Ok(self.list()),
            "extensions.run" => {
                let (Some(name), Some(args)) = (
                    request.payload.get("name").and_then(Value::as_str),
                    request.payload.get("args").and_then(Value::as_str),
                ) else {
                    return Err("Extension command service is unavailable".into());
                };
                let Some(host) = self.extensions.as_ref().filter(|h| {
                    h.router()
                        .registered_commands()
                        .iter()
                        .any(|c| c.invocation_name == name)
                }) else {
                    return Err(format!("Unknown extension command: /{name}"));
                };
                host.run_command(name, args, None).map_err(|e| e.to_string())?;
                Ok(json!({"notifications": []}))
            }
            other => Err(format!("Unknown extension request: {other}")),
        }
    }
}
