//! `/scoped-models` when its catalog refresh runs out of time: the selector says "Model refresh timed out; showing
//! cached models." (`interactive-mode.ts` `showScopedModelsSelector`, the `result.aborted && timedOut` branch), not a
//! per-provider failure. The refresh reaches only loopback: a models.json Radius provider whose gateway accepts the
//! `/v1/config` request and never answers, so the whole refresh outlives its 15 s budget. The anthropic catalog was
//! checked moments ago (models-store.json), so pi.dev is not asked; every other host goes through a loopback proxy
//! that records any attempt, and the test asserts there was none.

mod tui_harness;

use serde_json::json;
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use tui_harness::{TempDir, Tui};

/// Accepts connections, records each request line, and never answers; the streams stay open until the test ends.
fn silent_server(lines: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held: Vec<TcpStream> = Vec::new();
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let mut line = String::new();
            let _ = BufReader::new(stream.try_clone().unwrap()).read_line(&mut line);
            lines.lock().unwrap().push(line.trim_end().to_string());
            held.push(stream);
        }
    });
    port
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn cli_scoped_models_refresh_timeout_says_so_like_ts() {
    let gateway_requests = Arc::new(Mutex::new(Vec::new()));
    let gateway = silent_server(gateway_requests.clone());
    let proxied = Arc::new(Mutex::new(Vec::new()));
    let proxy = format!("http://127.0.0.1:{}", silent_server(proxied.clone()));
    let tmp = TempDir::new();
    let agent = tmp.0.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("auth.json"),
        json!({"gw": {"type": "api_key", "key": "gw-key"}}).to_string(),
    )
    .unwrap();
    // Checked now, older than the bundled catalog: not asked again, and the bundled models stay.
    std::fs::write(
        agent.join("models-store.json"),
        json!({"anthropic": {"checkedAt": now_ms(), "lastModified": 0, "models": []}}).to_string(),
    )
    .unwrap();
    let models = json!({"providers": {
        "anthropic": {"baseUrl": "http://127.0.0.1:9"},
        "gw": {"oauth": "radius", "baseUrl": format!("http://127.0.0.1:{gateway}/v1"), "name": "My Radius"},
    }})
    .to_string();
    let tui = Tui::start_with_models(
        &tmp.0,
        &models,
        120,
        40,
        &["--model", "anthropic/claude-sonnet-4-5"],
        &[
            ("ULTRON_AUTO_NAME", "off"),
            ("ULTRON_TUI_PARITY", "1"),
            ("ULTRON_SPLASH", "off"),
            ("PI_SKIP_VERSION_CHECK", "1"),
            ("HTTP_PROXY", &proxy),
            ("HTTPS_PROXY", &proxy),
            ("ALL_PROXY", &proxy),
            ("NO_PROXY", "127.0.0.1,localhost"),
        ],
    );
    tui.wait("Server:");
    tui.send_text("/scoped-models");
    tui.send_keys(&["Enter"]);
    tui.wait("Refreshing model catalogs…");
    let screen = tui.wait("Model refresh timed out; showing cached models.");
    assert!(!screen.contains("Could not refresh"), "{screen}");
    assert_eq!(
        gateway_requests.lock().unwrap().as_slice(),
        ["GET /v1/config HTTP/1.1"],
        "the Radius gateway is asked once"
    );
    assert!(
        proxied.lock().unwrap().is_empty(),
        "a request left loopback: {:?}",
        proxied.lock().unwrap()
    );
}
