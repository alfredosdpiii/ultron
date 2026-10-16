//! Package management (`core/package-manager.ts` and the helpers it uses): sources, npm and git installs, resource
//! resolution for `ultron config`.

pub mod git_url;
pub mod glob;
pub mod ignore;
pub mod manager;
pub mod resources;
pub mod semver;
pub mod themes;
