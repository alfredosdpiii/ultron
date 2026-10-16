//! The remote client's pure parts, with literal expectations from client-tui.ts: which session a remote client
//! opens (`prepareClientSession`), and what a Radius recovery shows (`#handleConnectionState`,
//! `#handleAttachmentState`, `#handleLocalConnectionState`), including when the session is shown again.

use cli::interactive::recovery_events;
use cli::link::{
    RemoteSelection, Selected, connection_event, radius_attachment_event, radius_connection_event,
    select_remote_session,
};
use client::radius::ConnectionState;
use interactive::lane::LaneEvent;
use serde_json::json;

fn sessions() -> Vec<serde_json::Value> {
    vec![
        json!({"serverId": "s1", "sessionId": "b", "modifiedAt": 10}),
        json!({"serverId": "s1", "sessionId": "a", "modifiedAt": 10}),
        json!({"serverId": "s1", "sessionId": "c", "modifiedAt": 5}),
    ]
}

#[test]
fn cli_remote_session_selection_matches_prepare_client_session() {
    let named = |id: &str| RemoteSelection {
        session_id: Some(id.into()),
        ..Default::default()
    };
    // An existing session is opened (and renamed when a name is given).
    assert_eq!(
        select_remote_session(&named("c"), &sessions(), true),
        Ok(Selected::Existing {
            session_id: "c".into(),
            rename: None
        })
    );
    let renamed = RemoteSelection {
        name: Some("Renamed".into()),
        ..named("c")
    };
    assert_eq!(
        select_remote_session(&renamed, &sessions(), false),
        Ok(Selected::Existing {
            session_id: "c".into(),
            rename: Some("Renamed".into())
        })
    );
    // A missing one: a Radius server must already have it; a local one creates it under that id.
    assert_eq!(
        select_remote_session(&named("zz"), &sessions(), true),
        Err("Remote server does not contain Session zz".into())
    );
    assert_eq!(
        select_remote_session(&renamed_missing(), &sessions(), false),
        Ok(Selected::Create(json!({"id": "zz", "name": "N"})))
    );
    // Continue: the newest, ties by server id then session id.
    let continued = RemoteSelection {
        continue_: true,
        ..Default::default()
    };
    assert_eq!(
        select_remote_session(&continued, &sessions(), true),
        Ok(Selected::Existing {
            session_id: "a".into(),
            rename: None
        })
    );
    // Nothing to continue, or nothing selected: a new session (a fork when asked, with the name).
    assert_eq!(
        select_remote_session(&continued, &[], true),
        Ok(Selected::Create(json!({})))
    );
    let fork = RemoteSelection {
        fork: Some("c".into()),
        name: Some("N".into()),
        ..Default::default()
    };
    assert_eq!(
        select_remote_session(&fork, &sessions(), true),
        Ok(Selected::Create(json!({"forkFromSessionId": "c", "name": "N"})))
    );
    assert_eq!(
        serde_json::to_string(&match select_remote_session(&fork, &[], true).unwrap() {
            Selected::Create(options) => options,
            other => panic!("{other:?}"),
        })
        .unwrap(),
        r#"{"forkFromSessionId":"c","name":"N"}"#
    );
}

fn renamed_missing() -> RemoteSelection {
    RemoteSelection {
        session_id: Some("zz".into()),
        name: Some("N".into()),
        ..Default::default()
    }
}

fn recovery(busy: bool, status: &str) -> LaneEvent {
    LaneEvent::Recovery {
        busy,
        status: status.into(),
    }
}

#[test]
fn cli_radius_recovery_events_show_client_tui_statuses() {
    let mut lane_open = true;
    let mut step = |event: serde_json::Value| recovery_events(&event, &mut lane_open);
    // The connection drops: busy, retrying; the lane closes.
    assert_eq!(
        step(radius_connection_event(ConnectionState::Disconnected)),
        (vec![recovery(true, "Radius disconnected; retrying…")], false)
    );
    assert_eq!(
        step(radius_connection_event(ConnectionState::Connecting)),
        (vec![recovery(true, "Reconnecting to Radius…")], false)
    );
    // Connected with the lane closed: reattaching.
    assert_eq!(
        step(radius_connection_event(ConnectionState::Connected)),
        (vec![recovery(true, "Reattaching Session…")], false)
    );
    assert_eq!(
        step(radius_attachment_event("attaching", "s")),
        (vec![recovery(true, "Reattaching Session…")], false)
    );
    // Attached again: not busy, the status cleared, and the session shown again.
    assert_eq!(
        step(radius_attachment_event("attached", "s")),
        (vec![recovery(false, "")], true)
    );
    // Connected with the lane open changes nothing; attached again does not reopen an open lane.
    assert_eq!(
        step(radius_connection_event(ConnectionState::Connected)),
        (vec![], false)
    );
    assert_eq!(
        step(radius_attachment_event("attached", "s")),
        (vec![recovery(false, "")], false)
    );
    // A remote Unix server that went away is reported (no reconnect).
    assert_eq!(
        step(connection_event(
            "lost",
            Some("The Ultron server stopped (gone); resume")
        )),
        (
            vec![recovery(false, "Error: The Ultron server stopped (gone); resume")],
            false
        )
    );
    // Other events are not recovery events.
    assert_eq!(step(json!({"type": "run_start"})), (vec![], false));
    assert_eq!(step(connection_event("reconnecting", Some("x"))), (vec![], false));
}

#[test]
fn cli_radius_server_reports_only_the_latest_startup_status() {
    use cli::experimental::pending_relay_status;
    use server::radius_relay::{RelayHostStatus, relay_status_line};
    let lines = |statuses: Vec<RelayHostStatus>| {
        let mut previous = String::new();
        pending_relay_status(statuses)
            .and_then(|status| relay_status_line(&mut previous, &status))
            .into_iter()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        lines(vec![
            RelayHostStatus::NotAuthenticated,
            RelayHostStatus::Connecting,
            RelayHostStatus::Retrying("refused".into()),
        ]),
        vec!["Radius: reconnecting: refused".to_string()]
    );
    // The latest was `connecting`, which is never printed.
    assert_eq!(
        lines(vec![RelayHostStatus::Connected, RelayHostStatus::Connecting]),
        Vec::<String>::new()
    );
    assert_eq!(
        lines(vec![RelayHostStatus::Connecting, RelayHostStatus::NotAuthenticated]),
        vec!["Radius: not connected; local only".to_string()]
    );
    assert_eq!(lines(Vec::new()), Vec::<String>::new());
}

/// Two servers' session directories: "shared" is on both, "only-a" on A, "only-b" (the newest) on B.
fn two_servers() -> Vec<(String, Vec<serde_json::Value>)> {
    vec![
        (
            "a".into(),
            vec![
                json!({"sessionId": "shared", "modifiedAt": 5}),
                json!({"sessionId": "only-a", "modifiedAt": 7}),
            ],
        ),
        (
            "b".into(),
            vec![
                json!({"sessionId": "only-b", "modifiedAt": 9}),
                json!({"sessionId": "shared", "modifiedAt": 5}),
            ],
        ),
    ]
}

#[test]
fn cli_client_server_choice_matches_run_client_and_prepare_client_session() {
    use cli::experimental::{ClientCommand, ServerChoice, choose_server};
    let servers = two_servers();
    let command = |session: Option<&str>, prompt: Option<&str>, continue_: bool| ClientCommand {
        session_id: session.map(str::to_string),
        prompt: prompt.map(str::to_string),
        continue_,
        ..Default::default()
    };
    let on = |server: usize, id: &str| {
        Ok(ServerChoice::Session {
            server,
            session_id: id.into(),
        })
    };
    // runClientOperation: the listing spans the servers, by server ID then session ID.
    assert_eq!(
        choose_server(&command(None, None, false), &servers, false),
        Ok(ServerChoice::List(vec![
            ("a".into(), "only-a".into()),
            ("a".into(), "shared".into()),
            ("b".into(), "only-b".into()),
            ("b".into(), "shared".into()),
        ]))
    );
    assert_eq!(
        choose_server(&command(Some("only-b"), None, false), &servers, false),
        on(1, "only-b")
    );
    assert_eq!(
        choose_server(&command(Some("only-a"), Some("p"), false), &servers, false),
        on(0, "only-a")
    );
    assert_eq!(
        choose_server(&command(Some("shared"), Some("p"), false), &servers, false),
        Err("Session shared is available from more than one server".into())
    );
    assert_eq!(
        choose_server(&command(Some("missing"), Some("p"), false), &servers, false),
        Err("No discovered server contains session missing".into())
    );
    assert_eq!(
        choose_server(&command(None, Some("p"), false), &servers, false),
        Err("Client prompt requires exactly one discovered server to create a Session".into())
    );
    // --continue takes the newest session of any server; ties go by server ID, then session ID.
    assert_eq!(
        choose_server(&command(None, Some("p"), true), &servers, false),
        on(1, "only-b")
    );
    let tied = vec![
        ("b".into(), vec![json!({"sessionId": "x", "modifiedAt": 3})]),
        (
            "a".into(),
            vec![
                json!({"sessionId": "z", "modifiedAt": 3}),
                json!({"sessionId": "y", "modifiedAt": 3}),
            ],
        ),
    ];
    assert_eq!(choose_server(&command(None, Some("p"), true), &tied, false), on(1, "y"));
    // A newest session on both servers is ambiguous for runClient.
    let both = vec![
        ("a".into(), vec![json!({"sessionId": "s", "modifiedAt": 3})]),
        ("b".into(), vec![json!({"sessionId": "s", "modifiedAt": 3})]),
    ];
    assert_eq!(
        choose_server(&command(None, Some("p"), true), &both, false),
        Err("Session s is available from more than one server".into())
    );
    let empty = vec![("a".into(), Vec::new()), ("b".into(), Vec::new())];
    assert_eq!(
        choose_server(&command(None, Some("p"), true), &empty, false),
        Err("Client prompt requires exactly one discovered server to create a Session".into())
    );
    // prepareClientSession (the TUI): the newest session is opened where it is, ambiguity is still refused, and a
    // new Session needs exactly one server.
    assert_eq!(choose_server(&command(None, None, true), &both, true), on(0, "s"));
    assert_eq!(
        choose_server(&command(Some("only-a"), None, false), &servers, true),
        on(0, "only-a")
    );
    assert_eq!(
        choose_server(&command(Some("shared"), None, false), &servers, true),
        Err("Session shared is available from more than one server".into())
    );
    for (session, continue_, list) in [
        (Some("missing"), false, &servers),
        (None, false, &servers),
        (None, true, &empty),
    ] {
        assert_eq!(
            choose_server(&command(session, None, continue_), list, true),
            Err("Starting a Session requires exactly one server".into()),
            "{session:?} {continue_}"
        );
    }
}
