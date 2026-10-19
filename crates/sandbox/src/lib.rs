//! Real isolation for the host's shell commands and guarantees for its file writes.
//!
//! - [`launcher`]: run a command under bubblewrap, rootless `unshare` namespaces or Docker (strongest that works),
//!   with the project and session dirs writable, the home read-only with credentials hidden, the environment cleared
//!   to an allowlist, configurable network; plus the review profile ported from autoreview's test launcher.
//! - [`doctor`]: verify that isolation holds by trying the escapes it must stop.
//! - [`approval`] and [`patterns`]: omp's approval tiers and bash critical-pattern guard, mapped onto host operations
//!   (the model's only tool is the Python REPL; the host decides each operation a cell reaches).
//! - [`refusal`]: the note a failed command's result gets when its output shows the sandbox refused it.
//! - [`settings`] reads the `sandbox.*` settings.

pub mod approval;
pub mod doctor;
pub mod launcher;
pub mod patterns;
pub mod refusal;
pub mod settings;

pub use approval::{ApprovalRequest, Approvals, Approver};
pub use launcher::{Launcher, Mechanism, Policy};
pub use settings::Settings;
