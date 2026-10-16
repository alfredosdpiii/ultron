//! What `/login` offers: replays fixtures/login-options.json (client-tui-auth.ts `loginProviderOptions` over
//! client-tui.ts `createLoginRuntime`, recorded by fixtures/generate-login-options.mjs) through
//! `cli::interactive::login_options`. The client's login runtime reads auth.json and models.json only, so a provider
//! an extension registers in the session is never offered; every offered sign-in has a flow in this build.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use cli::Env;
use serde_json::{Value, json};

fn env_for(root: &std::path::Path, vars: HashMap<String, String>) -> Env {
    let agent = root.join("agent").display().to_string();
    Env {
        cwd: root.to_path_buf(),
        home: root.join("home"),
        var: Arc::new(move |k| match k {
            "ULTRON_CODING_AGENT_DIR" => Some(agent.clone()),
            "PI_OFFLINE" => Some("1".into()),
            other => vars.get(other).cloned(),
        }),
        docs: PathBuf::new(),
    }
}

#[test]
fn cli_login_options_match_ts_login_provider_options() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/login-options.json")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("agent")).unwrap();
    std::fs::write(dir.path().join("agent/models.json"), fixture["models"].to_string()).unwrap();
    let vars: HashMap<String, String> = fixture["env"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    let env = env_for(dir.path(), vars);
    let got = cli::interactive::login_options(&env, &dir.path().join("agent/auth.json"));
    let custom: Vec<&str> = fixture["models"]["providers"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let want = fixture["options"].as_array().unwrap();
    let listed = |id: &str, name: &str, auth: &str| json!({"id": id, "name": name, "authType": auth});
    assert_eq!(
        got.iter()
            .map(|o| listed(&o.id, &o.name, &o.auth_type))
            .collect::<Vec<_>>(),
        want.iter()
            .map(|o| listed(
                o["id"].as_str().unwrap(),
                o["name"].as_str().unwrap(),
                o["authType"].as_str().unwrap()
            ))
            .collect::<Vec<_>>()
    );
    // models.json's own providers: the method, whether a key can be saved, and the status shown.
    for (option, want) in got.iter().zip(want) {
        if !custom.contains(&option.id.as_str()) || option.id == "anthropic" || option.id == "gw" {
            continue;
        }
        let (method, can_login) = option.api_key_method.clone().unwrap();
        assert_eq!(json!(method), want["method"], "{want}");
        assert_eq!(json!(can_login), want["canLogin"], "{want}");
        assert!(option.generic_login, "{want}");
        let status = option
            .status
            .as_ref()
            .map(|(kind, source)| json!({"type": kind, "source": source}));
        assert_eq!(status.unwrap_or(Value::Null), want["status"], "{want}");
    }
    // Every offered subscription sign-in runs in this client: the "not available in the Rust build" fallback of the
    // login dialog is never reached from this list.
    for option in got.iter().filter(|o| o.auth_type == "oauth") {
        assert!(cli::tui_login::supported(&env, &option.id), "{}", option.id);
    }
    for option in got.iter().filter(|o| o.auth_type == "api_key") {
        let custom_prompt = interactive::api_key_login::ApiKeyLogin::start(&option.id).is_some();
        assert!(
            custom_prompt
                || option
                    .api_key_method
                    .as_ref()
                    .is_some_and(|(_, save)| *save && option.generic_login)
                || option.api_key_method.as_ref().is_some_and(|(_, save)| !save),
            "{}",
            option.id
        );
    }
}

/// A stored credential for a models.json provider shows as one (`stored credential`).
#[test]
fn cli_login_options_show_a_stored_key_for_a_models_json_provider() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("agent")).unwrap();
    std::fs::write(
        dir.path().join("agent/models.json"),
        r#"{"providers":{"local":{"baseUrl":"http://127.0.0.1:9/v1","api":"openai-completions","models":[{"id":"m"}]}}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("agent/auth.json"),
        r#"{"local":{"type":"api_key","key":"k"}}"#,
    )
    .unwrap();
    let env = env_for(dir.path(), HashMap::new());
    let got = cli::interactive::login_options(&env, &dir.path().join("agent/auth.json"));
    let local = got.iter().find(|o| o.id == "local").unwrap();
    assert_eq!(local.status, Some(("api_key".into(), Some("stored credential".into()))));
}
