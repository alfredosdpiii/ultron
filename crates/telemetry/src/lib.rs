//! Span telemetry ported from `packages/telemetry` (MIT): an in-memory recorder with the TypeScript
//! `InMemoryTelemetryContext` semantics, a noop context, and an exporter that hands settled spans to a [`Sink`] in
//! batches on a background thread. Recording never waits on the sink: a full queue drops the span and counts it.
//! Also ports `isInstallTelemetryEnabled` from `packages/coding-agent/src/core/telemetry.ts` and, in submodules, the
//! attribution headers it gates, bug-report payloads and the crash log.

pub mod attribution;
pub mod bug_report;
pub mod crash_log;
mod js;
pub mod zip;

pub use js::{epoch_millis, iso_timestamp, parse_iso_millis};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize, Serializer};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// A span or event attribute value. Numbers are JavaScript numbers (f64).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum AttributeValue {
    String(String),
    Number(f64),
    Bool(bool),
    Strings(Vec<String>),
    Numbers(Vec<f64>),
    Bools(Vec<bool>),
}

/// Attributes keep insertion order; a merge overwrites in place and appends new names, like a JavaScript object.
pub type Attributes = IndexMap<String, AttributeValue>;

/// Serializes like `JSON.stringify` of a JavaScript number: integral values have no fraction.
fn js_number<S: Serializer>(value: f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        #[allow(clippy::cast_possible_truncation)]
        serializer.serialize_i64(value as i64)
    } else {
        serializer.serialize_f64(value)
    }
}

impl Serialize for AttributeValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        match self {
            Self::String(value) => serializer.serialize_str(value),
            Self::Number(value) => js_number(*value, serializer),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Strings(values) => values.serialize(serializer),
            Self::Bools(values) => values.serialize(serializer),
            Self::Numbers(values) => {
                struct Js(f64);
                impl Serialize for Js {
                    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                        js_number(self.0, serializer)
                    }
                }
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    seq.serialize_element(&Js(*value))?;
                }
                seq.end()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetails {
    pub name: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum SpanStatus {
    Ok,
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ErrorDetails>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordedEvent {
    pub name: String,
    pub attributes: Attributes,
}

/// A detached span snapshot, serialized like the TypeScript `RecordedTelemetrySpan`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedSpan {
    pub id: u64,
    pub parent_id: Option<u64>,
    pub name: String,
    pub attributes: Attributes,
    pub events: Vec<RecordedEvent>,
    pub status: SpanStatus,
    pub settled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_sequence: Option<u64>,
}

/// How a span ended. `Failed(None)` is a failure without inspectable details (a thrown non-Error in TypeScript).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    Ok,
    Failed(Option<ErrorDetails>),
}

/// Receives settled spans on the exporter thread. It may block; recording callers never wait for it.
pub trait Sink: Send + 'static {
    fn export(&mut self, batch: Vec<RecordedSpan>);
}

#[derive(Debug, Clone, Copy)]
pub struct BatchConfig {
    /// A batch is delivered once it holds this many spans.
    pub max_batch: usize,
    /// A non-empty batch is delivered at most this long after its first span arrived.
    pub max_delay: Duration,
    /// Settled spans waiting for the exporter thread; beyond this, spans are dropped and counted.
    pub queue_capacity: usize,
}

struct Record {
    span: RecordedSpan,
    explicit_status: bool,
}

#[derive(Default)]
struct Counters {
    next_id: u64,
    next_end: u64,
    retained: Option<Vec<Arc<Mutex<Record>>>>,
}

struct Exporter {
    queue: Mutex<Option<SyncSender<RecordedSpan>>>,
    done: Mutex<Option<Receiver<()>>>,
    dropped: AtomicU64,
}

struct Shared {
    counters: Mutex<Counters>,
    exporter: Option<Exporter>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A telemetry context. Clones share one recording scope.
#[derive(Clone)]
pub struct TelemetryContext(Option<Arc<Shared>>);

impl TelemetryContext {
    /// Records nothing; every span it starts is inert.
    pub fn noop() -> Self {
        Self(None)
    }

    /// Records every span in memory; read them with [`TelemetryContext::spans`].
    pub fn in_memory() -> Self {
        Self::with(Some(Vec::new()), None)
    }

    /// Delivers each settled span to `sink` in batches from a background thread; retains nothing in memory.
    pub fn exporting(mut sink: impl Sink, config: BatchConfig) -> Self {
        let (queue, rx) = sync_channel::<RecordedSpan>(config.queue_capacity);
        let (done_tx, done) = sync_channel(1);
        thread::spawn(move || {
            let mut batch = Vec::new();
            let mut deadline: Option<Instant> = None;
            loop {
                let next = match deadline {
                    None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
                };
                match next {
                    Ok(span) => {
                        deadline.get_or_insert_with(|| Instant::now() + config.max_delay);
                        batch.push(span);
                        if batch.len() < config.max_batch {
                            continue;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        if !batch.is_empty() {
                            sink.export(batch);
                        }
                        break;
                    }
                }
                deadline = None;
                sink.export(std::mem::take(&mut batch));
            }
            let _ = done_tx.send(());
        });
        let exporter = Exporter {
            queue: Mutex::new(Some(queue)),
            done: Mutex::new(Some(done)),
            dropped: AtomicU64::new(0),
        };
        Self::with(None, Some(exporter))
    }

    fn with(retained: Option<Vec<Arc<Mutex<Record>>>>, exporter: Option<Exporter>) -> Self {
        let counters = Counters {
            next_id: 1,
            next_end: 1,
            retained,
        };
        Self(Some(Arc::new(Shared {
            counters: Mutex::new(counters),
            exporter,
        })))
    }

    /// Starts a root span.
    pub fn start(&self, name: &str, attributes: Attributes) -> Span {
        match &self.0 {
            None => Span(None),
            Some(shared) => start_span(shared, None, name, attributes),
        }
    }

    /// Detached snapshots of retained spans in start order (empty unless created with [`TelemetryContext::in_memory`]).
    pub fn spans(&self) -> Vec<RecordedSpan> {
        let Some(shared) = &self.0 else { return Vec::new() };
        let records: Vec<_> = lock(&shared.counters).retained.iter().flatten().cloned().collect();
        records.iter().map(|record| lock(record).span.clone()).collect()
    }

    /// Settled spans the exporter could not queue because the queue was full or already shut down.
    pub fn dropped(&self) -> u64 {
        self.exporter()
            .map_or(0, |exporter| exporter.dropped.load(Ordering::Relaxed))
    }

    /// Closes the export queue and waits up to `timeout` for the sink to receive what was queued.
    /// Returns whether the exporter finished in time. Spans settled afterwards are dropped.
    pub fn shutdown(&self, timeout: Duration) -> bool {
        let Some(exporter) = self.exporter() else { return true };
        lock(&exporter.queue).take();
        let Some(done) = lock(&exporter.done).take() else {
            return true;
        };
        done.recv_timeout(timeout).is_ok()
    }

    fn exporter(&self) -> Option<&Exporter> {
        self.0.as_ref().and_then(|shared| shared.exporter.as_ref())
    }
}

struct SpanInner {
    shared: Arc<Shared>,
    record: Arc<Mutex<Record>>,
}

impl Drop for SpanInner {
    /// A span whose last handle is dropped without [`Span::end`] (a cancelled future, a panic) ends as failed.
    fn drop(&mut self) {
        settle(&self.shared, &self.record, End::Failed(None));
    }
}

/// A span handle. Clones refer to the same span; a noop span ignores everything.
#[derive(Clone)]
pub struct Span(Option<Arc<SpanInner>>);

fn start_span(shared: &Arc<Shared>, parent: Option<u64>, name: &str, attributes: Attributes) -> Span {
    let mut counters = lock(&shared.counters);
    let id = counters.next_id;
    counters.next_id += 1;
    let span = RecordedSpan {
        id,
        parent_id: parent,
        name: name.to_owned(),
        attributes,
        events: Vec::new(),
        status: SpanStatus::Ok,
        settled: false,
        end_sequence: None,
    };
    let record = Arc::new(Mutex::new(Record {
        span,
        explicit_status: false,
    }));
    if let Some(retained) = &mut counters.retained {
        retained.push(Arc::clone(&record));
    }
    Span(Some(Arc::new(SpanInner {
        shared: Arc::clone(shared),
        record,
    })))
}

fn settle(shared: &Shared, record: &Mutex<Record>, end: End) {
    let snapshot = {
        let mut record = lock(record);
        if record.span.settled {
            return;
        }
        if let End::Failed(error) = end
            && !record.explicit_status
        {
            record.span.status = SpanStatus::Error { error };
        }
        let mut counters = lock(&shared.counters);
        record.span.settled = true;
        record.span.end_sequence = Some(counters.next_end);
        counters.next_end += 1;
        record.span.clone()
    };
    let Some(exporter) = &shared.exporter else { return };
    let queued = match lock(&exporter.queue).as_ref() {
        Some(queue) => match queue.try_send(snapshot) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        },
        None => false,
    };
    if !queued {
        exporter.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

impl Span {
    /// The inert span, as handed out by the noop context.
    pub fn noop() -> Self {
        Self(None)
    }

    /// Runs `update` on an open span; settled and noop spans ignore it.
    fn open(&self, update: impl FnOnce(&mut Record)) {
        if let Some(inner) = &self.0 {
            let mut record = lock(&inner.record);
            if !record.span.settled {
                update(&mut record);
            }
        }
    }

    /// Starts a child span; a child of a settled or noop span is a noop span.
    pub fn start_child(&self, name: &str, attributes: Attributes) -> Span {
        let Some(inner) = &self.0 else { return Span(None) };
        let parent = lock(&inner.record).span.clone();
        if parent.settled {
            return Span(None);
        }
        start_span(&inner.shared, Some(parent.id), name, attributes)
    }

    pub fn add_event(&self, name: &str, attributes: Attributes) {
        self.open(|record| {
            record.span.events.push(RecordedEvent {
                name: name.to_owned(),
                attributes,
            })
        });
    }

    /// Merges `attributes` over the current ones.
    pub fn set_attributes(&self, attributes: Attributes) {
        self.open(|record| record.span.attributes.extend(attributes));
    }

    /// Sets the status; an explicit status is never overwritten by a failed [`End`].
    pub fn set_status(&self, status: SpanStatus) {
        self.open(|record| {
            record.span.status = status;
            record.explicit_status = true;
        });
    }

    /// Settles the span. Later calls on any handle of it are ignored.
    pub fn end(&self, end: End) {
        if let Some(inner) = &self.0 {
            settle(&inner.shared, &inner.record, end);
        }
    }
}

/// `isInstallTelemetryEnabled`: a set `PI_TELEMETRY` (`env`) decides alone (`1`, `true`, `yes`, any case);
/// otherwise the `enableInstallTelemetry` setting, which defaults to on.
pub fn is_install_telemetry_enabled(env: Option<&str>, setting: Option<bool>) -> bool {
    match env {
        Some(value) => value == "1" || matches!(value.to_lowercase().as_str(), "true" | "yes"),
        None => setting.unwrap_or(true),
    }
}
