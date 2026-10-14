//! The TUI's subscription login runner (`tui_login`, client-tui-auth.ts `runProviderLogin`): Esc while the device
//! code is shown stops the flow (`Login cancelled`): it ends, reports nothing and saves nothing, even though the
//! gateway would still answer later polls. Waits on the runner's own events (120 s hang guard).

use interactive::lane::{LaneEvent, LoginEvent, LoginPrompt, LoginStep};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// A Radius gateway whose token polls stay pending.
fn gateway() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let (status, reply) = if path == "/v1/oauth/device" {
                (
                    200,
                    json!({"device_code": "d", "user_code": "CODE-1", "verification_uri": "http://127.0.0.1/device",
                        "expires_in": 600, "interval": 1}),
                )
            } else {
                (400, json!({"error": "authorization_pending"}))
            };
            let text = reply.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    port
}

#[test]
fn cli_tui_login_esc_stops_the_flow_and_saves_nothing() {
    let port = gateway();
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("models.json"),
        json!({"providers": {"gw": {"oauth": "radius", "baseUrl": format!("http://127.0.0.1:{port}/v1")}}}).to_string(),
    )
    .unwrap();
    let agent_dir = agent.display().to_string();
    let env = cli::Env {
        cwd: tmp.path().to_path_buf(),
        home: tmp.path().to_path_buf(),
        var: Arc::new(move |k| (k == "ULTRON_CODING_AGENT_DIR").then(|| agent_dir.clone())),
        docs: tmp.path().join("docs"),
    };
    assert!(cli::tui_login::supported(&env, "gw"));
    let (tx, rx) = mpsc::channel::<LaneEvent>();
    let tx = std::sync::Mutex::new(tx);
    let send: Arc<dyn Fn(LaneEvent) + Send + Sync> = Arc::new(move |event| {
        let _ = tx.lock().unwrap().send(event);
    });
    let slot = Arc::new(cli::tui_login::LoginSlot::default());
    let (saved_tx, saved_rx) = mpsc::channel::<()>();
    let saved_tx = std::sync::Mutex::new(saved_tx);
    cli::tui_login::start(
        env,
        "gw".into(),
        "gw".into(),
        send,
        slot.clone(),
        Box::new(move || {
            let _ = saved_tx.lock().unwrap().send(());
        }),
    );
    let guard = Duration::from_secs(120);
    let mut events = Vec::new();
    loop {
        let event = rx.recv_timeout(guard).expect("the login's next step within 120 s");
        match &event {
            LaneEvent::Login(LoginStep::Prompt(LoginPrompt::Select { options, .. })) => {
                assert_eq!(options[1].0, "device-code");
                slot.reply(Some("device-code".into()));
            }
            // Esc in the dialog while the code is shown.
            LaneEvent::Login(LoginStep::Event(LoginEvent::DeviceCode { user_code, .. })) => {
                assert_eq!(user_code, "CODE-1");
                slot.reply(None);
            }
            LaneEvent::Login(LoginStep::Finished) => break,
            _ => {}
        }
        events.push(event);
    }
    // A cancel reports nothing and saves nothing.
    assert!(!events.iter().any(|e| matches!(e, LaneEvent::Status(_))), "{events:?}");
    assert!(saved_rx.try_recv().is_err());
    assert!(!agent.join("auth.json").exists());
}
