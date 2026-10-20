//! Scheduled prompts on `main` (kernel_apis::prompt_schedule): the server's timer asks the actor to fire a schedule
//! ([`Op::ScheduleFire`](super::Op::ScheduleFire)); the run is recorded in the schedule's session value and its
//! prompt, a displayed custom message (`ultron.schedule`), starts a run. With a run in progress the prompt waits for
//! that run to end (it never interrupts it), ahead of any later input. A fire counts only when its prompt started or
//! was queued.

use super::{Core, Input, OpResult};
use serde_json::{Value, json};

impl Core {
    /// Fire schedule `id` for its slot `due`: `{fired, run, schedules}` (the session's active schedules after it).
    pub(super) fn schedule_fire(&mut self, id: &str, due: i64) -> OpResult {
        let schedules = self.lane.stack.prompt_schedules().clone();
        // The run counts only once its prompt was taken: a prompt that cannot start (no key, a journal error)
        // leaves the slot due and the error goes back to the server, which tries again.
        let fired = schedules.fire_with(id, due, &mut |message| self.schedule_start(message.clone()))?;
        let run = fired.as_ref().map(|f| f.schedule.runs);
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
    pub(super) fn schedule_start(&mut self, message: Value) -> Result<(), String> {
        if self.run.is_some() {
            self.after_run.push(Input::Scheduled(message));
            return Ok(());
        }
        self.prompt_with(message, None, true).map_err(|(error, _)| error)
    }
}
