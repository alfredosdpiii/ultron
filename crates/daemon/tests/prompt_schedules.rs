//! The server's schedule timer (crate::prompt_schedules; a Rust-only feature, literal expectations) on a manual
//! clock with a recording delivery: the journals are read on start (only a session's own values, last write per id),
//! a schedule whose slots were missed fires once, the timer sleeps until the earliest due time (one look at the
//! clock per wake-up), a failed delivery waits [`RETRY_MS`], a gone session is dropped, workers' reports replace
//! what it holds, and a journal cancel without a server takes the session's locks.

use daemon::catalog::Meta;
use daemon::prompt_schedules::{
    Deliver, Delivery, RETRY_MS, Scheduler, cancel_in_journal, fire_call, scan_journal, scan_sessions,
};
use kernel_apis::ManualTickClock;
use kernel_apis::prompt_schedule::{Kind, NAMESPACE, Schedule, State};
use serde_json::{Value, json};
use session_journal::{SessionRepo, SystemClock, Write};
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const T0: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;
const HANG: Duration = Duration::from_secs(120);

fn schedule(id: &str, session: &str, next_due: Option<i64>, state: State) -> Schedule {
    Schedule {
        id: id.into(),
        name: format!("name-{id}"),
        session_id: session.into(),
        prompt: "check".into(),
        kind: Kind::Every,
        interval_ms: Some(HOUR),
        until: None,
        max_runs: None,
        created_at: T0 - 10 * HOUR,
        next_due,
        runs: 0,
        last_fired: None,
        state,
    }
}

/// A session journal holding `values` (one commit each), plus an unrelated value.
fn session_with(root: &Path, values: &[(String, Option<Value>)]) -> Meta {
    let repo = SessionRepo::new(root, SystemClock::shared());
    let mut session = repo.create("/work/project", None, None).unwrap();
    session
        .commit(&[Write::set("ultron.other", "x", json!({"sessionId": "x"}))])
        .unwrap();
    for (key, value) in values {
        let write = match value {
            Some(value) => Write::set(NAMESPACE, key, value.clone()),
            None => Write::ValueDelete {
                namespace: NAMESPACE.into(),
                key: key.clone(),
            },
        };
        session.commit(&[write]).unwrap();
    }
    daemon::catalog::read_meta(&session.metadata().path).unwrap()
}

/// Records each delivery with the clock's time; answers what the test queued (default: the schedule's next slot an
/// hour after now).
struct Recorder {
    clock: Arc<ManualTickClock>,
    seen: Sender<Seen>,
    answers: Mutex<Vec<Delivery>>,
}

impl Deliver for Recorder {
    fn fire(&self, meta: &Meta, id: &str, due: i64) -> Delivery {
        let now = kernel_apis::TickClock::now(&*self.clock);
        let _ = self.seen.send((meta.id.clone(), id.to_string(), due, now));
        let mut answers = self.answers.lock().unwrap();
        if answers.is_empty() {
            Delivery::Done(json!([{"id": id, "name": "n", "nextDue": now + HOUR}]))
        } else {
            answers.remove(0)
        }
    }
}

/// `(session id, schedule id, due, clock at delivery)`.
type Seen = (String, String, i64, i64);

fn scheduler(clock: &Arc<ManualTickClock>) -> (Arc<Scheduler>, Receiver<Seen>, Arc<Recorder>) {
    let (seen, deliveries) = channel();
    let recorder = Arc::new(Recorder {
        clock: clock.clone(),
        seen,
        answers: Mutex::new(Vec::new()),
    });
    (
        Scheduler::new(clock.clone(), recorder.clone(), None),
        deliveries,
        recorder,
    )
}

fn wait(what: &str, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + HANG;
    while !done() {
        assert!(Instant::now() < end, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The delivery that just arrived has been settled.
fn settled(s: &Scheduler) {
    wait("the delivery's end", || s.in_flight() == 0);
}

/// Poke the timer and wait until it has looked at the clock again.
fn poke(s: &Scheduler) {
    let before = s.passes();
    s.poke();
    wait("a pass of the timer", || s.passes() > before);
}

#[test]
fn daemon_prompt_schedules_scan_reads_only_the_sessions_own_last_values() {
    let dir = tempfile::tempdir().unwrap();
    let probe = session_with(dir.path(), &[]);
    let own = |id: &str, due: i64, state: State| schedule(id, &probe.id, Some(due), state).to_json();
    // Another session's journal: its own id differs, so a value written with `probe`'s id is foreign to it.
    let meta = session_with(
        dir.path(),
        &[
            (
                "sch-a".into(),
                Some(schedule("sch-a", "other", Some(T0), State::Active).to_json()),
            ),
            ("sch-b".into(), Some(json!({"not": "a schedule"}))),
        ],
    );
    assert!(scan_journal(&meta.path).is_empty());
    // The probe's own values: a later write replaces, a delete removes.
    let repo = SessionRepo::new(dir.path(), SystemClock::shared());
    let mut session = repo
        .open(&session_journal::repo::read_metadata(&probe.path).unwrap())
        .unwrap();
    for (key, value) in [
        ("sch-1", own("sch-1", T0, State::Active)),
        ("sch-2", own("sch-2", T0 + HOUR, State::Active)),
        ("sch-1", own("sch-1", T0 + 2 * HOUR, State::Active)),
        ("sch-3", own("sch-3", T0, State::Active)),
    ] {
        session.commit(&[Write::set(NAMESPACE, key, value)]).unwrap();
    }
    session
        .commit(&[Write::ValueDelete {
            namespace: NAMESPACE.into(),
            key: "sch-3".into(),
        }])
        .unwrap();
    drop(session);
    let found: Vec<(String, Option<i64>)> = scan_journal(&probe.path)
        .into_iter()
        .map(|s| (s.id, s.next_due))
        .collect();
    assert_eq!(
        found,
        vec![
            ("sch-1".to_string(), Some(T0 + 2 * HOUR)),
            ("sch-2".to_string(), Some(T0 + HOUR))
        ]
    );
    let sessions: Vec<String> = scan_sessions(dir.path()).into_iter().map(|(m, _)| m.id).collect();
    assert_eq!(sessions, vec![probe.id.clone()]);
    // A torn last line (a worker mid-write) is skipped, the file left as it is.
    let mut text = std::fs::read_to_string(&probe.path).unwrap();
    text.push_str(r#"{"seq":99,"kind":"value","op":"set","namespace":"ultron.schedule","key":"sch-9""#);
    std::fs::write(&probe.path, &text).unwrap();
    assert_eq!(scan_journal(&probe.path).len(), 2);
    assert_eq!(std::fs::read_to_string(&probe.path).unwrap(), text);
    assert_eq!(
        fire_call("sch-1", 5),
        json!({"serviceId": "ultron.session-ops", "member": "op", "args": [{"op": "scheduleFire", "id": "sch-1", "due": 5}]})
    );
}

#[test]
fn daemon_prompt_schedules_missed_slots_fire_once_on_start_then_wait_for_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let probe = session_with(dir.path(), &[]);
    let repo = SessionRepo::new(dir.path(), SystemClock::shared());
    let mut session = repo
        .open(&session_journal::repo::read_metadata(&probe.path).unwrap())
        .unwrap();
    // Due five hours ago (the server was down), and one not due yet.
    for s in [
        schedule("sch-late", &probe.id, Some(T0 - 5 * HOUR), State::Active),
        schedule("sch-later", &probe.id, Some(T0 + 3 * HOUR), State::Active),
        schedule("sch-done", &probe.id, None, State::Done),
    ] {
        session.commit(&[Write::set(NAMESPACE, &s.id, s.to_json())]).unwrap();
    }
    drop(session);
    let clock = ManualTickClock::new(T0);
    let (s, deliveries, _) = scheduler(&clock);
    s.start(dir.path().to_path_buf());
    let first = deliveries.recv_timeout(HANG).unwrap();
    assert_eq!(first, (probe.id.clone(), "sch-late".to_string(), T0 - 5 * HOUR, T0));
    // The answer (next slot after now) replaces what the timer held.
    settled(&s);
    assert_eq!(s.pending(), vec![(probe.id.clone(), "sch-late".to_string(), T0 + HOUR)]);
    // That answer listed only `sch-late`: the session's report replaces its schedules.
    s.update(
        &probe,
        &json!([{"id": "sch-late", "nextDue": T0 + HOUR}, {"id": "sch-later", "nextDue": T0 + 3 * HOUR}]),
    );
    // Nothing is due: pokes look at the clock once each and deliver nothing.
    poke(&s);
    poke(&s);
    assert!(deliveries.try_recv().is_err());
    clock.set(T0 + HOUR);
    poke(&s);
    assert_eq!(
        deliveries.recv_timeout(HANG).unwrap(),
        (probe.id.clone(), "sch-late".to_string(), T0 + HOUR, T0 + HOUR)
    );
    s.stop();
}

#[test]
fn daemon_prompt_schedules_retry_waits_and_a_gone_session_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let probe = session_with(dir.path(), &[]);
    let clock = ManualTickClock::new(T0);
    let (s, deliveries, recorder) = scheduler(&clock);
    s.start(dir.path().join("empty"));
    recorder
        .answers
        .lock()
        .unwrap()
        .extend([Delivery::Retry("worker startup failed".into()), Delivery::Gone]);
    s.update(&probe, &json!([{"id": "sch-1", "name": "a", "nextDue": T0}]));
    assert_eq!(deliveries.recv_timeout(HANG).unwrap().1, "sch-1");
    settled(&s);
    assert_eq!(s.pending().len(), 1);
    // Within the retry delay nothing is delivered again.
    clock.set(T0 + RETRY_MS - 1);
    poke(&s);
    assert!(deliveries.try_recv().is_err());
    clock.set(T0 + RETRY_MS);
    poke(&s);
    let again = deliveries.recv_timeout(HANG).unwrap();
    assert_eq!((again.1.as_str(), again.2, again.3), ("sch-1", T0, T0 + RETRY_MS));
    // Gone: the session's schedules are dropped.
    settled(&s);
    assert!(s.pending().is_empty());
    // A fire the session did not take (its slot unchanged) waits as a retry does.
    recorder
        .answers
        .lock()
        .unwrap()
        .push(Delivery::Done(json!([{"id": "sch-2", "nextDue": T0 + RETRY_MS}])));
    s.update(&probe, &json!([{"id": "sch-2", "nextDue": T0 + RETRY_MS}]));
    assert_eq!(deliveries.recv_timeout(HANG).unwrap().1, "sch-2");
    settled(&s);
    assert_eq!(s.pending().len(), 1);
    poke(&s);
    assert!(deliveries.try_recv().is_err());
    // An empty report removes the session.
    s.update(&probe, &json!([]));
    assert!(s.pending().is_empty());
    s.stop();
}

#[test]
fn daemon_prompt_schedules_cancel_in_journal_takes_the_locks_and_writes_the_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let probe = session_with(dir.path(), &[]);
    let repo = SessionRepo::new(dir.path(), SystemClock::shared());
    let mut session = repo
        .open(&session_journal::repo::read_metadata(&probe.path).unwrap())
        .unwrap();
    let active = schedule("sch-1", &probe.id, Some(T0 + HOUR), State::Active);
    session
        .commit(&[Write::set(NAMESPACE, "sch-1", active.to_json())])
        .unwrap();
    drop(session);
    let clock = ManualTickClock::new(T0);
    let cancelled = cancel_in_journal(&probe.path, "name-sch-1", clock.clone()).unwrap();
    assert_eq!((cancelled.state, cancelled.next_due), (State::Cancelled, None));
    let read = scan_journal(&probe.path);
    assert_eq!(read[0].state, State::Cancelled);
    assert!(!Path::new(&format!("{}.lock", probe.path.display())).exists());
    assert_eq!(
        cancel_in_journal(&probe.path, "sch-1", clock.clone()).unwrap_err(),
        "no active schedule sch-1"
    );
    // A live owner (a worker) holds the session: refused, nothing written.
    let owner = durable::owner::OwnerLock::acquire(&probe.path).unwrap();
    let error = cancel_in_journal(&probe.path, "sch-1", clock).unwrap_err();
    assert!(error.contains("is open in another Ultron process"), "{error}");
    drop(owner);
}

#[test]
fn daemon_prompt_schedules_hold_the_server_while_any_is_pending_and_keep_the_last_failure() {
    let dir = tempfile::tempdir().unwrap();
    let probe = session_with(dir.path(), &[]);
    let mut session = SessionRepo::new(dir.path(), SystemClock::shared())
        .open(&session_journal::repo::read_metadata(&probe.path).unwrap())
        .unwrap();
    let pending = schedule("sch-1", &probe.id, Some(T0), State::Active);
    session
        .commit(&[Write::set(NAMESPACE, "sch-1", pending.to_json())])
        .unwrap();
    drop(session);
    let clock = ManualTickClock::new(T0);
    let (s, deliveries, recorder) = scheduler(&clock);
    let errors = dir.path().join("schedule-errors.json");
    s.record_errors(errors.clone());
    let (held, holds) = channel();
    s.on_hold(Box::new(move |pending| {
        let _ = held.send(pending);
    }));
    // Held from the start: the journals are not read yet.
    assert!(holds.recv_timeout(HANG).unwrap());
    recorder
        .answers
        .lock()
        .unwrap()
        .push(Delivery::Retry("No API key found for anthropic".into()));
    s.start(dir.path().to_path_buf());
    assert_eq!(deliveries.recv_timeout(HANG).unwrap().1, "sch-1");
    settled(&s);
    // A failed delivery keeps the slot (and the hold), and its error is kept for `ultron schedule list`.
    assert!(s.holding());
    assert_eq!(s.pending(), vec![(probe.id.clone(), "sch-1".to_string(), T0)]);
    let kept = daemon::prompt_schedules::read_errors(&errors);
    assert_eq!(
        kept.get(&(probe.id.clone(), "sch-1".to_string())),
        Some(&("No API key found for anthropic".to_string(), T0, T0))
    );
    // The retry delivers: the failure is cleared, the schedule still pending.
    clock.set(T0 + RETRY_MS);
    poke(&s);
    assert_eq!(deliveries.recv_timeout(HANG).unwrap().2, T0);
    settled(&s);
    assert!(daemon::prompt_schedules::read_errors(&errors).is_empty());
    assert_eq!(std::fs::read_to_string(&errors).unwrap(), "{}\n");
    assert!(holds.try_recv().is_err(), "the hold did not change");
    // The session's last schedule ends (a cancel's report): the hold is released once.
    s.update(&probe, &json!([]));
    assert!(!holds.recv_timeout(HANG).unwrap());
    assert!(!s.holding());
    // A new schedule holds again; a gone session releases it.
    recorder.answers.lock().unwrap().push(Delivery::Gone);
    s.update(&probe, &json!([{"id": "sch-2", "nextDue": T0 + RETRY_MS}]));
    assert!(holds.recv_timeout(HANG).unwrap());
    assert_eq!(deliveries.recv_timeout(HANG).unwrap().1, "sch-2");
    assert!(!holds.recv_timeout(HANG).unwrap());
    s.stop();
}
