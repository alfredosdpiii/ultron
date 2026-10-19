//! `rlm.images` (images_api.py, kernel_apis::images) in the product's session: a root cell generates an image through
//! a 127.0.0.1 fake of OpenRouter (the `images.baseUrl` setting) with the session environment's `OPENROUTER_API_KEY`,
//! gets back the saved file's path under the session's output directory, and sees argument and provider errors as
//! Python exceptions. The session's runtime guide names the API.

mod p_session_support;

use base64::Engine;
use p_session_support::{Setup, actor, server, text, tool};
use serde_json::{Value, json};

fn image_answer(bytes: &[u8]) -> String {
    let url = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    );
    json!({"id": "gen-1", "choices": [{"message": {"role": "assistant", "content": null, "images": [{"type": "image_url", "image_url": {"url": url}}]}}]})
        .to_string()
}

#[test]
fn session_core_rlm_images_generate_saves_the_image_and_returns_its_path() {
    // The fake answers an image for "a red square" and a moderation-style answer without an image otherwise.
    let (port, images) = server(Box::new(|body: &Value, _| {
        if body.to_string().contains("a red square") {
            image_answer(b"PNG-RED")
        } else {
            json!({"id": "gen-2", "choices": [{"message": {"role": "assistant", "content": "I cannot draw that"}}]})
                .to_string()
        }
    }));
    let code = [
        "import json",
        "r = await rlm.images.generate('a red square', size='1:1')",
        "print('result', json.dumps(r))",
        "print('bytes', json.dumps(open(r['paths'][0], 'rb').read().decode()))",
        "for call in [lambda: rlm.images.generate(''), lambda: rlm.images.generate('x', n=9), lambda: rlm.images.generate('nothing')]:",
        "    try:",
        "        await call()",
        "    except Exception as e:",
        "        print('error', json.dumps([type(e).__name__, str(e)]))",
        "print('doc', json.dumps('saves the image to a' in (type(rlm).images.__doc__ or '')))",
        "print('models', json.dumps(len(await rlm.images.models()) > 10))",
    ]
    .join("\n");
    let script = Box::new(move |body: &Value, _| {
        if body["messages"].to_string().contains("tool_result") {
            text("done")
        } else {
            tool("toolu_images", &code)
        }
    });
    let a = actor(
        script,
        Setup {
            settings: Some(json!({"images": {"baseUrl": format!("http://127.0.0.1:{port}/api/v1")}})),
            vars: vec![("OPENROUTER_API_KEY", "or-test-key".into())],
            ..Setup::default()
        },
    );
    let ended = a.prompt("draw something");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let requests = a.requests();
    let output = requests
        .iter()
        .flat_map(p_session_support::tool_results)
        .map(|r| match r {
            Value::String(text) => text,
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let lines = |prefix: &str| -> Vec<Value> {
        output
            .lines()
            .filter_map(|l| l.strip_prefix(&format!("{prefix} ")))
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let result = &lines("result")[0];
    assert_eq!(result["model"], "openrouter/google/gemini-2.5-flash-image", "{output}");
    let path = result["paths"][0].as_str().unwrap();
    let outputs = a.root.join("agent/rlm-output");
    assert!(
        path.starts_with(&outputs.display().to_string()) && path.ends_with("/images/image-1.png"),
        "{path}"
    );
    assert_eq!(std::fs::read(path).unwrap(), b"PNG-RED");
    assert_eq!(lines("bytes"), vec![json!("PNG-RED")], "the cell reads the saved file");
    assert_eq!(
        lines("error"),
        vec![
            json!(["ValueError", "rlm.images.generate prompt must be a non-empty string"]),
            json!(["ValueError", "rlm.images.generate n must be an integer from 1 to 4"]),
            json!([
                "RuntimeError",
                "Image generation failed: google/gemini-2.5-flash-image returned no image: I cannot draw that"
            ]),
        ],
        "{output}"
    );
    assert_eq!(lines("doc"), vec![json!(true)], "{output}");
    assert_eq!(lines("models"), vec![json!(true)], "{output}");
    // The provider saw the session's key and the size as image_config.
    let sent = images.lock().unwrap().clone();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[0]["image_config"], json!({"aspect_ratio": "1:1"}));
    assert_eq!(sent[0]["model"], "google/gemini-2.5-flash-image");
    // The runtime guide in the system prompt tells the model about the API.
    assert!(
        requests[0]["system"].to_string().contains("rlm.images.generate(prompt"),
        "the guide names rlm.images"
    );
}
