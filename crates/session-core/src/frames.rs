//! Bounded-inference frames in the session (inference.ts `createSessionFrameStore`, and the `frames` executor and
//! `NativeHostApi` the TS host hands its `InferenceRuntime`): every `rlm.infer` / `rlm.map` frame is an
//! `rlm-frame@1` task of the session's typed-agent host (journaled, admitted under its caller, cancelled with the
//! calling cell, its requests reserved and settled in the usage ledger), its trace is a session value
//! (`ultron.rlm.frames/<id>`, with the index at `ultron.rlm.frames/index`), and a frame with depth runs its `rlm`
//! cell on the task's lane.

use crate::compose::Stack;
use crate::durable_store::{SharedSession, lock};
use delegation::agents::{Agents, FrameRun, FrameRunner};
use delegation::usage::{Measurement, Reservation, ReserveRequest, UsageLedger};
use inference::{FrameExecution, FrameSession, FrameTasks, TRACE_NAMESPACE, TraceStore};
use kernel::Signal;
use serde_json::{Value, json};
use session_journal::Write;
use std::sync::{Arc, Weak};

/// Frame traces as values of the session (`createSessionFrameStore`).
pub struct SessionTraceStore(pub SharedSession);

impl TraceStore for SessionTraceStore {
    fn read(&self, key: &str) -> Option<Value> {
        lock(&self.0).value(TRACE_NAMESPACE, key).cloned()
    }

    fn write(&self, key: &str, document: &Value) -> Result<(), String> {
        lock(&self.0)
            .commit(&[Write::set(TRACE_NAMESPACE, key, document.clone())])
            .map(drop)
    }
}

/// The typed-agent host's runner of `rlm-frame@1` tasks: the session's frames run them.
pub fn frame_runner(frames: Arc<FrameSession>) -> FrameRunner {
    Arc::new(move |run: FrameRun| {
        frames.execute(FrameExecution {
            task_id: run.task_id,
            input: run.input,
            lane: run.lane,
            usage_root: run.usage_root,
            deadline_at: run.deadline_at,
            timeout_ms: run.timeout_ms,
            signal: run.signal,
        })
    })
}

/// Frames' task host: the session's typed agents and usage ledger, and the stack's lanes for frames with depth.
pub struct StackFrameTasks {
    pub agents: Agents,
    pub usage: UsageLedger,
    pub stack: Weak<Stack>,
}

impl FrameTasks for StackFrameTasks {
    fn caller_task(&self, lane: &str) -> Option<String> {
        self.agents.caller_task(lane)
    }

    fn spawn(&self, lane: &str, input: Value, model: Option<&str>, timeout_ms: u64) -> Result<String, String> {
        let mut payload = json!({"definition": "rlm-frame@1", "input": input, "timeout_ms": timeout_ms});
        if let Some(model) = model {
            payload["model"] = json!(model);
        }
        self.agents.spawn_frame(lane, payload)
    }

    fn result(&self, lane: &str, task_id: &str, signal: &Signal) -> Value {
        let ask = |cell: Option<&Signal>| self.agents.handle("agents.result", json!({"id": task_id}), lane, cell);
        match ask(Some(signal)) {
            Ok(result) => result,
            Err(_) if signal.is_aborted() => {
                // The frame ends with its cell: cancel the task, then report its (cancelled) result.
                let _ = self
                    .agents
                    .cancel_task(task_id, "Inference cancelled: the calling cell was aborted");
                ask(None).unwrap_or_else(|error| json!({"status": "cancelled", "error": error}))
            }
            Err(error) => json!({"status": "failed", "error": error}),
        }
    }

    fn reserve(&self, request: &Value) -> Result<Option<Value>, String> {
        let text = |key: &str| request[key].as_str().map(str::to_string);
        let reservation = self.usage.reserve(&ReserveRequest {
            kind: "model".into(),
            root_id: text("rootId"),
            task_id: text("taskId"),
            parent_task_id: text("parentTaskId"),
            request_key: text("requestKey"),
            budget_id: text("budgetId"),
            timeout_ms: request["timeoutMs"].as_f64(),
            deadline_at: request["deadlineAt"].as_f64(),
            ..ReserveRequest::default()
        })?;
        Ok(Some(reservation.0))
    }

    fn settle(&self, reservation: &Value, status: &str, usage: Option<&Value>) {
        let measured = usage.map(|usage| Measurement {
            input_tokens: usage["inputTokens"].as_f64(),
            output_tokens: usage["outputTokens"].as_f64(),
            total_tokens: usage["totalTokens"].as_f64(),
            cost: usage["cost"].as_f64(),
            ..Measurement::default()
        });
        let _ = self
            .usage
            .settle(&Reservation(reservation.clone()), Some(status), measured.as_ref());
    }

    fn code_tool(&self, lane: &str) -> Result<Arc<dyn agent::AgentTool>, String> {
        let stack = self.stack.upgrade().ok_or("The session is closed")?;
        Ok(Arc::new(stack.rlm(lane)?))
    }

    fn close_lane(&self, lane: &str) {
        if let Some(stack) = self.stack.upgrade() {
            stack.context_reminders.set_frame_lane(lane, None);
            let _ = stack.lanes.close_lane(lane);
        }
    }

    /// A frame's cells are reminded as its conversation nears the frame model's window (it never compacts).
    fn frame_context(&self, lane: &str, model: &ai::Model, meter: inference::FrameMeter) {
        if let Some(stack) = self.stack.upgrade() {
            let frame = crate::context_reminder::lane_limit(model, false, 0).map(|limit| (limit, meter));
            stack.context_reminders.set_frame_lane(lane, frame);
        }
    }
}
