import type { AgentMessage, LaneQueuedItem, LaneWatchEvent } from "@earendil-works/pi-agent-core";
import type { AssistantMessageFrame } from "@earendil-works/pi-ai";

/** Pi RPC wire event, as emitted on stdout by `--mode rpc`. */
export type RpcWireEvent = { readonly type: string } & Record<string, unknown>;

/**
 * Translates native lane watch events into the Pi RPC event stream.
 *
 * The native transcript drops turn boundaries and carries streaming frames instead of
 * cumulative assistant events, so turns are reconstructed from message boundaries and
 * frames are reshaped into Pi's partial-free `assistantMessageEvent` form.
 */
export class RpcEventTranslator {
	#runMessages: AgentMessage[] = [];
	#turnOpen = false;
	#turnToolResults: AgentMessage[] = [];
	readonly #toolArgs = new Map<string, unknown>();

	translate(event: LaneWatchEvent): RpcWireEvent[] {
		switch (event.type) {
			case "run_start":
				this.#runMessages = [];
				this.#turnOpen = false;
				this.#turnToolResults = [];
				return [{ type: "agent_start" }];
			case "run_end": {
				const events: RpcWireEvent[] = this.#closeTurn();
				const messages = this.#runMessages;
				this.#runMessages = [];
				events.push({ type: "agent_end", messages, willRetry: false }, { type: "agent_settled" });
				return events;
			}
			case "message_start": {
				const events: RpcWireEvent[] = [];
				if (event.message.role === "assistant") {
					events.push(...this.#closeTurn());
					this.#turnOpen = true;
					events.push({ type: "turn_start" });
				}
				events.push({ type: "message_start", message: event.message });
				return events;
			}
			case "message_update": {
				if (event.frame === undefined || event.message.role !== "assistant") return [];
				const assistantMessageEvent = frameToAssistantMessageEvent(event.frame);
				if (assistantMessageEvent === undefined) return [];
				return [{ type: "message_update", usage: event.message.usage, assistantMessageEvent }];
			}
			case "message_end":
				if (event.runId !== undefined) this.#runMessages.push(event.message);
				if (event.message.role === "toolResult") this.#turnToolResults.push(event.message);
				return [{ type: "message_end", message: event.message }];
			case "tool_start":
				this.#toolArgs.set(event.toolCallId, event.args);
				return [
					{
						type: "tool_execution_start",
						toolCallId: event.toolCallId,
						toolName: event.toolName,
						args: event.args,
					},
				];
			case "tool_update":
				return [
					{
						type: "tool_execution_update",
						toolCallId: event.toolCallId,
						toolName: event.toolName,
						args: this.#toolArgs.get(event.toolCallId),
						partialResult: event.partialResult,
					},
				];
			case "tool_end":
				this.#toolArgs.delete(event.toolCallId);
				return [
					{
						type: "tool_execution_end",
						toolCallId: event.toolCallId,
						toolName: event.toolName,
						result: event.result,
						isError: event.isError,
					},
				];
			case "queue_update":
				return [queueUpdate(event.queues)];
			case "compaction_start":
				return [{ type: "compaction_start", reason: event.reason }];
			case "compaction_end":
				return [
					{
						type: "compaction_end",
						reason: event.reason,
						result: undefined,
						aborted: event.status === "aborted",
						willRetry: false,
						...(event.status === "failed" ? { errorMessage: event.error.message } : {}),
					},
				];
			case "retry_scheduled":
				return [
					{
						type: "auto_retry_start",
						attempt: event.attempt,
						maxAttempts: event.maxAttempts,
						delayMs: event.delayMs,
						errorMessage: event.errorMessage,
					},
				];
			case "retry_end":
				return [
					{
						type: "auto_retry_end",
						success: event.success,
						attempt: event.attempt,
						...(event.finalError === undefined ? {} : { finalError: event.finalError }),
					},
				];
			case "entry_added":
				return [{ type: "entry_appended", entry: event.entry }];
			case "config_update":
				return event.property === "thinkingLevel" ? [{ type: "thinking_level_changed", level: event.value }] : [];
			default:
				return [];
		}
	}

	#closeTurn(): RpcWireEvent[] {
		if (!this.#turnOpen) return [];
		this.#turnOpen = false;
		const lastAssistant = [...this.#runMessages].reverse().find((message) => message.role === "assistant");
		const toolResults = this.#turnToolResults;
		this.#turnToolResults = [];
		return [{ type: "turn_end", message: lastAssistant, toolResults }];
	}
}

export function queueUpdate(queues: readonly LaneQueuedItem[]): RpcWireEvent {
	const steering: string[] = [];
	const followUp: string[] = [];
	for (const item of queues) {
		if (item.type !== "message") continue;
		if (item.kind === "steer") steering.push(messageText(item.message));
		else if (item.kind === "followUp" || item.kind === "nextRun") followUp.push(messageText(item.message));
	}
	return { type: "queue_update", steering, followUp };
}

export function frameToAssistantMessageEvent(frame: AssistantMessageFrame): Record<string, unknown> | undefined {
	switch (frame.type) {
		case "start":
			return { type: "start" };
		case "text_start":
		case "thinking_start":
			return { type: frame.type, contentIndex: frame.contentIndex };
		case "text_delta":
		case "thinking_delta":
		case "toolcall_delta":
			return { type: frame.type, contentIndex: frame.contentIndex, delta: frame.delta };
		case "text_end":
		case "thinking_end":
			return { type: frame.type, contentIndex: frame.contentIndex, content: frame.content };
		case "toolcall_start":
			return {
				type: "toolcall_start",
				contentIndex: frame.contentIndex,
				id: frame.toolCall.id,
				toolName: frame.toolCall.name,
			};
		case "toolcall_end":
			return {
				type: "toolcall_end",
				contentIndex: frame.contentIndex,
				toolCall: {
					type: "toolCall",
					id: frame.id,
					name: frame.name,
					arguments: frame.arguments,
					...(frame.thoughtSignature === undefined ? {} : { thoughtSignature: frame.thoughtSignature }),
					...(frame.namespace === undefined ? {} : { namespace: frame.namespace }),
				},
			};
		default:
			// Checkpoints are a native recovery detail with no Pi RPC equivalent.
			return undefined;
	}
}

export function messageText(message: AgentMessage): string {
	if (!("content" in message)) return "";
	const content = message.content;
	if (typeof content === "string") return content;
	if (!Array.isArray(content)) return "";
	return content
		.filter((part): part is { type: "text"; text: string } => part.type === "text")
		.map((part) => part.text)
		.join("");
}
