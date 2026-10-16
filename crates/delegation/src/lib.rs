//! Host side of `rlm.spawn` / `rlm.collect` / `rlm.finish` / `rlm.merge` and background tasks, ported from Ultron's
//! `task-store.ts`, `kernel-pool.ts`, `worktrees.ts`, `workspace-snapshot.ts`, `verdict.ts` and the spawn/collect/
//! merge handling of `native-host.ts`; plus REPL lanes (`lanes.rs`), new versus TS: host-side waits that never hold
//! a cell, more than one lane per root, and detachable or cancellable cells.

pub mod agents;
pub mod child;
pub mod contract;
pub mod definitions;
pub mod fork;
pub mod host;
pub mod kernel_pool;
pub mod lanes;
pub mod reconcile;
pub mod schema;
pub mod task_store;
pub mod usage;
pub mod verdict;
pub mod workflow_worktrees;
pub mod workspace;
pub mod worktrees;
