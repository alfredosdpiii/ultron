//! Package management (`core/package-manager.ts` and the helpers it uses): sources, npm and git installs, resource
//! resolution for `ultron config`. The manager lives in session-core (a session resolves its settings' packages too);
//! the themes the client TUI registers stay here.

pub use session_core::packages::{git_url, glob, ignore, manager, resources, semver};
pub mod themes;
