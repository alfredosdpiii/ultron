//! Bug-report payloads (`packages/coding-agent/src/core/bug-report.ts`: redaction, metadata, diagnostics, report files
//! and the zip archive) and the crash log the bug report reads and clears (`core/crash-log.ts`).

pub mod bug_report;
pub mod crash_log;
mod js;
pub mod zip;
