/**
 * Model-owned context (`ctx` in the kernel) and collapse on return.
 *
 * Every change is a context edit on the lane's conversation branch (a `context_edit` custom entry, Pi's
 * append-only `context_edit` in native form): it changes only what the model sees on that branch, never the
 * durable transcript, and a branch that does not carry the edit never sees it (anchored like refinements).
 *
 * - `ctx.history/get` read the branch; `ctx.forget/summarize` append edits; `ctx.pin/unpin` and `ctx.note`
 *   append their own custom entries. Guard rails: the current user message, a tool call still waiting for its
 *   result, and pinned items are never edited.
 * - Collapse on return: a root-level task that ends while a root `rlm` cell runs (or before the next one) is
 *   attributed to that cell. Once the model has answered after the cell's output, the host appends an edit that
 *   replaces the output with one line per task (definition, key, status, cost, how to fetch the full result).
 *   The full result stays in the journal (`agents.result`) and in whatever kernel variable holds it.
 * - Compaction: edits apply before summarizing (see `applyContextEdits`), and notes and pinned items that a
 *   compaction cut off are shown again right after the compaction summary.
 * - Observers (extensions) get every edit that lands on a branch through `observe`.
 */
import {
	type AgentHarness,
	type AgentLane,
	type AgentMessage,
	applyContextEdits,
	CONTEXT_EDIT_CUSTOM_TYPE,
	CONTEXT_OMITTED_CUSTOM_TYPE,
	type Context,
	type ContextEdit,
	contextEditsOf,
	createCustomMessage,
	type Entry,
	type EntryProjector,
} from "@ultron/agent-core";
import type { JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import type { HostModuleRequest, NativeHostModule } from "./rlm/host-module.ts";
import type { NativeTask } from "./rlm/task-store.ts";

export const CONTEXT_NOTE_CUSTOM_TYPE = "ultron:context-note";
export const CONTEXT_PIN_CUSTOM_TYPE = "ultron:context-pin";
/** Custom message type of the notes and pinned items shown again after a compaction. */
export const CONTEXT_KEPT_MESSAGE_TYPE = "ultron-context-kept";
/** Extension event-bus channel that receives every context edit (`pi.events.on(...)`). */
export const CONTEXT_EDIT_EVENT = "ultron:context_edit";

/** Runtime-section text for the kernel's `ctx` API; the integrator places it in the system prompt. */
export const CONTEXT_PROMPT = [
	"- `ctx` manages your own context (the transcript itself is never changed): `await ctx.history(limit=20, kinds=None)` lists recent items with id, kind, bytes, preview and state; `await ctx.get(id)` returns one in full; `await ctx.forget(ids, reason)` removes items from what you see; `await ctx.summarize(ids, text)` replaces a span with your summary; `await ctx.pin(id)` keeps an item through edits and compaction; `await ctx.note(text)` records a note that survives compaction. The current user message and pinned items cannot be forgotten.",
	'- Task results collapse on return: after you have seen a cell\'s output once, it shrinks to one line per finished task (definition, key, status, cost). `await agents.result("<task id>")` returns the full value again; `await ctx.get(id)` shows the original output.',
].join("\n");

const HISTORY_DEFAULT = 20;
const HISTORY_MAX = 200;
const PREVIEW_CHARS = 160;
const NOTE_MAX_CHARS = 4000;
const SUMMARY_MAX_CHARS = 8000;
const REASON_MAX_CHARS = 500;
const KEPT_ITEM_CHARS = 4000;
const KEPT_TOTAL_CHARS = 16000;
/** Cell outputs at most this long are left alone: collapsing them saves nothing. */
const COLLAPSE_MIN_BYTES = 600;
const COLLAPSE_HEAD_CHARS = 200;
const COLLAPSE_MAX_TASKS = 8;
const MAX_PENDING_CELLS = 64;
const STATE_LIST_MAX = 20;

export type ContextItemKind = "user" | "assistant" | "tool" | "custom" | "note" | "summary" | "compaction";
const KINDS: readonly ContextItemKind[] = ["user", "assistant", "tool", "custom", "note", "summary", "compaction"];
export type ContextItemState = "visible" | "forgotten" | "summarized" | "collapsed" | "replaced";

export interface ContextItem {
	id: string;
	kind: ContextItemKind;
	tool?: string;
	/** Bytes the model sees now (0 when forgotten). */
	bytes: number;
	/** Bytes of the durable transcript item. */
	original_bytes: number;
	preview: string;
	state: ContextItemState;
	pinned: boolean;
	/** The current user message or a call still waiting for its result: never edited. */
	protected: boolean;
	at: number;
}

/** What one edit entry did, as observers receive it. */
export interface ContextEditEvent {
	lane: string;
	editId: string;
	source: string;
	reason?: string;
	edits: Array<{ targetId: string; action: "omit" | "replace" }>;
	tasks?: string[];
}

export interface ContextControlOptions {
	harness: AgentHarness;
	/** The root agent's lane ("main"). */
	rootLane: AgentLane;
	/** Receives every context edit that lands on a branch (the worker forwards it to extensions). */
	observe?: (event: ContextEditEvent) => void;
}

type Pending = { lane: string; entry: Entry };
type TaskLine = { id: string; definition: string; key: string; status: string; cost: number | null };

/** Entry projector for notes: a note is a custom message at its place in the branch. */
export const CONTEXT_ENTRY_PROJECTORS: Readonly<Record<string, EntryProjector>> = {
	[CONTEXT_NOTE_CUSTOM_TYPE]: (entry) => {
		const text = isRecord(entry.data) && typeof entry.data.text === "string" ? entry.data.text : "";
		return text
			? [createCustomMessage("ultron-note", `Note (ctx.note): ${text}`, true, undefined, entry.timestamp)]
			: [];
	},
};

function isRecord(value: unknown): value is Record<string, unknown> {
	return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** Readable text of a message, for previews and kept items. */
export function messageText(message: AgentMessage): string {
	const content = (message as { content?: unknown }).content;
	if (typeof content === "string") return content;
	if (Array.isArray(content)) {
		return content
			.map((part) => {
				if (!isRecord(part)) return "";
				if (part.type === "text" && typeof part.text === "string") return part.text;
				if (part.type === "thinking" && typeof part.thinking === "string") return part.thinking;
				if (part.type === "toolCall")
					return `${String(part.name)}(${JSON.stringify(part.arguments ?? {}).slice(0, 2000)})`;
				if (part.type === "image") return "[image]";
				return "";
			})
			.filter(Boolean)
			.join("\n");
	}
	const record = message as unknown as Record<string, unknown>;
	if (typeof record.summary === "string") return record.summary;
	if (typeof record.output === "string") return record.output;
	return "";
}

function oneLine(text: string, max = PREVIEW_CHARS): string {
	const flat = text.replace(/\s+/g, " ").trim();
	return flat.length > max ? `${flat.slice(0, max - 1)}…` : flat;
}

function entryBytes(entry: Entry): number {
	if (entry.type === "message")
		return Buffer.byteLength(
			JSON.stringify((entry.message as { content?: unknown }).content ?? messageText(entry.message)),
		);
	if (entry.type === "compaction") return Buffer.byteLength(entry.summary);
	if (entry.type === "branch_summary") return Buffer.byteLength(entry.summary);
	if (entry.type === "custom" && entry.customType === CONTEXT_NOTE_CUSTOM_TYPE)
		return Buffer.byteLength(noteText(entry));
	return 0;
}

function noteText(entry: Entry): string {
	return entry.type === "custom" && isRecord(entry.data) && typeof entry.data.text === "string" ? entry.data.text : "";
}

function kindOf(entry: Entry): ContextItemKind | undefined {
	switch (entry.type) {
		case "message":
			return entry.message.role === "user"
				? "user"
				: entry.message.role === "assistant"
					? "assistant"
					: entry.message.role === "toolResult"
						? "tool"
						: "custom";
		case "compaction":
			return "compaction";
		case "branch_summary":
			return "summary";
		case "custom":
			if (entry.customType === CONTEXT_NOTE_CUSTOM_TYPE) return "note";
			return undefined;
	}
}

function entryText(entry: Entry): string {
	if (entry.type === "message") return messageText(entry.message);
	if (entry.type === "compaction" || entry.type === "branch_summary") return entry.summary;
	return noteText(entry);
}

function toolCallIds(message: AgentMessage): string[] {
	if (message.role !== "assistant") return [];
	return message.content.flatMap((part) => (part.type === "toolCall" ? [part.id] : []));
}

function isUuid(value: string): boolean {
	return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(value);
}

/** One line that stands for a finished task in the root's context. */
export function taskLine(task: TaskLine): string {
	const key = isUuid(task.key) ? "" : ` key=${task.key}`;
	const cost = task.cost === null ? "cost=?" : `cost=$${task.cost.toFixed(4)}`;
	return `↳ task ${task.id} ${task.definition}${key} ${task.status} ${cost} · full result: await agents.result("${task.id}")`;
}

/** The collapsed form of a cell output that returned finished tasks. */
export function collapsedCellText(entryId: string, original: string, tasks: readonly TaskLine[]): string {
	const head = oneLine(original, COLLAPSE_HEAD_CHARS);
	const lines = tasks.slice(0, COLLAPSE_MAX_TASKS).map(taskLine);
	if (tasks.length > COLLAPSE_MAX_TASKS) lines.push(`↳ +${tasks.length - COLLAPSE_MAX_TASKS} more finished tasks`);
	return [
		`[collapsed on return: this cell output was ${Buffer.byteLength(original)} bytes; ctx.get("${entryId}") shows it]`,
		head,
		...lines,
	].join("\n");
}

/** A consistent read of one lane's branch plus this worker's edits still queued behind a running turn. */
interface BranchView {
	path: Entry[];
	/** Index of the newest compaction in `path`, or -1. */
	compactionIndex: number;
	/** Entries after the newest compaction: the part of the branch that reaches the model. */
	window: Entry[];
	byId: Map<string, Entry>;
	/** Model view of `window` entries after edits. */
	applied: Map<string, Entry>;
	/** Edit per target, with the source recorded on its edit entry. */
	edits: Map<string, { edit: ContextEdit; source: string; reason?: string }>;
	pins: Set<string>;
	protectedIds: Set<string>;
}

export class ContextControl {
	readonly #harness: AgentHarness;
	readonly #lanes = new Map<string, AgentLane>();
	readonly #observe: ContextControlOptions["observe"];
	/** Entries this worker appended that a running turn has not placed on the branch yet. */
	#pending: Pending[] = [];
	/** Root cell running now (tool call id). */
	#currentCell: string | undefined;
	/** Finished root-level tasks waiting for a cell to be attributed to. */
	#unattributed: TaskLine[] = [];
	/** Cells (tool call ids) whose output returned finished tasks and is not collapsed yet. */
	#cells = new Map<string, TaskLine[]>();
	#collapsing: Promise<void> = Promise.resolve();
	readonly #removers: Array<() => void> = [];

	constructor(options: ContextControlOptions) {
		this.#harness = options.harness;
		this.#lanes.set(options.rootLane.name, options.rootLane);
		this.#observe = options.observe;
	}

	/** Host module for `ctx.*` requests from any lane's kernel; each lane edits its own branch. */
	readonly module: NativeHostModule = {
		prefixes: ["ctx."],
		handle: (request) => this.#handle(request),
	};

	/** Wire collapse on return, compaction carry-over, and edit observation. Returns the uninstaller. */
	install(): () => void {
		const events = this.#harness.events;
		this.#removers.push(
			events.on("tool_start", (event) => {
				if (event.lane !== "main" || event.toolName !== "rlm") return;
				this.#currentCell = event.toolCallId;
				if (this.#unattributed.length > 0) {
					this.#cells.set(event.toolCallId, this.#unattributed);
					this.#unattributed = [];
				}
			}),
			events.on("tool_end", (event) => {
				if (event.lane === "main" && event.toolCallId === this.#currentCell) this.#currentCell = undefined;
			}),
			events.on("message_end", (event) => {
				if (event.lane === "main" && event.message.role === "assistant" && this.#cells.size > 0)
					this.#scheduleCollapse();
			}),
			events.on("run_end", (event) => {
				if (event.lane === "main" && this.#cells.size > 0) this.#scheduleCollapse();
			}),
			events.on("entry_added", (event) => {
				const entry = event.entry;
				if (entry.type !== "custom") return;
				this.#pending = this.#pending.filter((item) => item.entry.id !== entry.id);
				if (entry.customType !== CONTEXT_EDIT_CUSTOM_TYPE) return;
				const data = isRecord(entry.data) ? entry.data : {};
				this.#observe?.({
					lane: event.lane,
					editId: entry.id,
					source: typeof data.source === "string" ? data.source : "unknown",
					...(typeof data.reason === "string" ? { reason: data.reason } : {}),
					edits: contextEditsOf(entry).map((edit) => ({
						targetId: edit.targetId,
						action: edit.replacement === null ? "omit" : "replace",
					})),
					...(Array.isArray(data.tasks) ? { tasks: data.tasks.map(String) } : {}),
				});
			}),
			this.#harness.hooks.on("transform_context", async (event, context) => {
				const index = event.messages.findIndex((message) => message.role === "compactionSummary");
				if (index === -1) return undefined;
				// Only lanes that have used ctx (or the root) can carry notes and pins.
				const lane = this.#lanes.get(event.lane);
				if (lane === undefined) return undefined;
				const kept = await this.#keptAcrossCompaction(lane, context);
				if (!kept) return undefined;
				const messages = [...event.messages];
				messages.splice(
					index + 1,
					0,
					createCustomMessage(CONTEXT_KEPT_MESSAGE_TYPE, kept, false, undefined, Date.now()),
				);
				return { messages };
			}),
		);
		return () => {
			for (const remove of this.#removers.splice(0)) remove();
		};
	}

	/** A task's terminal result is durable. Root-level tasks are attributed to the root cell that returns them. */
	taskEnded(task: NativeTask, info: { cost: number | null }): void {
		if (task.parentId !== undefined) return;
		const line: TaskLine = {
			id: task.id,
			definition: task.definition,
			key: task.key,
			status: task.result?.status ?? task.state,
			cost: info.cost,
		};
		if (this.#currentCell !== undefined) {
			const lines = this.#cells.get(this.#currentCell) ?? [];
			lines.push(line);
			this.#cells.set(this.#currentCell, lines);
		} else {
			this.#unattributed.push(line);
			if (this.#unattributed.length > MAX_PENDING_CELLS) this.#unattributed.shift();
		}
		while (this.#cells.size > MAX_PENDING_CELLS) this.#cells.delete(this.#cells.keys().next().value!);
	}

	/** Forgotten, pinned and noted items of the root branch, for the `/rlm` panel. */
	async state(context: Context): Promise<JsonValue> {
		const lane = await this.#lane("main", context);
		const view = await this.#view(lane, context);
		const item = (id: string) => {
			const entry = view.byId.get(id);
			return entry ? { id, kind: kindOf(entry) ?? "custom", preview: oneLine(entryText(entry), 80) } : { id };
		};
		const edited = [...view.edits.entries()].filter(([id]) => view.byId.has(id));
		const forgotten = edited
			.filter(([, value]) => value.source !== "collapse")
			.map(([id, value]) => ({
				...item(id),
				source: value.source,
				...(value.reason === undefined ? {} : { reason: value.reason }),
			}));
		return {
			forgotten: forgotten.slice(-STATE_LIST_MAX),
			forgottenCount: forgotten.length,
			collapsedCount: edited.length - forgotten.length,
			pinned: [...view.pins].slice(-STATE_LIST_MAX).map(item),
			pinnedCount: view.pins.size,
			notes: view.path.filter((entry) => entry.type === "custom" && entry.customType === CONTEXT_NOTE_CUSTOM_TYPE)
				.length,
		} as JsonValue;
	}

	async #handle(request: HostModuleRequest): Promise<unknown> {
		const { type, payload, caller, context } = request;
		if (type === "ctx.state") {
			fields(payload, []);
			return this.state(context);
		}
		const lane = await this.#lane(caller.lane, context);
		const view = await this.#view(lane, context);
		switch (type) {
			case "ctx.history": {
				fields(payload, ["limit", "kinds"]);
				const limit = payload.limit === undefined ? HISTORY_DEFAULT : payload.limit;
				if (typeof limit !== "number" || !Number.isSafeInteger(limit) || limit < 1 || limit > HISTORY_MAX)
					throw new Error(`limit must be an integer between 1 and ${HISTORY_MAX}`);
				let kinds: Set<string> | undefined;
				if (payload.kinds !== undefined && payload.kinds !== null) {
					if (
						!Array.isArray(payload.kinds) ||
						payload.kinds.some((kind) => !KINDS.includes(kind as ContextItemKind))
					)
						throw new Error(`kinds must be a list of: ${KINDS.join(", ")}`);
					kinds = new Set(payload.kinds as string[]);
				}
				const items = view.window
					.map((entry) => this.#item(view, entry))
					.filter(
						(item): item is ContextItem => item !== undefined && (kinds === undefined || kinds.has(item.kind)),
					);
				const shown = items.slice(-limit);
				return {
					items: shown,
					total: items.length,
					visible_bytes: items.reduce((sum, item) => sum + item.bytes, 0),
					compacted: view.compactionIndex !== -1,
				};
			}
			case "ctx.get": {
				fields(payload, ["id"]);
				const id = nonempty(payload.id, "id");
				const entry = view.byId.get(id);
				const item = entry ? this.#item(view, entry) : undefined;
				if (!entry || !item) throw new Error(`Unknown context item: ${id}`);
				const visible = view.applied.get(id);
				return {
					...item,
					text: entryText(entry),
					...(entry.type === "message" ? { message: entry.message } : {}),
					visible_text: visible === undefined ? null : item.state === "forgotten" ? null : entryText(visible),
				};
			}
			case "ctx.forget": {
				fields(payload, ["ids", "reason"]);
				const ids = idList(payload.ids);
				const reason = nonempty(payload.reason, "reason").slice(0, REASON_MAX_CHARS);
				this.#guard(view, ids);
				const fresh = ids.filter((id) => view.edits.get(id)?.edit.replacement !== null);
				const before = this.#visibleBytes(view);
				if (fresh.length === 0) return { forgotten: [], already: ids, edit_id: null, freed_bytes: 0 };
				const edits = fresh.map((targetId) => ({ targetId, replacement: null }));
				const editId = await this.#append(
					lane,
					CONTEXT_EDIT_CUSTOM_TYPE,
					{ edits, source: "ctx.forget", reason },
					context,
				);
				const after = this.#visibleBytes(await this.#view(lane, context));
				return {
					forgotten: fresh,
					already: ids.filter((id) => !fresh.includes(id)),
					edit_id: editId,
					freed_bytes: Math.max(0, before - after),
				};
			}
			case "ctx.summarize": {
				fields(payload, ["ids", "text"]);
				const text = nonempty(payload.text, "text");
				if (text.length > SUMMARY_MAX_CHARS) throw new Error(`text exceeds ${SUMMARY_MAX_CHARS} characters`);
				const ids = this.#closure(view, idList(payload.ids));
				this.#guard(view, ids);
				const order = new Map(view.window.map((entry, index) => [entry.id, index]));
				ids.sort((a, b) => order.get(a)! - order.get(b)!);
				const [first, ...rest] = ids;
				const edits = [
					{
						targetId: first!,
						replacement: {
							content: `[Summary of ${ids.length} earlier item${ids.length === 1 ? "" : "s"} (ctx.summarize)]\n${text}`,
						},
					},
					...rest.map((targetId) => ({ targetId, replacement: null })),
				];
				const before = this.#visibleBytes(view);
				const editId = await this.#append(
					lane,
					CONTEXT_EDIT_CUSTOM_TYPE,
					{ edits, source: "ctx.summarize" },
					context,
				);
				const after = this.#visibleBytes(await this.#view(lane, context));
				return { summarized: ids, summary_id: first, edit_id: editId, freed_bytes: Math.max(0, before - after) };
			}
			case "ctx.pin":
			case "ctx.unpin": {
				fields(payload, ["id"]);
				const id = nonempty(payload.id, "id");
				const entry = view.byId.get(id);
				if (!entry || kindOf(entry) === undefined) throw new Error(`Unknown context item: ${id}`);
				const pin = type === "ctx.pin";
				if (view.pins.has(id) === pin) return { id, pinned: pin, changed: false };
				await this.#append(lane, CONTEXT_PIN_CUSTOM_TYPE, { targetId: id, pinned: pin }, context);
				return { id, pinned: pin, changed: true };
			}
			case "ctx.note": {
				fields(payload, ["text"]);
				const text = nonempty(payload.text, "text");
				if (text.length > NOTE_MAX_CHARS) throw new Error(`note exceeds ${NOTE_MAX_CHARS} characters`);
				return { id: await this.#append(lane, CONTEXT_NOTE_CUSTOM_TYPE, { text }, context) };
			}
			default:
				throw new Error(`Ultron RLM host request is not wired: ${type}`);
		}
	}

	async #lane(name: string, context: Context): Promise<AgentLane> {
		let lane = this.#lanes.get(name);
		if (lane === undefined) {
			lane = await this.#harness.lane(name, context);
			this.#lanes.set(name, lane);
		}
		return lane;
	}

	async #append(lane: AgentLane, customType: string, data: JsonValue, context: Context): Promise<string> {
		const id = await lane.appendCustomEntry(customType, data, context);
		// While a turn runs the entry waits for the next boundary; later reads in this cell must already see it.
		if ((await lane.getTipId(context)) !== id) {
			this.#pending.push({
				lane: lane.name,
				entry: {
					id,
					parentId: null,
					seq: Number.MAX_SAFE_INTEGER,
					timestamp: Date.now(),
					type: "custom",
					customType,
					data,
				},
			});
		}
		return id;
	}

	async #view(lane: AgentLane, context: Context): Promise<BranchView> {
		const committed = await lane.findEntries({ order: "oldestFirst" }, context);
		const committedIds = new Set(committed.map((entry) => entry.id));
		this.#pending = this.#pending.filter((item) => !committedIds.has(item.entry.id));
		const path = [...committed, ...this.#pending.filter((item) => item.lane === lane.name).map((item) => item.entry)];
		let compactionIndex = -1;
		for (let index = path.length - 1; index >= 0; index -= 1) {
			if (path[index]!.type === "compaction") {
				compactionIndex = index;
				break;
			}
		}
		const window = path.slice(compactionIndex + 1);
		const byId = new Map(window.map((entry) => [entry.id, entry]));
		const applied = new Map(applyContextEdits(window).map((entry) => [entry.id, entry]));
		const edits: BranchView["edits"] = new Map();
		for (const entry of window) {
			const data = entry.type === "custom" && isRecord(entry.data) ? entry.data : {};
			for (const edit of contextEditsOf(entry))
				edits.set(edit.targetId, {
					edit,
					source: typeof data.source === "string" ? data.source : "unknown",
					...(typeof data.reason === "string" ? { reason: data.reason } : {}),
				});
		}
		const pins = new Set<string>();
		for (const entry of path) {
			if (entry.type !== "custom" || entry.customType !== CONTEXT_PIN_CUSTOM_TYPE || !isRecord(entry.data)) continue;
			const target = entry.data.targetId;
			if (typeof target !== "string") continue;
			if (entry.data.pinned === false) pins.delete(target);
			else pins.add(target);
		}
		// The current user message, and a tool call still waiting for its result (the cell running now), are never edited.
		const protectedIds = new Set<string>();
		for (let index = window.length - 1; index >= 0; index -= 1) {
			const entry = window[index]!;
			if (entry.type === "message" && entry.message.role === "user") {
				protectedIds.add(entry.id);
				break;
			}
		}
		const results = new Set(
			window.flatMap((entry) =>
				entry.type === "message" && entry.message.role === "toolResult" ? [entry.message.toolCallId] : [],
			),
		);
		for (const entry of window) {
			if (entry.type === "message" && toolCallIds(entry.message).some((id) => !results.has(id)))
				protectedIds.add(entry.id);
		}
		return { path, compactionIndex, window, byId, applied, edits, pins, protectedIds };
	}

	#item(view: BranchView, entry: Entry): ContextItem | undefined {
		const kind = kindOf(entry);
		if (kind === undefined) return undefined;
		const visible = view.applied.get(entry.id);
		const omitted = visible?.type === "custom" && visible.customType === CONTEXT_OMITTED_CUSTOM_TYPE;
		const own = view.edits.get(entry.id);
		let state: ContextItemState = "visible";
		if (own !== undefined || omitted || visible !== entry) {
			if (own?.source === "collapse") state = "collapsed";
			else if (own?.source === "ctx.summarize") state = "summarized";
			else if (omitted || own?.edit.replacement === null) state = "forgotten";
			else state = "replaced";
		}
		const message = entry.type === "message" ? entry.message : undefined;
		return {
			id: entry.id,
			kind,
			...(message?.role === "toolResult" ? { tool: message.toolName } : {}),
			bytes: omitted || visible === undefined ? 0 : entryBytes(visible),
			original_bytes: entryBytes(entry),
			preview: oneLine(entryText(entry)),
			state,
			pinned: view.pins.has(entry.id),
			protected: view.protectedIds.has(entry.id),
			at: entry.timestamp,
		};
	}

	#visibleBytes(view: BranchView): number {
		let total = 0;
		for (const entry of view.window) {
			const visible = view.applied.get(entry.id);
			if (visible && !(visible.type === "custom" && visible.customType === CONTEXT_OMITTED_CUSTOM_TYPE))
				total += entryBytes(visible);
		}
		return total;
	}

	/** Refuse the whole request when any id is unknown, outside the context, protected, or pinned. */
	#guard(view: BranchView, ids: readonly string[]): void {
		const problems: string[] = [];
		for (const id of ids) {
			const entry = view.byId.get(id);
			const kind = entry === undefined ? undefined : kindOf(entry);
			if (entry === undefined)
				problems.push(
					`${id} is not in the model's context on this branch (unknown, or before the last compaction)`,
				);
			else if (kind === undefined || kind === "compaction" || kind === "summary")
				problems.push(`${id} is a ${entry.type} entry and cannot be edited`);
			else if (view.protectedIds.has(id))
				problems.push(`${id} belongs to the current user turn and cannot be edited`);
			else if (view.pins.has(id)) problems.push(`${id} is pinned (ctx.unpin it first)`);
		}
		if (problems.length > 0) throw new Error(`Context edit refused: ${problems.join("; ")}`);
	}

	/** Tool calls and their results move together in a summarized span. */
	#closure(view: BranchView, ids: readonly string[]): string[] {
		const out = new Set(ids);
		const callOwner = new Map<string, string>();
		const resultsOf = new Map<string, string[]>();
		for (const entry of view.window) {
			if (entry.type !== "message") continue;
			for (const call of toolCallIds(entry.message)) callOwner.set(call, entry.id);
			if (entry.message.role === "toolResult") {
				const owner = callOwner.get(entry.message.toolCallId);
				if (owner !== undefined) resultsOf.set(owner, [...(resultsOf.get(owner) ?? []), entry.id]);
			}
		}
		for (const id of ids) {
			const entry = view.byId.get(id);
			if (entry?.type !== "message") continue;
			if (entry.message.role === "toolResult") {
				const owner = callOwner.get(entry.message.toolCallId);
				if (owner !== undefined) out.add(owner);
			}
		}
		for (const id of [...out]) for (const result of resultsOf.get(id) ?? []) out.add(result);
		return [...out];
	}

	#scheduleCollapse(): void {
		this.#collapsing = this.#collapsing.then(() => this.#collapse().catch(() => {}));
	}

	/** Collapse cell outputs the model has already answered after. */
	async #collapse(): Promise<void> {
		if (this.#cells.size === 0) return;
		const lane = await this.#lane("main", BACKGROUND_CONTEXT);
		const view = await this.#view(lane, BACKGROUND_CONTEXT);
		const edits: Array<{ targetId: string; replacement: { content: string } }> = [];
		const tasks: string[] = [];
		let answeredAfter = false;
		for (let index = view.window.length - 1; index >= 0; index -= 1) {
			const entry = view.window[index]!;
			if (entry.type !== "message") continue;
			if (entry.message.role === "assistant") {
				answeredAfter = true;
				continue;
			}
			if (entry.message.role !== "toolResult" || !answeredAfter) continue;
			const lines = this.#cells.get(entry.message.toolCallId);
			if (lines === undefined) continue;
			this.#cells.delete(entry.message.toolCallId);
			if (view.pins.has(entry.id) || view.edits.has(entry.id)) continue;
			const original = messageText(entry.message);
			if (Buffer.byteLength(original) <= COLLAPSE_MIN_BYTES) continue;
			const content = collapsedCellText(entry.id, original, lines);
			if (content.length >= original.length) continue;
			edits.push({ targetId: entry.id, replacement: { content } });
			tasks.push(...lines.map((line) => line.id));
		}
		if (edits.length === 0) return;
		await this.#append(
			lane,
			CONTEXT_EDIT_CUSTOM_TYPE,
			{ edits: edits.reverse(), source: "collapse", reason: "task results returned", tasks },
			BACKGROUND_CONTEXT,
		);
	}

	/** Notes and pinned items that the newest compaction cut off, as one bounded text block. */
	async #keptAcrossCompaction(lane: AgentLane, context: Context): Promise<string | undefined> {
		const view = await this.#view(lane, context);
		if (view.compactionIndex === -1) return undefined;
		const compaction = view.path[view.compactionIndex]!;
		const retained = new Set(
			compaction.type === "compaction" ? compaction.retainedTail.map((message) => JSON.stringify(message)) : [],
		);
		const before = view.path.slice(0, view.compactionIndex);
		const omitted = new Set<string>();
		for (const entry of before)
			for (const edit of contextEditsOf(entry)) if (edit.replacement === null) omitted.add(edit.targetId);
		const lines: string[] = [];
		let budget = KEPT_TOTAL_CHARS;
		const add = (line: string) => {
			if (budget <= 0) return;
			const bounded = line.length > budget ? `${line.slice(0, budget - 1)}…` : line;
			lines.push(bounded);
			budget -= bounded.length;
		};
		for (const entry of before) {
			if (entry.type === "custom" && entry.customType === CONTEXT_NOTE_CUSTOM_TYPE && !omitted.has(entry.id)) {
				add(`- note ${entry.id}: ${noteText(entry)}`);
			} else if (
				view.pins.has(entry.id) &&
				entry.type === "message" &&
				!retained.has(JSON.stringify(entry.message))
			) {
				const text = messageText(entry.message);
				add(
					`- pinned ${entry.id} (${entry.message.role}): ${text.length > KEPT_ITEM_CHARS ? `${text.slice(0, KEPT_ITEM_CHARS - 1)}…` : text}`,
				);
			}
		}
		if (lines.length === 0) return undefined;
		return `Notes and pinned items kept across compaction (ctx.note / ctx.pin):\n${lines.join("\n")}`;
	}
}

function fields(payload: Record<string, unknown>, allowed: readonly string[]): void {
	for (const key of Object.keys(payload)) if (!allowed.includes(key)) throw new Error(`Unknown payload field: ${key}`);
}

function nonempty(value: unknown, name: string): string {
	if (typeof value !== "string" || !value.trim()) throw new Error(`${name} must be a nonempty string`);
	return value;
}

function idList(value: unknown): string[] {
	if (!Array.isArray(value) || value.length === 0) throw new Error("ids must be a nonempty list of item ids");
	const ids = value.map((id) => nonempty(id, "id"));
	if (ids.length > HISTORY_MAX) throw new Error(`at most ${HISTORY_MAX} ids per edit`);
	return [...new Set(ids)];
}
