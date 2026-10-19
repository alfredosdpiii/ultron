//! Ultron's session persistence format: the Pi session JSONL written by the TypeScript
//! `SessionManager` (`packages/coding-agent/src/core/session-manager.ts`).
//!
//! A session file is one JSON value per line: a `session` header (`version` 3), then entries that
//! form a tree through `id`/`parentId`. Files from versions 1 and 2 are migrated and rewritten on
//! load. Lines are written exactly as `JSON.stringify` writes them, so files stay byte-compatible
//! with the TypeScript implementation in both directions.
//!
//! Entries are kept as JSON values rather than typed structs: unknown fields and entry types from
//! newer or older writers survive a load/rewrite unchanged.

mod context;
pub mod diff;
mod env;
mod file;
pub mod js;
mod manager;
pub mod paths;

pub use context::{
    Leaf, ProjectedEntry, SessionContext, SessionProjection, build_context_entries, build_session_context,
    build_session_projection, current_system_message, current_tools, entry_messages, latest_compaction,
};
pub use env::{Env, SystemEnv, Uuidv7};
pub use file::{
    CURRENT_SESSION_VERSION, HeaderScanError, MAX_SESSION_HEADER_SCAN_BYTES, SessionInfo, default_session_dir,
    default_session_dir_path, find_most_recent_session, find_session_by_id, list_all_sessions, list_sessions,
    load_entries_from_file, migrate_session_entries, migrate_sessions_from_agent_root, parse_session_entries,
    read_session_header,
};
pub use manager::{Error, NewSessionOptions, SessionManager, SessionTree, TreeNode, assert_valid_session_id};
