//! `listCommands` of the session control (session-control-provider.ts), as RPC `get_commands` answers it: the
//! extensions' slash commands in load order with their `SourceInfo` (crate::extension_commands; the built-in `/mcp`
//! last, as the inline extension TS loads after the discovered ones), `/review`, the prompt
//! templates and the skills (`skill:<name>`), each with its source. Prompt templates and skills found in the profile
//! or project carry Pi's auto-discovered `SourceInfo` (`{path, source: "auto", scope, origin: "top-level",
//! baseDir}`, package-manager.ts); the bundled skills the worker passes as paths are `{source: "local", scope:
//! "temporary"}` with the skill's directory as `baseDir`.

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

/// Pi's auto-discovered `SourceInfo` of a resource file in `scope` (`user`: the profile, `project`: the project's
/// config directory); none for a resource from anywhere else.
fn auto_source(path: &str, scope: &str, cwd: &str, agent_dir: &str) -> Option<Value> {
    let base = match scope {
        "user" => agent_dir.to_string(),
        "project" => format!("{cwd}/{}", resources::skills::CONFIG_DIR_NAME),
        _ => return None,
    };
    Some(json!({"path": path, "source": "auto", "scope": scope, "origin": "top-level", "baseDir": base}))
}

/// The commands of a session in `cwd` with profile `agent_dir` and the bundled skills in `bundled`.
pub fn list_commands(cwd: &str, agent_dir: &str, bundled: &str, extensions: &[ExtensionCommand]) -> Vec<Value> {
    let inline = json!({"path": "<inline:mcp>", "source": "inline", "scope": "temporary", "origin": "top-level"});
    let mut commands: Vec<Value> = extensions
        .iter()
        .map(|c| entry(&c.name, c.description.as_deref(), "extension", c.source_info.clone()))
        .collect();
    commands.push(entry("mcp", Some(MCP_DESCRIPTION), "extension", Some(inline)));
    commands.push(entry("review", Some(REVIEW_DESCRIPTION), "prompt", None));
    for template in resources::prompt_templates::load_prompt_templates(cwd, agent_dir, &[], true).templates {
        let info = auto_source(&template.file_path, template.scope, cwd, agent_dir);
        commands.push(entry(&template.name, Some(&template.description), "prompt", info));
    }
    let skills = resources::skills::load_skills(&resources::skills::LoadSkillsOptions {
        cwd,
        agent_dir,
        skill_paths: &[],
        include_defaults: true,
    })
    .skills;
    let bundled = resources::skills::load_skills_from_dir(bundled, "bundled").skills;
    for skill in skills.into_iter().chain(bundled) {
        let info = match skill.source.as_str() {
            "bundled" => Some(json!({"path": skill.file_path, "source": "local", "scope": "temporary",
                "origin": "top-level", "baseDir": skill.base_dir})),
            scope => auto_source(&skill.file_path, scope, cwd, agent_dir),
        };
        commands.push(entry(
            &format!("skill:{}", skill.name),
            Some(&skill.description),
            "skill",
            info,
        ));
    }
    commands
}
