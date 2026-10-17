//! Ultron's local services, served to the Python kernel as host operations (RLM-first: the model's only tool stays
//! `rlm`; these are kernel APIs reached through `refinements.*`, `gates.*` and `rlm.host_request`):
//!
//! - [`local`]: refinements (lessons, instruction / skill / agent changes) with proposal, approval, activation,
//!   rejection and rollback over one event ledger anchored to the conversation branch, and the experiment history
//!   (`ultron/local-services.ts`);
//! - [`validation`]: the worker's activation checks for skill proposals and instructions
//!   (`ultron/refinement-validation.ts`);
//! - [`gates`]: release gates, frozen definitions and recorded baseline-versus-candidate comparisons
//!   (`ultron/release-gate.ts`);
//! - [`session_values`]: these documents (and the task journal, definitions, usage ledger and memory journal) as
//!   values of the session, so resume and fork restore matching state.
//!
//! The host enforces the guarantees (protected targets, capability non-escalation, evidence, versions, frozen gate
//! definitions, append-only history); nothing here prompts the user or calls a model.

pub mod gates;
pub mod local;
pub mod session_values;
pub mod validation;

pub use gates::ReleaseGateModule;
pub use local::{
    Branch, BranchSource, BranchedLocalServices, LocalOptions, LocalServices, LocalServicesModule, instruction_hook,
    instruction_hook_on,
};
pub use session_values::{SessionDocument, SessionKeyed, SessionValues};
pub use validation::{validate_refinement_content, worker_validator};
