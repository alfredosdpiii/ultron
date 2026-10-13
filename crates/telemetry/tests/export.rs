//! The exporter delivers settled spans to a sink in batches on a background thread; recording never waits for it.

use serde_json::json;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;
use telemetry::{Attributes, BatchConfig, End, RecordedSpan, Sink, TelemetryContext};

struct Channel(Sender<Vec<RecordedSpan>>);
impl Sink for Channel {
    fn export(&mut self, batch: Vec<RecordedSpan>) {
        let _ = self.0.send(batch);
    }
}

/// Signals when it receives its first batch, then never returns.
struct Hang(Sender<usize>, Receiver<()>);
impl Sink for Hang {
    fn export(&mut self, batch: Vec<RecordedSpan>) {
        let _ = self.0.send(batch.len());
        let _ = self.1.recv();
    }
}

fn config(max_batch: usize, max_delay_ms: u64, queue_capacity: usize) -> BatchConfig {
    BatchConfig {
        max_batch,
        max_delay: Duration::from_millis(max_delay_ms),
        queue_capacity,
    }
}

fn names(batch: &[RecordedSpan]) -> Vec<&str> {
    batch.iter().map(|span| span.name.as_str()).collect()
}

#[test]
fn telemetry_hung_sink_never_delays_the_caller() {
    let (entered_tx, entered) = channel();
    let (_never_tx, never) = channel();
    let context = TelemetryContext::exporting(Hang(entered_tx, never), config(1, 1000, 4));
    context.start("first", Attributes::new()).end(End::Ok);
    assert_eq!(
        entered.recv_timeout(Duration::from_secs(120)),
        Ok(1),
        "sink is now stuck inside export"
    );

    let caller = context.clone();
    let (done_tx, done) = channel();
    thread::spawn(move || {
        for i in 0..10_000 {
            let span = caller.start("work", Attributes::new());
            span.start_child("child", Attributes::new()).end(End::Ok);
            span.end(if i % 2 == 0 { End::Ok } else { End::Failed(None) });
        }
        let _ = done_tx.send(caller.dropped());
    });
    // 20,000 settled spans against a 4-slot queue whose consumer never returns: 4 queue, the rest are dropped.
    assert_eq!(done.recv_timeout(Duration::from_secs(120)), Ok(19_996));
    assert!(
        !context.shutdown(Duration::from_millis(50)),
        "the hung sink cannot finish"
    );
}

#[test]
fn telemetry_exports_full_batches_then_the_remainder_on_shutdown() {
    let (tx, rx) = channel();
    let context = TelemetryContext::exporting(Channel(tx), config(3, 60_000, 64));
    for name in ["a", "b", "c", "d", "e", "f", "g"] {
        context.start(name, Attributes::new()).end(End::Ok);
    }
    let first = rx.recv_timeout(Duration::from_secs(120)).expect("first batch");
    let second = rx.recv_timeout(Duration::from_secs(120)).expect("second batch");
    assert_eq!(
        (names(&first), names(&second)),
        (vec!["a", "b", "c"], vec!["d", "e", "f"])
    );
    assert!(rx.try_recv().is_err(), "a partial batch waits for its delay");
    assert!(context.shutdown(Duration::from_secs(120)));
    assert_eq!(names(&rx.recv().expect("remainder")), vec!["g"]);
    context.start("late", Attributes::new()).end(End::Ok);
    assert_eq!(context.dropped(), 1);
    assert!(rx.recv().is_err(), "exporter thread has exited");
}

#[test]
fn telemetry_partial_batch_is_delivered_after_max_delay() {
    let (tx, rx) = channel();
    let context = TelemetryContext::exporting(Channel(tx), config(100, 20, 64));
    let span = context.start(
        "only",
        [("k".to_owned(), telemetry::AttributeValue::Number(2.0))].into(),
    );
    span.add_event("e", Attributes::new());
    span.end(End::Ok);
    let batch = rx.recv_timeout(Duration::from_secs(120)).expect("delayed batch");
    assert_eq!(
        json!(batch),
        json!([{ "id": 1, "parentId": null, "name": "only", "attributes": { "k": 2 },
            "events": [{ "name": "e", "attributes": {} }], "status": { "status": "ok" }, "settled": true, "endSequence": 1 }])
    );
}

#[test]
fn telemetry_dropped_span_handle_ends_as_failed() {
    let (tx, rx) = channel();
    let context = TelemetryContext::exporting(Channel(tx), config(1, 60_000, 64));
    let span = context.start("abandoned", Attributes::new());
    let clone = span.clone();
    drop(span);
    assert!(rx.try_recv().is_err(), "a live clone keeps the span open");
    drop(clone);
    let batch = rx.recv_timeout(Duration::from_secs(120)).expect("batch");
    assert_eq!(json!(batch[0].status), json!({ "status": "error" }));
}
