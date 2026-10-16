//! `listCommands` of the session control (session-control-provider.ts), as RPC `get_commands` answers it: the
//! extensions' slash commands in load order with their `SourceInfo` (crate::extension_commands; the built-in `/mcp`
//! last, as the inline extension TS loads after the discovered ones), `/review`, the prompt
//! templates and the skills (`skill:<name>`), each with its `SourceInfo` as the resource loader gives it
//! (crate::session_resources): auto-discovered ones `{source: "auto", scope, origin: "top-level", baseDir}`, settings
//! entries `local`, packages theirs, `-e` skills `cli`; the bundled skills the worker passes as paths are
//! `{source: "local", scope: "temporary"}` with the skill's directory as `baseDir`.

use serde_json::{Map, Value, json};

/// `REVIEW_COMMAND` of ultron/review.ts.
const REVIEW_DESCRIPTION: &str = "Review code changes: specialist reviewer frames, then a verifier per finding";

const MCP_DESCRIPTION: &str = "MCP servers: status, connect or disconnect, sign in, enable or disable";

pub use crate::extension_commands::ExtensionCommand;

fn entry(name: &str, description: Option<&str>, source: &str, info: Option<Value>) -> Value {
    let mut out = Map::new();
    out.insert("name".into(), name.into());
    if let Some(description) = description {
        out.insert("description".into(), description.into());
    }
    out.insert("source".into(), source.into());
    if let Some(info) = info {
        out.insert("sourceInfo".into(), info);
    }
    Value::Object(out)
}

/// The commands of a session in `cwd` with profile `agent_dir`, the bundled skills in `bundled` and no resolved
/// resources (only the bundled skills; [`list_commands_with`] takes the session's).
pub fn list_commands(cwd: &str, agent_dir: &str, bundled: Option<&str>, extensions: &[ExtensionCommand]) -> Vec<Value> {
    list_commands_with(cwd, agent_dir, bundled, extensions, &Default::default())
}

/// [`list_commands`] with the session's resources (crate::session_resources): its prompts and skills are commands,
/// each with its `SourceInfo` (`resourceSourceInfo`).
pub fn list_commands_with(
    cwd: &str,
    agent_dir: &str,
    bundled: Option<&str>,
    extensions: &[ExtensionCommand],
    extra: &crate::session_resources::SessionResources,
) -> Vec<Value> {
    let inline = json!({"path": "<inline:mcp>", "source": "inline", "scope": "temporary", "origin": "top-level"});
    let mut commands: Vec<Value> = extensions
        .iter()
        .map(|c| entry(&c.name, c.description.as_deref(), "extension", c.source_info.clone()))
        .collect();
    commands.push(entry("mcp", Some(MCP_DESCRIPTION), "extension", Some(inline)));
    commands.push(entry("review", Some(REVIEW_DESCRIPTION), "prompt", None));
    for template in crate::session_resources::prompts(cwd, agent_dir, extra) {
        let info = crate::session_resources::prompt_source_info(extra, &template, cwd, agent_dir);
        commands.push(entry(&template.name, Some(&template.description), "prompt", Some(info)));
    }
    for skill in crate::session_resources::skills(cwd, agent_dir, extra, false, bundled) {
        let info = crate::session_resources::skill_source_info(extra, &skill);
        commands.push(entry(
            &format!("skill:{}", skill.name),
            Some(&skill.description),
            "skill",
            Some(info),
        ));
    }
    commands
}
