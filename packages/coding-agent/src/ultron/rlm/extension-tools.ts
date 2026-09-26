/**
 * Extension tools inside the REPL (Prime Intellect's RLM harness exposes MCP tools as pre-imported IPython skills).
 * Tools registered by Pi extensions (the pi-mcp-adapter's `mcp` gateway among them) are not separate model tools by
 * default: the kernel calls them with `await tools.call(name, params)` / `await mcp.call(tool, **args)` through
 * `tools.*` host requests, and the worker executes the real extension tool with its tool context (the cell's abort
 * signal, the extension UI bridge).
 *
 * Every call, from the REPL or (when extension tools are native, or for any other non-rlm tool on a lane) from the
 * model directly, is a record here: name, label, status, elapsed time, input and result previews. The list is
 * journaled as the session value `ultron.module/tool-calls` and shown in the RLM graph through `agents.status`.
 *
 * A slow REPL call detaches like a plain `bash`: after `yield_after` seconds (ULTRON_TOOL_YIELD_AFTER, else
 * ULTRON_BASH_YIELD_AFTER, default 30) the kernel gets a running handle, the call keeps running in the worker, and
 * its end is announced as a `<runtime_event kind="tool_done">`.
 */
import { randomBytes } from "node:crypto";
import type { AgentToolResult } from "@ultron/agent-core";
import { validateToolArguments } from "@ultron/ai";
import type { JsonValue } from "@ultron/chord";
import { readVersioned } from "../format-version.ts";
import type { HostCaller, HostModuleStore, NativeHostApi, NativeHostModule } from "./host-module.ts";
import { truncateToolOutput } from "./output-truncation.ts";
import { rlmToolMode } from "./prompt.ts";

export type ExtensionToolMode = "repl" | "native";

/** Settings-file form (`extensionTools` in settings.json); the environment overrides it. */
export interface ExtensionToolSettings {
	/** "repl" (default): extension tools are called from Python; "native": they are model tools as in Pi. */
	mode?: ExtensionToolMode;
	/** Extension tools that stay native model tools in "repl" mode. */
	native?: string[];
}

/**
 * Where extension tools live: ULTRON_EXTENSION_TOOLS=native|repl wins, then ULTRON_TOOLS=native (which restores
 * all of Pi's native tools), then the `extensionTools.mode` setting; the default is the REPL.
 */
export function extensionToolMode(
	env: NodeJS.ProcessEnv = process.env,
	settings: ExtensionToolSettings = {},
): ExtensionToolMode {
	const raw = env.ULTRON_EXTENSION_TOOLS?.trim().toLowerCase();
	if (raw === "native") return "native";
	if (raw === "repl") return "repl";
	if (rlmToolMode(env) === "native") return "native";
	return settings.mode === "native" ? "native" : "repl";
}

/** Extension tools that stay native in REPL mode: ULTRON_NATIVE_EXTENSION_TOOLS (comma-separated) plus the setting. */
export function nativeExtensionToolAllowlist(
	env: NodeJS.ProcessEnv = process.env,
	settings: ExtensionToolSettings = {},
): string[] {
	const fromEnv = (env.ULTRON_NATIVE_EXTENSION_TOOLS ?? "")
		.split(",")
		.map((name) => name.trim())
		.filter(Boolean);
	const fromSettings = Array.isArray(settings.native)
		? settings.native.filter((name): name is string => typeof name === "string" && name.length > 0)
		: [];
	return [...new Set([...fromEnv, ...fromSettings])];
}

/**
 * The extension tools the model gets as native tools: all of them in native mode; otherwise the allowlist and any
 * named explicitly with `--tools`.
 */
export function modelExtensionToolNames(
	all: readonly string[],
	options: { mode: ExtensionToolMode; allowlist: readonly string[]; explicit?: readonly string[] },
): string[] {
	if (options.mode === "native") return [...all];
	return all.filter((name) => options.allowlist.includes(name) || options.explicit?.includes(name) === true);
}

/** Seconds a plain REPL tool call waits before it detaches: ULTRON_TOOL_YIELD_AFTER, else ULTRON_BASH_YIELD_AFTER. */
export function toolYieldAfterEnv(env: NodeJS.ProcessEnv = process.env): string | undefined {
	const own = env.ULTRON_TOOL_YIELD_AFTER?.trim();
	return own ? own : env.ULTRON_BASH_YIELD_AFTER?.trim() || undefined;
}

/** What the REPL learns about one extension tool. */
export interface ExtensionToolInfo {
	readonly name: string;
	readonly label?: string;
	readonly description: string;
	readonly parameters: unknown;
}

/** The worker's view of the extension runner: the live tool list and how to execute one. */
export interface ExtensionToolRunner {
	tools(): ExtensionToolInfo[];
	execute(
		name: string,
		toolCallId: string,
		params: Record<string, unknown>,
		signal: AbortSignal,
		onUpdate: (partial: AgentToolResult<unknown>) => void,
	): Promise<AgentToolResult<unknown>>;
}

export type ToolCallStatus = "running" | "completed" | "failed" | "cancelled" | "interrupted";
export type ToolCallSource = "repl" | "native";

/** One journaled call (the durable part: previews only, never the whole result). */
export interface ToolCallRecord {
	id: string;
	lane: string;
	/** Usage root (root turn) the call was started under; its Esc abort cancels a REPL call. */
	rootId: string | null;
	source: ToolCallSource;
	name: string;
	/** Short display label, e.g. "mcp exa-agent_exa_agent_create_run". */
	label: string;
	/** Bounded preview of the arguments. */
	input: string;
	status: ToolCallStatus;
	startedAt: number;
	endedAt: number | null;
	/** Bounded preview of the result text (or the last progress update while running). */
	preview?: string;
	error?: string;
	/** The model's tool call id, for a native call. */
	toolCallId?: string;
}

/** A call's end, as reported to the completion-event dispatcher. */
export interface ToolCallEnd {
	readonly call: ToolCallRecord;
	/** A cell was waiting on this call when it ended (the model already has the result). */
	readonly awaited: boolean;
	/** Cancelled because its root turn was aborted: no event. */
	readonly rootAborted: boolean;
}

/** Wire form returned to the kernel's ToolCall / ToolResult. */
export type ToolCallSnapshot = {
	id: string;
	name: string;
	label: string;
	lane: string;
	status: ToolCallStatus;
	running: boolean;
	ok: boolean;
	text: string | null;
	details: JsonValue | null;
	truncated: boolean;
	elapsed_seconds: number;
	error: string | null;
};

/**
 * Byte budget for a tool result held in a Python variable: ULTRON_TOOL_RESULT_BYTES, default 256 KiB, within
 * 1 KiB..512 KiB. It is larger than the 20 KB cell output budget on purpose: the value is data to process (an MCP
 * listing parsed with `.json()`), and whatever the cell prints is still cut to the cell budget. The bound keeps the
 * reply within the kernel protocol's 1 MiB frame.
 */
export const DEFAULT_TOOL_RESULT_BYTES = 256 * 1024;
export const MAX_TOOL_RESULT_BYTES = 512 * 1024;

export function toolResultBudget(env: NodeJS.ProcessEnv = process.env): number {
	const raw = env.ULTRON_TOOL_RESULT_BYTES?.trim();
	const value = raw ? Number(raw) : Number.NaN;
	if (!Number.isInteger(value) || value <= 0) return DEFAULT_TOOL_RESULT_BYTES;
	return Math.min(Math.max(value, 1024), MAX_TOOL_RESULT_BYTES);
}

export const TOOL_CALL_MAX_ACTIVE = 32;
export const TOOL_CALL_MAX_RETAINED = 64;
export const TOOL_CALL_MAX_WAIT_SECONDS = 3600;
const PREVIEW_CHARS = 300;
const DETAILS_MAX_BYTES = 64 * 1024;
const REPLY_TEXT_MAX_BYTES = 768 * 1024;
const JOURNAL_VERSION = 1;

type Outcome = { text: string; details: JsonValue | null; truncated: boolean };

type Live = {
	record: ToolCallRecord;
	controller: AbortController;
	done: Promise<void>;
	waiters: number;
	cancelReason?: "cancelled" | "root_aborted" | "closed";
	release?: () => void;
};

export interface ExtensionToolCallsOptions {
	runner: () => ExtensionToolRunner | undefined;
	store: HostModuleStore;
	/** Called once per REPL call that ends while this worker runs. */
	onEnd?: (end: ToolCallEnd) => void;
	/** Keeps the worker alive while a call runs; returns the release. */
	holdActivity?: () => () => void;
	now?: () => number;
	maxRetained?: number;
	/** Result byte budget (default: ULTRON_TOOL_RESULT_BYTES, 256 KiB). */
	outputBytes?: number;
}

export class ExtensionToolCalls {
	readonly #options: ExtensionToolCallsOptions;
	readonly #live = new Map<string, Live>();
	/** Results of finished calls, kept in memory beside the journal (bounded like the records). */
	readonly #outcomes = new Map<string, Outcome>();
	/** Native tool call id -> record id. */
	readonly #native = new Map<string, string>();
	#records: ToolCallRecord[] = [];
	#loaded?: Promise<void>;
	#writes: Promise<void> = Promise.resolve();
	#closed = false;

	constructor(options: ExtensionToolCallsOptions) {
		this.#options = options;
	}

	get #now(): number {
		return (this.#options.now ?? Date.now)();
	}

	/** Host module for `tools.*` requests; identity comes from the calling lane. */
	readonly module: NativeHostModule = {
		prefixes: ["tools."],
		start: () => this.#load(),
		handle: (request, host) => this.#handle(request.type, request.payload, request.caller, host, request.context),
		close: () => this.close(),
	};

	#load(): Promise<void> {
		this.#loaded ??= (async () => {
			const saved = readVersioned("ultron.module/tool-calls", await this.#options.store.read());
			const calls =
				saved && typeof saved === "object" && !Array.isArray(saved) && Array.isArray(saved.calls)
					? (saved.calls as unknown as ToolCallRecord[])
					: [];
			// Calls recorded before the journal finished loading may already be in it.
			const pending = new Set(this.#records.map((record) => record.id));
			const journaled = calls.filter((call) => !pending.has(call.id));
			// A call does not outlive the worker that ran it: one still running in the journal was interrupted.
			this.#records = [
				...journaled.map((call) =>
					call.status === "running"
						? {
								...call,
								status: "interrupted" as const,
								endedAt: call.endedAt ?? this.#now,
								error: "worker restarted",
							}
						: call,
				),
				...this.#records,
			];
			if (journaled.some((call) => call.status === "running")) this.#persist();
		})();
		return this.#loaded;
	}

	#persist(): void {
		const calls = this.#records.map((record) => ({ ...record }));
		const snapshot = structuredClone({ version: JOURNAL_VERSION, calls } as unknown as JsonValue);
		this.#writes = this.#writes.then(() => this.#options.store.write(snapshot)).catch(() => {});
	}

	/** Wait for journal writes (tests and shutdown). */
	settled(): Promise<void> {
		return this.#writes;
	}

	async #handle(
		type: string,
		payload: Record<string, unknown>,
		caller: HostCaller,
		host: NativeHostApi,
		context: { abortSignal?: AbortSignal },
	): Promise<unknown> {
		await this.#load();
		const signal = context.abortSignal;
		switch (type) {
			case "tools.list": {
				fields(payload, []);
				return this.#runner()
					.tools()
					.map((tool) => ({
						name: tool.name,
						...(tool.label === undefined ? {} : { label: tool.label }),
						description: firstParagraph(tool.description),
					}));
			}
			case "tools.describe": {
				fields(payload, ["name"]);
				const tool = this.#tool(payload.name);
				return {
					name: tool.name,
					...(tool.label === undefined ? {} : { label: tool.label }),
					description: tool.description,
					parameters: jsonSafe(tool.parameters) ?? null,
				};
			}
			case "tools.call": {
				// Wait up to `yield_after` seconds (absent: until the call ends). A call still running when a waiting
				// cell is stopped is cancelled, unless the kernel asked for a handle (`detach`).
				fields(payload, ["name", "params", "yield_after", "detach"]);
				const tool = this.#tool(payload.name);
				const params = payload.params ?? {};
				if (params === null || typeof params !== "object" || Array.isArray(params))
					throw new Error("tool params must be a dict (a JSON object)");
				const yieldAfter = waitSeconds(payload.yield_after, "yield_after");
				const live = this.start(
					caller.lane,
					host.rootOf?.(caller) ?? null,
					tool.name,
					params as Record<string, unknown>,
				);
				await this.#wait(live, yieldAfter, signal);
				if (live.record.status === "running" && signal?.aborted && payload.detach !== true) {
					live.waiters += 1;
					try {
						await this.cancel(live.record.id, "cancelled");
					} finally {
						live.waiters -= 1;
					}
				}
				return this.snapshot(live.record);
			}
			case "tools.result": {
				fields(payload, ["id", "wait"]);
				const record = this.#visible(payload.id, caller);
				const live = this.#live.get(record.id);
				if (live) await this.#wait(live, waitSeconds(payload.wait, "wait"), signal);
				return this.snapshot(this.#record(record.id));
			}
			case "tools.get": {
				fields(payload, ["id"]);
				return this.snapshot(this.#visible(payload.id, caller));
			}
			case "tools.cancel": {
				fields(payload, ["id"]);
				const record = this.#visible(payload.id, caller);
				await this.cancel(record.id, "cancelled");
				return this.snapshot(this.#record(record.id));
			}
			case "tools.calls": {
				fields(payload, []);
				return [...this.#records]
					.reverse()
					.filter((record) => caller.lane === "main" || record.lane === caller.lane)
					.map((record) => ({ ...this.snapshot(record), text: null, details: null, source: record.source }));
			}
			default:
				throw new Error(`Ultron RLM host request is not wired: ${type}`);
		}
	}

	#runner(): ExtensionToolRunner {
		const runner = this.#options.runner();
		if (!runner) throw new Error("Extension tools are not available in this session");
		return runner;
	}

	#tool(name: unknown): ExtensionToolInfo {
		if (typeof name !== "string" || !name) throw new Error("tool name must be a non-empty string");
		const tools = this.#runner().tools();
		const normalized = (value: string) => value.replace(/-/g, "_");
		const tool =
			tools.find((candidate) => candidate.name === name) ??
			tools.find((candidate) => normalized(candidate.name) === normalized(name));
		if (!tool) {
			const names = tools.map((candidate) => candidate.name);
			throw new Error(
				`Unknown extension tool "${name}"${names.length > 0 ? `; available: ${names.slice(0, 20).join(", ")}` : " (no extension tools are registered)"}`,
			);
		}
		return tool;
	}

	#record(id: string): ToolCallRecord {
		const record = this.#records.find((candidate) => candidate.id === id);
		if (!record) throw new Error(`Unknown tool call ${id}`);
		return record;
	}

	/** The root lane sees every REPL call; a child lane sees its own. */
	#visible(id: unknown, caller: HostCaller): ToolCallRecord {
		if (typeof id !== "string" || !id) throw new Error("call id must be a nonempty string");
		const record = this.#records.find((candidate) => candidate.id === id);
		if (!record || (caller.lane !== "main" && record.lane !== caller.lane))
			throw new Error(`Unknown tool call ${id}`);
		return record;
	}

	/** Start a REPL call of extension tool `name` on `lane` under `rootId`. */
	start(lane: string, rootId: string | null, name: string, params: Record<string, unknown>): Live {
		if (this.#closed) throw new Error("Extension tool calls are closed");
		if (this.#live.size >= TOOL_CALL_MAX_ACTIVE)
			throw new Error(`At most ${TOOL_CALL_MAX_ACTIVE} extension tool calls can run at once`);
		const record: ToolCallRecord = {
			id: `call-${randomBytes(4).toString("hex")}`,
			lane,
			rootId,
			source: "repl",
			name,
			label: toolCallLabel(name, params),
			input: preview(params),
			status: "running",
			startedAt: this.#now,
			endedAt: null,
		};
		this.#add(record);
		const controller = new AbortController();
		const live: Live = { record, controller, done: Promise.resolve(), waiters: 0 };
		live.release = this.#options.holdActivity?.();
		this.#live.set(record.id, live);
		live.done = this.#run(live, params);
		return live;
	}

	async #run(live: Live, params: Record<string, unknown>): Promise<void> {
		const { record, controller } = live;
		let outcome: Outcome | undefined;
		let error: string | undefined;
		try {
			const result = await this.#runner().execute(
				record.name,
				`repl-${record.id}`,
				params,
				controller.signal,
				(partial) => {
					const text = resultText(partial);
					if (text) record.preview = preview(text);
				},
			);
			outcome = this.#outcome(result);
		} catch (caught) {
			error = caught instanceof Error ? caught.message : String(caught);
		}
		record.endedAt = this.#now;
		if (controller.signal.aborted) {
			record.status = "cancelled";
			delete record.preview;
		} else if (error !== undefined) {
			record.status = "failed";
			record.error = error.length > 2000 ? `${error.slice(0, 1999)}…` : error;
			record.preview = preview(error);
		} else {
			record.status = "completed";
			this.#outcomes.set(record.id, outcome!);
			record.preview = preview(outcome!.text);
		}
		this.#live.delete(record.id);
		this.#persist();
		live.release?.();
		if (live.cancelReason === "closed") return;
		try {
			this.#options.onEnd?.({
				call: structuredClone(record),
				awaited: live.waiters > 0,
				rootAborted: live.cancelReason === "root_aborted",
			});
		} catch {
			// Observers never affect a call.
		}
	}

	#outcome(result: AgentToolResult<unknown>): Outcome {
		const raw = resultText(result);
		let budget = this.#options.outputBytes ?? toolResultBudget();
		let text = truncateToolOutput(raw, budget);
		// JSON escaping can grow text (control characters six-fold): keep the reply within the kernel's frame.
		while (Buffer.byteLength(JSON.stringify(text)) > REPLY_TEXT_MAX_BYTES && budget > 1024) {
			budget = Math.floor(budget / 2);
			text = truncateToolOutput(raw, budget);
		}
		let details = jsonSafe(result?.details) ?? null;
		if (details !== null && Buffer.byteLength(JSON.stringify(details)) > DETAILS_MAX_BYTES)
			details = { truncated: true, preview: preview(details, 2000) };
		return { text, details, truncated: text !== raw };
	}

	/** Wait for a call for at most `seconds` (undefined: until it ends), or until the calling cell ends. */
	async #wait(live: Live, seconds: number | undefined, signal: AbortSignal | undefined): Promise<void> {
		live.waiters += 1;
		// Even `yield_after=0` gives a call that ends at once the chance to report inline.
		const ms = seconds === undefined ? undefined : Math.max(20, seconds * 1000);
		let timer: NodeJS.Timeout | undefined;
		let onAbort: (() => void) | undefined;
		const waits: Promise<unknown>[] = [live.done];
		if (ms !== undefined)
			waits.push(
				new Promise((resolve) => {
					timer = setTimeout(resolve, ms);
				}),
			);
		if (signal)
			waits.push(
				new Promise((resolve) => {
					onAbort = () => resolve(undefined);
					if (signal.aborted) onAbort();
					else signal.addEventListener("abort", onAbort, { once: true });
				}),
			);
		try {
			await Promise.race(waits);
		} finally {
			if (timer) clearTimeout(timer);
			if (onAbort) signal?.removeEventListener("abort", onAbort);
			live.waiters -= 1;
		}
	}

	async cancel(id: string, reason: "cancelled" | "root_aborted" | "closed"): Promise<void> {
		const live = this.#live.get(id);
		if (!live) return;
		live.cancelReason ??= reason;
		live.controller.abort(new Error(reason === "root_aborted" ? "turn aborted" : "tool call cancelled"));
		await live.done;
	}

	/** Esc aborted a root turn: stop every REPL call started under it. */
	cancelRoot(rootId: string): Promise<void> {
		return Promise.all(
			[...this.#live.values()]
				.filter((live) => live.record.rootId === rootId)
				.map((live) => this.cancel(live.record.id, "root_aborted")),
		).then(() => {});
	}

	/** Running REPL calls of `lane` (roots in `excluded` do not count). */
	running(lane: string, excluded: (rootId: string | undefined) => boolean = () => false): number {
		return [...this.#live.values()].filter(
			(live) => live.record.lane === lane && !excluded(live.record.rootId ?? undefined),
		).length;
	}

	/** A tool the model called directly started (native extension tools, or any other non-rlm tool). */
	nativeStarted(event: { lane: string; toolCallId: string; toolName: string; args: unknown }): void {
		// A call is recorded once, whatever the harness replays (a recovered run starts its calls again).
		if (this.#closed || this.#records.some((record) => record.toolCallId === event.toolCallId)) return;
		const params =
			event.args !== null && typeof event.args === "object" && !Array.isArray(event.args)
				? (event.args as Record<string, unknown>)
				: {};
		const record: ToolCallRecord = {
			id: `call-${randomBytes(4).toString("hex")}`,
			lane: event.lane,
			rootId: null,
			source: "native",
			name: event.toolName,
			label: toolCallLabel(event.toolName, params),
			input: preview(event.args ?? {}),
			status: "running",
			startedAt: this.#now,
			endedAt: null,
			toolCallId: event.toolCallId,
		};
		this.#native.set(event.toolCallId, record.id);
		this.#add(record);
	}

	nativeUpdated(event: { toolCallId: string; partialResult: AgentToolResult<unknown> }): void {
		const id = this.#native.get(event.toolCallId);
		const record = id === undefined ? undefined : this.#records.find((candidate) => candidate.id === id);
		if (!record || record.status !== "running") return;
		const text = resultText(event.partialResult);
		if (text) record.preview = preview(text);
	}

	nativeEnded(event: { toolCallId: string; result: AgentToolResult<unknown>; isError: boolean }): void {
		const id = this.#native.get(event.toolCallId);
		this.#native.delete(event.toolCallId);
		const record = id === undefined ? undefined : this.#records.find((candidate) => candidate.id === id);
		if (!record || record.status !== "running") return;
		const text = resultText(event.result);
		record.endedAt = this.#now;
		record.status = event.isError ? "failed" : "completed";
		if (text) record.preview = preview(text);
		if (event.isError) record.error = preview(text || "tool failed");
		this.#persist();
	}

	/** Native calls of a run that ended without a result (an abort) are no longer running. */
	nativeInterrupted(lane: string): void {
		let changed = false;
		for (const [toolCallId, id] of [...this.#native]) {
			const record = this.#records.find((candidate) => candidate.id === id);
			if (!record || record.lane !== lane) continue;
			this.#native.delete(toolCallId);
			if (record.status !== "running") continue;
			record.status = "cancelled";
			record.endedAt = this.#now;
			changed = true;
		}
		if (changed) this.#persist();
	}

	#add(record: ToolCallRecord): void {
		this.#records.push(record);
		this.#prune();
		this.#persist();
	}

	/** Running and recent calls for inspection (`agents.status`), newest first. */
	list(): ToolCallRecord[] {
		return [...this.#records].reverse().map((record) => structuredClone(record));
	}

	snapshot(record: ToolCallRecord): ToolCallSnapshot {
		const outcome = this.#outcomes.get(record.id);
		return {
			id: record.id,
			name: record.name,
			label: record.label,
			lane: record.lane,
			status: record.status,
			running: record.status === "running",
			ok: record.status === "completed",
			text: outcome?.text ?? null,
			details: outcome?.details ?? null,
			truncated: outcome?.truncated ?? false,
			elapsed_seconds: Math.round(((record.endedAt ?? this.#now) - record.startedAt) / 100) / 10,
			error:
				record.error ??
				(record.status === "completed" && outcome === undefined
					? "the result is no longer held (the worker restarted or the call is too old)"
					: null),
		};
	}

	/** Bounded retention: keep the newest finished calls. */
	#prune(): void {
		const max = this.#options.maxRetained ?? TOOL_CALL_MAX_RETAINED;
		const excess = this.#records.length - max;
		if (excess <= 0) return;
		const drop = new Set(
			this.#records
				.filter((record) => record.status !== "running")
				.slice(0, excess)
				.map((record) => record.id),
		);
		for (const id of drop) this.#outcomes.delete(id);
		this.#records = this.#records.filter((record) => !drop.has(record.id));
	}

	async close(): Promise<void> {
		this.#closed = true;
		await Promise.all([...this.#live.keys()].map((id) => this.cancel(id, "closed")));
		await this.#writes;
	}
}

/** One-line completion summary for a runtime event. */
export function toolCallSummary(call: ToolCallRecord, maxChars = 240): string {
	const status = call.status === "completed" ? "ok" : call.error ? `${call.status}: ${call.error}` : call.status;
	const body = call.status === "completed" && call.preview ? call.preview : "";
	const text = `${call.label}: ${status}${body ? `; ${body}` : ""}`.replace(/\s+/g, " ");
	return text.length > maxChars ? `${text.slice(0, maxChars - 1)}…` : text;
}

/**
 * A short label for the graph. The pi-mcp-adapter's `mcp` gateway is labelled by its mode: the MCP tool called,
 * described or searched, rather than just "mcp".
 */
export function toolCallLabel(name: string, params: Record<string, unknown>): string {
	if (name !== "mcp") return name;
	const text = (value: unknown) => (typeof value === "string" ? value : undefined);
	const tool = text(params.tool);
	if (tool) return `mcp ${tool}`;
	const action = text(params.action);
	if (action) return `mcp ${action}${text(params.server) ? ` ${params.server}` : ""}`;
	if (text(params.connect)) return `mcp connect ${params.connect}`;
	if (text(params.describe)) return `mcp describe ${params.describe}`;
	if (text(params.instructions)) return `mcp instructions ${params.instructions}`;
	if (params.search !== undefined) return `mcp search ${JSON.stringify(params.search)}`;
	if (text(params.server)) return `mcp list ${params.server}`;
	return "mcp status";
}

/** The text of a tool result: text parts joined; other parts named. */
export function resultText(result: AgentToolResult<unknown> | undefined): string {
	const content = (result as { content?: unknown } | undefined)?.content;
	if (!Array.isArray(content)) return "";
	return content
		.map((part: unknown) => {
			if (part === null || typeof part !== "object") return "";
			const item = part as { type?: unknown; text?: unknown; mimeType?: unknown };
			if (item.type === "text" && typeof item.text === "string") return item.text;
			if (item.type === "image") return `[image${typeof item.mimeType === "string" ? ` ${item.mimeType}` : ""}]`;
			return typeof item.type === "string" ? `[${item.type}]` : "";
		})
		.filter(Boolean)
		.join("\n");
}

function preview(value: unknown, limit = PREVIEW_CHARS): string {
	let text: string;
	try {
		text = typeof value === "string" ? value : (JSON.stringify(value) ?? String(value));
	} catch {
		text = String(value);
	}
	const flat = text.replace(/\s+/g, " ").trim();
	return flat.length > limit ? `${flat.slice(0, limit - 1)}…` : flat;
}

function firstParagraph(text: string, limit = 400): string {
	const paragraph = (text ?? "").trim().split(/\n\s*\n/)[0] ?? "";
	return preview(paragraph, limit);
}

/** A JSON copy of a value (functions, symbols and cycles dropped), or undefined when it has none. */
function jsonSafe(value: unknown): JsonValue | undefined {
	if (value === undefined) return undefined;
	try {
		const seen = new WeakSet<object>();
		const text = JSON.stringify(value, (_key, item: unknown) => {
			if (typeof item === "bigint") return item.toString();
			if (item !== null && typeof item === "object") {
				if (seen.has(item)) return undefined;
				seen.add(item);
			}
			return item;
		});
		return text === undefined ? undefined : (JSON.parse(text) as JsonValue);
	} catch {
		return undefined;
	}
}

function waitSeconds(value: unknown, name: string): number | undefined {
	if (value === undefined || value === null) return undefined;
	if (typeof value !== "number" || !Number.isFinite(value) || value < 0)
		throw new Error(`${name} must be a non-negative number of seconds`);
	return Math.min(value, TOOL_CALL_MAX_WAIT_SECONDS);
}

function fields(payload: Record<string, unknown>, allowed: readonly string[]): void {
	for (const key of Object.keys(payload)) if (!allowed.includes(key)) throw new Error(`Unknown payload field: ${key}`);
}

/** Validate and coerce arguments for an extension tool as the harness does for a model call. */
export function prepareToolArguments(
	tool: { name: string; description: string; parameters: unknown; prepareArguments?: (args: unknown) => unknown },
	params: Record<string, unknown>,
): Record<string, unknown> {
	const prepared = tool.prepareArguments ? tool.prepareArguments(params) : params;
	return validateToolArguments(
		{ name: tool.name, description: tool.description, parameters: tool.parameters as never },
		{ type: "toolCall", id: "repl", name: tool.name, arguments: prepared as never },
	) as Record<string, unknown>;
}
