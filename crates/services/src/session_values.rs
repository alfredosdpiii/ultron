//! Ultron session values: the documents of the local services, host modules, task journal, definition registry,
//! usage ledger and memory journal kept as values of the session itself (format-4 session journal), as the
//! TypeScript worker keeps them (`createSessionTaskStore` at `ultron.tasks/root`, `createSessionModuleStore` at
//! `ultron.module/<name>`, `createSessionDefinitionStore` at `ultron.definitions/root`, `createSessionUsageStore` at
//! `ultron.usage/root`, the memory journal at `ultron.memory.state/root`, the local services at `ultron.local/<key>`).
//!
//! Because they are session values, resuming a session restores them, a tree fork copies them at the fork point,
//! and deleting a fork leaves its source untouched: no state lives beside the session to drift from it (A07).

use kernel_apis::DocumentStore;
use kernel_apis::artifacts::KeyedDocuments;
use kernel_apis::util::lock;
use serde_json::Value;
use session_journal::{Session, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// An open session shared by the stores over its values; clones share it.
#[derive(Clone)]
pub struct SessionValues {
    session: Arc<Mutex<Session>>,
    /// Set once the worker hands the session to a successor: the journal is the successor's from then on, so what
    /// this worker would still write is dropped.
    handed_over: Option<Arc<AtomicBool>>,
}

impl SessionValues {
    pub fn new(session: Session) -> Self {
        SessionValues {
            session: Arc::new(Mutex::new(session)),
            handed_over: None,
        }
    }

    /// Values of a session another owner holds open (the session actor's); clones share it.
    pub fn shared(session: Arc<Mutex<Session>>) -> Self {
        SessionValues {
            session,
            handed_over: Some(Arc::new(AtomicBool::new(false))),
        }
    }

    /// The worker hands the session to a successor: from now on every write through these values is dropped.
    pub fn hand_over(&self) {
        if let Some(flag) = &self.handed_over {
            flag.store(true, Ordering::SeqCst);
        }
    }

    fn handed_over(&self) -> bool {
        self.handed_over
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }

    pub fn session(&self) -> &Arc<Mutex<Session>> {
        &self.session
    }

    pub fn get(&self, namespace: &str, key: &str) -> Option<Value> {
        lock(&self.session).value(namespace, key).cloned()
    }

    /// One committed transaction setting the value.
    pub fn set(&self, namespace: &str, key: &str, value: &Value) -> Result<(), String> {
        if self.handed_over() {
            return Ok(());
        }
        lock(&self.session)
            .commit(&[Write::set(namespace, key, value.clone())])
            .map(drop)
    }

    /// One whole document at `namespace`/`key`.
    pub fn document(&self, namespace: &str, key: &str) -> SessionDocument {
        SessionDocument {
            values: self.clone(),
            namespace: namespace.into(),
            key: key.into(),
        }
    }

    /// Keyed documents under `namespace` (the local services' `ultron.local`).
    pub fn keyed(&self, namespace: &str) -> SessionKeyed {
        SessionKeyed {
            values: self.clone(),
            namespace: namespace.into(),
        }
    }
}

/// One session value read and replaced whole (a module store, the task journal, the usage ledger).
#[derive(Clone)]
pub struct SessionDocument {
    values: SessionValues,
    namespace: String,
    key: String,
}

impl DocumentStore for SessionDocument {
    fn read(&self) -> Result<Option<Value>, String> {
        Ok(self.values.get(&self.namespace, &self.key))
    }
    fn write(&self, document: &Value) -> Result<(), String> {
        self.values.set(&self.namespace, &self.key, document)
    }
}

impl delegation::task_store::TaskStore for SessionDocument {
    fn read(&mut self) -> Result<Option<Value>, String> {
        DocumentStore::read(self)
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        DocumentStore::write(self, document)
    }
}

/// Keyed session values of one namespace; a listing scans the keys with a prefix (`scanValues`).
#[derive(Clone)]
pub struct SessionKeyed {
    values: SessionValues,
    namespace: String,
}

impl KeyedDocuments for SessionKeyed {
    fn get(&self, key: &str) -> Result<Option<Value>, String> {
        Ok(self.values.get(&self.namespace, key))
    }
    fn set(&self, key: &str, value: &Value) -> Result<(), String> {
        self.values.set(&self.namespace, key, value)
    }
    fn list(&self, prefix: &str) -> Result<Vec<(String, Value)>, String> {
        Ok(lock(&self.values.session)
            .state()
            .scan_values(&self.namespace, prefix)
            .into_iter()
            .map(|stored| (stored.key.clone(), stored.value.clone()))
            .collect())
    }
}
