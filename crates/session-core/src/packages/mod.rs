//! Package management (`core/package-manager.ts` and the helpers it uses): sources, npm and git installs, and the
//! resource resolution both `ultron config`/`install` (the CLI) and a session's resource loading use.

pub mod git_url;
pub mod glob;
pub mod ignore;
pub mod manager;
pub mod resources;
pub mod semver;
