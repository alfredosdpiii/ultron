//! What `main()` (main.ts) settles before a native run: `--offline` and the `httpProxy` setting as environment
//! (TypeScript writes them into `process.env`; here they overlay the run's [`Env`] and reach the session workers
//! through [`worker_env`]), the piped stdin prompt (`readPipedStdin`), the hint after a failed start
//! (`noModelHint`, cli/setup/command.ts), and what the tool flags mean when the model's only tool is `rlm`.

use crate::Env;
use crate::args::Args;
use std::sync::Arc;

/// The variables a run may set for itself; workers get the ones whose value differs from this process's.
const RUN_VARIABLES: [&str; 4] = ["PI_OFFLINE", "PI_SKIP_VERSION_CHECK", "HTTP_PROXY", "HTTPS_PROXY"];

/// `isTruthyEnvFlag`: `1`, `true` or `yes` (any case).
pub fn truthy(value: Option<&str>) -> bool {
    value.is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
}

/// `env` with `overrides` taking precedence.
fn overlay(env: &Env, overrides: Vec<(String, String)>) -> Env {
    if overrides.is_empty() {
        return env.clone();
    }
    let inner = env.var.clone();
    Env {
        var: Arc::new(move |key| {
            overrides
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .or_else(|| inner(key))
        }),
        ..env.clone()
    }
}

/// `--offline` (or a truthy `PI_OFFLINE`): `PI_OFFLINE=1` and `PI_SKIP_VERSION_CHECK=1` for the whole run.
pub fn offline_env(argv: &[String], env: &Env) -> Env {
    let offline = argv.iter().any(|a| a == "--offline") || truthy((env.var)("PI_OFFLINE").as_deref());
    if !offline {
        return env.clone();
    }
    overlay(
        env,
        vec![
            ("PI_OFFLINE".into(), "1".into()),
            ("PI_SKIP_VERSION_CHECK".into(), "1".into()),
        ],
    )
}

/// `applyHttpProxySettings(globalSettings.httpProxy)`: `HTTP_PROXY` and `HTTPS_PROXY` default to the trimmed
/// setting.
pub fn proxy_env(env: &Env) -> Env {
    let settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    let proxy = settings
        .global_settings()
        .get("httpProxy")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    let Some(proxy) = proxy else {
        return env.clone();
    };
    let overrides = ["HTTP_PROXY", "HTTPS_PROXY"]
        .into_iter()
        .filter(|key| (env.var)(key).is_none())
        .map(|key| (key.to_string(), proxy.clone()))
        .collect();
    overlay(env, overrides)
}

/// The run's own variables (see [`RUN_VARIABLES`]) that this process's environment does not already have, for the
/// session workers it starts.
pub fn worker_env(env: &Env) -> Vec<(String, String)> {
    RUN_VARIABLES
        .iter()
        .filter_map(|key| {
            let value = (env.var)(key)?;
            (std::env::var(key).ok().as_deref() != Some(value.as_str())).then(|| (key.to_string(), value))
        })
        .collect()
}

/// `readPipedStdin`'s result for the text read: trimmed, `None` when blank.
pub fn piped_prompt(text: &str) -> Option<String> {
    let trimmed = text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// `readPipedStdin`: all of stdin when it is not a terminal (RPC mode reads its commands there instead).
pub fn read_piped_stdin(parsed: &Args) -> Option<String> {
    use std::io::{IsTerminal, Read};
    if parsed.mode.as_deref() == Some("rpc") || std::io::stdin().is_terminal() {
        return None;
    }
    let mut bytes = Vec::new();
    std::io::stdin().lock().read_to_end(&mut bytes).ok()?;
    piped_prompt(&String::from_utf8_lossy(&bytes))
}

/// `noModelHint`: the hint after a failed start with no model to start with (crate::setup::start_model_state).
pub fn no_model_hint(env: &Env) -> Option<String> {
    crate::setup::no_model_hint(env)
}

/// The model's tools TypeScript's worker would activate for these flags (`effectiveActiveToolNames` over the
/// built-ins, whose default is `[rlm]`), and the warnings for what the Rust build cannot do: its model's tool list is
/// exactly `[rlm]`, so flags that would take `rlm` away or add a native tool are reported and the list stays.
pub fn tool_flag_warnings(parsed: &Args) -> Vec<String> {
    const BUILTINS: [&str; 8] = ["read", "bash", "edit", "write", "grep", "find", "ls", "rlm"];
    if !parsed.no_tools && parsed.tools.is_none() && parsed.exclude_tools.is_none() && !parsed.no_builtin_tools {
        return Vec::new();
    }
    let mut active: Vec<String> = if parsed.no_tools {
        Vec::new()
    } else {
        match &parsed.tools {
            None => vec!["rlm".to_string()],
            Some(tools) => BUILTINS
                .iter()
                .filter(|name| tools.iter().any(|t| t == *name))
                .map(|name| name.to_string())
                .collect(),
        }
    };
    if let Some(excluded) = &parsed.exclude_tools {
        active.retain(|name| !excluded.contains(name));
    }
    if parsed.no_builtin_tools {
        active.retain(|name| name == "rlm");
    }
    let mut warnings = Vec::new();
    let native: Vec<&str> = active.iter().map(String::as_str).filter(|n| *n != "rlm").collect();
    if !native.is_empty() {
        warnings.push(format!(
            "{} {} not a model tool here: the model's only tool is rlm, whose REPL has {} as Python functions",
            native.join(", "),
            if native.len() == 1 { "is" } else { "are" },
            native.join(", ")
        ));
    }
    if !active.iter().any(|n| n == "rlm") {
        warnings
            .push("The model keeps the rlm tool: it is the model's only tool, so it cannot be left out".to_string());
    }
    warnings
}

/// What a Pi extension flag gets: Pi extensions are not supported by ultron-rs, so the flag is ignored.
pub fn extension_flag_warning(flag: &str) -> String {
    format!("Pi extensions are not supported by ultron-rs; ignoring {flag}")
}

/// One warning per Pi extension flag on the command line (`-e`/`--extension <source>`, `--no-extensions`), in
/// order: each is parsed (as TypeScript parses it) and ignored.
pub fn extension_flag_warnings(parsed: &Args) -> Vec<String> {
    let mut warnings: Vec<String> = parsed
        .extensions
        .iter()
        .flatten()
        .map(|source| extension_flag_warning(&format!("--extension {source}")))
        .collect();
    if parsed.no_extensions {
        warnings.push(extension_flag_warning("--no-extensions"));
    }
    warnings
}

/// The startup note for the settings `packages` ultron-rs ignores (the worker's settings: the project's only when
/// it is trusted); none without packages.
pub fn package_settings_note(env: &Env) -> Option<String> {
    let settings = config::settings::SettingsManager::for_worker(&env.cwd, &env.agent_dir(), &env.home);
    session_core::local_resources::ignored_packages_note(&settings)
}

/// The warnings for the environment and settings that would give the model native tools in TypeScript
/// (prompt.ts `rlmToolMode`, extension-tools.ts `extensionToolMode` / `nativeExtensionToolAllowlist`): the Rust
/// build's model has exactly one tool, `rlm`, so `ULTRON_TOOLS=native`, `ULTRON_EXTENSION_TOOLS=native`,
/// `ULTRON_NATIVE_EXTENSION_TOOLS` and the `extensionTools` setting (`mode: "native"`, `native: [...]`) are reported
/// and ignored (Pi extensions are not supported by ultron-rs, so there are no extension tools either). The settings
/// are the worker's (its project trust).
pub fn tool_env_warnings(env: &Env) -> Vec<String> {
    const ONLY_RLM: &str = "the model's only tool is rlm";
    let mut warnings = Vec::new();
    let lower = |key: &str| (env.var)(key).map(|v| v.trim().to_lowercase());
    if lower("ULTRON_TOOLS").as_deref() == Some("native") {
        warnings.push(format!(
            "ULTRON_TOOLS=native is ignored: {ONLY_RLM}, whose REPL has read, edit, write and bash as Python functions"
        ));
    }
    let extension_tools = format!("{ONLY_RLM}, and Pi extensions are not supported by ultron-rs");
    if lower("ULTRON_EXTENSION_TOOLS").as_deref() == Some("native") {
        warnings.push(format!("ULTRON_EXTENSION_TOOLS=native is ignored: {extension_tools}"));
    }
    let listed: Vec<String> = (env.var)("ULTRON_NATIVE_EXTENSION_TOOLS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    if !listed.is_empty() {
        warnings.push(format!(
            "ULTRON_NATIVE_EXTENSION_TOOLS ({}) is ignored: {extension_tools}",
            listed.join(", ")
        ));
    }
    let settings = config::settings::SettingsManager::for_worker(&env.cwd, &env.agent_dir(), &env.home);
    let configured = settings.settings().get("extensionTools");
    if configured
        .and_then(|c| c.get("mode"))
        .and_then(serde_json::Value::as_str)
        == Some("native")
    {
        warnings.push(format!(
            "The extensionTools.mode setting \"native\" is ignored: {extension_tools}"
        ));
    }
    let native: Vec<&str> = configured
        .and_then(|c| c.get("native"))
        .and_then(serde_json::Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(serde_json::Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .collect()
        })
        .unwrap_or_default();
    if !native.is_empty() {
        warnings.push(format!(
            "The extensionTools.native setting ({}) is ignored: {extension_tools}",
            native.join(", ")
        ));
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn env_with(vars: &[(&str, &str)]) -> Env {
        let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Env {
            cwd: PathBuf::from("/nonexistent-cwd"),
            home: PathBuf::from("/nonexistent-home"),
            var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
            docs: PathBuf::new(),
        }
    }

    #[test]
    fn cli_startup_offline_flag_or_env_sets_offline_for_the_run_and_its_workers() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let plain = offline_env(&args(&["-p", "x"]), &env_with(&[]));
        assert_eq!((plain.var)("PI_OFFLINE"), None);
        assert!(worker_env(&plain).is_empty());
        let flagged = offline_env(&args(&["--offline", "-p"]), &env_with(&[]));
        assert_eq!((flagged.var)("PI_OFFLINE").as_deref(), Some("1"));
        assert_eq!((flagged.var)("PI_SKIP_VERSION_CHECK").as_deref(), Some("1"));
        assert_eq!(
            worker_env(&flagged),
            [
                ("PI_OFFLINE".to_string(), "1".to_string()),
                ("PI_SKIP_VERSION_CHECK".to_string(), "1".to_string())
            ]
        );
        // `isTruthyEnvFlag`: 1, true, yes; anything else is not offline.
        for (value, offline) in [("YES", true), ("true", true), ("1", true), ("0", false), ("no", false)] {
            let env = offline_env(&args(&[]), &env_with(&[("PI_OFFLINE", value)]));
            assert_eq!((env.var)("PI_SKIP_VERSION_CHECK").is_some(), offline, "{value}");
        }
    }

    #[test]
    fn cli_startup_piped_prompt_is_trimmed_and_blank_is_none() {
        assert_eq!(piped_prompt("  q\n\n").as_deref(), Some("q"));
        assert_eq!(piped_prompt("\u{feff} a b \t"), Some("a b".to_string()));
        assert_eq!(piped_prompt(" \n\t"), None);
    }

    #[test]
    fn cli_startup_tool_flags_map_onto_the_rlm_only_tool_list() {
        let parse = |a: &[&str]| crate::args::parse_args(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(tool_flag_warnings(&parse(&["x"])).is_empty());
        assert!(tool_flag_warnings(&parse(&["--no-builtin-tools"])).is_empty());
        assert!(tool_flag_warnings(&parse(&["--tools", "rlm"])).is_empty());
        assert!(tool_flag_warnings(&parse(&["--exclude-tools", "read"])).is_empty());
        let keeps = "The model keeps the rlm tool: it is the model's only tool, so it cannot be left out";
        assert_eq!(tool_flag_warnings(&parse(&["--no-tools"])), [keeps]);
        assert_eq!(tool_flag_warnings(&parse(&["--exclude-tools", "rlm"])), [keeps]);
        assert_eq!(tool_flag_warnings(&parse(&["--tools", "nonesuch"])), [keeps]);
        assert_eq!(
            tool_flag_warnings(&parse(&["--tools", "bash,edit"])),
            [
                "bash, edit are not a model tool here: the model's only tool is rlm, whose REPL has bash, edit as Python functions",
                keeps
            ]
        );
        // --no-builtin-tools keeps only rlm of what --tools named.
        assert!(tool_flag_warnings(&parse(&["--tools", "rlm,read", "--no-builtin-tools"])).is_empty());
    }

    #[test]
    fn cli_startup_http_proxy_setting_fills_only_unset_proxy_variables() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("settings.json"),
            r#"{"httpProxy": "  http://proxy.test:3128 "}"#,
        )
        .unwrap();
        let agent_dir = agent.display().to_string();
        let env = env_with(&[
            ("ULTRON_CODING_AGENT_DIR", &agent_dir),
            ("HTTPS_PROXY", "http://mine:1"),
        ]);
        let proxied = proxy_env(&env);
        assert_eq!((proxied.var)("HTTP_PROXY").as_deref(), Some("http://proxy.test:3128"));
        assert_eq!((proxied.var)("HTTPS_PROXY").as_deref(), Some("http://mine:1"));
        std::fs::write(agent.join("settings.json"), r#"{"httpProxy": "  "}"#).unwrap();
        assert_eq!((proxy_env(&env).var)("HTTP_PROXY"), None);
    }
}
