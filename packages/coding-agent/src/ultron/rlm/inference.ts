/**
 * Bounded inference (`rlm.load`, `rlm.infer`, `rlm.map`, `rlm.frames`).
 *
 * The root never reads a large input: `rlm.load` interns it in the kernel and content-addresses it on disk
 * under the session; the root slices it in code and hands explicit views to *inference frames*. A frame is an
 * `rlm-frame@1` task admitted through the host like `agents.invoke` (task journal, admission and wall budget,
 * cancellation cascade, usage ledger), run on its own lane with its own system prompt, no parent transcript,
 * and no tools (or only the `rlm` cell when its budget allows depth). Its reply is validated against the
 * caller's JSON-schema contract and re-asked within budget; running out yields an `Incomplete` observation.
 *
 * Every frame draws on a budget subtree `{calls, tokens, depth}`. Each provider request holds one call and
 * an input estimate plus an output tranche (at most a quarter of the remaining pool and 16k tokens, which also
 * caps the request's max output tokens), then settles against the provider-reported usage (cache reads
 * charged at a tenth) and releases the hold; a failed request charges only what the provider reported.
 * Holds are charged up the whole tree at once, so concurrent siblings cannot oversubscribe a shared pool.
 */
import { randomUUID } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { type AgentHarness, type AgentLane, type Context, type Entry, type Session, value } from "@ultron/agent-core";
import { isJsonValue, type JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import type { TSchema } from "typebox";
import { Check, Errors } from "typebox/value";
import type { NativeUsageCallStatus, NativeUsageLedgerLike, NativeUsageMeasurement } from "../usage.ts";
import { validateJsonSchema } from "./definition-registry.ts";
import type { HostCaller, NativeHostApi, NativeHostModule } from "./host-module.ts";
import type { NativeFrameExecutor, NativeFrameRun, NativeResult } from "./native-host.ts";

/**
 * API description for the model. It belongs in the system prompt's Runtime section; until then it is appended
 * to the `rlm` tool description.
 */
export const INFERENCE_PROMPT = [
	"Bounded inference: never read a large input into your context; load it as a handle and program over it.",
	"- `h = await rlm.load(path_or_text)` returns a ContextHandle (`h.label`, `h.size`, `h.digest`; printing it never shows content). `h.length()`, `h.slice(a, b)`, `h.lines(a, b)` (0-based, end-exclusive), `h.search(regex, limit=20)` -> [{start, end, line, text}], `h.count(regex)`, `h.chunks(chars, overlap=0)` (line-aligned). Views print their text: print only what you must read.",
	"- `v = await rlm.infer(task, context=[view, 'literal', ...], contract=schema_or_type, budget=Budget(calls, tokens, depth))` asks a private sub-model that sees only the task and those views (no transcript, no tools) and returns the contract-validated value (a JSON schema, or int/str/float/bool/list/dict/list[T]); without a contract, the reply text. Bad answers are re-asked within budget (`max_repairs=2`); running out returns an `Incomplete` (falsy: `.status`, `.spent`, `.remaining`, `.trace_id`, `.last_outputs`), not an exception.",
	"- `vs = await rlm.map(task, items, contract=..., budget=...)` runs one frame per item (a view or a list of views) under one shared budget, results in order; failed items are `Incomplete` or `FrameError` entries. `await rlm.frames()` lists frame traces.",
	"- Compute exact numbers in plain Python over the handle (`h.count`, `csv` over `h.lines`); use frames to read and judge text.",
	"Example (log forensics): `h = await rlm.load('app.log'); hits = h.search(r'ERROR .*timeout', limit=8); causes = await rlm.map('Root cause of this failure, 10 words max.', [h.lines(m['line'] - 20, m['line'] + 5) for m in hits], contract=str)`",
	"Example (huge CSV): `h = await rlm.load('orders.csv'); head = h.lines(0, 1).text; ids = await rlm.map('Header: ' + head + ' Return the order_id of every row whose comment is a refund complaint.', h.chunks(40000), contract=list[str], budget=Budget(calls=120)); refunds = [i for part in ids if isinstance(part, list) for i in part]`",
].join("\n");

export const RLM_FRAME_DEFINITION = "rlm-frame@1";
const MAX_TRANCHE = 16_384;
const MIN_TRANCHE = 32;
const CACHE_READ_WEIGHT = 0.1;
/** Added to every input estimate for message framing the character count does not see. */
const REQUEST_OVERHEAD_TOKENS = 64;
const OUTPUT_PREVIEW_CHARS = 2_000;
const LAST_OUTPUTS = 3;
const MAX_TASK_CHARS = 65_536;
const MAX_FRAME_CONTEXT_CHARS = 4_000_000;
const MAX_MAP_FRAMES = 10_000;
const MAX_CONCURRENCY = 16;
const MAX_REPAIRS = 8;
const MAX_DEPTH = 4;
const DEFAULT_TIMEOUT_MS = 10 * 60 * 1000;
const TRACE_INDEX_LIMIT = 200;
const TRACE_REQUEST_LIMIT = 50;
const DIGEST = /^sha256:([0-9a-f]{64})$/;

type Payload = Record<string, unknown>;
type PoolKind = "calls" | "tokens";
type Pool = { limit: number | null; spent: number; held: number };

/**
 * One node of a budget subtree. Holds and charges apply to the node and every ancestor, so a node's
 * availability is the tightest remaining pool on its path to the root.
 */
export class BudgetNode {
	readonly id: string;
	readonly depth: number;
	readonly parent: BudgetNode | undefined;
	readonly calls: Pool;
	readonly tokens: Pool;

	constructor(
		id: string,
		limits: { calls: number | null; tokens: number | null },
		depth: number,
		parent?: BudgetNode,
	) {
		this.id = id;
		this.depth = depth;
		this.parent = parent;
		this.calls = { limit: limits.calls, spent: 0, held: 0 };
		this.tokens = { limit: limits.tokens, spent: 0, held: 0 };
	}

	private *chain(): Generator<BudgetNode> {
		for (let node: BudgetNode | undefined = this; node; node = node.parent) yield node;
	}

	/** Remaining capacity (Infinity when no node on the path limits it). */
	available(kind: PoolKind): number {
		let remaining = Number.POSITIVE_INFINITY;
		for (const node of this.chain()) {
			const pool = node[kind];
			if (pool.limit !== null) remaining = Math.min(remaining, pool.limit - pool.spent - pool.held);
		}
		return Math.max(0, remaining);
	}

	hold(kind: PoolKind, amount: number): boolean {
		if (amount > this.available(kind)) return false;
		for (const node of this.chain()) node[kind].held += amount;
		return true;
	}

	release(kind: PoolKind, amount: number): void {
		for (const node of this.chain()) node[kind].held = Math.max(0, node[kind].held - amount);
	}

	charge(kind: PoolKind, amount: number): void {
		for (const node of this.chain()) node[kind].spent += amount;
	}

	snapshot(): {
		id: string;
		parent: string | null;
		depth: number;
		limits: { calls: number | null; tokens: number | null };
		spent: { calls: number; tokens: number };
	} {
		return {
			id: this.id,
			parent: this.parent?.id ?? null,
			depth: this.depth,
			limits: { calls: this.calls.limit, tokens: this.tokens.limit },
			spent: { calls: this.calls.spent, tokens: this.tokens.spent },
		};
	}

	remaining(): { calls: number | null; tokens: number | null; depth: number } {
		const finite = (amount: number) => (Number.isFinite(amount) ? amount : null);
		return { calls: finite(this.available("calls")), tokens: finite(this.available("tokens")), depth: this.depth };
	}
}

type FrameView = {
	label: string;
	chars: number;
	/** Materialized text; absent for a handle passed by reference, and dropped once the frame ends. */
	text?: string;
	byReference?: boolean;
	digest?: string;
	start?: number;
	end?: number;
};

type FrameSpec = {
	task: string;
	views: FrameView[];
	contract?: JsonValue;
	maxRepairs: number;
	depth: number;
	model?: string;
	timeoutMs: number;
};

type IncompleteReason = "budget_exhausted" | "contract_unmet" | "depth_exhausted";

type FrameOutcome =
	| { status: "complete"; value: JsonValue }
	| { status: "incomplete"; reason: IncompleteReason; detail: string }
	| { status: "error"; error: string };

/** One provider request's reservation: a call, the input estimate, and the output tranche. */
type Hold = { tokens: number; tranche: number };

type FrameState = {
	id: string;
	node: BudgetNode;
	spec: FrameSpec;
	callerTaskId: string | null;
	parentFrame?: string;
	taskId?: string;
	laneName?: string;
	lane?: AgentLane;
	model?: string;
	/** The call claimed at admission, not yet used by the first request. */
	callHeld: boolean;
	/** Hold claimed for the next request of the current prompt (consumed by the request hook). */
	pending?: Hold;
	/** Requests sent and not yet settled. */
	open: Hold[];
	/** Requests settled by the response hook during the current prompt. */
	settled: { requests: number; tokens: number };
	conversationChars: number;
	/** Input-token estimate of the frame's latest request, used to size the next one before it is built. */
	lastEstimate?: number;
	exhausted?: string;
	outputs: string[];
	attempts: Array<{ attempt: number; at: number; output: string; error?: string }>;
	requests: Array<{ at: number; tranche: number; held: number; charged: number; status: string }>;
	outcome?: FrameOutcome;
	startedAt: number;
	endedAt?: number;
	finalized?: boolean;
	/** `rlm.map` frames share one budget node; `batch` is the map's frame count (1 for `rlm.infer`). */
	kind: "infer" | "map";
	batch: number;
};

/** Where frame traces live: session values `ultron.rlm.frames/<id>` (and an index at `ultron.rlm.frames/index`). */
export type FrameTraceStore = {
	read(id: string): Promise<JsonValue | undefined>;
	write(id: string, document: JsonValue): Promise<void>;
};

export function createSessionFrameStore(session: Pick<Session, "getValue" | "setValue">): FrameTraceStore {
	return {
		read: async (id) =>
			(await session.getValue(value<JsonValue>("ultron.rlm.frames", id), BACKGROUND_CONTEXT))?.value,
		write: (id, document) =>
			session.setValue(value<JsonValue>("ultron.rlm.frames", id), document, BACKGROUND_CONTEXT),
	};
}

export function createMemoryFrameStore(): FrameTraceStore & { documents: Map<string, JsonValue> } {
	const documents = new Map<string, JsonValue>();
	return {
		documents,
		read: async (id) => (documents.has(id) ? structuredClone(documents.get(id)) : undefined),
		write: async (id, document) => {
			documents.set(id, structuredClone(document));
		},
	};
}

export type InferenceRuntimeOptions = {
	/** Content-addressed store for loaded handles (a directory under the session). */
	contextDir: string;
	traces?: FrameTraceStore;
	usage?: NativeUsageLedgerLike;
	now?: () => number;
};

function objectPayload(item: unknown, name: string): Payload {
	if (item === null || typeof item !== "object" || Array.isArray(item)) throw new Error(`${name} must be an object`);
	return item as Payload;
}

function fields(payload: Payload, allowed: readonly string[]): void {
	for (const key of Object.keys(payload)) if (!allowed.includes(key)) throw new Error(`Unknown payload field: ${key}`);
}

function optionalInteger(item: unknown, name: string, minimum: number, maximum: number): number | undefined {
	if (item === undefined || item === null) return undefined;
	if (typeof item !== "number" || !Number.isSafeInteger(item) || item < minimum || item > maximum)
		throw new Error(`${name} must be an integer between ${minimum} and ${maximum}`);
	return item;
}

/** A budget node's id and limits (the frames' shared pool), for frame summaries. */
function budgetSummary(node: BudgetNode | undefined): JsonValue {
	if (node === undefined) return null;
	return { id: node.id, calls: node.calls.limit, tokens: node.tokens.limit, depth: node.depth };
}

function bounded(text: string, limit: number): string {
	return text.length <= limit ? text : `${text.slice(0, limit - 1)}…`;
}

function chargedTokens(usage: unknown): number {
	const item = usage as
		| { input?: unknown; output?: unknown; cacheRead?: unknown; cacheWrite?: unknown }
		| undefined
		| null;
	const count = (entry: unknown) => (typeof entry === "number" && Number.isFinite(entry) && entry > 0 ? entry : 0);
	if (!item) return 0;
	return (
		count(item.input) +
		count(item.output) +
		count(item.cacheWrite) +
		Math.ceil(count(item.cacheRead) * CACHE_READ_WEIGHT)
	);
}

function assistantMessages(entries: readonly Entry[]) {
	return entries.flatMap((entry) =>
		entry.type === "message" && entry.message.role === "assistant" ? [entry.message] : [],
	);
}

function replyText(entry: Entry | undefined): string {
	if (!entry || entry.type !== "message" || entry.message.role !== "assistant") return "";
	return entry.message.content
		.filter((part) => part.type === "text")
		.map((part) => part.text)
		.join("\n");
}

/** Provider-reported usage of a prompt's assistant messages; unknown fields stay null. */
function measurement(entries: readonly Entry[]): NativeUsageMeasurement | undefined {
	const messages = assistantMessages(entries);
	if (messages.length === 0) return undefined;
	const total = { inputTokens: 0, outputTokens: 0, totalTokens: 0, cost: 0 };
	for (const message of messages) {
		const usage = (
			message as { usage?: { input?: number; output?: number; totalTokens?: number; cost?: { total?: number } } }
		).usage;
		if (!usage) return undefined;
		total.inputTokens += usage.input ?? 0;
		total.outputTokens += usage.output ?? 0;
		total.totalTokens += usage.totalTokens ?? 0;
		total.cost += usage.cost?.total ?? 0;
	}
	return total;
}

/**
 * Cap a provider payload's output tokens at the tranche. Known fields are lowered in place; an
 * OpenAI-compatible payload without one gets one, since an uncapped reply could overrun the hold.
 */
export function capOutputTokens(payload: unknown, limit: number, api?: string, provider?: string): unknown {
	if (payload === null || typeof payload !== "object" || Array.isArray(payload)) return payload;
	const body = payload as Record<string, unknown>;
	let capped = false;
	const cap = (holder: Record<string, unknown>, key: string, floor = 1) => {
		if (typeof holder[key] !== "number") return;
		holder[key] = Math.max(floor, Math.min(holder[key] as number, limit));
		capped = true;
	};
	cap(body, "max_tokens");
	cap(body, "max_completion_tokens");
	cap(body, "max_output_tokens", 16);
	cap(body, "maxTokens");
	for (const nested of ["generationConfig", "inferenceConfig", "config"]) {
		const holder = body[nested];
		if (holder !== null && typeof holder === "object" && !Array.isArray(holder)) {
			cap(holder as Record<string, unknown>, "maxOutputTokens");
			cap(holder as Record<string, unknown>, "maxTokens");
		}
	}
	// Anthropic requires max_tokens above the thinking budget (itself at least 1024).
	const thinking = body.thinking as { type?: unknown; budget_tokens?: unknown } | undefined;
	const budget = thinking?.budget_tokens;
	if (thinking?.type === "enabled" && typeof budget === "number" && typeof body.max_tokens === "number") {
		const adjusted = Math.max(1024, Math.min(budget, body.max_tokens - 1));
		thinking.budget_tokens = adjusted;
		body.max_tokens = Math.max(body.max_tokens, adjusted + 1);
	}
	if (!capped && api === "openai-completions")
		body[provider === "openai" || provider === "azure-openai" ? "max_completion_tokens" : "max_tokens"] = limit;
	return payload;
}

function frameSystemPrompt(depth: number): string {
	const base =
		"You are an inference frame inside Ultron: a private, bounded sub-call made by a program. You see only the task and the context views in the user message; there is no earlier conversation and no files. Answer from the views alone and never invent content that is not in them. When a contract (JSON schema) is given, reply with exactly one JSON value that satisfies it: no prose, no code fences. Without a contract, reply with the answer only.";
	return depth > 1
		? `${base}\n\nYou have one tool, \`rlm\`: a Python cell. \`h = await rlm.open(digest)\` reopens a context handle listed in the message (then \`h.slice\`, \`h.lines\`, \`h.search\`, \`h.chunks\`), and \`await rlm.infer(...)\` / \`await rlm.map(...)\` delegate smaller frames within your remaining budget (depth ${depth - 1} below you). Read only what you need; finish with the answer as your reply.`
		: `${base} You have no tools.`;
}

function framePrompt(spec: FrameSpec): string {
	const parts = [`Task:\n${spec.task}`];
	const materialized = spec.views.filter((view) => !view.byReference);
	const references = spec.views.filter((view) => view.byReference);
	if (spec.views.length === 0) parts.push("Context: none.");
	else {
		const chars = materialized.reduce((sum, view) => sum + view.chars, 0);
		parts.push(`Context: ${materialized.length} view(s), ${chars} characters.`);
		materialized.forEach((view, index) => {
			parts.push(
				`--- view ${index + 1}: ${view.label} (${view.chars} chars) ---\n${view.text}\n--- end of view ${index + 1} ---`,
			);
		});
		if (references.length > 0)
			parts.push(
				`Handles (open them in the rlm tool):\n${references.map((view) => `- ${view.label}: await rlm.open("${view.digest}") (${view.chars} characters)`).join("\n")}`,
			);
	}
	if (spec.contract !== undefined)
		parts.push(
			`Contract (JSON schema): ${JSON.stringify(spec.contract)}\nReply with only a JSON value that satisfies the contract.`,
		);
	return parts.join("\n\n");
}

function isStringContract(contract: JsonValue): boolean {
	return (
		contract !== null &&
		typeof contract === "object" &&
		!Array.isArray(contract) &&
		contract.type === "string" &&
		Object.keys(contract).every((key) => ["type", "description", "title"].includes(key))
	);
}

/** Parse a reply against a contract: the value, or the reason to re-ask. */
function parseReply(text: string, contract: JsonValue): { ok: true; value: JsonValue } | { ok: false; error: string } {
	const cleaned = text
		.trim()
		.replace(/^```(?:json)?\s*\n([\s\S]*?)\n?```$/, "$1")
		.trim();
	let parsed: unknown;
	let isJson = true;
	try {
		parsed = JSON.parse(cleaned);
	} catch {
		isJson = false;
		const start = cleaned.search(/[[{]/);
		const end = Math.max(cleaned.lastIndexOf("}"), cleaned.lastIndexOf("]"));
		if (start !== -1 && end > start) {
			try {
				parsed = JSON.parse(cleaned.slice(start, end + 1));
				isJson = true;
			} catch {}
		}
	}
	// A string contract takes the reply as-is unless it is a JSON string.
	if (isStringContract(contract) && (!isJson || typeof parsed !== "string")) return { ok: true, value: cleaned };
	if (!isJson) return { ok: false, error: "the reply is not JSON" };
	if (!isJsonValue(parsed)) return { ok: false, error: "the reply is not a JSON value" };
	if (Check(contract as TSchema, parsed)) return { ok: true, value: parsed };
	const errors = Errors(contract as TSchema, parsed)
		.slice(0, 3)
		.map((error) => `${error.instancePath || "value"} ${error.message}`);
	return { ok: false, error: errors.length ? errors.join("; ") : "the value does not match the contract" };
}

function repairPrompt(error: string, contract: JsonValue): string {
	return `Your reply does not satisfy the contract: ${error}.\nReply again with only a JSON value that satisfies this schema: ${JSON.stringify(contract)}`;
}

type FrameRequest = { task: string; context: FrameView[] };

export class InferenceRuntime {
	readonly module: NativeHostModule;
	readonly executor: NativeFrameExecutor;
	private readonly contextDir: string;
	private readonly traces: FrameTraceStore | undefined;
	private readonly usage: NativeUsageLedgerLike | undefined;
	private readonly now: () => number;
	private readonly frames = new Map<string, FrameState>();
	private readonly byLane = new Map<string, FrameState>();
	private readonly byTask = new Map<string, FrameState>();
	private readonly handles = new Map<string, { label: string; size: number; chars: number; loadedAt: number }>();
	private index: JsonValue[] | undefined;
	private traceWrites: Promise<void> = Promise.resolve();
	private readonly pendingSummaries = new Map<string, JsonValue>();
	private indexQueued = false;

	constructor(options: InferenceRuntimeOptions) {
		this.contextDir = options.contextDir;
		this.traces = options.traces;
		this.usage = options.usage;
		this.now = options.now ?? Date.now;
		this.module = {
			prefixes: ["rlm.load", "rlm.infer", "rlm.map", "rlm.frames"],
			handle: ({ type, payload, caller, context }, host) => {
				if (type === "rlm.load") return this.load(payload);
				if (type === "rlm.infer") return this.infer(payload, caller, context, host);
				if (type === "rlm.map") return this.map(payload, caller, context, host);
				if (type === "rlm.frames") return this.list(payload);
				throw new Error(`Ultron RLM host request is not wired: ${type}`);
			},
		};
		this.executor = (run) => this.execute(run);
	}

	/**
	 * Per-request budget accounting on frame lanes: the frame's own system prompt, the tranche hold and output
	 * cap before each provider request, and settlement against the reported usage after it. Returns the remover.
	 */
	install(harness: Pick<AgentHarness, "hooks">): () => void {
		const removers = [
			harness.hooks.on("transform_context", (event) => {
				const frame = this.byLane.get(event.lane);
				return frame ? { systemPrompt: frameSystemPrompt(frame.spec.depth) } : undefined;
			}),
			// A retry or tool round that the budget cannot cover is refused before it is sent (the run fails with
			// request_blocked and the frame reports Incomplete); before_payload still guards estimate misses.
			harness.hooks.on("before_request", (event) => {
				const frame = this.byLane.get(event.lane);
				if (!frame || frame.pending || frame.lastEstimate === undefined) return undefined;
				if (this.canCover(frame.node, frame.lastEstimate, event.model.maxTokens)) return undefined;
				frame.exhausted = "budget exhausted before a follow-up request (a retry or tool round)";
				return { block: { reason: `Inference frame ${frame.id}: ${frame.exhausted}` } };
			}),
			harness.hooks.on("before_payload", (event) => {
				const frame = this.byLane.get(event.lane);
				return frame ? { payload: this.beforePayload(frame, event.payload, event.model) } : undefined;
			}),
			harness.hooks.on("after_response", (event) => {
				const frame = this.byLane.get(event.lane);
				if (frame) this.afterResponse(frame, event.message);
				return undefined;
			}),
		];
		return () => {
			for (const remove of removers) remove();
		};
	}

	/** Hook: consume the prompt's pre-claimed hold, or claim one for a retry or tool round, and cap output. */
	beforePayload(frame: FrameState, payload: unknown, model?: { api?: string; provider?: string; maxTokens?: number }) {
		let hold = frame.pending;
		frame.pending = undefined;
		const estimate = Math.ceil(JSON.stringify(payload ?? null).length / 4) + REQUEST_OVERHEAD_TOKENS;
		frame.lastEstimate = estimate;
		if (!hold) {
			hold = this.claim(frame.node, estimate, model?.maxTokens, false);
			if (!hold) {
				// The payload outgrew the before_request estimate: stop the lane and cap what still goes out to one token.
				frame.exhausted = "budget exhausted before a follow-up request (a retry or tool round)";
				void frame.lane?.abort(BACKGROUND_CONTEXT).catch(() => {});
				return capOutputTokens(payload, 1, model?.api, model?.provider);
			}
		}
		frame.open.push(hold);
		return capOutputTokens(payload, hold.tranche, model?.api, model?.provider);
	}

	/** Hook: settle the oldest open request against its reported usage and release its hold. */
	afterResponse(frame: FrameState, message: { usage?: unknown; stopReason?: string }): void {
		const hold = frame.open.shift();
		if (!hold) return;
		const charged = chargedTokens(message.usage);
		frame.node.release("calls", 1);
		frame.node.release("tokens", hold.tokens);
		frame.node.charge("calls", 1);
		frame.node.charge("tokens", charged);
		frame.settled.requests += 1;
		frame.settled.tokens += charged;
		if (frame.requests.length < TRACE_REQUEST_LIMIT)
			frame.requests.push({
				at: this.now(),
				tranche: hold.tranche,
				held: hold.tokens,
				charged,
				status: message.stopReason ?? "unknown",
			});
	}

	/** Whether a request of about `estimate` input tokens fits: one call and a minimum output tranche. */
	private canCover(node: BudgetNode, estimate: number, modelMax: number | undefined): boolean {
		if (node.available("calls") < 1) return false;
		const available = node.available("tokens");
		if (!Number.isFinite(available)) return true;
		const ceiling = Math.min(MAX_TRANCHE, modelMax && modelMax > 0 ? modelMax : MAX_TRANCHE);
		return Math.min(ceiling, Math.floor(available / 4), available - estimate) >= MIN_TRANCHE;
	}

	/**
	 * Claim one call (unless already held) and `estimate` input tokens plus an output tranche of at most a quarter
	 * of the remaining token pool and 16k. Returns undefined, holding nothing, when the budget cannot cover it.
	 */
	private claim(
		node: BudgetNode,
		estimate: number,
		modelMax: number | undefined,
		callHeld: boolean,
	): Hold | undefined {
		if (!callHeld && !node.hold("calls", 1)) return undefined;
		const available = node.available("tokens");
		const ceiling = Math.min(MAX_TRANCHE, modelMax && modelMax > 0 ? modelMax : MAX_TRANCHE);
		const tranche = Number.isFinite(available)
			? Math.min(ceiling, Math.floor(available / 4), available - estimate)
			: ceiling;
		if (tranche < MIN_TRANCHE || !node.hold("tokens", estimate + tranche)) {
			node.release("calls", 1);
			return undefined;
		}
		return { tokens: estimate + tranche, tranche };
	}

	/** After a prompt: release unsettled holds and charge usage the hooks did not see (the fallback path). */
	private settlePrompt(frame: FrameState, entries: readonly Entry[]): void {
		const messages = assistantMessages(entries);
		const runCharged = messages.reduce(
			(sum, message) => sum + chargedTokens((message as { usage?: unknown }).usage),
			0,
		);
		const extraTokens = Math.max(0, runCharged - frame.settled.tokens);
		const extraCalls = Math.max(0, messages.length - frame.settled.requests);
		for (const hold of [frame.pending, ...frame.open]) {
			if (!hold) continue;
			frame.node.release("calls", 1);
			frame.node.release("tokens", hold.tokens);
		}
		frame.pending = undefined;
		frame.open = [];
		if (extraCalls > 0) frame.node.charge("calls", extraCalls);
		if (extraTokens > 0) frame.node.charge("tokens", extraTokens);
		if ((extraCalls > 0 || extraTokens > 0) && frame.requests.length < TRACE_REQUEST_LIMIT)
			frame.requests.push({ at: this.now(), tranche: 0, held: 0, charged: extraTokens, status: "settled-from-run" });
		frame.settled = { requests: 0, tokens: 0 };
	}

	private async load(payload: Payload): Promise<JsonValue> {
		fields(payload, ["digest", "label", "size", "chars", "source"]);
		const digest = typeof payload.digest === "string" ? DIGEST.exec(payload.digest) : null;
		if (!digest) throw new Error("digest must be sha256:<64 hex>");
		const path = join(this.contextDir, digest[1]!);
		if (payload.size === undefined) {
			// Reopen by digest.
			if (!existsSync(path)) throw new Error(`Unknown context handle ${payload.digest}`);
			const known = this.handles.get(payload.digest as string);
			return { path, stored: true, label: known?.label ?? null, size: statSync(path).size };
		}
		const size = optionalInteger(payload.size, "size", 0, Number.MAX_SAFE_INTEGER)!;
		const chars = optionalInteger(payload.chars, "chars", 0, Number.MAX_SAFE_INTEGER) ?? size;
		const label = typeof payload.label === "string" && payload.label.trim() ? bounded(payload.label, 200) : "context";
		mkdirSync(this.contextDir, { recursive: true, mode: 0o700 });
		const stored = existsSync(path) && statSync(path).size === size;
		this.handles.set(payload.digest as string, { label, size, chars, loadedAt: this.now() });
		return { path, stored };
	}

	private views(items: unknown, depth: number): FrameView[] {
		if (!Array.isArray(items)) throw new Error("context must be a list");
		let total = 0;
		const views = items.map((raw): FrameView => {
			const item = objectPayload(raw, "context item");
			const label = typeof item.label === "string" ? bounded(item.label, 200) : "context";
			if (item.kind === "text") {
				if (typeof item.text !== "string") throw new Error("context text must be a string");
				total += item.text.length;
				return {
					label,
					chars: item.text.length,
					text: item.text,
					...(typeof item.digest === "string" ? { digest: item.digest } : {}),
					...(typeof item.start === "number" ? { start: item.start } : {}),
					...(typeof item.end === "number" ? { end: item.end } : {}),
				};
			}
			if (item.kind !== "handle") throw new Error("context item kind must be text or handle");
			const digest = typeof item.digest === "string" ? DIGEST.exec(item.digest) : null;
			if (!digest) throw new Error("context handle digest must be sha256:<64 hex>");
			const path = join(this.contextDir, digest[1]!);
			if (!existsSync(path)) throw new Error(`Unknown context handle ${item.digest}`);
			const chars = typeof item.chars === "number" ? item.chars : statSync(path).size;
			// A frame that can run code gets the whole handle by reference; one that cannot gets its text.
			if (depth > 1) return { label, chars, digest: item.digest as string, byReference: true };
			if (statSync(path).size > MAX_FRAME_CONTEXT_CHARS * 4)
				throw new Error(`Context handle ${label} is too large to give a frame whole; pass slices or chunks`);
			const text = readFileSync(path, "utf8");
			total += text.length;
			return { label, chars: text.length, text, digest: item.digest as string };
		});
		if (total > MAX_FRAME_CONTEXT_CHARS)
			throw new Error(
				`A frame's context is ${total} characters (limit ${MAX_FRAME_CONTEXT_CHARS}); pass slices or chunks`,
			);
		return views;
	}

	private options(payload: Payload, caller: HostCaller, host: NativeHostApi) {
		const contract = payload.contract ?? undefined;
		if (contract !== undefined) validateJsonSchema(contract, "$.contract");
		const budget = payload.budget == null ? {} : objectPayload(payload.budget, "budget");
		fields(budget, ["calls", "tokens", "depth"]);
		const calls = optionalInteger(budget.calls, "budget.calls", 0, Number.MAX_SAFE_INTEGER) ?? null;
		const tokens = optionalInteger(budget.tokens, "budget.tokens", 0, Number.MAX_SAFE_INTEGER) ?? null;
		const requestedDepth = optionalInteger(budget.depth, "budget.depth", 1, MAX_DEPTH);
		let model: string | undefined;
		if (payload.model != null) {
			if (typeof payload.model !== "string" || !/^[^/\s]+\/[^/\s]+(?:\/[^/\s]+)*$/.test(payload.model))
				throw new Error("model must be provider/model");
			model = payload.model;
		}
		const maxRepairs = optionalInteger(payload.max_repairs, "max_repairs", 0, MAX_REPAIRS) ?? 2;
		const timeoutMs = optionalInteger(payload.timeout_ms, "timeout_ms", 1, 60 * 60 * 1000) ?? DEFAULT_TIMEOUT_MS;
		const callerTaskId = host.callerTaskId(caller);
		const parentFrame = callerTaskId === null ? undefined : this.byTask.get(callerTaskId);
		// A nested request gets at most one level less than its frame; the root defaults to depth 1.
		const ceiling = parentFrame ? parentFrame.node.depth - 1 : MAX_DEPTH;
		const depth = Math.min(requestedDepth ?? (parentFrame ? ceiling : 1), ceiling);
		const node = new BudgetNode(`budget-${randomUUID()}`, { calls, tokens }, Math.max(depth, 0), parentFrame?.node);
		return {
			contract: contract as JsonValue | undefined,
			node,
			depth,
			model,
			maxRepairs,
			timeoutMs,
			callerTaskId,
			parentFrame,
		};
	}

	private newFrame(request: FrameRequest, options: ReturnType<InferenceRuntime["options"]>): FrameState {
		const id = `frame-${randomUUID()}`;
		const frame: FrameState = {
			id,
			node: new BudgetNode(id, { calls: null, tokens: null }, Math.max(options.depth, 0), options.node),
			spec: {
				task: request.task,
				views: request.context,
				...(options.contract === undefined ? {} : { contract: options.contract }),
				maxRepairs: options.maxRepairs,
				depth: options.depth,
				...(options.model === undefined ? {} : { model: options.model }),
				timeoutMs: options.timeoutMs,
			},
			callerTaskId: options.callerTaskId,
			...(options.parentFrame ? { parentFrame: options.parentFrame.id } : {}),
			callHeld: false,
			open: [],
			settled: { requests: 0, tokens: 0 },
			conversationChars: 0,
			outputs: [],
			attempts: [],
			requests: [],
			startedAt: this.now(),
			kind: "infer",
			batch: 1,
		};
		this.frames.set(id, frame);
		if (options.depth < 1)
			frame.outcome = {
				status: "incomplete",
				reason: "depth_exhausted",
				detail: "this frame's budget allows no nested frames",
			};
		// The first request's call is claimed in order at admission, so a shared pool decides deterministically
		// which frames run.
		else if (options.node.hold("calls", 1)) frame.callHeld = true;
		else
			frame.outcome = {
				status: "incomplete",
				reason: "budget_exhausted",
				detail: "no call left in the budget for this frame",
			};
		return frame;
	}

	private request(raw: unknown, depth: number): FrameRequest {
		const item = objectPayload(raw, "frame");
		const task = item.task;
		if (typeof task !== "string" || !task.trim()) throw new Error("task must be a nonempty string");
		if (task.length > MAX_TASK_CHARS)
			throw new Error(`task exceeds ${MAX_TASK_CHARS} characters; pass data as context`);
		return { task, context: this.views(item.context ?? [], depth) };
	}

	private async infer(payload: Payload, caller: HostCaller, context: Context, host: NativeHostApi) {
		fields(payload, ["task", "context", "contract", "budget", "model", "max_repairs", "timeout_ms"]);
		const options = this.options(payload, caller, host);
		const frame = this.newFrame(
			this.request({ task: payload.task, context: payload.context }, options.depth),
			options,
		);
		await this.runFrame(frame, context, host);
		return this.observation(frame);
	}

	private async map(payload: Payload, caller: HostCaller, context: Context, host: NativeHostApi) {
		fields(payload, ["frames", "contract", "budget", "model", "max_repairs", "concurrency", "timeout_ms"]);
		if (!Array.isArray(payload.frames)) throw new Error("frames must be a list");
		if (payload.frames.length > MAX_MAP_FRAMES) throw new Error(`rlm.map takes at most ${MAX_MAP_FRAMES} frames`);
		const concurrency = optionalInteger(payload.concurrency, "concurrency", 1, MAX_CONCURRENCY) ?? 8;
		const options = this.options(payload, caller, host);
		// Validate every item before any frame runs or any budget is touched.
		const requests = payload.frames.map((item) => this.request(item, options.depth));
		const frames = requests.map((request) => this.newFrame(request, options));
		for (const frame of frames) {
			frame.kind = "map";
			frame.batch = frames.length;
		}
		let next = 0;
		const worker = async () => {
			while (next < frames.length) {
				const frame = frames[next++]!;
				await this.runFrame(frame, context, host);
			}
		};
		await Promise.all(Array.from({ length: Math.min(concurrency, frames.length) }, worker));
		return {
			results: frames.map((frame) => this.observation(frame)),
			budget: { ...options.node.snapshot(), remaining: options.node.remaining() },
		};
	}

	/** Admit the frame as an `rlm-frame@1` task under the caller, wait for its result, and cancel it with the cell. */
	private async runFrame(frame: FrameState, context: Context, host: NativeHostApi): Promise<void> {
		try {
			if (frame.outcome) return;
			if (context.abortSignal?.aborted) {
				frame.outcome = { status: "error", error: "cancelled: the calling cell was aborted" };
				return;
			}
			let taskId: string;
			try {
				const task = await host.spawn(
					{
						definition: RLM_FRAME_DEFINITION,
						input: { frame: frame.id, task: bounded(frame.spec.task, 200) },
						...(frame.spec.model === undefined ? {} : { model: frame.spec.model }),
						timeoutMs: frame.spec.timeoutMs,
					},
					frame.callerTaskId,
					context,
				);
				taskId = task.id;
			} catch (error) {
				frame.outcome = {
					status: "error",
					error: `Admission refused: ${error instanceof Error ? error.message : String(error)}`,
				};
				return;
			}
			frame.taskId = taskId;
			this.byTask.set(taskId, frame);
			// Frames are admitted as module work (detached from the cell); the cell's abort still stops them.
			const onAbort = () => {
				void host.cancel(taskId, "Inference cancelled: the calling cell was aborted").catch(() => {});
			};
			context.abortSignal?.addEventListener("abort", onAbort, { once: true });
			if (context.abortSignal?.aborted) onAbort();
			try {
				const result = await host.result(taskId);
				frame.outcome ??=
					result.status === "succeeded"
						? { status: "complete", value: result.value ?? null }
						: { status: "error", error: `${result.status}: ${result.error ?? "frame did not finish"}` };
			} finally {
				context.abortSignal?.removeEventListener("abort", onAbort);
			}
		} finally {
			if (frame.callHeld) {
				frame.callHeld = false;
				frame.node.release("calls", 1);
			}
			await this.finalize(frame);
		}
	}

	private async execute(run: NativeFrameRun): Promise<NativeResult> {
		const input = run.input as { frame?: unknown } | null;
		const frame = typeof input?.frame === "string" ? this.frames.get(input.frame) : undefined;
		if (!frame)
			return {
				status: "failed",
				error: "rlm-frame tasks are started only by rlm.infer and rlm.map",
				verification: "unverified",
			};
		frame.taskId = run.taskId;
		frame.lane = run.lane;
		frame.laneName = run.laneName;
		this.byLane.set(run.laneName, frame);
		this.byTask.set(run.taskId, frame);
		try {
			frame.outcome = await this.converse(frame, run);
		} catch (error) {
			frame.outcome = run.signal.aborted
				? { status: "error", error: "cancelled" }
				: { status: "error", error: error instanceof Error ? error.message : String(error) };
		} finally {
			this.byLane.delete(run.laneName);
			if (frame.callHeld) {
				frame.callHeld = false;
				frame.node.release("calls", 1);
			}
		}
		const outcome = frame.outcome;
		if (run.signal.aborted && outcome.status !== "complete")
			return { status: "cancelled", error: "cancelled", verification: "unverified" };
		if (outcome.status === "complete")
			return { status: "succeeded", value: outcome.value, verification: "unverified" };
		return {
			status: "failed",
			error: outcome.status === "incomplete" ? `Incomplete (${outcome.reason}): ${outcome.detail}` : outcome.error,
			verification: "unverified",
		};
	}

	/** The frame's conversation: the seeded prompt, then re-asks within budget until the contract holds. */
	private async converse(frame: FrameState, run: NativeFrameRun): Promise<FrameOutcome> {
		const { lane, signal, context } = run;
		await lane.setActiveTools(frame.spec.depth > 1 ? ["rlm"] : [], context);
		const model = await lane.getModel(context).catch(() => undefined);
		if (model) frame.model = `${model.provider}/${model.id}`;
		frame.conversationChars = frameSystemPrompt(frame.spec.depth).length;
		let message = framePrompt(frame.spec);
		for (let attempt = 0; ; attempt += 1) {
			signal.throwIfAborted();
			const estimate = Math.ceil((frame.conversationChars + message.length) / 3) + REQUEST_OVERHEAD_TOKENS;
			if (model?.contextWindow && estimate + MIN_TRANCHE > model.contextWindow)
				return {
					status: "error",
					error: `context too large for ${frame.model}: about ${estimate} tokens against a ${model.contextWindow}-token window; pass smaller slices`,
				};
			const hold = this.claim(frame.node, estimate, model?.maxTokens, frame.callHeld);
			frame.callHeld = false;
			if (!hold)
				return {
					status: "incomplete",
					reason: "budget_exhausted",
					detail:
						attempt === 0
							? "the budget cannot cover the first request"
							: `the budget ran out after ${attempt} attempt(s) without a valid answer`,
				};
			frame.pending = hold;
			frame.conversationChars += message.length;
			let reservation: Awaited<ReturnType<NativeUsageLedgerLike["reserve"]>> | undefined;
			try {
				reservation = await this.usage?.reserve({
					kind: "model",
					...(run.usageRootId === undefined ? {} : { rootId: run.usageRootId }),
					taskId: run.taskId,
					parentTaskId: run.taskId,
					requestKey: `${run.taskId}:frame:${attempt}`,
					budgetId: frame.node.parent?.id ?? frame.node.id,
					...(run.deadlineAt === null ? { timeoutMs: run.timeoutMs } : { deadlineAt: run.deadlineAt }),
					signal,
				});
			} catch (error) {
				this.settlePrompt(frame, []);
				throw error;
			}
			let entries: Entry[] = [];
			let status: NativeUsageCallStatus = "failed";
			let response: Awaited<ReturnType<AgentLane["prompt"]>>;
			try {
				response = await lane.prompt(message, undefined, context);
				// A failed run's entries still carry what the provider reported, so it is charged, not lost.
				if (response.ok && response.value.status !== "suspended" && response.value.tipId) {
					const fromTipId = response.value.fromTipId ?? null;
					entries = (
						await lane.findEntries(
							{
								start: response.value.tipId,
								order: "newestFirst",
								...(fromTipId === null ? {} : { stopAtId: fromTipId }),
							},
							context,
						)
					).filter((entry) => entry.id !== fromTipId);
				}
				status = response.ok ? "succeeded" : signal.aborted ? "cancelled" : "failed";
			} catch (error) {
				status = signal.aborted ? "cancelled" : "failed";
				throw error;
			} finally {
				this.settlePrompt(frame, entries);
				if (reservation) {
					const usage = measurement(entries);
					await this.usage?.settle(reservation, { status, ...(usage === undefined ? {} : { usage }) });
				}
			}
			signal.throwIfAborted();
			if (frame.exhausted) return { status: "incomplete", reason: "budget_exhausted", detail: frame.exhausted };
			if (!response.ok) return { status: "error", error: `frame request failed: ${JSON.stringify(response.error)}` };
			if (response.value.status !== "completed")
				return {
					status: "error",
					error: `frame run did not complete: ${response.value.status}${"error" in response.value && response.value.error ? ` (${JSON.stringify(response.value.error)})` : ""}`,
				};
			const tipId = response.value.tipId;
			const text = replyText(entries.find((entry) => entry.id === tipId));
			frame.conversationChars += text.length;
			frame.outputs.push(bounded(text, OUTPUT_PREVIEW_CHARS));
			if (frame.outputs.length > LAST_OUTPUTS) frame.outputs.shift();
			let error: string;
			if (!text.trim()) error = "the reply is empty";
			else if (frame.spec.contract === undefined) {
				frame.attempts.push({ attempt, at: this.now(), output: bounded(text, OUTPUT_PREVIEW_CHARS) });
				return { status: "complete", value: text };
			} else {
				const parsed = parseReply(text, frame.spec.contract);
				if (parsed.ok) {
					frame.attempts.push({ attempt, at: this.now(), output: bounded(text, OUTPUT_PREVIEW_CHARS) });
					return { status: "complete", value: parsed.value };
				}
				error = parsed.error;
			}
			frame.attempts.push({ attempt, at: this.now(), output: bounded(text, OUTPUT_PREVIEW_CHARS), error });
			if (attempt >= frame.spec.maxRepairs)
				return {
					status: "incomplete",
					reason: "contract_unmet",
					detail: `no valid answer after ${attempt + 1} attempt(s): ${error}`,
				};
			message = repairPrompt(error, frame.spec.contract ?? { type: "string", minLength: 1 });
		}
	}

	private observation(frame: FrameState): JsonValue {
		const outcome = frame.outcome ?? { status: "error" as const, error: "frame did not run" };
		const base = {
			trace_id: frame.id,
			...(frame.taskId === undefined ? {} : { task_id: frame.taskId }),
			spent: { calls: frame.node.calls.spent, tokens: frame.node.tokens.spent },
			remaining: frame.node.remaining(),
		};
		if (outcome.status === "complete") return { status: "complete", value: outcome.value, ...base };
		if (outcome.status === "incomplete")
			return {
				status: "incomplete",
				reason: outcome.reason,
				detail: outcome.detail,
				...base,
				last_outputs: [...frame.outputs],
			};
		return { status: "error", error: outcome.error, ...base, last_outputs: [...frame.outputs] };
	}

	private trace(frame: FrameState): JsonValue {
		const outcome = frame.outcome;
		return {
			version: 1,
			id: frame.id,
			status: outcome?.status ?? "running",
			...(outcome?.status === "incomplete" ? { reason: outcome.reason, detail: outcome.detail } : {}),
			...(outcome?.status === "error" ? { error: bounded(outcome.error, OUTPUT_PREVIEW_CHARS) } : {}),
			...(outcome?.status === "complete"
				? { value: bounded(JSON.stringify(outcome.value) ?? "null", OUTPUT_PREVIEW_CHARS) }
				: {}),
			task: bounded(frame.spec.task, OUTPUT_PREVIEW_CHARS),
			views: frame.spec.views.map((view) => ({
				label: view.label,
				chars: view.chars,
				byReference: view.byReference === true,
				...(view.digest === undefined ? {} : { digest: view.digest }),
				...(view.start === undefined ? {} : { start: view.start }),
				...(view.end === undefined ? {} : { end: view.end }),
			})),
			contract: frame.spec.contract ?? null,
			maxRepairs: frame.spec.maxRepairs,
			depth: frame.spec.depth,
			budget: frame.node.parent?.snapshot() ?? null,
			spent: { calls: frame.node.calls.spent, tokens: frame.node.tokens.spent },
			remaining: frame.node.remaining(),
			taskId: frame.taskId ?? null,
			lane: frame.laneName ?? null,
			callerTaskId: frame.callerTaskId,
			parentFrame: frame.parentFrame ?? null,
			model: frame.model ?? frame.spec.model ?? null,
			startedAt: frame.startedAt,
			endedAt: frame.endedAt ?? null,
			attempts: frame.attempts,
			requests: frame.requests,
		};
	}

	private summary(frame: FrameState): JsonValue {
		const outcome = frame.outcome;
		return {
			id: frame.id,
			status: outcome?.status ?? "running",
			...(outcome?.status === "incomplete" ? { reason: outcome.reason } : {}),
			task: bounded(frame.spec.task.replace(/\s+/g, " "), 120),
			views: frame.spec.views.length,
			spent: { calls: frame.node.calls.spent, tokens: frame.node.tokens.spent },
			taskId: frame.taskId ?? null,
			parentFrame: frame.parentFrame ?? null,
			startedAt: frame.startedAt,
			endedAt: frame.endedAt ?? null,
			// Read-only grouping for the graph view: one `rlm.map` is one budget node shared by `batch` frames.
			kind: frame.kind,
			batch: frame.batch,
			callerTaskId: frame.callerTaskId,
			lane: frame.laneName ?? null,
			budget: budgetSummary(frame.node.parent),
		};
	}

	/**
	 * Persist a finished frame's trace. Writes are serialized; index updates are coalesced, so a wide map writes
	 * the index once per burst rather than once per frame.
	 */
	private writeTrace(frame: FrameState): Promise<void> {
		const traces = this.traces;
		if (!traces) return Promise.resolve();
		const document = this.trace(frame);
		this.pendingSummaries.set(frame.id, this.summary(frame));
		this.traceWrites = this.traceWrites.then(() => traces.write(frame.id, document)).catch(() => {});
		if (!this.indexQueued) {
			this.indexQueued = true;
			this.traceWrites = this.traceWrites
				.then(async () => {
					this.indexQueued = false;
					const index = await this.loadIndex();
					for (const [id, summary] of this.pendingSummaries) {
						const at = index.findIndex((item) => (item as { id?: unknown }).id === id);
						if (at === -1) index.push(summary);
						else index[at] = summary;
					}
					this.pendingSummaries.clear();
					if (index.length > TRACE_INDEX_LIMIT) index.splice(0, index.length - TRACE_INDEX_LIMIT);
					await traces.write("index", index);
				})
				.catch(() => {});
		}
		return this.traceWrites;
	}

	private async loadIndex(): Promise<JsonValue[]> {
		if (this.index === undefined) {
			const stored = await this.traces?.read("index").catch(() => undefined);
			this.index = Array.isArray(stored) ? stored : [];
		}
		return this.index;
	}

	private async finalize(frame: FrameState): Promise<void> {
		if (frame.finalized) return;
		frame.finalized = true;
		frame.endedAt = this.now();
		await this.writeTrace(frame);
		this.frames.delete(frame.id);
		if (frame.taskId !== undefined) this.byTask.delete(frame.taskId);
		// Keep the metadata for the observation; drop the materialized text.
		for (const view of frame.spec.views) delete view.text;
	}

	private async list(payload: Payload): Promise<JsonValue> {
		fields(payload, ["id", "limit"]);
		if (payload.id !== undefined) {
			if (typeof payload.id !== "string" || !payload.id.startsWith("frame-"))
				throw new Error("id must be a frame id");
			const live = this.frames.get(payload.id);
			if (live) return this.trace(live);
			const stored = await this.traces?.read(payload.id);
			if (stored === undefined) throw new Error(`Unknown frame ${payload.id}`);
			return stored;
		}
		const limit = optionalInteger(payload.limit, "limit", 1, TRACE_INDEX_LIMIT) ?? 20;
		await this.traceWrites;
		const index = await this.loadIndex();
		const live = new Map([...this.frames.values()].map((frame) => [frame.id, this.summary(frame)]));
		const merged = index.map((item) => live.get((item as { id: string }).id) ?? item);
		for (const [id, item] of live)
			if (!merged.some((entry) => (entry as { id: string }).id === id)) merged.push(item);
		return { frames: merged.slice(-limit).reverse() };
	}
}

export function createInferenceRuntime(options: InferenceRuntimeOptions): InferenceRuntime {
	return new InferenceRuntime(options);
}
