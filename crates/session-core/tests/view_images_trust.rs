//! `images.autoResize` is read with the worker's project trust (session-worker.ts reads every setting from its
//! `SettingsManager.create(cwd, agentDir, {projectTrusted: workerProjectTrusted(...)})`): a trusted project's
//! `.pi/settings.json` turning it off keeps an oversized image at its size; a distrusted project's does not, and
//! the image is downscaled as by default. The stack is the session's composition (`Runtime::stack`).

mod p_session_support;

use agent::AgentTool;
use ai::{AbortSignal, UserContent};
use serde_json::{Value, json};
use session_core::RunEnv;
use session_core::compose::{Runtime, Services};
use std::path::Path;
use std::sync::Arc;

fn view_big_image(global: Value) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    let agent_dir = home.join(".ultron/agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let mut settings = json!({"sandbox": {"mode": "off"}});
    settings
        .as_object_mut()
        .unwrap()
        .extend(global.as_object().unwrap().clone());
    std::fs::write(agent_dir.join("settings.json"), settings.to_string()).unwrap();
    let cwd = root.join("project");
    std::fs::create_dir_all(cwd.join(".pi")).unwrap();
    std::fs::write(
        cwd.join(".pi/settings.json"),
        json!({"images": {"autoResize": false}}).to_string(),
    )
    .unwrap();
    std::fs::write(cwd.join("big.png"), p_session_support::png(3000, 2400)).unwrap();
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent_dir.display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
    ];
    let env = RunEnv {
        cwd: cwd.clone(),
        home,
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let runtime = Runtime::new(&env, "print", root.join("sessions"), Services::default()).unwrap();
    let mut model = ai::models::builtin_model("anthropic", "claude-sonnet-4-5")
        .unwrap()
        .clone();
    model.provider = "test".into();
    model.id = "vision".into();
    model.input = vec!["text".into(), "image".into()];
    let stack = runtime.stack(&model, None, "view-image-trust").unwrap();
    let tool = stack.rlm("main").unwrap();
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let update = |_| {};
    let content = tokio
        .block_on(tool.execute(
            "call",
            json!({"code": "print(await view_image('big.png'))"}),
            &AbortSignal::new(),
            &update,
        ))
        .unwrap()
        .content;
    stack.close();
    match &content[0] {
        UserContent::Text(text) => text.text.clone(),
        other => panic!("not text: {other:?}"),
    }
}

#[test]
fn session_core_view_image_auto_resize_follows_the_worker_project_trust() {
    // No stored decision and the default "ask": the worker trusts the project, whose setting keeps the size.
    let trusted = view_big_image(json!({}));
    assert!(!trusted.contains("downscaled"), "{trusted}");
    assert!(trusted.contains("3000x2400"), "{trusted}");
    // defaultProjectTrust "never": the project's settings are not read; images are resized by default.
    let distrusted = view_big_image(json!({"defaultProjectTrust": "never"}));
    assert!(distrusted.contains("(downscaled from 3000x2400)"), "{distrusted}");
}
