/**
 * Claude Code as the model of an Ultron lane that has tools (`ultron --claude`): the root lane, and every subagent
 * lane that inherits its model. The harness drives the lane exactly as for any other provider (it streams the
 * reply, runs the tool calls with its own tools, charges usage, records the transcript), and this runner turns
 * that loop into one live `claude -p` process per lane run:
 *
 * - the lane's tools are served to the CLI as MCP tools by a bridge (`ultron mcp --bridge`) that connects back to
 *   this worker; a call the CLI makes is not run by the bridge: it waits for the harness to run the tool call the
 *   runner reported, and gets the harness's result (so the rlm cell runs on the lane's own kernel, with hints,
 *   Loki, budgets and the transcript entry exactly as in a native turn);
 * - each assistant message of the CLI's stream-json output becomes one response of the lane: text streams as it
 *   arrives, the `mcp__ultron__<tool>` calls become tool calls, and the response ends where the CLI waits for
 *   tool results (stop reason `toolUse`) or at the turn's `result` (usage and the CLI's reported cost);
 * - the next request of the same run delivers the tool results to the waiting MCP calls and reads on; user
 *   messages that arrive mid-run (steering, follow-ups) are written to the CLI's stdin, where Claude Code queues
 *   them and hands them to the model at its next step;
 * - a run's process ends with the run; the next run resumes the lane's Claude Code session (`--resume`, the id is
 *   kept in the Ultron session), so the conversation lives in Claude Code, which also manages its context.
 *
 * When the transcript no longer continues the Claude Code session (another model answered in between, or the
 * user navigated the tree), a fresh Claude Code session starts with the conversation so far rendered as text.
 *
 * Only public CLI flags are used; credentials are never read: the CLI authenticates itself.
 */
import { randomBytes, randomUUID } from "node:crypto";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type {
	Api,
	AssistantMessage,
	ImageContent,
	JsonObject,
	Message,
	Model,
	SimpleStreamOptions,
	TextContent,
	ToolCall,
	ToolResultMessage,
	TranscriptContext,
	Usage,
} from "@ultron/ai";
import {
	type CliEvent,
	CliProcess,
	childEnv,
	classifyFailure,
	ensureClaudeCli,
	type RateLimitInfo,
	rateLimitInfo,
	resolveClaudeCli,
	USAGE_LIMIT_DIAGNOSTIC,
} from "@ultron/ai/api/claude-code-cli";
import type { AssistantMessageDiagnostic } from "@ultron/ai/utils/diagnostics";
import { AssistantMessageEventStream } from "@ultron/ai/utils/event-stream";
import { parseStreamingJson } from "@ultron/ai/utils/json-parse";
import { getCurrentSystemPrompt, getCurrentTools } from "@ultron/ai/utils/transcript";

/** MCP server name of the bridge: tools appear to Claude Code as `mcp__ultron__<name>`. */
export const BRIDGE_SERVER = "ultron";
const TOOL_PREFIX = `mcp__${BRIDGE_SERVER}__`;
/** A cell may wait on subagents for a long time; Claude Code's MCP tool timeout must not cut it off. */
const MCP_TOOL_TIMEOUT_MS = 2 * 60 * 60 * 1000;

/** What the runner remembers of a lane's Claude Code session (persisted in the Ultron session). */
export interface ClaudeLaneRecord {
	/** Claude Code session id (`--session-id` on the first run, `--resume` after). */
	sessionId: string;
	/**
	 * Id of the last settled Claude response (one the lane's transcript keeps; aborted and failed ones are not sent
	 * to models again). The transcript continues the session from it.
	 */
	lastResponseId?: string;
	/** User messages after that response the session already has (the input of an aborted or failed turn). */
	consumed?: number;
}

export interface BridgeTool {
	name: string;
	description: string;
	inputSchema: Record<string, unknown>;
}

export type BridgeContent = { type: "text"; text: string } | { type: "image"; data: string; mimeType: string };
export interface BridgeResult {
	content: BridgeContent[];
	isError: boolean;
}

/** One lane process's side of the bridge: the worker's control server routes MCP calls here by token. */
export interface LaneBridge {
	tools(): BridgeTool[];
	call(
		toolUseId: string | undefined,
		name: string,
		args: Record<string, unknown>,
		signal: AbortSignal,
	): Promise<BridgeResult>;
}

/** Subscription usage the CLI reported (`rate_limit_event`), for the footer. */
export interface ClaudeRateLimits {
	status?: string;
	/** Per window (`five_hour`, `seven_day`, ...): utilization 0..1 and reset time (epoch seconds). */
	windows: Record<string, { utilization?: number; resetsAt?: number }>;
}

export interface ClaudeRootHost {
	/** The lane's working directory (Claude Code runs there, with no tools of its own). */
	readonly cwd: string;
	/** A lane's own working directory when it has one (a worktree subagent), by lane key. */
	cwdFor?(key: string): string | undefined;
	/** The MCP server command for a lane process: the bridge back to this worker, serving `token`'s tools. */
	bridgeCommand(token: string): { command: string; args: string[] };
	/** Route bridge calls for `token` to `lane` until the returned function is called. */
	registerBridge(token: string, lane: LaneBridge): () => void;
	load(key: string): Promise<ClaudeLaneRecord | undefined>;
	save(key: string, record: ClaudeLaneRecord): Promise<void>;
	/** Appended to the lane's system prompt (how the tools are named under Claude Code). */
	readonly systemPromptNote?: string;
	readonly env?: NodeJS.ProcessEnv;
	onRateLimits?(limits: ClaudeRateLimits): void;
}

type ToolContent = ToolResultMessage["content"][number];

/** A lane's live `claude -p` process (one per run). */
interface LaneProcess {
	readonly key: string;
	readonly child: CliProcess;
	readonly token: string;
	readonly sessionId: string;
	readonly dir: string;
	readonly tools: BridgeTool[];
	/** Tool uses of the last response whose results the harness has not delivered yet. */
	readonly awaiting: Map<string, { name: string; args: Record<string, unknown> }>;
	/** Results delivered before the CLI asked for them. */
	readonly results: Map<string, BridgeResult>;
	/** MCP calls waiting for their result. */
	readonly calls: Map<string, (result: BridgeResult) => void>;
	/** The CLI finished a turn (a `result` event) and waits for the next user message. */
	turnDone: boolean;
	/** The last settled response (see ClaudeLaneRecord). */
	lastResponseId?: string;
	/** User messages the session got after `lastResponseId`. */
	delivered: number;
	/** The CLI's cumulative reported cost at the last `result`. */
	reportedCost: number;
	/** Ids of messages already read from partial events (their complete `assistant` events are duplicates). */
	readonly streamed: Set<string>;
	/** Resumed an existing Claude Code session (a failed resume starts a fresh one). */
	readonly resumed: boolean;
	unregister: () => void;
	/** Set when the process is being replaced or stopped, so a late exit is not reported as a failure. */
	closing: boolean;
}

function record(value: unknown): Record<string, unknown> | undefined {
	return typeof value === "object" && value !== null && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

function num(value: unknown): number | undefined {
	return typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : undefined;
}

function emptyUsage(): Usage {
	return {
		input: 0,
		output: 0,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens: 0,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
	};
}

/** Subscription windows from a `rate_limit_event` (`unifiedWindows`), with the event's own window as a fallback. */
export function rateLimitsFromEvent(event: CliEvent): ClaudeRateLimits | undefined {
	const info = record(event.rate_limit_info);
	if (!info) return undefined;
	const windows: ClaudeRateLimits["windows"] = {};
	for (const [name, value] of Object.entries(record(info.unifiedWindows) ?? {})) {
		const window = record(value);
		if (!window) continue;
		windows[name] = {
			...(num(window.utilization) === undefined ? {} : { utilization: num(window.utilization) }),
			...(num(window.resetsAt) === undefined ? {} : { resetsAt: num(window.resetsAt) }),
		};
	}
	if (typeof info.rateLimitType === "string" && windows[info.rateLimitType] === undefined)
		windows[info.rateLimitType] = {
			...(num(info.utilization) === undefined ? {} : { utilization: num(info.utilization) }),
			...(num(info.resetsAt) === undefined ? {} : { resetsAt: num(info.resetsAt) }),
		};
	return { ...(typeof info.status === "string" ? { status: info.status } : {}), windows };
}

function contentText(content: string | readonly (TextContent | ImageContent)[]): string {
	return typeof content === "string"
		? content
		: content.map((part) => (part.type === "text" ? part.text : "[image]")).join("");
}

type InputBlock =
	| { type: "text"; text: string }
	| { type: "image"; source: { type: "base64"; media_type: string; data: string } };

function pushText(blocks: InputBlock[], text: string): void {
	if (!text) return;
	const last = blocks.at(-1);
	if (last?.type === "text") last.text += text;
	else blocks.push({ type: "text", text });
}

function pushContent(blocks: InputBlock[], content: string | readonly (TextContent | ImageContent)[]): void {
	if (typeof content === "string") {
		pushText(blocks, content);
		return;
	}
	for (const part of content) {
		if (part.type === "text") pushText(blocks, part.text);
		else blocks.push({ type: "image", source: { type: "base64", media_type: part.mimeType, data: part.data } });
	}
}

function toolCallText(call: ToolCall): string {
	const code = typeof call.arguments.code === "string" ? call.arguments.code : undefined;
	return code === undefined
		? `[${call.name} call ${call.id}: ${JSON.stringify(call.arguments)}]`
		: `[${call.name} call ${call.id}]\n\`\`\`python\n${code}\n\`\`\``;
}

function toolResultText(message: ToolResultMessage): string {
	return `[${message.toolName} result ${message.toolCallId}${message.isError ? " (error)" : ""}]\n${contentText(message.content)}`;
}

/**
 * The messages the Claude Code session has not seen, as one user message. With `transcript` (a fresh session for a
 * conversation that already has history) earlier turns are rendered as a text transcript first.
 */
export function renderInput(messages: readonly Message[], transcript: boolean): InputBlock[] {
	const blocks: InputBlock[] = [];
	const history = transcript ? messages.slice(0, lastUserIndex(messages)) : [];
	const fresh = transcript ? messages.slice(lastUserIndex(messages)) : messages;
	if (history.length > 0) {
		pushText(
			blocks,
			"<conversation_so_far>\nThis conversation started before this session; here it is so far (tool calls and results included):\n",
		);
		for (const message of history) {
			if (message.role === "user") {
				pushText(blocks, "<user>\n");
				pushContent(blocks, message.content);
				pushText(blocks, "\n</user>\n");
			} else if (message.role === "assistant") {
				const parts = message.content.flatMap((part) =>
					part.type === "text" ? [part.text] : part.type === "toolCall" ? [toolCallText(part)] : [],
				);
				if (parts.length > 0) pushText(blocks, `<assistant>\n${parts.join("\n")}\n</assistant>\n`);
			} else if (message.role === "toolResult") {
				pushText(blocks, `<tool_result>\n${toolResultText(message)}\n</tool_result>\n`);
			}
		}
		pushText(blocks, "</conversation_so_far>\n\n");
	}
	const results = fresh.filter((message): message is ToolResultMessage => message.role === "toolResult");
	if (results.length > 0)
		pushText(
			blocks,
			`Results of your last tool calls (the session was interrupted before they were delivered):\n${results.map(toolResultText).join("\n\n")}\n\n`,
		);
	let first = true;
	for (const message of fresh) {
		if (message.role !== "user") continue;
		if (!first) pushText(blocks, "\n\n");
		pushContent(blocks, message.content);
		first = false;
	}
	if (blocks.length === 0) pushText(blocks, "Continue.");
	return blocks;
}

function lastUserIndex(messages: readonly Message[]): number {
	for (let index = messages.length - 1; index >= 0; index--) {
		const message = messages[index]!;
		if (message.role === "user") {
			// The run's input starts at the first user message after the last assistant message.
			let start = index;
			while (start > 0 && messages[start - 1]!.role === "user") start--;
			return start;
		}
		if (message.role === "assistant") break;
	}
	return messages.length;
}

/**
 * Where the transcript continues the Claude Code session: the index of its last response (-1 for a session that has
 * none yet), or undefined when the transcript has moved on without it (another model answered, the tree was
 * navigated) or never had it.
 */
function continuationIndex(messages: readonly Message[], responseId: string | undefined): number | undefined {
	for (let index = messages.length - 1; index >= 0; index--) {
		const message = messages[index]!;
		if (message.role !== "assistant") continue;
		// Aborted and failed responses are not part of what the lane replays.
		if (message.stopReason === "aborted" || message.stopReason === "error") continue;
		return responseId !== undefined && message.responseId === responseId ? index : undefined;
	}
	return responseId === undefined ? -1 : undefined;
}

/** Drop the first `count` user messages (the session already has them). */
function withoutConsumed(messages: readonly Message[], count: number): Message[] {
	let left = count;
	return messages.filter((message) => {
		if (message.role !== "user" || left <= 0) return true;
		left -= 1;
		return false;
	});
}

function effortFor(model: Model<Api>, options: SimpleStreamOptions | undefined): string | undefined {
	const level = options?.reasoning;
	if (!level || !model.reasoning) return undefined;
	const mapped = model.thinkingLevelMap?.[level];
	return mapped === null ? undefined : (mapped ?? level);
}

function mcpContent(content: readonly ToolContent[]): BridgeContent[] {
	return content.map((part) =>
		part.type === "text"
			? { type: "text" as const, text: part.text }
			: { type: "image" as const, data: part.data, mimeType: part.mimeType },
	);
}

const EXIT_GRACE_MS = 3_000;

/** One per worker: every lane with tools on a claude-code model runs here. */
/** Diagnostic type carrying the subscription usage Claude Code last reported (the TUI footer shows it). */
export const CLAUDE_USAGE_DIAGNOSTIC = "claude_code_usage";

/** The usage diagnostic: `details.windows` maps a window name to `{ utilization, resetsAt }`. */
export function usageDiagnostic(limits: ClaudeRateLimits): AssistantMessageDiagnostic {
	const windows: JsonObject = {};
	for (const [name, window] of Object.entries(limits.windows))
		windows[name] = {
			...(window.utilization === undefined ? {} : { utilization: window.utilization }),
			...(window.resetsAt === undefined ? {} : { resetsAt: window.resetsAt }),
		};
	return {
		type: CLAUDE_USAGE_DIAGNOSTIC,
		timestamp: Date.now(),
		details: { ...(limits.status === undefined ? {} : { status: limits.status }), windows },
	};
}

export class ClaudeRootRunner {
	readonly #host: ClaudeRootHost;
	readonly #lanes = new Map<string, LaneProcess>();
	#limits: ClaudeRateLimits | undefined;

	constructor(host: ClaudeRootHost) {
		this.#host = host;
	}

	/** Live lane processes (for tests and status). */
	get liveLanes(): string[] {
		return [...this.#lanes.keys()];
	}

	pid(key: string): number | undefined {
		return this.#lanes.get(key)?.child.child.pid;
	}

	/** The lane's run ended: its process finishes (or stops, when a tool call is still open). */
	endRun(key: string): void {
		const lane = this.#lanes.get(key);
		if (!lane) return;
		this.#close(lane, lane.awaiting.size > 0 || lane.calls.size > 0 || !lane.turnDone);
	}

	/** Stop every process (worker shutdown). */
	close(): void {
		for (const lane of [...this.#lanes.values()]) this.#close(lane, true);
	}

	#close(lane: LaneProcess, kill: boolean): void {
		if (this.#lanes.get(lane.key) === lane) this.#lanes.delete(lane.key);
		lane.closing = true;
		lane.unregister();
		for (const resolve of lane.calls.values())
			resolve({
				content: [{ type: "text", text: "The Ultron run ended before this tool call completed." }],
				isError: true,
			});
		lane.calls.clear();
		if (kill) lane.child.kill();
		else {
			lane.child.finish();
			const timer = setTimeout(() => lane.child.kill(), EXIT_GRACE_MS);
			timer.unref?.();
		}
		const cleanup = () => rmSync(lane.dir, { recursive: true, force: true });
		if (lane.child.exited) cleanup();
		else lane.child.child.once("exit", cleanup);
	}

	/** The ClaudeCodeToolRunner: one lane response. */
	stream(
		model: Model<Api>,
		context: TranscriptContext,
		options: SimpleStreamOptions | undefined,
	): AssistantMessageEventStream {
		const stream = new AssistantMessageEventStream();
		const output: AssistantMessage = {
			role: "assistant",
			content: [],
			api: model.api,
			provider: model.provider,
			model: model.id,
			usage: emptyUsage(),
			stopReason: "stop",
			timestamp: Date.now(),
		};
		void this.#respond(model, context, options, stream, output);
		return stream;
	}

	async #respond(
		model: Model<Api>,
		context: TranscriptContext,
		options: SimpleStreamOptions | undefined,
		stream: AssistantMessageEventStream,
		output: AssistantMessage,
	): Promise<void> {
		const signal = options?.signal;
		const key = (options as { sessionId?: string } | undefined)?.sessionId ?? "default";
		const warnings: string[] = [];
		let started = false;
		const start = () => {
			if (started) return;
			started = true;
			stream.push({ type: "start", partial: output });
		};
		let lane: LaneProcess | undefined;
		try {
			signal?.throwIfAborted();
			const messages = context.messages.filter((message) => message.role !== "system") as Message[];
			const tools = getCurrentTools(context.messages).map(
				(tool): BridgeTool => ({
					name: tool.name,
					description: tool.description,
					inputSchema: JSON.parse(JSON.stringify(tool.parameters ?? { type: "object" })) as Record<
						string,
						unknown
					>,
				}),
			);
			lane = this.#lanes.get(key);
			const continueAt = continuationIndex(messages, lane?.lastResponseId);
			if (lane?.child.alive && continueAt !== undefined) {
				this.#deliver(lane, messages.slice(continueAt + 1));
				start();
				await this.#read(lane, output, stream, signal, warnings);
			} else {
				if (lane) this.#close(lane, true);
				lane = await this.#spawn(key, model, context, options, messages, tools, warnings, false);
				start();
				try {
					await this.#read(lane, output, stream, signal, warnings);
				} catch (error) {
					// Claude Code no longer has the session (deleted, or made elsewhere): start over with the transcript.
					if (!lane.resumed || output.content.length > 0 || !/could not resume session/.test(String(error)))
						throw error;
					this.#close(lane, true);
					warnings.push(String((error as Error).message));
					lane = await this.#spawn(key, model, context, options, messages, tools, warnings, true);
					await this.#read(lane, output, stream, signal, warnings);
				}
			}
			if (output.responseId !== undefined && output.responseId !== lane.lastResponseId) {
				lane.lastResponseId = output.responseId;
				lane.delivered = 0;
				await this.#host
					.save(key, { sessionId: lane.sessionId, lastResponseId: output.responseId })
					.catch(() => {});
			}
			const diagnostics: AssistantMessageDiagnostic[] = warnings.map((message) => ({
				type: "claude_code_warning",
				timestamp: Date.now(),
				error: { message },
			}));
			if (this.#limits) diagnostics.push(usageDiagnostic(this.#limits));
			if (diagnostics.length > 0) output.diagnostics = diagnostics;
			stream.push({
				type: "done",
				reason: output.stopReason === "toolUse" ? "toolUse" : output.stopReason === "length" ? "length" : "stop",
				message: output,
			});
			stream.end(output);
		} catch (error) {
			const aborted = signal?.aborted === true || (error as { aborted?: boolean })?.aborted === true;
			if (lane) {
				// The session keeps this turn's input; the next run continues after it.
				const failed = lane;
				await this.#host
					.save(key, {
						sessionId: failed.sessionId,
						...(failed.lastResponseId === undefined ? {} : { lastResponseId: failed.lastResponseId }),
						...(failed.delivered > 0 ? { consumed: failed.delivered } : {}),
					})
					.catch(() => {});
				this.#close(lane, true);
			}
			start();
			output.stopReason = aborted ? "aborted" : "error";
			output.errorMessage = aborted ? "Request was aborted" : error instanceof Error ? error.message : String(error);
			const diagnostics: AssistantMessageDiagnostic[] = warnings.map((message) => ({
				type: "claude_code_warning",
				timestamp: Date.now(),
				error: { message },
			}));
			if (this.#limits) diagnostics.push(usageDiagnostic(this.#limits));
			if ((error as { usageLimit?: boolean })?.usageLimit === true)
				diagnostics.push({
					type: USAGE_LIMIT_DIAGNOSTIC,
					timestamp: Date.now(),
					error: { message: output.errorMessage },
				});
			if (diagnostics.length > 0) output.diagnostics = diagnostics;
			stream.push({ type: "error", reason: output.stopReason, error: output });
			stream.end(output);
		}
	}

	/**
	 * Hand the harness's tool results to the waiting MCP calls. New user messages (steering, follow-ups, runtime
	 * events) go to the CLI's stdin first, so Claude Code has them queued when the results arrive and hands them
	 * to the model with the results (or, after a finished turn, starts a new turn with them).
	 */
	#deliver(lane: LaneProcess, fresh: readonly Message[]): void {
		const users = fresh.filter((message) => message.role === "user");
		const results = fresh.filter((message): message is ToolResultMessage => message.role === "toolResult");
		if (users.length > 0) {
			lane.child.send({ type: "user", message: { role: "user", content: renderInput(users, false) } });
			lane.delivered += users.length;
			lane.turnDone = false;
		} else if (lane.turnDone && results.length === 0) {
			// Asked for another response with nothing new (a retry after a failed read): nudge the session on.
			lane.child.send({ type: "user", message: { role: "user", content: [{ type: "text", text: "Continue." }] } });
			lane.turnDone = false;
		}
		for (const message of results) {
			const result: BridgeResult = { content: mcpContent(message.content), isError: message.isError };
			lane.awaiting.delete(message.toolCallId);
			const waiting = lane.calls.get(message.toolCallId);
			if (waiting) {
				lane.calls.delete(message.toolCallId);
				waiting(result);
			} else lane.results.set(message.toolCallId, result);
		}
	}

	async #spawn(
		key: string,
		model: Model<Api>,
		context: TranscriptContext,
		options: SimpleStreamOptions | undefined,
		messages: readonly Message[],
		tools: BridgeTool[],
		warnings: string[],
		startOver: boolean,
	): Promise<LaneProcess> {
		const env = childEnv(options);
		if (!env.MCP_TOOL_TIMEOUT) env.MCP_TOOL_TIMEOUT = String(MCP_TOOL_TIMEOUT_MS);
		const bin = resolveClaudeCli(env);
		const cwd = this.#host.cwdFor?.(key) ?? this.#host.cwd;
		const capabilities = await ensureClaudeCli(bin, env, cwd);
		warnings.push(...capabilities.warnings);
		for (const flag of ["--session-id", "--resume", "--allowedtools"])
			if (!capabilities.flags.has(flag))
				throw new Error(
					`Claude Code ${capabilities.version} lacks ${flag}, which \`ultron --claude\` needs. Update Claude Code (\`claude update\`).`,
				);
		options?.signal?.throwIfAborted();
		const saved = startOver ? undefined : await this.#host.load(key).catch(() => undefined);
		const continueAt = continuationIndex(messages, saved?.lastResponseId);
		const resume = saved !== undefined && continueAt !== undefined;
		const sessionId = resume ? saved.sessionId : randomUUID();
		const consumed = resume ? (saved.consumed ?? 0) : 0;
		const fresh = resume ? withoutConsumed(messages.slice(continueAt + 1), consumed) : messages;
		const input = renderInput(fresh, !resume && messages.some((message) => message.role === "assistant"));
		const delivered = consumed + fresh.filter((message) => message.role === "user").length;
		if (!resume) await this.#host.save(key, { sessionId }).catch(() => {});
		const token = randomBytes(16).toString("hex");
		const dir = mkdtempSync(join(tmpdir(), "ultron-claude-root-"));
		const system = [getCurrentSystemPrompt(context.messages).trim(), this.#host.systemPromptNote?.trim()]
			.filter(Boolean)
			.join("\n\n");
		const promptPath = join(dir, "system-prompt.md");
		writeFileSync(promptPath, system || "You are a helpful assistant.", { mode: 0o600 });
		const bridge = this.#host.bridgeCommand(token);
		const allowed = tools.map((tool) => `${TOOL_PREFIX}${tool.name}`);
		const effort = effortFor(model, options);
		const args = [
			"--print",
			"--output-format=stream-json",
			"--input-format=stream-json",
			"--verbose",
			"--include-partial-messages",
			"--tools=",
			"--strict-mcp-config",
			`--mcp-config=${JSON.stringify({ mcpServers: { [BRIDGE_SERVER]: { type: "stdio", command: bridge.command, args: bridge.args } } })}`,
			`--allowedTools=${allowed.join(",")}`,
			"--setting-sources=",
			...(capabilities.flags.has("--permission-prompts") ? ["--permission-prompts=none"] : []),
			...(capabilities.flags.has("--disable-slash-commands") ? ["--disable-slash-commands"] : []),
			`--settings=${JSON.stringify({ permissions: { allow: allowed }, ...(effort ? {} : { alwaysThinkingEnabled: false }) })}`,
			`--model=${model.id}`,
			`--system-prompt-file=${promptPath}`,
			...(effort && capabilities.flags.has("--effort") ? [`--effort=${effort}`] : []),
			...(resume ? [`--resume=${sessionId}`] : [`--session-id=${sessionId}`]),
		];
		const child = new CliProcess(bin, args, env, cwd);
		const lane: LaneProcess = {
			key,
			child,
			token,
			sessionId,
			dir,
			tools,
			awaiting: new Map(),
			results: new Map(),
			calls: new Map(),
			turnDone: false,
			...(resume && saved?.lastResponseId !== undefined ? { lastResponseId: saved.lastResponseId } : {}),
			delivered,
			reportedCost: 0,
			streamed: new Set(),
			resumed: resume,
			unregister: () => {},
			closing: false,
		};
		lane.unregister = this.#host.registerBridge(token, {
			tools: () => lane.tools,
			call: (toolUseId, name, args, signal) => this.#bridgeCall(lane, toolUseId, name, args, signal),
		});
		this.#lanes.set(key, lane);
		child.send({ type: "user", message: { role: "user", content: input } });
		return lane;
	}

	#bridgeCall(
		lane: LaneProcess,
		toolUseId: string | undefined,
		name: string,
		args: Record<string, unknown>,
		signal: AbortSignal,
	): Promise<BridgeResult> {
		let id = toolUseId;
		if (id === undefined) {
			// An older CLI without the tool-use id in `_meta`: the oldest open call of this tool with these arguments.
			const wanted = JSON.stringify(args);
			for (const [candidate, call] of lane.awaiting)
				if (call.name === name && JSON.stringify(call.args) === wanted && !lane.calls.has(candidate)) {
					id = candidate;
					break;
				}
			if (id === undefined)
				for (const [candidate] of lane.results) {
					id = candidate;
					break;
				}
		}
		if (id === undefined)
			return Promise.resolve({
				content: [{ type: "text", text: `No pending ${name} call to answer.` }],
				isError: true,
			});
		const ready = lane.results.get(id);
		if (ready) {
			lane.results.delete(id);
			return Promise.resolve(ready);
		}
		const callId = id;
		return new Promise((resolve) => {
			const onAbort = () => {
				lane.calls.delete(callId);
				resolve({ content: [{ type: "text", text: "cancelled" }], isError: true });
			};
			signal.addEventListener("abort", onAbort, { once: true });
			lane.calls.set(callId, (result) => {
				signal.removeEventListener("abort", onAbort);
				resolve(result);
			});
		});
	}

	/** Read the CLI's events into `output` until it waits for tool results or finishes the turn. */
	async #read(
		lane: LaneProcess,
		output: AssistantMessage,
		stream: AssistantMessageEventStream,
		signal: AbortSignal | undefined,
		warnings: string[],
	): Promise<void> {
		const child = lane.child;
		let aborted = false;
		const onAbort = () => {
			aborted = true;
			child.kill();
			child.interrupt();
		};
		signal?.addEventListener("abort", onAbort, { once: true });
		if (signal?.aborted) onAbort();
		/** CLI block index -> output content index. */
		const blocks = new Map<number, number>();
		const partialJson = new Map<number, string>();
		const streamed = lane.streamed;
		let stopReason: string | undefined;
		let rateLimit: RateLimitInfo | undefined;
		let messageUsage = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
		const settleUsage = () => {
			output.usage.input = messageUsage.input;
			output.usage.output = messageUsage.output;
			output.usage.cacheRead = messageUsage.cacheRead;
			output.usage.cacheWrite = messageUsage.cacheWrite;
			output.usage.totalTokens =
				messageUsage.input + messageUsage.output + messageUsage.cacheRead + messageUsage.cacheWrite;
		};
		const usageOf = (value: unknown) => {
			const usage = record(value);
			if (!usage) return;
			messageUsage = {
				input: num(usage.input_tokens) ?? messageUsage.input,
				output: num(usage.output_tokens) ?? messageUsage.output,
				cacheRead: num(usage.cache_read_input_tokens) ?? messageUsage.cacheRead,
				cacheWrite: num(usage.cache_creation_input_tokens) ?? messageUsage.cacheWrite,
			};
		};
		const addToolCall = (id: string, name: string, args: Record<string, unknown>): number => {
			const call: ToolCall = {
				type: "toolCall",
				id,
				name: name.startsWith(TOOL_PREFIX) ? name.slice(TOOL_PREFIX.length) : name,
				arguments: args as ToolCall["arguments"],
			};
			const index = output.content.push(call) - 1;
			stream.push({ type: "toolcall_start", contentIndex: index, partial: output });
			return index;
		};
		const endToolCall = (index: number) => {
			const call = output.content[index] as ToolCall;
			lane.awaiting.set(call.id, { name: call.name, args: call.arguments as Record<string, unknown> });
			stream.push({ type: "toolcall_end", contentIndex: index, toolCall: call, partial: output });
		};
		const addText = (text: string) => {
			const index = output.content.push({ type: "text", text: "" }) - 1;
			stream.push({ type: "text_start", contentIndex: index, partial: output });
			(output.content[index] as TextContent).text = text;
			stream.push({ type: "text_delta", contentIndex: index, delta: text, partial: output });
			stream.push({ type: "text_end", contentIndex: index, content: text, partial: output });
		};
		try {
			for (;;) {
				const event = aborted ? undefined : await child.next();
				if (aborted) throw Object.assign(new Error("Request was aborted"), { aborted: true });
				if (!event) {
					if (child.spawnError) throw new Error(`Claude Code CLI failed to start: ${child.spawnError.message}`);
					const stderr = child.stderr.trim();
					if (/No conversation found/i.test(stderr))
						throw new Error(`Claude Code could not resume session ${lane.sessionId}: ${stderr.slice(-500)}`);
					throw new Error(
						`Claude Code CLI exited (${child.exitSignal ?? `code ${child.exitCode}`}) before finishing the turn${stderr ? `: ${stderr.slice(-1_000)}` : ""}`,
					);
				}
				if (event.type === "system" && event.subtype === "init") {
					const source = event.apiKeySource;
					if (typeof source === "string" && source !== "none")
						warnings.push(
							`the Claude Code CLI is using an API key (apiKeySource: ${source}) instead of the subscription login; calls may be billed to that key`,
						);
				} else if (event.type === "rate_limit_event") {
					rateLimit = rateLimitInfo(event) ?? rateLimit;
					const limits = rateLimitsFromEvent(event);
					if (limits) {
						this.#limits = limits;
						this.#host.onRateLimits?.(limits);
					}
				} else if (event.type === "stream_event") {
					const inner = record(event.event);
					if (!inner) continue;
					const type = inner.type;
					if (type === "message_start") {
						const message = record(inner.message);
						blocks.clear();
						partialJson.clear();
						if (typeof message?.id === "string") {
							streamed.add(message.id);
							output.responseId = message.id;
						}
						if (typeof message?.model === "string" && message.model !== output.model)
							output.responseModel = message.model;
						usageOf(message?.usage);
						settleUsage();
						continue;
					}
					if (type === "message_delta") {
						const delta = record(inner.delta);
						if (typeof delta?.stop_reason === "string") stopReason = delta.stop_reason;
						usageOf(inner.usage);
						settleUsage();
						continue;
					}
					if (type === "message_stop") {
						// The CLI now runs the tool calls: the response ends here and the harness runs them.
						if (stopReason === "tool_use" && output.content.some((part) => part.type === "toolCall")) {
							output.stopReason = "toolUse";
							output.rawStopReason = "tool_use";
							return;
						}
						continue;
					}
					const cliIndex = typeof inner.index === "number" ? inner.index : -1;
					if (type === "content_block_start") {
						const block = record(inner.content_block);
						if (block?.type === "text") {
							const index = output.content.push({ type: "text", text: "" }) - 1;
							blocks.set(cliIndex, index);
							stream.push({ type: "text_start", contentIndex: index, partial: output });
						} else if (block?.type === "thinking") {
							const index = output.content.push({ type: "thinking", thinking: "" }) - 1;
							blocks.set(cliIndex, index);
							stream.push({ type: "thinking_start", contentIndex: index, partial: output });
						} else if (block?.type === "tool_use" && typeof block.id === "string") {
							const index = addToolCall(block.id, String(block.name ?? ""), {});
							blocks.set(cliIndex, index);
							partialJson.set(cliIndex, "");
						}
						continue;
					}
					const index = blocks.get(cliIndex);
					if (index === undefined) continue;
					const target = output.content[index]!;
					if (type === "content_block_delta") {
						const delta = record(inner.delta);
						if (delta?.type === "text_delta" && typeof delta.text === "string" && target.type === "text") {
							target.text += delta.text;
							stream.push({ type: "text_delta", contentIndex: index, delta: delta.text, partial: output });
						} else if (
							delta?.type === "thinking_delta" &&
							typeof delta.thinking === "string" &&
							target.type === "thinking"
						) {
							if (!delta.thinking) continue;
							target.thinking += delta.thinking;
							stream.push({
								type: "thinking_delta",
								contentIndex: index,
								delta: delta.thinking,
								partial: output,
							});
						} else if (
							delta?.type === "signature_delta" &&
							typeof delta.signature === "string" &&
							target.type === "thinking"
						) {
							target.thinkingSignature = (target.thinkingSignature ?? "") + delta.signature;
						} else if (
							delta?.type === "input_json_delta" &&
							typeof delta.partial_json === "string" &&
							target.type === "toolCall"
						) {
							const json = (partialJson.get(cliIndex) ?? "") + delta.partial_json;
							partialJson.set(cliIndex, json);
							target.arguments = parseStreamingJson(json) as ToolCall["arguments"];
							stream.push({
								type: "toolcall_delta",
								contentIndex: index,
								delta: delta.partial_json,
								partial: output,
							});
						}
					} else if (type === "content_block_stop") {
						blocks.delete(cliIndex);
						if (target.type === "text")
							stream.push({ type: "text_end", contentIndex: index, content: target.text, partial: output });
						else if (target.type === "thinking") {
							// Claude Code streams no thinking text for these models (only a signature): show nothing.
							if (!target.thinking) target.redacted = true;
							stream.push({
								type: "thinking_end",
								contentIndex: index,
								content: target.thinking,
								partial: output,
							});
						} else if (target.type === "toolCall") {
							const json = partialJson.get(cliIndex);
							partialJson.delete(cliIndex);
							if (json) {
								try {
									target.arguments = JSON.parse(json) as ToolCall["arguments"];
								} catch {
									target.arguments = parseStreamingJson(json) as ToolCall["arguments"];
								}
							}
							endToolCall(index);
						}
					}
				} else if (event.type === "assistant") {
					// A complete message the CLI sent without partial events: take its content at once.
					const message = record(event.message);
					if (!message || message.model === "<synthetic>") continue;
					if (typeof message.id === "string" && streamed.has(message.id)) continue;
					if (typeof message.id === "string") output.responseId = message.id;
					usageOf(message.usage);
					settleUsage();
					for (const block of Array.isArray(message.content) ? message.content : []) {
						const part = record(block);
						if (part?.type === "text" && typeof part.text === "string" && part.text) addText(part.text);
						else if (part?.type === "tool_use" && typeof part.id === "string")
							endToolCall(addToolCall(part.id, String(part.name ?? ""), record(part.input) ?? {}));
					}
					if (message.stop_reason === "tool_use" && output.content.some((part) => part.type === "toolCall")) {
						output.stopReason = "toolUse";
						output.rawStopReason = "tool_use";
						return;
					}
				} else if (event.type === "result") {
					lane.turnDone = true;
					const total = num(event.total_cost_usd);
					if (total !== undefined) {
						// The CLI reports the process's running total; this turn's cost is the difference.
						output.usage.cost.total = total >= lane.reportedCost ? total - lane.reportedCost : total;
						lane.reportedCost = total;
					} else output.usage.cost.total = Number.NaN;
					if (event.is_error === true) {
						const failure = classifyFailure(event, rateLimit);
						if (failure.kind === "length") {
							output.stopReason = "length";
							output.rawStopReason = "max_output_tokens";
							return;
						}
						throw Object.assign(new Error(failure.message), {
							usageLimit: failure.kind === "error" && failure.usageLimit === true,
						});
					}
					if (
						!output.content.some((part) => part.type === "text") &&
						typeof event.result === "string" &&
						event.result
					)
						addText(event.result);
					const reason = typeof event.stop_reason === "string" ? event.stop_reason : stopReason;
					output.stopReason = reason === "max_tokens" ? "length" : "stop";
					if (reason) output.rawStopReason = reason;
					output.endTurn = reason === "end_turn";
					return;
				}
			}
		} finally {
			signal?.removeEventListener("abort", onAbort);
		}
	}
}
