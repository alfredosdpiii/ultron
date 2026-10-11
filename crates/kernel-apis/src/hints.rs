//! `hints.mute` / `hints.unmute` / `hints.muted` (the request side of `ultron/rlm/hints.ts`): per-lane hint mutes,
//! persisted in the module document `{version: 1, lanes: {<lane>: {muted, fired}}}`.

use crate::store::{DocumentStore, read_versioned};
use crate::util::{lock, stringify};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

pub const HINT_TAGS: &[&str] = &[
    "stuck-loop",
    "job-detached",
    "blocked-on-job",
    "poll-loop",
    "repeated-failure",
    "repeated-cell",
    "large-read",
    "output-truncated",
];

pub struct HintsModule {
    store: Arc<dyn DocumentStore>,
    /// `None` until loaded; lane → {muted, fired}.
    lanes: Mutex<Option<Map<String, Value>>>,
}

fn tags(value: Option<&Value>) -> Result<Vec<String>, String> {
    let items = value
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or("hint tags must be one or more strings")?;
    items
        .iter()
        .map(|tag| match tag.as_str() {
            Some(tag) if HINT_TAGS.contains(&tag) => Ok(tag.to_string()),
            _ => Err(format!(
                "Unknown hint tag {}; tags: {}",
                stringify(tag),
                HINT_TAGS.join(", ")
            )),
        })
        .collect()
}

impl HintsModule {
    pub fn new(store: Arc<dyn DocumentStore>) -> Self {
        Self {
            store,
            lanes: Mutex::new(None),
        }
    }

    fn load(&self, lanes: &mut Option<Map<String, Value>>) -> Result<(), String> {
        if lanes.is_some() {
            return Ok(());
        }
        let saved = read_versioned("ultron.module/hints", self.store.read()?, "version")?;
        let mut loaded = Map::new();
        if let Some(Value::Object(saved)) = saved.as_ref().and_then(|saved| saved.get("lanes")) {
            for (lane, entry) in saved {
                let muted: Vec<Value> = entry["muted"]
                    .as_array()
                    .map(|items| items.iter().filter(|tag| tag.is_string()).cloned().collect())
                    .unwrap_or_default();
                let fired = entry
                    .get("fired")
                    .filter(|fired| fired.is_object())
                    .cloned()
                    .unwrap_or(json!({}));
                loaded.insert(lane.clone(), json!({"muted": muted, "fired": fired}));
            }
        }
        *lanes = Some(loaded);
        Ok(())
    }

    pub fn start(&self) -> Result<(), String> {
        self.load(&mut lock(&self.lanes))
    }

    /// The tags `lane` muted.
    pub fn muted(&self, lane: &str) -> Vec<String> {
        let mut lanes = lock(&self.lanes);
        if self.load(&mut lanes).is_err() {
            return Vec::new();
        }
        lanes
            .as_ref()
            .and_then(|lanes| lanes.get(lane))
            .and_then(|entry| entry["muted"].as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|tag| tag.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn handle(&self, kind: &str, payload: &Map<String, Value>, lane: &str) -> Result<Value, String> {
        let mut guard = lock(&self.lanes);
        self.load(&mut guard)?;
        let lanes = guard.as_mut().expect("loaded");
        let requested = if kind == "hints.muted" {
            Vec::new()
        } else {
            tags(payload.get("tags"))?
        };
        let entry = lanes
            .entry(lane.to_string())
            .or_insert_with(|| json!({"muted": [], "fired": {}}));
        let mut muted: Vec<String> = entry["muted"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|tag| tag.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        match kind {
            "hints.mute" => {
                muted.extend(requested);
                muted.sort();
                muted.dedup();
            }
            "hints.unmute" => muted.retain(|tag| !requested.contains(tag)),
            "hints.muted" => {}
            _ => return Err(format!("Ultron RLM host request is not wired: {kind}")),
        }
        entry["muted"] = json!(muted);
        if kind != "hints.muted" {
            let document = json!({"version": 1, "lanes": lanes});
            // As in TS, a failed journal write does not fail the request.
            let _ = self.store.write(&document);
        }
        Ok(json!({"muted": muted}))
    }
}

impl crate::ApiModule for HintsModule {
    fn prefixes(&self) -> &[&str] {
        &["hints."]
    }

    fn start(&self, _host: &dyn crate::TaskHost) -> Result<(), String> {
        HintsModule::start(self)
    }

    fn handle(&self, request: &crate::Request<'_>) -> Result<Value, String> {
        HintsModule::handle(self, request.kind, request.payload, request.lane)
    }
}
