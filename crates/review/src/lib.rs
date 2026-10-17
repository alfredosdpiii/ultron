//! Autoreview: the review engine (`autoreview_api.run`, the kernel Python, in a kernel of its own, its only model
//! calls `rlm.*` inference), its plan and verdict, and the GitHub side of `ultron autoreview` (`autoreview/*.ts`):
//! accounts, checkouts, the reviewer, state, the daemon loop and the service files. `/review` and the review
//! pipeline itself run as Python in the kernel; nothing here re-implements them.

pub mod accounts;
pub mod checkout;
pub mod daemon;
pub mod engine;
pub mod github;
pub mod js;
pub mod ledger;
pub mod local;
pub mod plan;
pub mod prepare;
pub mod prepare_env;
pub mod reviewer;
pub mod runner;
pub mod service;
pub mod settings;
pub mod state;
