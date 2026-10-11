//! Host side of `rlm.spawn` / `rlm.collect` / `rlm.finish` / `rlm.merge` and background tasks, ported from Ultron's
//! `task-store.ts`, `kernel-pool.ts`, `worktrees.ts`, `workspace-snapshot.ts`, `verdict.ts` and the spawn/collect/
//! merge handling of `native-host.ts`; plus REPL lanes (`lanes.rs`), new versus TS: host-side waits that never hold
//! a cell, more than one lane per root, and detachable or cancellable cells.

pub mod child;
pub mod host;
pub mod kernel_pool;
pub mod lanes;
pub mod task_store;
pub mod verdict;
pub mod workspace;
pub mod worktrees;
