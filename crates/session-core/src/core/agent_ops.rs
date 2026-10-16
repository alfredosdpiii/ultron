//! The agent hub's operations on the session actor (crate::agent_hub): steer, revive and read a subagent.

use super::*;
use crate::agent_hub::{child_conversation, child_task};
use crate::subagents::LANE_PREFIX;

impl Core {
    /// [`Op::SteerAgent`]: the root lane's `agent_message.send` to its child `id`, delivered by steering.
    pub(super) fn steer_agent(&mut self, id: &str, message: &str) -> Result<Value, String> {
        let mut payload = Map::new();
        payload.insert("message".into(), message.into());
        payload.insert("receiver_role".into(), "child".into());
        payload.insert("receiver_id".into(), id.into());
        payload.insert("steer".into(), true.into());
        self.lane
            .stack
            .cell_handler(LANE, None)
            .handle("agent_message.send", payload, &kernel::Signal::new())
    }

    /// [`Op::ReviveAgent`]: a subagent its worker's death left `interrupted` runs again from its brief, as a new
    /// subagent of the root (`rlm.spawn` with the same prompt, model, thinking level and timeout).
    pub(super) fn revive_agent(&mut self, id: &str) -> Result<Value, String> {
        let delegation = self.lane.stack.delegation.clone();
        let record = delegation
            .tasks()?
            .into_iter()
            .find(|task| task["id"].as_str() == Some(id))
            .ok_or_else(|| format!("Unknown subagent {id}"))?;
        let state = record["state"].as_str().unwrap_or_default();
        if state != "interrupted" {
            return Err(format!(
                "Subagent {id} is {state}: only a subagent its worker left interrupted can be revived"
            ));
        }
        let (_, input, _) = child_task(&self.durable.chat, &format!("{LANE_PREFIX}{id}"))
            .ok_or_else(|| format!("Subagent {id} left no brief to revive it from"))?;
        if input.cwd.is_some() {
            return Err(format!(
                "Subagent {id} worked in its own worktree: start a new one instead"
            ));
        }
        // The revived subagent keeps the name it was spawned with (a session older than that record: named after it).
        let name = crate::subagents::subagent_name(&self.session, id).unwrap_or_else(|| format!("revived-{id}"));
        let mut kwargs = Map::new();
        kwargs.insert("name".into(), name.into());
        if let Some(model) = &input.model {
            kwargs.insert("model".into(), format!("{}/{}", model.provider, model.model_id).into());
        }
        if let Some(thinking) = &input.thinking {
            kwargs.insert("thinking".into(), thinking.clone().into());
        }
        if let Some(timeout) = input.timeout_ms {
            kwargs.insert("timeout_ms".into(), timeout.into());
        }
        let spawned = delegation.handle("rlm.spawn", json!({"prompt": input.prompt, "kwargs": kwargs}), LANE)?;
        Ok(json!({"id": spawned["rlm_child_id"], "revived": id, "name": spawned["name"]}))
    }

    /// [`Op::AgentTranscript`]: the messages of subagent `id`'s child conversation, and whether it runs.
    pub(super) fn agent_transcript(&mut self, id: &str) -> Result<Value, String> {
        let chat = &self.durable.chat;
        let missing = || format!("No transcript for agent {id}");
        let (task, _, state) = child_task(chat, &format!("{LANE_PREFIX}{id}")).ok_or_else(missing)?;
        let conversation = child_conversation(chat, task, &state)
            .and_then(|conversation| chat.conversation(conversation).ok().flatten())
            .ok_or_else(missing)?;
        let messages: Vec<Value> = conversation
            .entries()
            .map_err(|e| e.to_string())?
            .iter()
            .filter_map(|entry| entry.model.as_ref()?.first().cloned())
            .filter(|message| matches!(message["role"].as_str(), Some("user" | "assistant" | "toolResult")))
            .collect();
        let live = chat.live(conversation.id).map_err(|e| e.to_string())?;
        Ok(json!({"id": id, "busy": !live["run"].is_null(), "messages": messages}))
    }
}
