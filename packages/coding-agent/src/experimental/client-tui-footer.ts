/**
 * Pi's footer (cwd, git branch, session name, token and cost totals, context-window use, model and thinking
 * level) for the native TUI, fed from the replicated lane snapshot instead of an in-process `AgentSession`.
 *
 * `FooterComponent` reads a handful of `AgentSession` members; this module supplies exactly those from the
 * snapshot and the Models catalog, so the footer is Pi's own rendering, not a copy.
 */

import type { AgentMessage, Entry, LaneSnapshot, ThinkingLevel } from "@ultron/agent-core";
import type { Api, Model } from "@ultron/ai";
import type { Component } from "@ultron/tui";
import type { AgentSession } from "../core/agent-session.ts";
import { calculateContextTokens, estimateTokens } from "../core/compaction/compaction.ts";
import type { ContextUsage } from "../core/extensions/types.ts";
import { FooterDataProvider } from "../core/footer-data-provider.ts";
import { FooterComponent } from "../modes/interactive/components/footer.ts";
import type { ModelsState } from "./services/models.ts";

export interface NativeFooterSource {
	snapshot(): LaneSnapshot | undefined;
	models(): ModelsState | undefined;
	sessionName(): string | undefined;
}

/** Pi's footer over the native lane. Dispose it to stop the git branch watcher. */
export class NativeFooter implements Component {
	readonly #data: FooterDataProvider;
	readonly #footer: FooterComponent;
	readonly #source: NativeFooterSource;
	readonly #cwd: string;

	constructor(cwd: string, source: NativeFooterSource, onBranchChange?: () => void) {
		this.#cwd = cwd;
		this.#source = source;
		this.#data = new FooterDataProvider(cwd);
		if (onBranchChange) this.#data.onBranchChange(onBranchChange);
		this.#footer = new FooterComponent(this.#sessionShim(), this.#data);
	}

	/** Extension statuses (`ctx.ui.setStatus`) shown on the footer's third line, as in Pi. */
	setExtensionStatus(key: string, text: string | undefined): void {
		this.#data.setExtensionStatus(key, text);
	}

	setAutoCompactEnabled(enabled: boolean): void {
		this.#footer.setAutoCompactEnabled(enabled);
	}

	render(width: number): string[] {
		const models = this.#source.models();
		const providers = new Set(models?.catalog.availableModels.map((model) => model.provider) ?? []);
		this.#data.setAvailableProviderCount(providers.size);
		return this.#footer.render(width);
	}

	invalidate(): void {
		this.#footer.invalidate();
	}

	dispose(): void {
		this.#data.dispose();
	}

	/** The `AgentSession` members `FooterComponent` reads, answered from the lane snapshot. */
	#sessionShim(): AgentSession {
		const source = this.#source;
		const cwd = this.#cwd;
		const currentModel = (): Model<Api> | undefined => {
			const snapshot = source.snapshot();
			const models = source.models();
			const ref = models?.configuration.model ?? snapshot?.configuration.model;
			if (ref === undefined || ref === null) return undefined;
			const summary = models?.catalog.availableModels.find(
				(model) => model.provider === ref.provider && model.modelId === ref.modelId,
			);
			return (
				summary?.model ??
				({
					id: ref.modelId,
					name: summary?.name ?? ref.modelId,
					provider: ref.provider,
					reasoning: summary?.reasoning ?? false,
				} as Model<Api>)
			);
		};
		const shim = {
			get state() {
				const snapshot = source.snapshot();
				return {
					model: currentModel(),
					thinkingLevel: (source.models()?.configuration.thinkingLevel ??
						snapshot?.configuration.thinkingLevel ??
						"off") as ThinkingLevel,
				};
			},
			sessionManager: {
				getEntries: () => footerEntries(source.snapshot()?.transcript ?? []),
				getCwd: () => cwd,
				getSessionName: () => source.sessionName(),
			},
			getContextUsage: () => laneContextUsage(source.snapshot()?.transcript ?? [], currentModel()?.contextWindow),
			modelRuntime: { isUsingSubscription: () => false },
		};
		return shim as unknown as AgentSession;
	}
}

/** Lane entries in the shape Pi's footer totals usage over. */
function footerEntries(transcript: readonly Entry[]): unknown[] {
	return transcript.map((entry) => {
		if (entry.type === "message") return { type: "message", message: entry.message };
		if (entry.type === "compaction" || entry.type === "branch_summary") {
			return { type: entry.type, ...(entry.usage === undefined ? {} : { usage: entry.usage }) };
		}
		return { type: "custom" };
	});
}

/**
 * Pi's `getContextUsage()` over the lane: the last valid assistant usage plus an estimate for the messages after
 * it. Unknown (`percent: null`) after a compaction until the next response.
 */
export function laneContextUsage(
	transcript: readonly Entry[],
	contextWindow: number | undefined,
): ContextUsage | undefined {
	if (contextWindow === undefined || contextWindow <= 0) return undefined;
	let usageIndex = -1;
	let tokens = 0;
	for (let index = transcript.length - 1; index >= 0; index--) {
		const entry = transcript[index]!;
		if (entry.type === "compaction") return { tokens: null, contextWindow, percent: null };
		if (entry.type !== "message") continue;
		const message = entry.message;
		if (
			message.role === "assistant" &&
			message.stopReason !== "aborted" &&
			message.stopReason !== "error" &&
			calculateContextTokens(message.usage) > 0
		) {
			usageIndex = index;
			tokens = calculateContextTokens(message.usage);
			break;
		}
	}
	if (usageIndex === -1) {
		const messages = transcript.flatMap((entry) => (entry.type === "message" ? [entry.message] : []));
		if (messages.length === 0) return { tokens: 0, contextWindow, percent: 0 };
		tokens = sumEstimates(messages);
	} else {
		tokens += sumEstimates(
			transcript.slice(usageIndex + 1).flatMap((entry) => (entry.type === "message" ? [entry.message] : [])),
		);
	}
	return { tokens, contextWindow, percent: (tokens / contextWindow) * 100 };
}

function sumEstimates(messages: readonly AgentMessage[]): number {
	let total = 0;
	for (const message of messages) total += estimateTokens(message);
	return total;
}
