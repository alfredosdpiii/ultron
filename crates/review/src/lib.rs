//! Code review (`/review`) and autoreview: the deterministic host logic behind the Python review API
//! (`packages/coding-agent/src/ultron/rlm/review_prompts.py`, `review_api.py`, `autoreview_*.py`), the review engine
//! (`autoreview_api.run` in a kernel of its own) and the GitHub side of `ultron autoreview` (`autoreview/*.ts`). See
//! `PLAN.md` for the split between Rust host operations and the thin Python API that remains in the kernel.

pub mod accounts;
pub mod args;
pub mod checkout;
pub mod chunks;
pub mod compiled;
pub mod compiled_eval;
pub mod compiled_hybrid;
pub mod context;
pub mod daemon;
pub mod deep;
pub mod deep_investigate;
pub mod deep_map;
pub mod deep_repo;
pub mod diff;
pub mod engine;
pub mod findings;
pub mod frames;
pub mod github;
pub mod js;
pub mod ledger;
pub mod levels;
pub mod local;
pub mod merge;
pub mod plan;
pub mod prepare;
pub mod prompt_text;
pub mod prompts;
pub mod py;
pub mod reference;
pub mod reference_facts;
pub mod reference_tf;
pub mod reference_ts;
pub mod reference_yaml;
pub mod report;
pub mod review_tests;
pub mod reviewer;
pub mod runner;
pub mod scope;
pub mod service;
pub mod settings;
pub mod slices;
pub mod state;
pub mod views;
