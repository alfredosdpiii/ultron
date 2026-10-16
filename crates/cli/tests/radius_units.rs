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
