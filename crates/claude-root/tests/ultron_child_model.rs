//! `ultron claude --children ultron`: the model of a spawn that names none. Children of a Claude root keep the
//! frames' Claude model (`claude-code/<id>`, whose `rlm` lanes run on the Claude subscription) unless a setting
//! names another model; the settings' default model is used only under a frames' model that is not Claude's.

use claude_root::mcp_server::{Resolved, ultron_child_model};

fn resolved(ultron: Option<&str>, default: Option<&str>) -> Resolved {
    Resolved {
        frame_model: "claude-code/claude-opus-5-5".into(),
        frame_model_explicit: false,
        children: "ultron".into(),
        child_model: "claude-opus-5-5".into(),
        child_model_explicit: false,
        ultron_child_model: ultron.map(str::to_string),
        default_model: default.map(str::to_string),
    }
}

fn model(provider: &str, id: &str) -> ai::Model {
    let mut model = ai::claude_subscription::catalog::model("claude-opus-5-5").unwrap();
    model.provider = provider.into();
    model.id = id.into();
    model
}

#[test]
fn claude_root_ultron_children_of_a_claude_root_stay_on_claude() {
    let default = Some("anthropic/claude-opus-5-5");
    for frames in [
        model("claude-code", "claude-opus-5-5"),
        model("claude-subscription", "claude-opus-5-5"),
    ] {
        assert_eq!(
            ultron_child_model(&resolved(None, default), None, &frames),
            None,
            "{}",
            frames.provider
        );
    }
    // A non-Claude frames' model (the frame model did not resolve) takes the settings' default, as TypeScript.
    assert_eq!(
        ultron_child_model(&resolved(None, default), None, &model("groq", "llama")),
        default.map(str::to_string)
    );
}

#[test]
fn claude_root_ultron_child_model_settings_still_win() {
    let frames = model("claude-code", "claude-opus-5-5");
    let default = Some("anthropic/claude-opus-5-5");
    assert_eq!(
        ultron_child_model(
            &resolved(Some("groq/llama"), default),
            Some("openai/gpt".into()),
            &frames
        )
        .as_deref(),
        Some("groq/llama")
    );
    assert_eq!(
        ultron_child_model(&resolved(None, default), Some("openai/gpt".into()), &frames).as_deref(),
        Some("openai/gpt"),
        "rlm.childModel before the default"
    );
}
