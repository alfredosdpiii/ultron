import type { AgentMessage } from "../../types.ts";
import type { Context } from "../context.ts";
import { createBranchSummaryMessage, createCompactionSummaryMessage } from "../messages.ts";
import type { CompactionEntry, Entry, EntryProjector, JsonValue } from "./types.ts";

export interface SessionContextBuildOptions {
	entryProjectors?: Readonly<Record<string, EntryProjector>>;
}

export function buildContextEntries(pathEntries: readonly Entry[]): Entry[] {
	let compaction: CompactionEntry | undefined;
	let compactionIndex = -1;
	for (let index = pathEntries.length - 1; index >= 0; index--) {
		const entry = pathEntries[index];
		if (entry?.type === "compaction") {
			compaction = entry;
			compactionIndex = index;
			break;
		}
	}
	return compaction === undefined ? [...pathEntries] : [compaction, ...pathEntries.slice(compactionIndex + 1)];
}

function isContextMessage(message: AgentMessage): boolean {
	return (
		message.role !== "assistant" ||
		(message.stopReason !== "error" && message.stopReason !== "aborted" && message.stopReason !== "deferred")
	);
}

export function sessionEntryToContextMessages(entry: Entry): AgentMessage[] {
	switch (entry.type) {
		case "message":
			return isContextMessage(entry.message) ? [entry.message] : [];
		case "compaction":
			return [
				createCompactionSummaryMessage(entry.summary, entry.tokensBefore, entry.timestamp),
				...entry.retainedTail.filter(isContextMessage),
			];
		case "branch_summary":
			return entry.summary ? [createBranchSummaryMessage(entry.summary, entry.fromId, entry.timestamp)] : [];
		case "custom":
			return [];
	}
}

/**
 * customType of a context edit: an append-only change to what earlier entries contribute to model context, as
 * Pi's `context_edit` entry. The transcript keeps every entry; only the model's view changes, and only on the
 * branch that carries the edit. Data: `{ edits: [{ targetId, replacement }] }` (or one `{ targetId, replacement }`),
 * where a null replacement omits the target and `{ content }` replaces only its content. Later edits win.
 */
export const CONTEXT_EDIT_CUSTOM_TYPE = "context_edit";
/** customType standing in for an entry a context edit omitted; it contributes nothing to context. */
export const CONTEXT_OMITTED_CUSTOM_TYPE = "context_edit:omitted";
/** Text of a tool result removed from context while its call stays visible (providers need the pair). */
export const CONTEXT_OMITTED_TOOL_RESULT_TEXT = "[Tool result removed from context.]";

export type ContextEditReplacement = { content: string | JsonValue[] } | null;

export interface ContextEdit {
	targetId: string;
	replacement: ContextEditReplacement;
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return value !== null && typeof value === "object" && !Array.isArray(value);
}

function parseContextEdit(value: unknown): ContextEdit | undefined {
	if (!isRecord(value) || typeof value.targetId !== "string" || !value.targetId) return undefined;
	const replacement = value.replacement;
	if (replacement === null) return { targetId: value.targetId, replacement: null };
	if (!isRecord(replacement)) return undefined;
	const content = replacement.content;
	if (typeof content !== "string" && !Array.isArray(content)) return undefined;
	return { targetId: value.targetId, replacement: { content: content as string | JsonValue[] } };
}

/** The edits one entry carries; none for any other entry or malformed data. */
export function contextEditsOf(entry: Entry): ContextEdit[] {
	if (entry.type !== "custom" || entry.customType !== CONTEXT_EDIT_CUSTOM_TYPE || !isRecord(entry.data)) return [];
	const list = Array.isArray(entry.data.edits) ? entry.data.edits : [entry.data];
	return list.flatMap((item) => parseContextEdit(item) ?? []);
}

function replaceContent(message: AgentMessage, content: string | JsonValue[]): AgentMessage {
	if (
		message.role !== "user" &&
		message.role !== "assistant" &&
		message.role !== "toolResult" &&
		message.role !== "custom"
	) {
		return message;
	}
	const next =
		(message.role === "assistant" || message.role === "toolResult") && typeof content === "string"
			? [{ type: "text" as const, text: content }]
			: content;
	return { ...message, content: next } as AgentMessage;
}

function toolCallIds(content: unknown): string[] {
	if (!Array.isArray(content)) return [];
	return content.flatMap((part) =>
		isRecord(part) && part.type === "toolCall" && typeof part.id === "string" ? [part.id] : [],
	);
}

function omittedEntry(entry: Entry): Entry {
	return {
		id: entry.id,
		parentId: entry.parentId,
		seq: entry.seq,
		timestamp: entry.timestamp,
		type: "custom",
		customType: CONTEXT_OMITTED_CUSTOM_TYPE,
	};
}

/**
 * Apply the context edits found among `entries` to the entries before them. Tool calls and results stay paired:
 * results of an omitted assistant message (or of calls a replacement dropped) are omitted with it, and an omitted
 * result whose call is still visible becomes a stub result. Entries are returned unchanged when there are no edits.
 */
export function applyContextEdits(entries: readonly Entry[]): Entry[] {
	const edits = new Map<string, ContextEditReplacement>();
	for (const entry of entries) for (const edit of contextEditsOf(entry)) edits.set(edit.targetId, edit.replacement);
	if (edits.size === 0) return [...entries];
	const hiddenCalls = new Set<string>();
	for (const entry of entries) {
		if (entry.type !== "message" || entry.message.role !== "assistant" || !edits.has(entry.id)) continue;
		const replacement = edits.get(entry.id);
		const kept = new Set(replacement ? toolCallIds(replacement.content) : []);
		for (const id of toolCallIds(entry.message.content)) if (!kept.has(id)) hiddenCalls.add(id);
	}
	return entries.map((entry) => {
		if (entry.type === "custom") return edits.get(entry.id) === null ? omittedEntry(entry) : entry;
		if (entry.type !== "message") return entry;
		const message = entry.message;
		if (message.role === "toolResult" && hiddenCalls.has(message.toolCallId)) return omittedEntry(entry);
		if (!edits.has(entry.id)) return entry;
		const replacement = edits.get(entry.id)!;
		if (replacement === null) {
			return message.role === "toolResult"
				? { ...entry, message: replaceContent(message, CONTEXT_OMITTED_TOOL_RESULT_TEXT) }
				: omittedEntry(entry);
		}
		return { ...entry, message: replaceContent(message, replacement.content) };
	});
}

export async function buildSessionContext(
	pathEntries: readonly Entry[],
	options: SessionContextBuildOptions | undefined,
	context: Context,
): Promise<AgentMessage[]> {
	options ??= {};
	const entries = applyContextEdits(buildContextEntries(pathEntries));
	const messages: AgentMessage[] = [];
	for (const entry of entries) {
		if (entry.type !== "custom") {
			messages.push(...sessionEntryToContextMessages(entry));
			continue;
		}
		const projector = options.entryProjectors?.[entry.customType];
		if (projector !== undefined) messages.push(...((await projector(entry, context)) ?? []));
	}
	return messages;
}
