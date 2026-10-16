//! Settings, credentials, project trust and agent paths for the Rust product.
//!
//! Every file this crate reads or writes (`settings.json`, `auth.json`, `models-store.json`, `trust.json`) keeps the byte format the
//! TypeScript product writes (`JSON.stringify(value, null, 2)`) and takes the same `proper-lockfile` directory
//! locks, so both implementations can share one agent directory.

pub mod auth;
pub mod json;
pub mod lock;
pub mod models_store;
pub mod paths;
pub mod settings;
pub mod settings_keys;
pub mod trust;
