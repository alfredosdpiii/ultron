//! The knowledge graph behind review and self-improvement: one SQLite store for every repository Ultron works in.
//!
//! The code layer records what each file is made of, keyed by git blob: constructs (a `BEFORE UPDATE` trigger, a
//! `@Controller` decorator, a call to `runWithBypass`) and the symbols it defines. A blob's constructs never change,
//! so indexing a new revision only reads the blobs the store has not seen, and reviews of different revisions of
//! one repository never disturb each other: each revision's tree is its own path-to-blob table.
//!
//! This crate only writes. The kernel reads the same file with Python's `sqlite3` (`rlm/knowledge.py`), read-only,
//! and refuses a schema version it does not know.

pub mod extract;
pub mod index;
pub mod store;

pub use index::{IndexStats, index_tree};
pub use store::{Note, SCHEMA_VERSION, Store};
