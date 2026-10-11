//! Deterministic parts of `/review` and autoreview, ported from the Python review API
//! (`packages/coding-agent/src/ultron/rlm/review_prompts.py`, `review_api.py`). See `PLAN.md` for the split between
//! Rust host operations and the thin Python API that remains in the kernel.

pub mod args;
pub mod diff;
pub mod findings;
pub mod js;
pub mod ledger;
pub mod levels;
pub mod plan;
pub mod prepare;
pub mod prompt_text;
pub mod prompts;
pub mod py;
pub mod settings;
