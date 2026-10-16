//! `/autoreview [on|off]` behind the client (`autoreview_toggle::autoreview`, client-tui-pi-commands.ts
//! `setAutoreview`): the settings it saves, the user service it writes, starts, stops and removes, and its status
//! texts. A private home and agent dir; commands go to a recording runner, so no real gh or systemctl runs.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cli::autoreview_toggle::{Ran, autoreview};
use interactive::lane::{AutoreviewRequest, LaneEvent};
use serde_json::{Value, json};

fn env_for(dir: &Path) -> cli::Env {
    let (agent, home) = (dir.join("agent"), dir.join("home"));
    let config = home.join(".config");
    cli::Env {
        cwd: dir.to_path_buf(),
        home: home.clone(),
        var: Arc::new(move |key| match key {
            "ULTRON_CODING_AGENT_DIR" => Some(agent.display().to_string()),
            "XDG_CONFIG_HOME" => Some(config.display().to_string()),
            "ULTRON_SELF_COMMAND" => Some(r#"["/opt/ultron/bin/ultron"]"#.into()),
            "PATH" => Some("/usr/bin:/bin".into()),
            "HOME" => Some(home.display().to_string()),
            _ => None,
        }),
        docs: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs"),
    }
}

fn status(event: LaneEvent) -> String {
    match event {
        LaneEvent::Status(text) => text,
        _ => panic!("expected a status"),
    }
}

fn saved(dir: &Path) -> Value {
    let text = std::fs::read_to_string(dir.join("agent/settings.json")).unwrap_or_else(|_| "{}".into());
    serde_json::from_str::<Value>(&text).unwrap()["autoreview"].clone()
}

#[test]
fn cli_autoreview_toggle_on_and_off_write_the_settings_and_the_user_service() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_for(dir.path());
    let unit = dir.path().join("home/.config/systemd/user/ultron-autoreview.service");
    let calls = RefCell::new(Vec::<String>::new());
    let ok = |command: &str, args: &[&str]| {
        calls.borrow_mut().push(format!("{command} {}", args.join(" ")));
        Ok(Ran {
            code: 0,
            stdout: r#"{"hosts":{"github.com":[{"login":"octo","active":true,"state":"success"}],"ghe.example.com":[{"login":"work","state":"success"}]}}"#.into(),
            stderr: String::new(),
        })
    };

    let text = status(autoreview(AutoreviewRequest::Status, &env, "linux", &ok));
    assert_eq!(
        text,
        "Autoreview is on: reviewing as every logged-in gh account; service not installed. /autoreview on|off"
    );

    match autoreview(AutoreviewRequest::Accounts, &env, "linux", &ok) {
        LaneEvent::AutoreviewAccounts(accounts) => assert_eq!(
            accounts,
            [
                ("octo".to_string(), "github.com".to_string()),
                ("work".to_string(), "ghe.example.com".to_string())
            ]
        ),
        _ => panic!("expected the accounts"),
    }
    assert_eq!(calls.take(), ["gh auth status --json hosts"]);

    let on = AutoreviewRequest::On {
        accounts: Some(vec!["ghe.example.com/work".into()]),
        reviewing_as: "ghe.example.com/work".into(),
    };
    let text = status(autoreview(on, &env, "linux", &ok));
    assert_eq!(
        text,
        format!(
            "Autoreview on, reviewing as ghe.example.com/work: the service is started ({}); it reviews pull requests that request or mention the account",
            unit.display()
        )
    );
    assert_eq!(
        saved(dir.path()),
        json!({"enabled": true, "accounts": ["ghe.example.com/work"]})
    );
    let content = std::fs::read_to_string(&unit).unwrap();
    assert!(
        content.contains("ExecStart=/opt/ultron/bin/ultron autoreview run"),
        "{content}"
    );
    assert_eq!(
        calls.take(),
        [
            "systemctl --user daemon-reload",
            "systemctl --user enable --now ultron-autoreview.service"
        ]
    );
    let text = status(autoreview(AutoreviewRequest::Status, &env, "linux", &ok));
    assert_eq!(
        text,
        format!(
            "Autoreview is on: reviewing as ghe.example.com/work; service installed ({}). /autoreview on|off",
            unit.display()
        )
    );

    let text = status(autoreview(AutoreviewRequest::Off, &env, "linux", &ok));
    assert_eq!(
        text,
        format!(
            "Autoreview off: ultron autoreview run and its service review nothing; service removed ({})",
            unit.display()
        )
    );
    assert!(!unit.exists());
    assert_eq!(
        calls.take(),
        ["systemctl --user disable --now ultron-autoreview.service"]
    );
    assert_eq!(
        saved(dir.path()),
        json!({"enabled": false, "accounts": ["ghe.example.com/work"]})
    );

    // Every account: `accounts` is removed.
    let all = AutoreviewRequest::On {
        accounts: None,
        reviewing_as: "every logged-in account".into(),
    };
    autoreview(all, &env, "linux", &ok);
    assert_eq!(saved(dir.path()), json!({"enabled": true}));
}

#[test]
fn cli_autoreview_toggle_reports_a_missing_gh_a_failed_start_and_no_service() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_for(dir.path());
    let missing = |_: &str, _: &[&str]| Err("No such file or directory".to_string());
    assert_eq!(
        status(autoreview(AutoreviewRequest::Accounts, &env, "linux", &missing)),
        "Autoreview needs the GitHub CLI (gh): install it and run gh auth login, then /autoreview on"
    );
    let failing = |command: &str, _: &[&str]| {
        Ok(Ran {
            code: if command == "systemctl" { 1 } else { 0 },
            stdout: String::new(),
            stderr: "Failed to connect to bus\n".into(),
        })
    };
    let on = || AutoreviewRequest::On {
        accounts: None,
        reviewing_as: "github.com/octo".into(),
    };
    let unit = dir.path().join("home/.config/systemd/user/ultron-autoreview.service");
    assert_eq!(
        status(autoreview(on(), &env, "linux", &failing)),
        format!(
            "Autoreview on, reviewing as github.com/octo; the service file is written ({}) but could not be started: `systemctl --user daemon-reload` failed: Failed to connect to bus; `systemctl --user enable --now ultron-autoreview.service` failed: Failed to connect to bus. Start it by hand: systemctl --user daemon-reload && systemctl --user enable --now ultron-autoreview.service",
            unit.display()
        )
    );
    assert_eq!(
        status(autoreview(on(), &env, "win32", &failing)),
        "Autoreview on, reviewing as github.com/octo. No user service on win32: run ultron autoreview run under your own supervisor"
    );
}
