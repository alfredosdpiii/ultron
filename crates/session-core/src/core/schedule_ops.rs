//! Scheduled prompts on `main` (kernel_apis::prompt_schedule): the server's timer asks the actor to fire a schedule
//! ([`Op::ScheduleFire`](super::Op::ScheduleFire)); the run is recorded in the schedule's session value and its
//! prompt, a displayed custom message (`ultron.schedule`), starts a run. With a run in progress the prompt waits for
//! that run to end (it never interrupts it), ahead of any later input.

use super::{Core, Input, OpResult};
use serde_json::{Value, json};

impl Core {
    /// Fire schedule `id` for its slot `due`: `{fired, run, schedules}` (the session's active schedules after it).
    pub(super) fn schedule_fire(&mut self, id: &str, due: i64) -> OpResult {
        let schedules = self.lane.stack.prompt_schedules().clone();
        let fired = schedules.fire(id, due)?;
        let run = fired.as_ref().map(|f| f.schedule.runs);
        if let Some(fired) = fired {
            self.schedule_start(fired.message);
        }
        Ok(Some(
            json!({"fired": run.is_some(), "run": run, "schedules": schedules.entries()?}),
        ))
    }

    /// Cancel the active schedule `id` (or name): the schedule as cancelled.
    pub(super) fn schedule_cancel(&mut self, id: &str) -> OpResult {
        let schedules = self.lane.stack.prompt_schedules().clone();
        let cancelled = schedules.cancel(id)?;
        Ok(Some(
            json!({"schedule": cancelled.view(), "schedules": schedules.entries()?}),
        ))
    }

    /// Start a run of a fired schedule's prompt, or keep it for the end of the run in progress.
    pub(super) fn schedule_start(&mut self, message: Value) {
        if self.run.is_some() {
            self.after_run.push(Input::Scheduled(message));
            return;
        }
        // A prompt that cannot start (no key, a journal error) is dropped: its run is already counted, and the next
        // slot fires as usual.
        let _ = self.prompt_with(message, None, true);
    }
}
