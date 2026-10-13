//! The worker's run watchdog (crates/daemon `watchdog`), the restart backoff of crashed workers, the signal exit
//! codes and the session-in-use message (`SessionInUseError` of session-worker-manager.ts).

use daemon::watchdog::{Change, DEFAULT_STUCK_AFTER, Progress, stuck_after};
use daemon::workers::{MAX_RESTARTS, RESTART_MAX_DELAY, restart_delay};
use serde_json::json;
use std::time::Duration;

#[test]
fn daemon_watchdog_tracks_runs_and_flags_a_run_without_progress() {
    let progress = Progress::default();
    let after = Some(Duration::from_millis(1000));
    assert!(!progress.busy());
    assert!(!progress.check(10_000, after), "no run, nothing stuck");
    assert_eq!(
        progress.event(&json!({"type": "run_start", "runId": "r1"}), 1_000),
        Change::Started("r1".into())
    );
    assert!(progress.busy());
    assert_eq!(progress.run().as_deref(), Some("r1"));
    // Lane events are progress; names and usage totals are not.
    assert_eq!(
        progress.event(&json!({"type": "message_update", "runId": "r1"}), 1_500),
        Change::None
    );
    assert_eq!(progress.event(&json!({"type": "usage"}), 2_400), Change::None);
    assert!(!progress.check(2_499, after), "999 ms since the last progress");
    assert!(progress.check(2_500, after), "1000 ms since the last progress");
    assert!(!progress.check(9_000, after), "reported once per episode");
    let health = progress.health(3_000, after);
    assert_eq!(
        health,
        json!({"running": true, "runId": "r1", "startedAt": 1000, "lastProgressAt": 1500, "idleMs": 1500,
            "stuck": true, "stuckAfterMs": 1000})
    );
    assert_eq!(
        progress.event(&json!({"type": "tool_end", "runId": "r1"}), 3_100),
        Change::Unstuck
    );
    assert_eq!(progress.health(3_100, after)["stuck"], false);
    // Another run's end does not end this one.
    assert_eq!(
        progress.event(&json!({"type": "run_end", "runId": "r0"}), 3_200),
        Change::None
    );
    assert_eq!(
        progress.event(&json!({"type": "run_end", "runId": "r1"}), 3_300),
        Change::Ended("r1".into())
    );
    assert!(!progress.busy());
    assert_eq!(
        progress.health(4_000, None),
        json!({"running": false, "runId": null, "startedAt": null, "lastProgressAt": null, "idleMs": 0,
            "stuck": false, "stuckAfterMs": null})
    );
    assert!(!progress.check(100_000, None), "detection off");
}

#[test]
fn daemon_watchdog_stuck_threshold_comes_from_the_environment() {
    let none = |_: &str| None;
    assert_eq!(stuck_after(&none), Some(DEFAULT_STUCK_AFTER));
    assert_eq!(DEFAULT_STUCK_AFTER, Duration::from_secs(600));
    assert_eq!(stuck_after(&|_| Some("1500".into())), Some(Duration::from_millis(1500)));
    assert_eq!(stuck_after(&|_| Some("0".into())), None);
    assert_eq!(stuck_after(&|_| Some("soon".into())), Some(DEFAULT_STUCK_AFTER));
}

#[test]
fn daemon_restart_backoff_doubles_to_its_cap() {
    let delays: Vec<u128> = (1..=7).map(|a| restart_delay(a).as_millis()).collect();
    assert_eq!(delays, [250, 500, 1000, 2000, 4000, 8000, 8000]);
    assert_eq!(restart_delay(40), RESTART_MAX_DELAY);
    assert_eq!(MAX_RESTARTS, 5);
}

#[test]
fn daemon_signal_exit_codes_and_session_in_use_message() {
    use daemon::signals::{Sig, exit_code};
    assert_eq!([Sig::SIGINT, Sig::SIGTERM, Sig::SIGHUP].map(exit_code), [130, 143, 129]);
    assert_eq!(
        daemon::worker::session_in_use("s-1", Some(42)),
        "Session s-1 is open in another Ultron process (worker pid 42); close it there, or wait for its work to finish, then resume it"
    );
}
