//! The Chord remote-service host the server and the session worker expose (packages/chord/src/services/provider.ts,
//! state.ts, state-codec.ts, wire.ts): singleton services whose members are methods or replicated JSON states, one
//! endpoint per consumer owning its subscriptions, and the per-subscription wire encoder the server applies.

use chord_delta::{Encoder, Op};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

pub const SERVICE_CONTROL_ID: &str = "$chord.service";

/// A failure a service reports. With a code (a `RemoteServiceError`/`ServerError`) it crosses the wire as is;
/// without one it is an internal error the server reports as "Internal server error".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceError {
    pub code: Option<String>,
    pub message: String,
}

impl ServiceError {
    pub fn coded(code: &str, message: impl Into<String>) -> Self {
        ServiceError {
            code: Some(code.into()),
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        ServiceError {
            code: None,
            message: message.into(),
        }
    }
}

impl From<String> for ServiceError {
    fn from(message: String) -> Self {
        ServiceError::internal(message)
    }
}

pub type ServiceResult = Result<Option<Value>, ServiceError>;
pub type Method = Arc<dyn Fn(Vec<Value>) -> ServiceResult + Send + Sync>;
type StateListener = Arc<dyn Fn(&[Op], u64) + Send + Sync>;

struct StateInner {
    value: Value,
    sequence: u64,
    listeners: Vec<(u64, StateListener)>,
    next_listener: u64,
}

/// A replicated JSON value: every change publishes `diffRevisions(previous, next)` with the next sequence.
pub struct State {
    inner: Mutex<StateInner>,
}

impl State {
    /// A state at sequence 0, as `replicatedState(value)` starts.
    pub fn new(value: Value) -> Arc<State> {
        Self::with_sequence(value, 0)
    }

    /// A state whose value was already published `sequence` times.
    pub fn with_sequence(value: Value, sequence: u64) -> Arc<State> {
        Arc::new(State {
            inner: Mutex::new(StateInner {
                value,
                sequence,
                listeners: Vec::new(),
                next_listener: 0,
            }),
        })
    }

    pub fn value(&self) -> Value {
        self.lock().value.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StateInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mutate a copy of the value and publish the difference (nothing when it is unchanged).
    pub fn change(&self, mutate: impl FnOnce(&mut Value)) {
        let mut inner = self.lock();
        let mut next = inner.value.clone();
        mutate(&mut next);
        let ops = crate::diff::diff_revisions(&inner.value, &next);
        if ops.is_empty() {
            return;
        }
        inner.value = next;
        inner.sequence += 1;
        let sequence = inner.sequence;
        // Listeners run under the lock so every subscriber sees publications in sequence order.
        for (_, listener) in &inner.listeners {
            listener(&ops, sequence);
        }
    }

    pub fn replace(&self, value: Value) {
        self.change(|current| *current = value);
    }

    /// The current `(sequence, value)` and a listener for later publications, taken atomically.
    fn subscribe(&self, listener: StateListener) -> (u64, Value, u64) {
        let mut inner = self.lock();
        let id = inner.next_listener;
        inner.next_listener += 1;
        inner.listeners.push((id, listener));
        (inner.sequence, inner.value.clone(), id)
    }

    fn unsubscribe(&self, id: u64) {
        self.lock().listeners.retain(|(i, _)| *i != id);
    }
}

#[derive(Clone)]
pub enum Member {
    Method(Method),
    State(Arc<State>),
}

/// One singleton service: members are listed by name, as upstream sorts an implementation's keys.
#[derive(Clone)]
pub struct Service {
    pub id: String,
    pub members: BTreeMap<String, Member>,
    /// Callable and subscribable but left out of the catalogue (a Rust-only service TypeScript clients never see).
    pub unlisted: bool,
}

impl Service {
    pub fn new(id: &str) -> Self {
        Service {
            id: id.into(),
            members: BTreeMap::new(),
            unlisted: false,
        }
    }

    /// Leave the service out of the catalogue.
    pub fn unlisted(mut self) -> Self {
        self.unlisted = true;
        self
    }

    pub fn method(mut self, name: &str, method: impl Fn(Vec<Value>) -> ServiceResult + Send + Sync + 'static) -> Self {
        self.members.insert(name.into(), Member::Method(Arc::new(method)));
        self
    }

    /// A method upstream exposes that is not ported: it fails like a method that throws.
    pub fn unported(self, name: &str) -> Self {
        let message = format!("{}.{name} is not available in the Rust build yet", self.id);
        self.method(name, move |_| Err(ServiceError::internal(message.clone())))
    }

    pub fn state(mut self, name: &str, state: Arc<State>) -> Self {
        self.members.insert(name.into(), Member::State(state));
        self
    }
}

/// The services one provider publishes, in catalogue order.
#[derive(Clone, Default)]
pub struct Provider {
    pub services: Vec<Service>,
}

/// Receives `(subscriptionId, update)` for a consumer's subscriptions; updates carry plain (unencoded) ops.
pub type Publish = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// A parsed strict service call (`parseServiceCall`).
pub struct Call {
    pub service_id: String,
    pub member: String,
    pub args: Vec<Value>,
    pub has_instance: bool,
}

impl Call {
    pub fn parse(value: &Value) -> Option<Call> {
        let map = value.as_object()?;
        if !map
            .keys()
            .all(|k| ["serviceId", "member", "args", "instance"].contains(&k.as_str()))
        {
            return None;
        }
        let id = |key: &str| map.get(key)?.as_str().filter(|s| !s.is_empty()).map(str::to_string);
        Some(Call {
            service_id: id("serviceId")?,
            member: id("member")?,
            args: map.get("args")?.as_array()?.clone(),
            has_instance: map.contains_key("instance"),
        })
    }
}

enum Control {
    Catalogue,
    Subscribe { id: String, service: String, mode: String },
    Unsubscribe { id: String },
}

fn control(call: &Call) -> Option<Control> {
    if call.service_id != SERVICE_CONTROL_ID || call.has_instance {
        return None;
    }
    let is_id = |v: &Value| v.as_str().is_some_and(|s| !s.is_empty());
    match (call.member.as_str(), call.args.as_slice()) {
        ("catalogue", []) => Some(Control::Catalogue),
        ("subscribe", [id, service, mode])
            if is_id(id) && is_id(service) && matches!(mode.as_str(), Some("singleton" | "keyed")) =>
        {
            Some(Control::Subscribe {
                id: id.as_str()?.into(),
                service: service.as_str()?.into(),
                mode: mode.as_str()?.into(),
            })
        }
        ("unsubscribe", [id]) if is_id(id) => Some(Control::Unsubscribe {
            id: id.as_str()?.into(),
        }),
        _ => None,
    }
}

/// `true` when the call is a `$chord.service` subscribe; its subscription ID.
pub fn subscribe_id(call: &Value) -> Option<String> {
    match control(&Call::parse(call)?)? {
        Control::Subscribe { id, .. } => Some(id),
        _ => None,
    }
}

pub fn unsubscribe_id(call: &Value) -> Option<String> {
    match control(&Call::parse(call)?)? {
        Control::Unsubscribe { id } => Some(id),
        _ => None,
    }
}

type Subscription = Vec<(Arc<State>, u64)>;

/// One consumer of a provider (`createRemoteServiceEndpoint`).
pub struct Endpoint {
    provider: Arc<Provider>,
    subscriptions: Mutex<HashMap<String, Subscription>>,
}

impl Endpoint {
    pub fn new(provider: Arc<Provider>) -> Self {
        Endpoint {
            provider,
            subscriptions: Mutex::new(HashMap::new()),
        }
    }

    fn service(&self, id: &str) -> Result<&Service, ServiceError> {
        self.provider.services.iter().find(|s| s.id == id).ok_or_else(|| {
            ServiceError::coded("service_not_allowed", format!("Remote service {id} is not allowlisted"))
        })
    }

    pub fn invoke(&self, call: &Call, publish: Publish) -> ServiceResult {
        match control(call) {
            Some(Control::Catalogue) => Ok(Some(Value::Array(
                self.provider
                    .services
                    .iter()
                    .filter(|s| !s.unlisted)
                    .map(|s| json!({"serviceId": s.id, "mode": "singleton"}))
                    .collect(),
            ))),
            Some(Control::Subscribe { id, service, mode }) => self.subscribe(id, &service, &mode, publish),
            Some(Control::Unsubscribe { id }) => {
                let removed = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                let Some(states) = removed else {
                    return Err(ServiceError::internal("Service subscription was not found"));
                };
                for (state, listener) in states {
                    state.unsubscribe(listener);
                }
                Ok(None)
            }
            None => {
                let service = self.service(&call.service_id)?;
                if call.has_instance {
                    return Err(ServiceError::coded(
                        "service_mode_mismatch",
                        format!("Remote service {} is singleton", service.id),
                    ));
                }
                match service.members.get(&call.member) {
                    None => Err(ServiceError::coded(
                        "service_member_not_found",
                        format!("Unknown remote service member {}.{}", call.service_id, call.member),
                    )),
                    Some(Member::State(_)) => Err(ServiceError::coded(
                        "service_member_mismatch",
                        format!(
                            "Remote service member {}.{} is not a method",
                            call.service_id, call.member
                        ),
                    )),
                    Some(Member::Method(method)) => method(call.args.clone()),
                }
            }
        }
    }

    fn subscribe(&self, id: String, service_id: &str, mode: &str, publish: Publish) -> ServiceResult {
        let mut subscriptions = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
        if subscriptions.contains_key(&id) {
            return Err(ServiceError::internal("Service subscription ID is already active"));
        }
        let service = self.service(service_id)?;
        if mode != "singleton" {
            return Err(ServiceError::coded(
                "service_mode_mismatch",
                format!("Remote service {service_id} is singleton, not {mode}"),
            ));
        }
        let mut members = Vec::new();
        let mut listeners = Vec::new();
        for (name, member) in &service.members {
            match member {
                Member::Method(_) => members.push(json!({"name": name, "kind": "method"})),
                Member::State(state) => {
                    let (publish, subscription, member) = (publish.clone(), id.clone(), name.clone());
                    let listener: StateListener = Arc::new(move |ops: &[Op], sequence: u64| {
                        let ops: Vec<Value> = ops.iter().map(Op::to_json).collect();
                        publish(
                            &subscription,
                            json!({"type": "state", "member": member, "sequence": sequence, "ops": ops}),
                        );
                    });
                    let (sequence, value, listener) = state.subscribe(listener);
                    listeners.push((state.clone(), listener));
                    members.push(json!({"name": name, "kind": "state", "sequence": sequence, "ops": [["r", value]]}));
                }
            }
        }
        subscriptions.insert(id, listeners);
        Ok(Some(
            json!({"serviceId": service_id, "mode": "singleton", "instances": [{"members": members}]}),
        ))
    }

    /// Close every subscription (endpoint disposal).
    pub fn dispose(&self) {
        let all: Vec<Subscription> = self
            .subscriptions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, v)| v)
            .collect();
        for (state, listener) in all.into_iter().flatten() {
            state.unsubscribe(listener);
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.dispose();
    }
}

/// The server's per-subscription state encoder (`createServiceStateEncoder`): plain ops in, wire ops out.
#[derive(Default)]
pub struct WireEncoder {
    members: HashMap<String, Encoder>,
}

fn encode_ops(encoder: &mut Encoder, ops: &Value) -> Result<Value, String> {
    let ops = ops
        .as_array()
        .ok_or("Invalid service state update")?
        .iter()
        .map(|op| Op::from_json(op).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Value::Array(encoder.encode(&ops)))
}

impl WireEncoder {
    fn encode_instance(&mut self, instance: &Value) -> Result<Value, String> {
        let mut instance = instance.clone();
        if let Some(members) = instance.get_mut("members").and_then(Value::as_array_mut) {
            for member in members {
                if member["kind"] == "state" {
                    let name = member["name"].as_str().unwrap_or_default().to_string();
                    let encoder = self.members.entry(name).or_default();
                    member["ops"] = encode_ops(encoder, &member["ops"])?;
                }
            }
        }
        Ok(instance)
    }

    pub fn encode_snapshot(&mut self, snapshot: &Value) -> Result<Value, String> {
        self.members.clear();
        let mut snapshot = snapshot.clone();
        if let Some(instances) = snapshot.get_mut("instances").and_then(Value::as_array_mut) {
            for instance in instances {
                *instance = self.encode_instance(instance)?;
            }
        }
        Ok(snapshot)
    }

    pub fn encode_update(&mut self, update: &Value) -> Result<Value, String> {
        let mut update = update.clone();
        match update["type"].as_str() {
            Some("state") => {
                let member = update["member"].as_str().unwrap_or_default().to_string();
                let encoder = self
                    .members
                    .get_mut(&member)
                    .ok_or_else(|| format!("Unknown service state {member}"))?;
                update["ops"] = encode_ops(encoder, &update["ops"])?;
            }
            Some("replaced") => {
                self.members.clear();
                update["snapshot"] = self.encode_instance(&update["snapshot"])?;
            }
            Some("unavailable") => self.members.clear(),
            _ => {}
        }
        Ok(update)
    }
}
