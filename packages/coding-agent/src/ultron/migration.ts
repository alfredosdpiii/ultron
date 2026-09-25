/**
 * Explicit, repeatable migration between Pi and Ultron.
 *
 * - `importPiSession` copies a whole Pi session tree into a new native Session.
 * - `exportNativeSessionToPi` writes a native Session's whole tree back as a Pi session (rollback).
 * - `backupProfile` / `restoreProfile` snapshot and restore the Ultron config profile (never sessions).
 *
 * Nothing here reads Pi's data directory implicitly: every source path is supplied by the caller.
 *
 * Pi entry mapping (Pi entry ids are kept as native entry ids, parents are unchanged):
 * - `message`, `compaction`, `branch_summary`, `custom` -> the native entry of the same type.
 * - `custom_message` -> a native `message` entry with a `custom` role message.
 * - Every other Pi entry type (`model_change`, `thinking_level_change`, `label`, `session_info`, `usage`,
 *   `context_edit`, and types added later) -> a native `custom` entry whose customType is
 *   `pi-session:<type>` and whose data is the Pi entry without `type`/`id`/`parentId`/`timestamp`.
 * - Derived native state at import end: the `main` branch tip is the Pi leaf, label values are Pi's
 *   resolved labels, the session name is Pi's name, and the `main` lane configuration carries the
 *   model and thinking level Pi resolves at the leaf.
 * - A `context_edit` whose target reaches only leaves that pass through the edit is baked into the native
 *   target (and into compaction tails that keep it): a replacement changes the message content, an
 *   omission turns the message into a `pi-import:omitted-message` custom entry. Edits that would also
 *   change sibling branches cannot be baked and are reported in `unappliedContextEdits`.
 * - When the native entry cannot reproduce the Pi entry exactly (compactions, edited messages), the
 *   original Pi entry is kept in the `ultron.pi.original` value for that entry id so export is lossless.
 */
import { createHash } from "node:crypto";
import { chmod, lstat, mkdir, readdir, readFile, readlink, rename, rm, symlink, writeFile } from "node:fs/promises";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { isDeepStrictEqual } from "node:util";
import type { Context } from "@earendil-works/chord";
import {
	type AgentMessage,
	BACKGROUND_CONTEXT,
	branchTip,
	type Entry,
	entryLabel,
	insertEntry,
	type JsonlSessionMetadata,
	JsonlSessionRepo,
	type JsonValue,
	type LaneConfiguration,
	laneConfig,
	laneState,
	type CompactionEntry as NativeCompactionEntry,
	type NewEntry,
	type Session,
	sessionName,
	setValue,
	type Value,
	value,
	type Write,
} from "@earendil-works/pi-agent-core";
import { NodeExecutionEnv } from "@earendil-works/pi-agent-core/node";
import { createCustomMessage } from "../core/messages.ts";
import {
	buildContextEntries,
	buildSessionContext,
	type ContextEditableContent,
	type ContextEditEntry,
	CURRENT_SESSION_VERSION,
	type FileEntry,
	loadEntriesFromFile,
	type SessionEntry,
	type SessionHeader,
	SessionManager,
	sessionEntryToContextMessages,
} from "../core/session-manager.ts";

// =============================================================================
// Shared Pi <-> native mapping
// =============================================================================

/** Native customType prefix for Pi entry types that have no native entry type. */
export const PI_ENTRY_CUSTOM_TYPE_PREFIX = "pi-session:";
/** Native customType for a message a Pi `context_edit` removed from model context. Data: `{ message }`. */
export const PI_OMITTED_MESSAGE_CUSTOM_TYPE = "pi-import:omitted-message";
/** Native customType for the marker export adds when the native tip is not a leaf (Pi resumes at a leaf). */
export const PI_LEAF_MARKER_CUSTOM_TYPE = "ultron:pi-leaf";
/** The original Pi entry, stored when its native entry cannot reproduce it exactly. */
export const piOriginalEntry = (entryId: string): Value<JsonValue> => value<JsonValue>("ultron.pi.original", entryId);

/** Pi entry types with a dedicated mapping; anything else is preserved and reported. */
const KNOWN_PI_ENTRY_TYPES = new Set([
	"message",
	"custom_message",
	"compaction",
	"branch_summary",
	"custom",
	"model_change",
	"thinking_level_change",
	"label",
	"session_info",
	"usage",
	"context_edit",
]);

function toJson(input: unknown): JsonValue {
	return JSON.parse(JSON.stringify(input ?? null)) as JsonValue;
}

function iso(timestamp: number): string {
	return new Date(timestamp).toISOString();
}

function parseTimestamp(value: string | undefined, fallback: number): number {
	const parsed = value === undefined ? Number.NaN : Date.parse(value);
	return Number.isFinite(parsed) ? parsed : fallback;
}

/** Pi's content replacement for one projected message (mirrors Pi's context-edit projection). */
function replaceContent(message: AgentMessage, content: ContextEditableContent): AgentMessage {
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

function applyEdit(messages: AgentMessage[], edit: ContextEditEntry["replacement"] | undefined): AgentMessage[] {
	if (edit === undefined) return messages;
	if (edit === null) return [];
	return messages.map((message) => replaceContent(message, edit.content));
}

type PiEntryBase = Pick<SessionEntry, "id" | "parentId" | "timestamp">;

/**
 * Convert one native entry to the Pi entry that represents it. Compactions need their branch to resolve
 * `firstKeptEntryId`, so they are converted by `compactionToPi`.
 */
function nativeToPiEntry(entry: Exclude<Entry, NativeCompactionEntry>, base: PiEntryBase): SessionEntry {
	switch (entry.type) {
		case "message": {
			const message = entry.message;
			if (message.role === "custom") {
				return {
					...base,
					type: "custom_message",
					customType: message.customType,
					content: message.content,
					display: message.display,
					...(message.details === undefined ? {} : { details: message.details }),
				};
			}
			return { ...base, type: "message", message };
		}
		case "branch_summary":
			return {
				...base,
				type: "branch_summary",
				fromId: entry.fromId ?? "root",
				summary: entry.summary,
				...(entry.details === undefined ? {} : { details: entry.details }),
				...(entry.usage === undefined ? {} : { usage: entry.usage }),
				fromHook: entry.fromHook,
			};
		case "custom": {
			if (entry.customType.startsWith(PI_ENTRY_CUSTOM_TYPE_PREFIX)) {
				const data = (entry.data ?? {}) as Record<string, unknown>;
				const type = entry.customType.slice(PI_ENTRY_CUSTOM_TYPE_PREFIX.length);
				return { ...data, ...base, type } as SessionEntry;
			}
			if (entry.customType === PI_OMITTED_MESSAGE_CUSTOM_TYPE) {
				const message = (entry.data as { message?: AgentMessage } | undefined)?.message;
				if (message !== undefined) {
					return nativeToPiEntry({ ...entry, type: "message", message }, base);
				}
			}
			return {
				...base,
				type: "custom",
				customType: entry.customType,
				...(entry.data === undefined ? {} : { data: entry.data }),
			};
		}
	}
}

/** Parent-first order that otherwise keeps file order (Pi writes parents first; this tolerates files that do not). */
class PiTree {
	readonly byId = new Map<string, SessionEntry>();
	private readonly children = new Map<string, string[]>();

	constructor(entries: readonly SessionEntry[]) {
		for (const entry of entries) this.byId.set(entry.id, entry);
		for (const entry of entries) {
			const parentId = this.parentOf(entry);
			if (parentId === null) continue;
			const siblings = this.children.get(parentId) ?? [];
			siblings.push(entry.id);
			this.children.set(parentId, siblings);
		}
	}

	/** The parent as Pi's tree resolves it: missing or self parents make the entry a root. */
	parentOf(entry: SessionEntry): string | null {
		const parentId = entry.parentId;
		return parentId === null || parentId === entry.id || !this.byId.has(parentId) ? null : parentId;
	}

	/** True when `descendantId` is below `ancestorId` and every leaf under `ancestorId` passes through it. */
	dominates(ancestorId: string, descendantId: string): boolean {
		let current = this.byId.get(descendantId);
		if (current === undefined) return false;
		let parentId = this.parentOf(current);
		while (parentId !== null) {
			if ((this.children.get(parentId)?.length ?? 0) !== 1) return false;
			if (parentId === ancestorId) return true;
			current = this.byId.get(parentId)!;
			parentId = this.parentOf(current);
		}
		return false;
	}

	insertionOrder(entries: readonly SessionEntry[]): SessionEntry[] {
		const out: SessionEntry[] = [];
		const placed = new Set<string>();
		const waiting = new Map<string, SessionEntry[]>();
		const place = (entry: SessionEntry): void => {
			const stack = [entry];
			while (stack.length > 0) {
				const next = stack.pop()!;
				out.push(next);
				placed.add(next.id);
				const children = waiting.get(next.id);
				if (children !== undefined) {
					waiting.delete(next.id);
					stack.push(...children.reverse());
				}
			}
		};
		for (const entry of entries) {
			if (placed.has(entry.id)) continue;
			const parentId = this.parentOf(entry);
			if (parentId === null || placed.has(parentId)) {
				place(entry);
				continue;
			}
			const siblings = waiting.get(parentId) ?? [];
			siblings.push(entry);
			waiting.set(parentId, siblings);
		}
		// Only cycles remain; Pi cannot reach these entries from a root either.
		for (const children of waiting.values()) for (const entry of children) if (!placed.has(entry.id)) place(entry);
		return out;
	}
}

// =============================================================================
// Pi -> native import
// =============================================================================

export interface ImportPiSessionOptions {
	/** Pi JSONL session file (format v1-v3; older formats are migrated in memory, the file is never modified). */
	piSessionPath: string;
	/** Native sessions root (the directory `JsonlSessionRepo` stores `<cwd-dir>/<file>.jsonl` under). */
	sessionsRoot: string;
}

export interface ImportPiSessionResult {
	sessionId: string;
	path: string;
	/** Number of native entries written (one per Pi entry). */
	imported: number;
	/** Pi entries that could not be imported. Empty: every entry type is mapped or preserved. */
	skipped: Array<{ type: string; count: number }>;
	/** Unknown Pi entry types kept verbatim as `pi-session:<type>` custom entries, with counts. */
	preserved: Array<{ type: string; count: number }>;
	/**
	 * `context_edit` entry ids whose effect is branch-local: the target is shared with branches that do not
	 * carry the edit, so it is kept only as a `pi-session:context_edit` entry (Pi still honors it after export).
	 */
	unappliedContextEdits: string[];
}

/** The Pi session was already imported; re-import is refused so the native copy is never duplicated. */
export class PiSessionAlreadyImportedError extends Error {
	readonly sessionId: string;
	readonly path: string;

	constructor(piSessionId: string, sessionId: string, path: string) {
		super(`Pi session ${piSessionId} was already imported as native session ${sessionId}: ${path}`);
		this.name = "PiSessionAlreadyImportedError";
		this.sessionId = sessionId;
		this.path = path;
	}
}

/**
 * Deterministic native session id for an imported Pi session: a UUID-shaped (version 8)
 * digest of the Pi session id, so the same Pi file always maps to the same native session.
 */
export function nativeSessionIdForPiSession(piSessionId: string): string {
	const bytes = createHash("sha256").update(`ultron:pi-import:${piSessionId}`).digest().subarray(0, 16);
	bytes[6] = (bytes[6]! & 0x0f) | 0x80;
	bytes[8] = (bytes[8]! & 0x3f) | 0x80;
	const hex = bytes.toString("hex");
	return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

interface PiSessionSnapshot {
	header: SessionHeader;
	manager: SessionManager;
	entries: SessionEntry[];
}

function readPiSession(piSessionPath: string): PiSessionSnapshot {
	const fileEntries: FileEntry[] = loadEntriesFromFile(piSessionPath);
	const header = fileEntries[0];
	if (header === undefined || header.type !== "session") {
		throw new Error(`Not a Pi session file (missing session header): ${piSessionPath}`);
	}
	// In-memory manager: applies Pi's format migrations without rewriting the source file.
	const manager = SessionManager.inMemory(header.cwd, undefined, fileEntries);
	return { header, manager, entries: manager.getEntries() };
}

type NativeEntryBody = NewEntry extends infer T ? (T extends unknown ? Omit<T, "id" | "parentId"> : never) : never;

interface ImportPlan {
	/** Effective replacement per message entry id, for edits that reach every leaf under the target. */
	messageEdits: Map<string, ContextEditEntry["replacement"]>;
	unappliedContextEdits: string[];
	/** Native retained tail per compaction id. */
	tails: Map<string, AgentMessage[]>;
}

function planImport(entries: readonly SessionEntry[], tree: PiTree): ImportPlan {
	const edits = entries.filter((entry): entry is ContextEditEntry => entry.type === "context_edit");
	const messageEdits = new Map<string, ContextEditEntry["replacement"]>();
	const unappliedContextEdits: string[] = [];
	for (const edit of edits) {
		const target = tree.byId.get(edit.targetId);
		const editable = target?.type === "message" || target?.type === "custom_message";
		// Edits appear in path order along a single chain, so the later edit wins as in Pi.
		if (editable && tree.dominates(target.id, edit.id)) messageEdits.set(target.id, edit.replacement);
		else unappliedContextEdits.push(edit.id);
	}

	const tails = new Map<string, AgentMessage[]>();
	for (const compaction of entries) {
		if (compaction.type !== "compaction") continue;
		// What Pi keeps in context at the compaction: [compaction, kept entries...], with edits among them.
		const contextEntries = buildContextEntries([...entries], compaction.id, tree.byId);
		const replacements = new Map<string, ContextEditEntry["replacement"]>();
		for (const entry of contextEntries) {
			if (entry.type === "context_edit") replacements.set(entry.targetId, entry.replacement);
		}
		// Later edits of kept entries apply on every leaf below the compaction only when they dominate it.
		for (const edit of edits) {
			if (tree.dominates(compaction.id, edit.id)) replacements.set(edit.targetId, edit.replacement);
		}
		tails.set(
			compaction.id,
			contextEntries
				.slice(1)
				.flatMap((entry) =>
					entry.type === "compaction"
						? []
						: applyEdit(sessionEntryToContextMessages(entry), replacements.get(entry.id)),
				),
		);
	}
	return { messageEdits, unappliedContextEdits, tails };
}

function editedMessageBody(message: AgentMessage, edit: ContextEditEntry["replacement"] | undefined): NativeEntryBody {
	if (edit === undefined) return { type: "message", message };
	if (edit === null) return { type: "custom", customType: PI_OMITTED_MESSAGE_CUSTOM_TYPE, data: toJson({ message }) };
	return { type: "message", message: replaceContent(message, edit.content) };
}

function toNativeEntryBody(entry: SessionEntry, plan: ImportPlan): NativeEntryBody {
	const edit = plan.messageEdits.has(entry.id) ? plan.messageEdits.get(entry.id) : undefined;
	switch (entry.type) {
		case "message":
			return editedMessageBody(entry.message, edit);
		case "custom_message":
			return editedMessageBody(
				createCustomMessage(entry.customType, entry.content, entry.display, entry.details, entry.timestamp),
				edit,
			);
		case "compaction":
			return {
				type: "compaction",
				summary: entry.summary,
				retainedTail: plan.tails.get(entry.id) ?? [],
				tokensBefore: entry.tokensBefore,
				...(entry.details === undefined ? {} : { details: entry.details as JsonValue }),
				...(entry.usage === undefined ? {} : { usage: entry.usage }),
				fromHook: entry.fromHook ?? false,
			};
		case "branch_summary":
			return {
				type: "branch_summary",
				// Pi encodes a root source as the "root" sentinel.
				fromId: entry.fromId === "root" ? null : entry.fromId,
				summary: entry.summary,
				...(entry.details === undefined ? {} : { details: entry.details as JsonValue }),
				...(entry.usage === undefined ? {} : { usage: entry.usage }),
				fromHook: entry.fromHook ?? false,
			};
		case "custom":
			return {
				type: "custom",
				customType: entry.customType,
				...(entry.data === undefined ? {} : { data: entry.data as JsonValue }),
			};
		default: {
			const { type, id: _id, parentId: _parentId, timestamp: _timestamp, ...data } = entry as SessionEntry;
			return { type: "custom", customType: `${PI_ENTRY_CUSTOM_TYPE_PREFIX}${type}`, data: toJson(data) };
		}
	}
}

/** True when export would rebuild exactly this Pi entry from its native entry. */
function reproducesPiEntry(original: SessionEntry, native: NewEntry, timestamp: number): boolean {
	if (native.type === "compaction") return false;
	const rebuilt = nativeToPiEntry({ ...native, seq: 0, timestamp } as Exclude<Entry, NativeCompactionEntry>, {
		id: native.id,
		parentId: native.parentId,
		timestamp: iso(timestamp),
	});
	return isDeepStrictEqual(toJson(rebuilt), toJson(original));
}

interface RepoHandle {
	repo: JsonlSessionRepo;
	fileSystem: NodeExecutionEnv;
	close(): Promise<void>;
}

function openRepo(sessionsRoot: string, now?: () => number): RepoHandle {
	const fileSystem = new NodeExecutionEnv({ cwd: process.cwd() });
	const repo = new JsonlSessionRepo({ fileSystem, sessionsRoot, ...(now === undefined ? {} : { now }) });
	return {
		repo,
		fileSystem,
		async close() {
			await repo.close(BACKGROUND_CONTEXT);
			await fileSystem.cleanup(BACKGROUND_CONTEXT);
		},
	};
}

/**
 * Import the whole tree of a Pi session as a new native Session with the same cwd (see the module comment
 * for the entry mapping). Entry ids, parents, and timestamps are preserved; the native `main` branch tip is
 * the Pi leaf. Throws `PiSessionAlreadyImportedError` when the Pi session was imported before.
 */
export async function importPiSession(options: ImportPiSessionOptions): Promise<ImportPiSessionResult> {
	const piPath = resolve(options.piSessionPath);
	const pi = readPiSession(piPath);
	const sessionId = nativeSessionIdForPiSession(pi.header.id);
	const tree = new PiTree(pi.entries);
	const plan = planImport(pi.entries, tree);

	// Storage stamps every commit with the repo clock; drive it from the Pi timestamps.
	let clock = parseTimestamp(pi.header.timestamp, Date.now());
	const handle = openRepo(options.sessionsRoot, () => clock);
	let session: Session<JsonlSessionMetadata> | undefined;
	let created = false;
	try {
		const existing = (await handle.repo.list(undefined, BACKGROUND_CONTEXT)).find((m) => m.id === sessionId);
		if (existing !== undefined) throw new PiSessionAlreadyImportedError(pi.header.id, sessionId, existing.path);

		session = await handle.repo.create({ id: sessionId, cwd: pi.header.cwd }, BACKGROUND_CONTEXT);
		created = true;
		const preserved = new Map<string, number>();
		let imported = 0;
		for (const entry of tree.insertionOrder(pi.entries)) {
			if (!KNOWN_PI_ENTRY_TYPES.has(entry.type)) preserved.set(entry.type, (preserved.get(entry.type) ?? 0) + 1);
			clock = parseTimestamp(entry.timestamp, clock);
			const native = { ...toNativeEntryBody(entry, plan), id: entry.id, parentId: tree.parentOf(entry) } as NewEntry;
			const writes: Write[] = [insertEntry(native)];
			if (!reproducesPiEntry(entry, native, clock)) writes.push(setValue(piOriginalEntry(entry.id), toJson(entry)));
			await session.mutate(async (mutator) => {
				await mutator.commit(writes, BACKGROUND_CONTEXT);
			}, BACKGROUND_CONTEXT);
			imported++;
		}

		// Derived state at the Pi leaf: tip, labels, name, and the lane configuration Pi would resume with.
		const leafId = pi.manager.getLeafId();
		const writes: Write[] = [setValue(branchTip("main"), leafId !== null && tree.byId.has(leafId) ? leafId : null)];
		for (const entry of pi.entries) {
			const label = pi.manager.getLabel(entry.id);
			if (label !== undefined) writes.push(setValue(entryLabel(entry.id), label));
		}
		const name = pi.manager.getSessionName();
		if (name !== undefined) writes.push(setValue(sessionName, name));
		const { model, thinkingLevel } = pi.manager.buildSessionContext();
		if (model !== null) {
			const configuration: LaneConfiguration = {
				model: { provider: model.provider, modelId: model.modelId },
				thinkingLevel: thinkingLevel as LaneConfiguration["thinkingLevel"],
				// Tools are not recorded by Pi; the host activates its own tool set when it opens the lane.
				activeToolNames: [],
			};
			writes.push(
				setValue(laneConfig("main"), configuration),
				setValue(laneState("main"), { currentOperationId: null, lastOperationId: null, inbox: [] }),
			);
		}
		await session.mutate(async (mutator) => {
			await mutator.commit(writes, BACKGROUND_CONTEXT);
		}, BACKGROUND_CONTEXT);
		return {
			sessionId,
			path: session.metadata.path,
			imported,
			skipped: [],
			preserved: [...preserved].map(([type, count]) => ({ type, count })),
			unappliedContextEdits: plan.unappliedContextEdits,
		};
	} catch (error) {
		if (created && session !== undefined) {
			// Never leave a partial import behind: it would block a retry.
			const path = session.metadata.path;
			await session.close(BACKGROUND_CONTEXT).catch(() => undefined);
			session = undefined;
			await rm(path, { force: true });
		}
		throw error;
	} finally {
		await session?.close(BACKGROUND_CONTEXT);
		await handle.close();
	}
}

// =============================================================================
// native -> Pi export (rollback)
// =============================================================================

export interface ExportNativeSessionToPiOptions {
	/** Native session file, located at `<sessionsRoot>/<cwd-dir>/<file>.jsonl`. */
	sessionPath: string;
	/** Destination Pi JSONL file. Must not exist. */
	outputPath: string;
}

export interface ExportNativeSessionToPiResult {
	sessionId: string;
	path: string;
	/** Number of Pi entries written (excluding the header). */
	entries: number;
}

/**
 * Pi keeps compaction tails by reference (`firstKeptEntryId`). Find the earliest preceding entry on the
 * compaction's branch whose projected context equals the native retained tail.
 */
function findFirstKeptEntryId(previous: readonly SessionEntry[], retainedTail: readonly AgentMessage[]): string | null {
	if (retainedTail.length === 0) return null;
	const collected: AgentMessage[] = [];
	for (let index = previous.length - 1; index >= 0; index--) {
		const entry = previous[index]!;
		if (entry.type === "compaction") continue;
		if (entry.type === "message" && entry.message.role === "system") continue;
		collected.unshift(...sessionEntryToContextMessages(entry));
		if (collected.length === retainedTail.length) {
			return isDeepStrictEqual(collected, retainedTail) ? entry.id : null;
		}
		if (collected.length > retainedTail.length) return null;
	}
	return null;
}

/** A native compaction as Pi entries: the compaction, plus a replayed tail when Pi cannot reference it. */
function compactionToPi(entry: NativeCompactionEntry, base: PiEntryBase, branch: SessionEntry[]): SessionEntry[] {
	const firstKept = findFirstKeptEntryId(branch, entry.retainedTail);
	const out: SessionEntry[] = [
		{
			...base,
			type: "compaction",
			summary: entry.summary,
			// No kept tail (or it could not be matched): nothing before the compaction is kept.
			firstKeptEntryId: firstKept ?? entry.id,
			tokensBefore: entry.tokensBefore,
			...(entry.details === undefined ? {} : { details: entry.details }),
			...(entry.usage === undefined ? {} : { usage: entry.usage }),
			fromHook: entry.fromHook,
		},
	];
	if (firstKept === null) {
		// Unmatched tail: replay it after the summary so Pi's model context is unchanged.
		let parentId = entry.id;
		for (const [index, message] of entry.retainedTail.entries()) {
			const id = `${entry.id}-tail-${index}`;
			out.push({ id, parentId, timestamp: base.timestamp, type: "message", message });
			parentId = id;
		}
	}
	return out;
}

function piLabelsOf(entries: readonly SessionEntry[]): Map<string, string> {
	const labels = new Map<string, string>();
	for (const entry of entries) {
		if (entry.type !== "label") continue;
		if (entry.label) labels.set(entry.targetId, entry.label);
		else labels.delete(entry.targetId);
	}
	return labels;
}

function piNameOf(entries: readonly SessionEntry[]): string | undefined {
	for (let index = entries.length - 1; index >= 0; index--) {
		const entry = entries[index]!;
		if (entry.type === "session_info") return entry.name?.trim() || undefined;
	}
	return undefined;
}

/** A native Session's whole tree as Pi entries, in storage (parent-first) order. */
export interface NativeSessionAsPi {
	readonly entries: SessionEntry[];
	/** The Pi id of the `main` lane tip (the entry Pi would resume at). */
	readonly leafId: string | null;
	readonly tipId: string | null;
	readonly nativeEntries: Entry[];
	readonly nativeById: Map<string, Entry>;
	readonly produced: Map<string, SessionEntry[]>;
	/** Native labels by target entry id (the resolved labels). */
	readonly labels: Map<string, string>;
}

/**
 * Convert every native entry of a Session (all branches) to the Pi entries that represent it, keeping entry ids.
 * Entries imported from Pi that the native form cannot reproduce exactly come back as their original Pi entry.
 */
export async function nativeSessionEntriesToPi(
	session: Pick<Session, "findEntries" | "scanValues" | "branch">,
	context: Context,
	lane = "main",
): Promise<NativeSessionAsPi> {
	const nativeEntries = await session.findEntries({ order: "asc" }, context);
	const originals = new Map(
		(await session.scanValues(piOriginalEntry(""), context)).map((stored) => [
			stored.address.key,
			stored.value as unknown as SessionEntry,
		]),
	);
	const nativeById = new Map(nativeEntries.map((entry) => [entry.id, entry]));

	// Storage order is parent-first, so every parent is converted before its children.
	const produced = new Map<string, SessionEntry[]>();
	const piIdOf = (nativeId: string | null): string | null =>
		nativeId === null ? null : (produced.get(nativeId)?.at(-1)?.id ?? nativeId);
	const piBranchTo = (nativeId: string | null): SessionEntry[] => {
		const ids: string[] = [];
		for (let id = nativeId; id !== null; id = nativeById.get(id)?.parentId ?? null) ids.push(id);
		return ids.reverse().flatMap((id) => produced.get(id) ?? []);
	};
	for (const entry of nativeEntries) {
		const base = { id: entry.id, parentId: piIdOf(entry.parentId), timestamp: iso(entry.timestamp) };
		const original = originals.get(entry.id);
		const items =
			original !== undefined
				? [original]
				: entry.type === "compaction"
					? compactionToPi(entry, base, piBranchTo(entry.parentId))
					: [nativeToPiEntry(entry, base)];
		produced.set(entry.id, items);
	}
	const entries = nativeEntries.flatMap((entry) => produced.get(entry.id)!);
	const branch = await session.branch(lane, context);
	const tipId = (await branch?.getTipId(context)) ?? null;
	const labels = new Map(
		(await session.scanValues(entryLabel(""), context)).map((stored) => [stored.address.key, stored.value]),
	);
	return { entries, leafId: piIdOf(tipId), tipId, nativeEntries, nativeById, produced, labels };
}

/**
 * Write a native Session's whole tree as a Pi-format JSONL session (for rollback to Pi). Pi resumes at the
 * last entry, which is the native `main` tip. Labels, the session name, and the `main` lane model and thinking
 * level that differ from what the entries already say are appended as Pi entries after that leaf.
 */
export async function exportNativeSessionToPi(
	options: ExportNativeSessionToPiOptions,
): Promise<ExportNativeSessionToPiResult> {
	const path = resolve(options.sessionPath);
	const handle = openRepo(dirname(dirname(path)));
	let session: Session<JsonlSessionMetadata> | undefined;
	try {
		const metadata = (await handle.repo.list(undefined, BACKGROUND_CONTEXT)).find((m) => m.path === path);
		if (metadata === undefined) throw new Error(`Not a native session in its session directory: ${path}`);
		session = await handle.repo.open(metadata, BACKGROUND_CONTEXT);
		const converted = await nativeSessionEntriesToPi(session, BACKGROUND_CONTEXT);
		const { nativeEntries, nativeById, produced, tipId, leafId: tipPiId } = converted;
		let entries = [...converted.entries];
		const ids = new Set(entries.map((entry) => entry.id));
		let stamp = iso(Math.max(metadata.createdAt, ...nativeEntries.map((entry) => entry.timestamp)));
		let leafId = entries.at(-1)?.id ?? null;
		const append = (entry: Record<string, unknown> & { type: string }, kind: string): void => {
			let id = `ultron-${kind}`;
			for (let counter = 1; ids.has(id); counter++) id = `ultron-${kind}-${counter}`;
			ids.add(id);
			entries.push({ ...entry, id, parentId: leafId, timestamp: stamp } as SessionEntry);
			leafId = id;
		};

		// Pi resumes at its last entry: make that the native main tip.
		if (tipId !== null && tipPiId !== leafId) {
			if (nativeEntries.some((entry) => entry.parentId === tipId)) {
				leafId = tipPiId;
				append({ type: "custom", customType: PI_LEAF_MARKER_CUSTOM_TYPE }, "leaf");
			} else {
				const tipItems = produced.get(tipId)!;
				entries = [...entries.filter((entry) => !tipItems.includes(entry)), ...tipItems];
				leafId = tipPiId;
			}
		}
		stamp = entries.find((entry) => entry.id === leafId)?.timestamp ?? stamp;

		// Labels set natively after import (or never recorded as Pi entries).
		const piLabels = piLabelsOf(entries);
		const nativeLabels = converted.labels;
		for (const targetId of new Set([...piLabels.keys(), ...nativeLabels.keys()])) {
			const label = nativeLabels.get(targetId);
			if (piLabels.get(targetId) === label || !nativeById.has(targetId)) continue;
			append({ type: "label", targetId, ...(label === undefined ? {} : { label }) }, "label");
		}
		const name = await session.getName(BACKGROUND_CONTEXT);
		if (piNameOf(entries) !== name) append({ type: "session_info", name: name ?? "" }, "name");

		const configuration = (await session.getValue(laneConfig("main"), BACKGROUND_CONTEXT))?.value;
		if (configuration !== undefined) {
			const resolved = buildSessionContext(entries, leafId);
			if (
				resolved.model?.provider !== configuration.model.provider ||
				resolved.model?.modelId !== configuration.model.modelId
			) {
				append({ type: "model_change", ...configuration.model }, "model");
			}
			if (resolved.thinkingLevel !== configuration.thinkingLevel) {
				append({ type: "thinking_level_change", thinkingLevel: configuration.thinkingLevel }, "thinking");
			}
		}

		const header: SessionHeader = {
			type: "session",
			version: CURRENT_SESSION_VERSION,
			id: metadata.id,
			timestamp: iso(metadata.createdAt),
			cwd: metadata.cwd,
		};
		const output = resolve(options.outputPath);
		await mkdir(dirname(output), { recursive: true });
		const content = `${[header, ...entries].map((line) => JSON.stringify(line)).join("\n")}\n`;
		await writeFile(output, content, { encoding: "utf8", flag: "wx", mode: 0o600 });
		return { sessionId: metadata.id, path: output, entries: entries.length };
	} finally {
		await session?.close(BACKGROUND_CONTEXT);
		await handle.close();
	}
}

// =============================================================================
// Profile backup / restore
// =============================================================================

export const PROFILE_BACKUP_FORMAT = "ultron-profile-backup";
export const PROFILE_BACKUP_VERSION = 1;
const MANIFEST_FILE = "manifest.json";
const FILES_DIR = "files";

/** Top-level profile entries that are data, caches, or regenerable installs rather than configuration. */
const EXCLUDED_TOP_LEVEL = new Set(["sessions", "traces", "bin", "npm", "node_modules", "backups"]);

export interface ProfileManifestFile {
	path: string;
	type: "file";
	size: number;
	sha256: string;
	mode: number;
}

export interface ProfileManifestSymlink {
	path: string;
	type: "symlink";
	target: string;
}

export interface ProfileManifest {
	format: typeof PROFILE_BACKUP_FORMAT;
	version: typeof PROFILE_BACKUP_VERSION;
	createdAt: string;
	sourceAgentDir: string;
	entries: Array<ProfileManifestFile | ProfileManifestSymlink>;
}

export interface BackupProfileOptions {
	agentDir: string;
	/** Parent directory; the backup is written to a new timestamped directory inside it. */
	backupDir: string;
}

export interface BackupProfileResult {
	path: string;
	manifest: ProfileManifest;
}

export interface RestoreProfileOptions {
	backupPath: string;
	agentDir: string;
}

export interface RestoreProfileResult {
	restored: number;
}

/** The backup failed verification; nothing was written to the profile. */
export class ProfileBackupVerificationError extends Error {
	readonly problems: string[];

	constructor(problems: string[]) {
		super(`Profile backup failed verification:\n${problems.map((problem) => `  - ${problem}`).join("\n")}`);
		this.name = "ProfileBackupVerificationError";
		this.problems = problems;
	}
}

function isExcluded(relativePath: string): boolean {
	const segments = relativePath.split("/");
	if (segments.length === 1 && EXCLUDED_TOP_LEVEL.has(segments[0]!)) return true;
	const name = segments.at(-1)!;
	// Session stores anywhere in the profile (sessions/, experimental/sessions/, *-sessions/).
	return name === "sessions" || name.endsWith("-sessions") || name.endsWith(".lock");
}

function isInside(parent: string, child: string): boolean {
	const rel = relative(parent, child);
	return rel === "" || (!rel.startsWith("..") && !isAbsolute(rel));
}

async function collectProfile(
	agentDir: string,
	skipDir: string | undefined,
): Promise<Array<{ path: string; kind: "file" | "symlink" }>> {
	const out: Array<{ path: string; kind: "file" | "symlink" }> = [];
	const walk = async (relativeDir: string): Promise<void> => {
		const absoluteDir = join(agentDir, relativeDir);
		const names = (await readdir(absoluteDir)).sort();
		for (const name of names) {
			const rel = relativeDir === "" ? name : `${relativeDir}/${name}`;
			const absolute = join(agentDir, rel);
			if (isExcluded(rel) || (skipDir !== undefined && isInside(skipDir, absolute))) continue;
			const stats = await lstat(absolute);
			if (stats.isSymbolicLink()) out.push({ path: rel, kind: "symlink" });
			else if (stats.isDirectory()) await walk(rel);
			else if (stats.isFile()) out.push({ path: rel, kind: "file" });
			// Sockets, FIFOs, and devices are not configuration.
		}
	};
	await walk("");
	return out;
}

function sha256(data: Buffer): string {
	return createHash("sha256").update(data).digest("hex");
}

async function mkdirPrivate(path: string): Promise<void> {
	await mkdir(path, { recursive: true, mode: 0o700 });
	await chmod(path, 0o700);
}

async function createUniqueDirectory(parent: string, base: string): Promise<string> {
	for (let attempt = 0; attempt < 100; attempt++) {
		const candidate = join(parent, attempt === 0 ? base : `${base}-${attempt}`);
		try {
			await mkdir(candidate, { mode: 0o700 });
			await chmod(candidate, 0o700);
			return candidate;
		} catch (error) {
			if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
		}
	}
	throw new Error(`Could not create a unique backup directory under ${parent}`);
}

/**
 * Copy the Ultron config profile (everything under agentDir except sessions, traces, caches of
 * installed binaries/packages, and lock files) into `<backupDir>/ultron-profile-<timestamp>/`.
 * Directories are 0700 and files 0600; `manifest.json` records a sha256 per file.
 */
export async function backupProfile(options: BackupProfileOptions): Promise<BackupProfileResult> {
	const agentDir = resolve(options.agentDir);
	const backupDir = resolve(options.backupDir);
	if ((await lstat(agentDir).catch(() => undefined))?.isDirectory() !== true) {
		throw new Error(`Profile directory does not exist: ${agentDir}`);
	}
	// Never tighten a caller-supplied parent (it may be $HOME); the backup directory itself is 0700.
	await mkdir(backupDir, { recursive: true, mode: 0o700 });
	const createdAt = new Date();
	const target = await createUniqueDirectory(
		backupDir,
		`ultron-profile-${createdAt.toISOString().replace(/[:.]/g, "-")}`,
	);
	try {
		const filesRoot = join(target, FILES_DIR);
		await mkdirPrivate(filesRoot);
		const manifest: ProfileManifest = {
			format: PROFILE_BACKUP_FORMAT,
			version: PROFILE_BACKUP_VERSION,
			createdAt: createdAt.toISOString(),
			sourceAgentDir: agentDir,
			entries: [],
		};
		for (const item of await collectProfile(agentDir, isInside(agentDir, backupDir) ? backupDir : undefined)) {
			const source = join(agentDir, item.path);
			const destination = join(filesRoot, item.path);
			await mkdirPrivateTree(filesRoot, dirname(destination));
			if (item.kind === "symlink") {
				const linkTarget = await readlink(source);
				await symlink(linkTarget, destination);
				manifest.entries.push({ path: item.path, type: "symlink", target: linkTarget });
				continue;
			}
			const data = await readFile(source);
			const mode = (await lstat(source)).mode & 0o777;
			await writeFile(destination, data, { mode: 0o600, flag: "wx" });
			await chmod(destination, 0o600);
			manifest.entries.push({ path: item.path, type: "file", size: data.length, sha256: sha256(data), mode });
		}
		await writeFile(join(target, MANIFEST_FILE), `${JSON.stringify(manifest, null, 2)}\n`, {
			mode: 0o600,
			flag: "wx",
		});
		return { path: target, manifest };
	} catch (error) {
		await rm(target, { recursive: true, force: true });
		throw error;
	}
}

async function mkdirPrivateTree(root: string, directory: string): Promise<void> {
	const rel = relative(root, directory);
	if (rel === "") return;
	let current = root;
	for (const segment of rel.split(sep)) {
		current = join(current, segment);
		await mkdir(current, { mode: 0o700 }).catch((error: NodeJS.ErrnoException) => {
			if (error.code !== "EEXIST") throw error;
		});
		await chmod(current, 0o700);
	}
}

function isSafeRelativePath(path: unknown): path is string {
	if (typeof path !== "string" || path.length === 0 || path.includes("\\") || path.includes("\0")) return false;
	if (isAbsolute(path)) return false;
	return path.split("/").every((segment) => segment !== "" && segment !== "." && segment !== "..");
}

async function listBackupFiles(root: string, relativeDir = ""): Promise<string[]> {
	const out: string[] = [];
	for (const entry of await readdir(join(root, relativeDir), { withFileTypes: true })) {
		const rel = relativeDir === "" ? entry.name : `${relativeDir}/${entry.name}`;
		if (entry.isDirectory()) out.push(...(await listBackupFiles(root, rel)));
		else out.push(rel);
	}
	return out;
}

/** Verify a backup against its manifest without touching any profile. Returns the manifest. */
export async function verifyProfileBackup(backupPath: string): Promise<ProfileManifest> {
	const root = resolve(backupPath);
	let manifest: ProfileManifest;
	try {
		manifest = JSON.parse(await readFile(join(root, MANIFEST_FILE), "utf8")) as ProfileManifest;
	} catch (error) {
		throw new ProfileBackupVerificationError([`unreadable manifest: ${(error as Error).message}`]);
	}
	if (manifest?.format !== PROFILE_BACKUP_FORMAT || manifest.version !== PROFILE_BACKUP_VERSION) {
		throw new ProfileBackupVerificationError(["unsupported manifest format or version"]);
	}
	if (!Array.isArray(manifest.entries)) throw new ProfileBackupVerificationError(["manifest has no entries list"]);
	const problems: string[] = [];
	const filesRoot = join(root, FILES_DIR);
	const expected = new Set<string>();
	for (const entry of manifest.entries) {
		if (!isSafeRelativePath(entry?.path)) {
			problems.push(`unsafe path in manifest: ${JSON.stringify(entry?.path)}`);
			continue;
		}
		expected.add(entry.path);
		const absolute = join(filesRoot, entry.path);
		const stats = await lstat(absolute).catch(() => undefined);
		if (stats === undefined) {
			problems.push(`missing: ${entry.path}`);
		} else if (entry.type === "symlink") {
			if (!stats.isSymbolicLink() || (await readlink(absolute)) !== entry.target) {
				problems.push(`symlink changed: ${entry.path}`);
			}
		} else if (entry.type === "file") {
			if (!stats.isFile()) problems.push(`not a regular file: ${entry.path}`);
			else if (sha256(await readFile(absolute)) !== entry.sha256 || stats.size !== entry.size) {
				problems.push(`hash mismatch: ${entry.path}`);
			}
		} else {
			problems.push(`unknown entry type for ${(entry as { path: string }).path}`);
		}
	}
	const present = await listBackupFiles(filesRoot).catch(() => [] as string[]);
	for (const path of present) if (!expected.has(path)) problems.push(`not in manifest: ${path}`);
	if (problems.length > 0) throw new ProfileBackupVerificationError(problems);
	return manifest;
}

/**
 * Restore a profile backup into agentDir. Every file is verified against the manifest before
 * anything is written; a mismatch refuses the whole restore. Files outside the backup are left alone.
 */
export async function restoreProfile(options: RestoreProfileOptions): Promise<RestoreProfileResult> {
	const root = resolve(options.backupPath);
	const agentDir = resolve(options.agentDir);
	const manifest = await verifyProfileBackup(root);
	const filesRoot = join(root, FILES_DIR);
	await mkdir(agentDir, { recursive: true, mode: 0o700 });
	let restored = 0;
	for (const entry of manifest.entries) {
		const destination = join(agentDir, entry.path);
		await mkdir(dirname(destination), { recursive: true, mode: 0o700 });
		const temporary = `${destination}.ultron-restore-${process.pid}`;
		await rm(temporary, { force: true });
		if (entry.type === "symlink") {
			await symlink(entry.target, temporary);
		} else {
			const data = await readFile(join(filesRoot, entry.path));
			// Re-check at write time so a file swapped after verification is still refused.
			if (sha256(data) !== entry.sha256) {
				throw new ProfileBackupVerificationError([`hash mismatch: ${entry.path}`]);
			}
			await writeFile(temporary, data, { mode: 0o600, flag: "wx" });
			await chmod(temporary, entry.mode & 0o777);
		}
		const existing = await lstat(destination).catch(() => undefined);
		if (existing?.isDirectory()) {
			await rm(temporary, { force: true });
			throw new Error(`Cannot restore ${entry.path}: a directory is in the way`);
		}
		await rename(temporary, destination);
		restored++;
	}
	return { restored };
}
