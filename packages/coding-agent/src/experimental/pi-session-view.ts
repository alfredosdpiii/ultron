/**
 * Pi's read-side session views (tree, entries, context messages, stats) over a native Session tree that the
 * worker converted to Pi entries. The computations are Pi's own (`SessionManager`, `AgentSession.getSessionStats`),
 * so RPC clients get the shapes and numbers Pi would report for the same entries.
 */

import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { AssistantMessage } from "@earendil-works/pi-ai";
import type { SessionStats } from "../core/agent-session.ts";
import { calculateContextTokens, estimateProjectedContextTokens } from "../core/compaction/compaction.ts";
import type { ContextUsage } from "../core/extensions/types.ts";
import {
	CURRENT_SESSION_VERSION,
	getLatestCompactionEntry,
	type SessionEntry,
	type SessionHeader,
	SessionManager,
	type SessionTreeNode,
} from "../core/session-manager.ts";
import { addUsageToTotals, createUsageTotals } from "../core/usage-totals.ts";
import type { SessionTreeRead } from "./services/session-control.ts";

export class PiSessionView {
	readonly entries: SessionEntry[];
	readonly leafId: string | null;
	readonly sessionFile: string | undefined;
	readonly #labels: Readonly<Record<string, string>>;
	readonly #manager: SessionManager;

	constructor(read: SessionTreeRead, sessionId: string, cwd: string) {
		this.entries = read.entries as unknown as SessionEntry[];
		this.leafId = read.leafId;
		this.sessionFile = read.sessionFile ?? undefined;
		this.#labels = read.labels;
		const header: SessionHeader = {
			type: "session",
			version: CURRENT_SESSION_VERSION,
			id: sessionId,
			timestamp: new Date(0).toISOString(),
			cwd,
		};
		this.#manager = SessionManager.inMemory(cwd, undefined, [header, ...this.entries]);
		if (this.leafId === null) this.#manager.resetLeaf();
		else this.#manager.branch(this.leafId);
	}

	/** Pi's `getTree()`, with labels resolved from the native Session (the authoritative label store). */
	tree(): SessionTreeNode[] {
		const roots = this.#manager.getTree();
		const stack = [...roots];
		while (stack.length > 0) {
			const node = stack.pop()!;
			const label = this.#labels[node.entry.id];
			if (label === undefined) {
				delete node.label;
				delete node.labelTimestamp;
			} else if (node.label !== label) {
				node.label = label;
				delete node.labelTimestamp;
			}
			stack.push(...node.children);
		}
		return roots;
	}

	/** Pi's `session.messages`: the model context at the leaf. */
	messages(): AgentMessage[] {
		return this.#manager.buildSessionContext().messages;
	}

	/** Pi's `getLastAssistantText()`. */
	lastAssistantText(): string | undefined {
		const last = [...this.messages()]
			.reverse()
			.find(
				(message): message is AssistantMessage =>
					message.role === "assistant" && !(message.stopReason === "aborted" && message.content.length === 0),
			);
		if (last === undefined) return undefined;
		return last.content
			.filter((content) => content.type === "text")
			.map((content) => content.text)
			.join("");
	}

	/** Pi's `getSessionStats()`: totals over every entry, including compacted and abandoned history. */
	stats(sessionId: string, model: { readonly contextWindow?: number } | undefined): SessionStats {
		let userMessages = 0;
		let assistantMessages = 0;
		let toolResults = 0;
		let totalMessages = 0;
		let toolCalls = 0;
		const usageTotals = createUsageTotals();
		for (const entry of this.entries) {
			if (entry.type === "usage") {
				addUsageToTotals(usageTotals, entry.usage);
			} else if ((entry.type === "branch_summary" || entry.type === "compaction") && entry.usage) {
				addUsageToTotals(usageTotals, entry.usage);
			}
			if (entry.type !== "message") continue;
			totalMessages++;
			const message = entry.message;
			if (message.role === "user") {
				userMessages++;
			} else if (message.role === "toolResult") {
				toolResults++;
				if (message.usage) addUsageToTotals(usageTotals, message.usage);
			} else if (message.role === "assistant") {
				assistantMessages++;
				if (Array.isArray(message.content)) {
					toolCalls += message.content.filter((content) => content.type === "toolCall").length;
				}
				addUsageToTotals(usageTotals, message.usage);
			}
		}
		const contextUsage = this.#contextUsage(model?.contextWindow ?? 0);
		return {
			sessionFile: this.sessionFile,
			sessionId,
			userMessages,
			assistantMessages,
			toolCalls,
			toolResults,
			totalMessages,
			tokens: {
				input: usageTotals.input,
				output: usageTotals.output,
				cacheRead: usageTotals.cacheRead,
				cacheWrite: usageTotals.cacheWrite,
				total: usageTotals.input + usageTotals.output + usageTotals.cacheRead + usageTotals.cacheWrite,
			},
			cost: usageTotals.cost,
			...(contextUsage === undefined ? {} : { contextUsage }),
		};
	}

	/** Pi's `getContextUsage()`. */
	#contextUsage(contextWindow: number): ContextUsage | undefined {
		if (contextWindow <= 0) return undefined;
		const projection = this.#manager.buildSessionProjection();
		const branch = this.#manager.getBranch();
		const latestCompaction = getLatestCompactionEntry(branch);
		if (latestCompaction) {
			const projectedAssistants = new Set(
				projection.entries.flatMap((entry) =>
					entry.messages.some(
						(message) =>
							message.role === "assistant" &&
							message.stopReason !== "aborted" &&
							message.stopReason !== "error" &&
							calculateContextTokens(message.usage) > 0,
					)
						? [entry.sourceEntry.id]
						: [],
				),
			);
			const compactionIndex = branch.findIndex((entry) => entry.id === latestCompaction.id);
			const hasPostCompactionUsage = branch
				.slice(compactionIndex + 1)
				.some((entry) => projectedAssistants.has(entry.id));
			if (!hasPostCompactionUsage) return { tokens: null, contextWindow, percent: null };
		}
		const estimate = estimateProjectedContextTokens(projection, branch);
		return { tokens: estimate.tokens, contextWindow, percent: (estimate.tokens / contextWindow) * 100 };
	}
}
