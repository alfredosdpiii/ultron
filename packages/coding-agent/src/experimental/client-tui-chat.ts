import type { AgentMessage, Entry, LaneSnapshot } from "@ultron/agent-core";
import type { AssistantMessage } from "@ultron/ai";
import { type Component, Container, Spacer, Text, TruncatedText, type TUI } from "@ultron/tui";
import type { BashExecutionMessage } from "../core/messages.ts";
import { createAllToolRenderers } from "../core/tools/renderers/index.ts";
import type { TruncationResult } from "../core/tools/truncate.ts";
import { AssistantMessageComponent } from "../modes/interactive/components/assistant-message.ts";
import { BashExecutionComponent } from "../modes/interactive/components/bash-execution.ts";
import { keyDisplayText, keyText } from "../modes/interactive/components/keybinding-hints.ts";
import {
	RetryStatusIndicator,
	type StatusIndicator,
	WorkingStatusIndicator,
} from "../modes/interactive/components/status-indicator.ts";
import { ToolExecutionComponent, type ToolRenderers } from "../modes/interactive/components/tool-execution.ts";
import { UserMessageComponent } from "../modes/interactive/components/user-message.ts";
import { theme } from "../modes/interactive/theme/theme.ts";
import { rlmToolRenderers } from "./rlm-tool-renderer.ts";

export interface ChatViewOptions {
	/** Pi's `hideThinkingBlock` setting (Ctrl+T toggles it). */
	readonly hideThinkingBlock?: boolean;
	/** Pi's tool output expansion (Ctrl+O toggles it). */
	readonly toolsExpanded?: boolean;
}

interface Expandable extends Component {
	setExpanded(expanded: boolean): void;
}

const QUEUE_LABELS: Record<LaneSnapshot["queues"][number]["kind"], string> = {
	steer: "Steering",
	followUp: "Follow-up",
	nextRun: "Next run",
	write: "Pending",
};

function isExpandable(component: Component): component is Expandable {
	return "setExpanded" in component && typeof component.setExpanded === "function";
}

function userMessageText(message: AgentMessage): string {
	if (message.role !== "user") return "";
	if (typeof message.content === "string") return message.content;
	return message.content
		.filter((content) => content.type === "text")
		.map((content) => content.text)
		.join("");
}

/** Snapshot-driven transcript used by the service-only experimental presentation. */
export class ExperimentalChatView {
	static readonly #renderers: Record<string, ToolRenderers> = { ...createAllToolRenderers(), rlm: rlmToolRenderers };

	readonly transcript = new Container();
	readonly pendingMessages = new Container();
	readonly status = new Container();
	readonly #ui: TUI;
	readonly #cwd: string;
	readonly #tools = new Map<string, ToolExecutionComponent>();
	#renderedEntryIds: string[] = [];
	#streaming: AssistantMessageComponent | undefined;
	#indicator: StatusIndicator | undefined;
	/** What the status indicator shows (operation kind and retry attempt); empty when idle. */
	#working = "";
	#hideThinkingBlock: boolean;
	#toolsExpanded: boolean;
	/** Components that follow the tool expansion toggle (tool calls, `!` output), in transcript order. */
	#expandables: Expandable[] = [];

	constructor(ui: TUI, cwd: string, options: ChatViewOptions = {}) {
		this.#ui = ui;
		this.#cwd = cwd;
		this.#hideThinkingBlock = options.hideThinkingBlock ?? false;
		this.#toolsExpanded = options.toolsExpanded ?? false;
	}

	get toolsExpanded(): boolean {
		return this.#toolsExpanded;
	}

	/** Pi's Ctrl+O: expand or collapse every tool output and `!` command output. */
	setToolsExpanded(expanded: boolean): void {
		this.#toolsExpanded = expanded;
		for (const component of this.#expandables) component.setExpanded(expanded);
		this.transcript.invalidate();
	}

	/** Pi's Ctrl+T: show or hide thinking blocks in every assistant message. */
	setHideThinkingBlock(hide: boolean): void {
		this.#hideThinkingBlock = hide;
		for (const child of this.transcript.children) {
			if (child instanceof AssistantMessageComponent) child.setHideThinkingBlock(hide);
		}
		this.transcript.invalidate();
	}

	/** Append a client-side notice (command output such as `/session` or `/hotkeys`) after the transcript so far. */
	appendNotice(component: Component): void {
		this.transcript.addChild(new Spacer(1));
		this.transcript.addChild(component);
		if (isExpandable(component)) {
			component.setExpanded(this.#toolsExpanded);
			this.#expandables.push(component);
		}
		this.transcript.invalidate();
	}

	/** The text of the last assistant message with any text (Pi's `getLastAssistantText`). */
	static lastAssistantText(snapshot: LaneSnapshot): string | undefined {
		for (let index = snapshot.transcript.length - 1; index >= 0; index--) {
			const entry = snapshot.transcript[index]!;
			if (entry.type !== "message" || entry.message.role !== "assistant") continue;
			const message = entry.message;
			if (message.stopReason === "aborted" && message.content.length === 0) continue;
			return message.content
				.filter((content) => content.type === "text")
				.map((content) => content.text)
				.join("");
		}
		return undefined;
	}

	apply(snapshot: LaneSnapshot): void {
		this.#syncTranscript(snapshot.transcript);
		this.#syncStreaming(snapshot.operation?.streamingMessage);
		for (const tool of snapshot.operation?.runningTools ?? []) {
			const component = this.#tool(tool.toolName, tool.toolCallId, tool.args);
			if (tool.status === "running") {
				component.markExecutionStarted();
				if (tool.result !== undefined) component.updateResult({ ...tool.result, isError: false }, true);
			} else {
				component.updateResult({ ...tool.result, isError: tool.isError }, false);
			}
		}
		this.#syncQueues(snapshot.queues);
		this.#setWorking(snapshot.operation);
		this.transcript.invalidate();
		this.pendingMessages.invalidate();
		this.status.invalidate();
	}

	refreshTheme(snapshot: LaneSnapshot): void {
		this.#indicator?.dispose();
		this.#indicator = undefined;
		this.#working = "";
		this.transcript.clear();
		this.pendingMessages.clear();
		this.status.clear();
		this.#tools.clear();
		this.#expandables = [];
		this.#renderedEntryIds = [];
		this.#streaming = undefined;
		this.apply(snapshot);
	}

	dispose(): void {
		this.#indicator?.dispose();
	}

	/** Pi's pending-messages display: steering first, then follow-ups, then the dequeue hint. */
	#syncQueues(queues: LaneSnapshot["queues"]): void {
		this.pendingMessages.clear();
		if (queues.length === 0) return;
		this.pendingMessages.addChild(new Spacer(1));
		const ordered = [
			...queues.filter((item) => item.kind === "steer"),
			...queues.filter((item) => item.kind !== "steer"),
		];
		for (const item of ordered) {
			const text =
				item.type === "message" ? userMessageText(item.message).replace(/\s+/g, " ") : `<${item.customType}>`;
			const label = QUEUE_LABELS[item.kind];
			this.pendingMessages.addChild(new TruncatedText(theme.fg("dim", `${label}: ${text}`), 1, 0));
		}
		if (queues.some((item) => item.type === "message" && item.message.role === "user")) {
			const hint = `↳ ${keyDisplayText("app.message.dequeue")} to edit all queued messages`;
			this.pendingMessages.addChild(new TruncatedText(theme.fg("dim", hint), 1, 0));
		}
	}

	#syncTranscript(transcript: readonly Entry[]): void {
		const diverged = this.#renderedEntryIds.some((id, index) => transcript[index]?.id !== id);
		if (diverged) {
			this.transcript.clear();
			this.#tools.clear();
			this.#expandables = [];
			this.#renderedEntryIds = [];
			this.#streaming = undefined;
		}
		for (const entry of transcript.slice(this.#renderedEntryIds.length)) {
			this.#addEntry(entry);
			this.#renderedEntryIds.push(entry.id);
		}
	}

	#addEntry(entry: Entry): void {
		if (entry.type === "compaction") {
			this.#addText(theme.fg("muted", `[compaction] compacted from ${entry.tokensBefore} tokens`));
			for (const retained of entry.retainedTail) this.#addMessage(retained);
			return;
		}
		if (entry.type === "branch_summary") {
			this.#addText(theme.fg("muted", "[branch summary]"));
			this.#addText(entry.summary);
			return;
		}
		if (entry.type === "custom") {
			this.#addText(theme.fg("muted", `[${entry.customType}]`));
			return;
		}
		this.#addMessage(entry.message);
	}

	#addMessage(message: AgentMessage): void {
		if (message.role === "user") {
			this.transcript.addChild(new Spacer(1));
			this.transcript.addChild(new UserMessageComponent(userMessageText(message)));
			return;
		}
		if (message.role === "assistant") {
			const component = this.#streaming ?? new AssistantMessageComponent(undefined, this.#hideThinkingBlock);
			if (!this.#streaming) this.transcript.addChild(component);
			this.#streaming = undefined;
			component.updateContent(message, false);
			for (const content of message.content) {
				if (content.type === "toolCall") this.#tool(content.name, content.id, content.arguments).setArgsComplete();
			}
			return;
		}
		if (message.role === "toolResult") this.#tool(message.toolName, message.toolCallId).updateResult(message);
		if (message.role === "bashExecution") {
			this.#addBashExecution(message);
			return;
		}
		// Injected context (automatic memory) is shown muted, so what reached the model is visible.
		if (message.role === "custom" && message.display) {
			const text =
				typeof message.content === "string"
					? message.content
					: message.content.map((part) => (part.type === "text" ? part.text : "")).join("\n");
			this.#addText(theme.fg("muted", `[${message.customType}]\n${text}`));
		}
	}

	#syncStreaming(message: AssistantMessage | undefined): void {
		if (!message) return;
		if (!this.#streaming) {
			this.#streaming = new AssistantMessageComponent(undefined, this.#hideThinkingBlock);
			this.transcript.addChild(this.#streaming);
		}
		this.#streaming.updateContent(message, true);
		for (const content of message.content) {
			if (content.type === "toolCall") this.#tool(content.name, content.id, content.arguments);
		}
	}

	#tool(toolName: string, toolCallId: string, args?: unknown): ToolExecutionComponent {
		const existing = this.#tools.get(toolCallId);
		if (existing) {
			if (args !== undefined) existing.updateArgs(args);
			return existing;
		}
		const component = new ToolExecutionComponent(
			toolName,
			toolCallId,
			args ?? {},
			{},
			ExperimentalChatView.#renderers[toolName],
			this.#ui,
			this.#cwd,
		);
		component.setExpanded(this.#toolsExpanded);
		this.transcript.addChild(component);
		this.#tools.set(toolCallId, component);
		this.#expandables.push(component);
		return component;
	}

	/** A user `!` command recorded in the Session, drawn as Pi draws it. */
	#addBashExecution(message: BashExecutionMessage): void {
		const component = new BashExecutionComponent(message.command, this.#ui, message.excludeFromContext === true);
		if (message.output) component.appendOutput(message.output);
		component.setComplete(
			message.exitCode,
			message.cancelled,
			message.truncated ? ({ truncated: true, content: message.output } as TruncationResult) : undefined,
			message.fullOutputPath,
		);
		component.setExpanded(this.#toolsExpanded);
		this.transcript.addChild(component);
		this.#expandables.push(component);
	}

	#addText(text: string): void {
		this.transcript.addChild(new Spacer(1));
		this.transcript.addChild(new Text(text, 1, 0));
	}

	/** Pi's status indicators: working, compacting, summarizing a branch, or retrying with a countdown. */
	#setWorking(operation: LaneSnapshot["operation"]): void {
		const retry = operation?.retry;
		const key =
			operation === null
				? ""
				: `${operation.kind}:${retry === undefined ? "" : `${retry.attempt}/${retry.maxAttempts}`}`;
		if (key === this.#working) return;
		this.#working = key;
		this.#indicator?.dispose();
		this.#indicator = undefined;
		this.status.clear();
		if (operation === null) return;
		const cancel = `(${keyText("app.interrupt")} to ${operation.kind === "run" ? "abort" : "cancel"})`;
		if (retry !== undefined) {
			this.#indicator = new RetryStatusIndicator(
				this.#ui,
				retry.attempt,
				retry.maxAttempts,
				Math.max(0, retry.nextAttemptAt - Date.now()),
			);
		} else if (operation.kind === "compaction") {
			this.#indicator = new WorkingStatusIndicator(this.#ui, `Compacting context... ${cancel}`);
		} else if (operation.kind === "navigation") {
			this.#indicator = new WorkingStatusIndicator(this.#ui, `Navigating... ${cancel}`);
		} else {
			this.#indicator = new WorkingStatusIndicator(this.#ui, `Working... ${cancel}`);
		}
		this.status.addChild(this.#indicator);
	}
}
