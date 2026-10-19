//! `listCommands` of the session control (session-control-provider.ts), as RPC `get_commands` answers it: the
//! built-in `/mcp` (TypeScript's inline MCP extension, `source: "extension"`), `/review`, the prompt templates and
//! the skills (`skill:<name>`), each with its `SourceInfo` as the resource loader gives it
//! (crate::session_resources): auto-discovered ones `{source: "auto", scope, origin: "top-level", baseDir}`,
//! settings entries `local`; the bundled skills the worker passes as paths are `{source: "local", scope:
//! "temporary"}` with the skill's directory as `baseDir`. Pi extensions are not supported by ultron-rs, so no other
//! extension command is listed.

use serde_json::{Map, Value, json};

/// `REVIEW_COMMAND` of ultron/review.ts.
const REVIEW_DESCRIPTION: &str = "Review code changes: specialist reviewer frames, then a verifier per finding";

const MCP_DESCRIPTION: &str = "MCP servers: status, connect or disconnect, sign in, enable or disable";

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
pub fn list_commands(cwd: &str, agent_dir: &str, bundled: Option<&str>) -> Vec<Value> {
    list_commands_with(cwd, agent_dir, bundled, &Default::default())
}

/// [`list_commands`] with the session's resources (crate::session_resources): its prompts and skills are commands,
/// each with its `SourceInfo` (`resourceSourceInfo`).
pub fn list_commands_with(
    cwd: &str,
    agent_dir: &str,
    bundled: Option<&str>,
    extra: &crate::session_resources::SessionResources,
) -> Vec<Value> {
    let mut commands: Vec<Value> = vec![mcp_command()];
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

/// `/mcp`, the built-in MCP gateway's command, as `get_commands` lists it.
pub fn mcp_command() -> Value {
    let inline = json!({"path": "<inline:mcp>", "source": "inline", "scope": "temporary", "origin": "top-level"});
    entry("mcp", Some(MCP_DESCRIPTION), "extension", Some(inline))
}

/// `{name, description?, sourceInfo?}`: a `source: "extension"` command (only `/mcp`) as
/// `pi.legacy-extension-commands.list` answers it.
pub fn command_info(command: &Value) -> Value {
    let mut out = Map::new();
    out.insert("name".into(), command["name"].clone());
    for key in ["description", "sourceInfo"] {
        if let Some(value) = command.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    Value::Object(out)
}

/// `listCommands` of the session control service: each command with `description` and `sourceInfo`, `null`
/// when it has none (`SessionCommandInfo`).
pub fn session_command_infos(commands: &[Value]) -> Vec<Value> {
    commands
        .iter()
        .map(|c| {
            json!({"name": c["name"], "description": c.get("description").cloned().unwrap_or(Value::Null),
                "source": c["source"], "sourceInfo": c.get("sourceInfo").cloned().unwrap_or(Value::Null)})
        })
        .collect()
}
