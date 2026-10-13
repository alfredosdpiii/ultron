//! Rust port of the interactive TUI of packages/coding-agent: the conversation components of
//! src/modes/interactive/components and the client TUI of src/experimental/client-tui*.ts (header, transcript,
//! editor, footer, slash commands, selectors) on the `tui` crate. It renders plain view data and talks to its
//! session through [`lane::LaneClient`]; it never links the engine.

pub mod agent_hub;
pub mod app;
pub mod changelog;
pub mod chat;
pub mod commands;
mod components;
pub mod copy_targets;
pub mod header;
pub mod history;
pub mod history_search;
pub mod lane;
pub mod model_hub;
pub mod qol;
pub mod retry;
pub mod rlm;
pub mod rlm_cell;
pub mod run;
pub mod selectors;
pub mod session_selector;
pub mod settings_selector;
pub mod transcript_picker;
pub mod tree_selector;
pub mod word_diff;

pub use components::*;
