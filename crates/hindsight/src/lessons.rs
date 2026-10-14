//! `HindsightLessons` of `autoreview/lessons.ts`: autoreview's fates and rules retained in Hindsight, in the
//! "ultron-autoreview" bank on the same server the REPL's memory uses, tagged per repository, as tool evidence. One
//! retain per fate and per rule, each with a document id that replaces the previous version.

use crate::backend::{HindsightBackend, HindsightOptions};
use crate::transport::Transport;
use kernel::Signal;
use kernel_apis::MemoryError;
use kernel_apis::util::sha256_hex;
use review::ledger::{FATE_DAYS, Fate, FateKind, Lesson};
use serde_json::json;
use std::sync::Arc;

const BANK: &str = "ultron-autoreview";

fn repo_identity(repo: &str) -> String {
    sha256_hex(repo.to_lowercase().as_bytes())[..16].to_string()
}

pub struct HindsightLessons {
    backend: HindsightBackend,
    now: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl HindsightLessons {
    /// `new HindsightLessons({baseUrl, now})`: INVALID_CONFIG for a URL the client refuses.
    pub fn new(
        base_url: &str,
        now: Box<dyn Fn() -> i64 + Send + Sync>,
        transport: Option<Arc<dyn Transport>>,
    ) -> Result<Self, MemoryError> {
        let tags = json!({"project": ["ultron:autoreview:project:bank"]});
        let mut options = HindsightOptions::new(base_url, BANK, tags.as_object().cloned().unwrap_or_default());
        options.ensure_bank = true;
        options.transport = transport;
        Ok(Self {
            backend: HindsightBackend::new(options)?,
            now,
        })
    }

    /// Retains each fate and each rule of `repo`; stops at the first failure (the reviewer logs it, never fails).
    pub fn retain(&self, repo: &str, fates: &[Fate], lessons: &[Lesson]) -> Result<(), MemoryError> {
        let identity = repo_identity(repo);
        let tag = format!("ultron:autoreview:project:{identity}");
        let mut items: Vec<(String, String)> = fates
            .iter()
            .map(|fate| {
                (
                    format!("autoreview:{identity}:{}:{}", fate.pull, fate.id),
                    format!(
                        "In {repo}, a {} {} finding of the {} pass at {}:{} was {} ({}): {}",
                        fate.level,
                        fate.category,
                        fate.kind,
                        fate.file,
                        fate.line,
                        match fate.fate {
                            FateKind::Accepted => "accepted",
                            FateKind::Rejected => "rejected",
                        },
                        fate.how,
                        fate.claim
                    ),
                )
            })
            .collect();
        for lesson in lessons {
            let total = lesson.accepted + lesson.rejected;
            items.push((
                format!("autoreview:{identity}:lesson:{}:{}", lesson.category, lesson.kind),
                format!(
                    "In {repo}, {} findings of the {} pass were accepted {} of {total} times in the last {FATE_DAYS} days{}",
                    lesson.category,
                    lesson.kind,
                    lesson.accepted,
                    if lesson.lowered {
                        "; the reviewer now posts such findings as low, non-blocking notes."
                    } else {
                        "."
                    }
                ),
            ));
        }
        let signal = Signal::new();
        for (document_id, content) in items {
            let operation = sha256_hex(format!("{document_id}:{}", (self.now)()).as_bytes())[..32].to_string();
            let request = json!({
                "async": true,
                "operation_id": operation,
                "items": [{
                    "content": content,
                    "document_id": document_id,
                    "tags": [tag],
                    "observation_scopes": [[tag]],
                    "update_mode": "replace",
                    "metadata": {"ultron_operation": operation, "ultron_evidence_class": "tool_evidence"},
                }],
            });
            self.backend.retain_items(&request, &signal)?;
        }
        Ok(())
    }
}

/// The reviewer's lesson memory (`LessonMemory` of autoreview/reviewer.ts): a failed retain is logged, never fatal.
impl review::reviewer::LessonMemory for HindsightLessons {
    fn retain(&self, repo: &str, fates: &[Fate], lessons: &[Lesson]) -> Result<(), String> {
        HindsightLessons::retain(self, repo, fates, lessons).map_err(|error| error.to_string())
    }
}
