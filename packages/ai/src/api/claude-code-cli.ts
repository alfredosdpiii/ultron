/**
 * Claude Code CLI transport: one model call = one headless `claude -p` process speaking stream-json.
 *
 * Only public CLI flags are used, and only after `claude --help` shows them. The child `claude` does its own
 * authentication; this module never reads, copies or mints Claude credentials, and passes CLAUDE_CONFIG_DIR
 * through untouched. Before the first request `claude auth status --json` must report a logged-in first-party
 * account. Every call is isolated from the user's Claude Code setup: no tools, no MCP servers, no settings
 * sources (so no hooks, no CLAUDE.md), no slash commands, no session persistence, an empty working directory,
 * and a replaced system prompt (`--system-prompt`, never the default Claude Code prompt).
 *
 * The provider has no tool calling. A request whose transcript declares tools fails before any process starts.
 *
 * A process serves exactly one request: a stream-json process carries context between the prompts it is
 * given, so reusing one would leak one frame into the next. What is pooled is startup: after a request, one
 * idle process with the same arguments is started ahead of time (bounded, expiring), and the next request of
 * that shape writes its prompt into it instead of paying the CLI's start-up. Active requests are capped
 * (ULTRON_CLAUDE_CODE_CONCURRENCY, default 4) because subscription rate limits are shared.
 */
import type * as NodeChildProcess from "node:child_process";
import type * as NodeFs from "node:fs";
import type * as NodeOs from "node:os";
import type * as NodePath from "node:path";
import { CLAUDE_CODE_BIN_ENV } from "../providers/claude-code.ts";
import type {
	Api,
	AssistantMessage,
	ImageContent,
	Message,
	Model,
	SimpleStreamOptions,
	StopReason,
	StreamOptions,
	TextContent,
	ThinkingContent,
	TranscriptContext,
	Usage,
} from "../types.ts";
import type { AssistantMessageDiagnostic } from "../utils/diagnostics.ts";
import { AssistantMessageEventStream } from "../utils/event-stream.ts";
import { getCurrentSystemPrompt, getCurrentTools } from "../utils/transcript.ts";

// ---------------------------------------------------------------------------------------------------------
// Node built-ins, loaded without a static import so the module stays bundle-safe.

interface NodeModules {
	childProcess: typeof NodeChildProcess;
	fs: typeof NodeFs;
	os: typeof NodeOs;
	path: typeof NodePath;
}

type ProcessWithBuiltins = typeof process & { getBuiltinModule?: (id: string) => unknown };

let nodeModules: NodeModules | undefined;
function node(): NodeModules {
	if (nodeModules) return nodeModules;
	const load = typeof process === "undefined" ? undefined : (process as ProcessWithBuiltins).getBuiltinModule;
	if (!load) throw new Error("The claude-code provider needs Node.js or Bun: it runs the local `claude` CLI.");
	nodeModules = {
		childProcess: load("node:child_process") as typeof NodeChildProcess,
		fs: load("node:fs") as typeof NodeFs,
		os: load("node:os") as typeof NodeOs,
		path: load("node:path") as typeof NodePath,
	};
	return nodeModules;
}

// ---------------------------------------------------------------------------------------------------------
// Configuration

export const CLAUDE_CODE_CONCURRENCY_ENV = "ULTRON_CLAUDE_CODE_CONCURRENCY";
export const CLAUDE_CODE_TIMEOUT_ENV = "ULTRON_CLAUDE_CODE_TIMEOUT_MS";
export const CLAUDE_CODE_WARM_ENV = "ULTRON_CLAUDE_CODE_WARM";
export const CLAUDE_CODE_WARM_TTL_ENV = "ULTRON_CLAUDE_CODE_WARM_TTL_MS";
export const CLAUDE_CODE_ALLOW_THIRD_PARTY_ENV = "ULTRON_CLAUDE_CODE_ALLOW_THIRD_PARTY";

export const DEFAULT_CONCURRENCY = 4;
export const DEFAULT_TIMEOUT_MS = 10 * 60 * 1000;
export const DEFAULT_WARM_SPARES = 2;
export const DEFAULT_WARM_TTL_MS = 30_000;
const PROBE_TIMEOUT_MS = 30_000;
const PROBE_FAILURE_TTL_MS = 10_000;
const EXIT_GRACE_MS = 5_000;
const KILL_GRACE_MS = 2_000;
const STDERR_TAIL_CHARS = 4_000;
/** Longer system prompts go into the user message: one argv string is capped near 128 KiB on Linux. */
const MAX_SYSTEM_PROMPT_ARG_CHARS = 100_000;
const NEUTRAL_SYSTEM_PROMPT = "You are a helpful assistant. Follow the user's instructions exactly.";
const TOOLS_UNSUPPORTED =
	"does not support tool calling: `claude -p` returns completions, not raw tool calls. Use it for tool-free lanes (rlm.infer/rlm.map frames, /review frames, judges), or run Claude Code as the root agent with `ultron claude`.";

/** Flags every request relies on. Each must appear in `claude --help`. */
export const REQUIRED_FLAGS = [
	"--print",
	"--output-format",
	"--input-format",
	"--verbose",
	"--include-partial-messages",
	"--tools",
	"--strict-mcp-config",
	"--mcp-config",
	"--setting-sources",
	"--permission-prompts",
	"--disable-slash-commands",
	"--no-session-persistence",
	"--model",
	"--system-prompt",
	"--settings",
] as const;
/** Flags used only by some requests; a request that needs a missing one fails naming it. */
export const OPTIONAL_FLAGS = ["--json-schema", "--effort"] as const;

/**
 * Session variables a parent Claude Code sets for its own children. A frame is not a child session of whatever
 * Claude Code may be running Ultron, so they are dropped; CLAUDE_CONFIG_DIR and everything else pass through.
 */
const PARENT_SESSION_ENV = [
	"CLAUDECODE",
	"CLAUDE_CODE_ENTRYPOINT",
	"CLAUDE_CODE_SESSION_ID",
	"CLAUDE_CODE_CHILD_SESSION",
	"CLAUDE_CODE_SESSION_ATTENDED",
	"CLAUDE_CODE_MESSAGING_SOCKET",
	"CLAUDE_CODE_MESSAGING_TOKEN",
	"CLAUDE_CODE_EXECPATH",
	"CLAUDE_CODE_SSE_PORT",
	"CLAUDE_PID",
	"CLAUDE_EFFORT",
	"CLAUDE_CODE_MAX_OUTPUT_TOKENS",
];

type Env = Record<string, string | undefined>;

function positiveInt(value: string | undefined, fallback: number, allowZero = false): number {
	const parsed = Number(value?.trim());
	if (!value?.trim() || !Number.isSafeInteger(parsed) || parsed < (allowZero ? 0 : 1)) return fallback;
	return parsed;
}

interface RuntimeConfig {
	concurrency: number;
	timeoutMs: number;
	warmSpares: number;
	warmTtlMs: number;
}

function runtimeConfig(env: Env, options: StreamOptions | undefined): RuntimeConfig {
	return {
		concurrency: positiveInt(env[CLAUDE_CODE_CONCURRENCY_ENV], DEFAULT_CONCURRENCY),
		timeoutMs: options?.timeoutMs ?? positiveInt(env[CLAUDE_CODE_TIMEOUT_ENV], DEFAULT_TIMEOUT_MS),
		warmSpares: positiveInt(env[CLAUDE_CODE_WARM_ENV], DEFAULT_WARM_SPARES, true),
		warmTtlMs: positiveInt(env[CLAUDE_CODE_WARM_TTL_ENV], DEFAULT_WARM_TTL_MS),
	};
}

function childEnv(options: StreamOptions | undefined): Env {
	const env: Env = { ...process.env, ...(options?.env ?? {}) };
	for (const name of PARENT_SESSION_ENV) delete env[name];
	return env;
}

// ---------------------------------------------------------------------------------------------------------
// Locating the CLI

const resolvedCli = new Map<string, string | undefined>();

function isExecutable(file: string): boolean {
	const { fs } = node();
	try {
		if (!fs.statSync(file).isFile()) return false;
		if (process.platform === "win32") return true;
		fs.accessSync(file, fs.constants.X_OK);
		return true;
	} catch {
		return false;
	}
}

/** The `claude` executable: ULTRON_CLAUDE_CODE_BIN, else the first `claude` on PATH. Resolved once per PATH. */
export function resolveClaudeCli(env: Env): string {
	const configured = env[CLAUDE_CODE_BIN_ENV]?.trim();
	const key = `${configured ?? ""}\0${env.PATH ?? ""}`;
	if (!resolvedCli.has(key)) {
		const { path } = node();
		let found: string | undefined;
		if (configured) found = isExecutable(configured) ? configured : undefined;
		else {
			const names = process.platform === "win32" ? ["claude.exe", "claude.cmd", "claude"] : ["claude"];
			outer: for (const dir of (env.PATH ?? "").split(path.delimiter)) {
				if (!dir) continue;
				for (const name of names) {
					const candidate = path.join(dir, name);
					if (isExecutable(candidate)) {
						found = candidate;
						break outer;
					}
				}
			}
		}
		// Only a found CLI is remembered, so installing Claude Code later works without a restart.
		if (found) resolvedCli.set(key, found);
	}
	const found = resolvedCli.get(key);
	if (!found)
		throw new Error(
			configured
				? `claude-code provider: ${CLAUDE_CODE_BIN_ENV}=${configured} is not an executable file.`
				: "claude-code provider: the Claude Code CLI (`claude`) was not found on PATH. Install Claude Code and run `claude auth login`, or set ULTRON_CLAUDE_CODE_BIN.",
		);
	return found;
}

// ---------------------------------------------------------------------------------------------------------
// Feature detection and the login check (once per CLI and config dir)

export interface ClaudeCliCapabilities {
	version: string;
	flags: ReadonlySet<string>;
	auth: { loggedIn: boolean; authMethod?: string; apiProvider?: string; subscriptionType?: string };
	warnings: string[];
}

interface ProbeEntry {
	promise: Promise<ClaudeCliCapabilities>;
	failedAt?: number;
}

const probes = new Map<string, ProbeEntry>();

function execCli(
	bin: string,
	args: string[],
	env: Env,
	cwd: string,
): Promise<{ code: number | null; stdout: string; stderr: string }> {
	const { childProcess } = node();
	return new Promise((resolve, reject) => {
		childProcess.execFile(
			bin,
			args,
			{
				env: env as NodeJS.ProcessEnv,
				cwd,
				timeout: PROBE_TIMEOUT_MS,
				maxBuffer: 4 * 1024 * 1024,
				windowsHide: true,
			},
			(error, stdout, stderr) => {
				const code = error ? (typeof error.code === "number" ? error.code : null) : 0;
				if (error && typeof error.code !== "number") {
					reject(new Error(`claude-code provider: could not run ${bin} ${args.join(" ")}: ${error.message}`));
					return;
				}
				resolve({ code, stdout: String(stdout), stderr: String(stderr) });
			},
		);
	});
}

/** Long flags a help text lists, e.g. `--json-schema` from "  --json-schema <schema>  JSON Schema ...". */
export function parseHelpFlags(help: string): Set<string> {
	const flags = new Set<string>();
	for (const match of help.matchAll(/(?:^|[\s,])(--[a-z][a-z0-9-]*)/gim)) flags.add(match[1].toLowerCase());
	return flags;
}

function parseAuthStatus(stdout: string): ClaudeCliCapabilities["auth"] | undefined {
	try {
		const value = JSON.parse(stdout.trim()) as Record<string, unknown>;
		if (typeof value !== "object" || value === null || typeof value.loggedIn !== "boolean") return undefined;
		return {
			loggedIn: value.loggedIn,
			...(typeof value.authMethod === "string" ? { authMethod: value.authMethod } : {}),
			...(typeof value.apiProvider === "string" ? { apiProvider: value.apiProvider } : {}),
			...(typeof value.subscriptionType === "string" ? { subscriptionType: value.subscriptionType } : {}),
		};
	} catch {
		return undefined;
	}
}

async function probeCli(bin: string, env: Env, cwd: string): Promise<ClaudeCliCapabilities> {
	const [versionRun, helpRun, authRun] = await Promise.all([
		execCli(bin, ["--version"], env, cwd),
		execCli(bin, ["--help"], env, cwd),
		execCli(bin, ["auth", "status", "--json"], env, cwd),
	]);
	const version = versionRun.stdout.trim().split(/\s+/)[0] || "unknown";
	const flags = parseHelpFlags(helpRun.stdout);
	const missing = REQUIRED_FLAGS.filter((flag) => !flags.has(flag));
	if (missing.length > 0)
		throw new Error(
			`claude-code provider: the installed Claude Code CLI (${bin}, version ${version}) does not support ${missing.join(", ")} (not listed in \`claude --help\`). Update Claude Code (\`claude update\`).`,
		);
	const auth = parseAuthStatus(authRun.stdout);
	if (!auth)
		throw new Error(
			`claude-code provider: \`claude auth status --json\` did not report a login state (exit ${authRun.code}): ${tail(authRun.stderr || authRun.stdout, 500)}`,
		);
	if (!auth.loggedIn)
		throw new Error(
			"claude-code provider: the Claude Code CLI is not logged in. Run `claude auth login` (or `claude` and /login) and try again.",
		);
	const warnings: string[] = [];
	if (auth.apiProvider && auth.apiProvider !== "firstParty") {
		const message = `the Claude Code CLI is configured for ${auth.apiProvider}, not a first-party Anthropic login`;
		if (env[CLAUDE_CODE_ALLOW_THIRD_PARTY_ENV] !== "1")
			throw new Error(
				`claude-code provider: ${message}. Set ${CLAUDE_CODE_ALLOW_THIRD_PARTY_ENV}=1 to use it anyway.`,
			);
		warnings.push(message);
	}
	if (auth.authMethod && auth.authMethod !== "claude.ai")
		warnings.push(
			`the Claude Code CLI authenticates with ${auth.authMethod}, not a claude.ai subscription login; calls may be billed to that account`,
		);
	return { version, flags, auth, warnings };
}

/** Feature-detect the CLI and check its login once; a failure is retried after a short delay. */
export function ensureClaudeCli(bin: string, env: Env, cwd: string): Promise<ClaudeCliCapabilities> {
	const key = `${bin}\0${env.CLAUDE_CONFIG_DIR ?? ""}`;
	const cached = probes.get(key);
	if (cached && (cached.failedAt === undefined || Date.now() - cached.failedAt < PROBE_FAILURE_TTL_MS))
		return cached.promise;
	const entry: ProbeEntry = { promise: probeCli(bin, env, cwd) };
	entry.promise.catch(() => {
		entry.failedAt = Date.now();
	});
	probes.set(key, entry);
	return entry.promise;
}

// ---------------------------------------------------------------------------------------------------------
// Working directory: one empty directory per user, so no project CLAUDE.md or settings are near the call.

let workDir: string | undefined;
function emptyWorkDir(): string {
	if (workDir) return workDir;
	const { fs, os, path } = node();
	let user = "user";
	try {
		user = String(os.userInfo().uid >= 0 ? os.userInfo().uid : os.userInfo().username);
	} catch {}
	const dir = path.join(os.tmpdir(), `ultron-claude-code-${user}`);
	fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
	workDir = dir;
	return dir;
}

// ---------------------------------------------------------------------------------------------------------
// Concurrency cap

class Limiter {
	private active = 0;
	private readonly waiting: Array<() => void> = [];

	async acquire(limit: number, signal: AbortSignal | undefined): Promise<() => void> {
		while (this.active >= limit) {
			signal?.throwIfAborted();
			await new Promise<void>((resolve, reject) => {
				const wake = () => {
					signal?.removeEventListener("abort", onAbort);
					resolve();
				};
				const onAbort = () => {
					const index = this.waiting.indexOf(wake);
					if (index >= 0) this.waiting.splice(index, 1);
					reject(signal?.reason ?? new Error("aborted"));
				};
				this.waiting.push(wake);
				signal?.addEventListener("abort", onAbort, { once: true });
			});
		}
		this.active += 1;
		let released = false;
		return () => {
			if (released) return;
			released = true;
			this.active -= 1;
			this.waiting.shift()?.();
		};
	}

	get inFlight(): number {
		return this.active;
	}
}

const limiter = new Limiter();

// ---------------------------------------------------------------------------------------------------------
// Child processes

type CliEvent = Record<string, unknown> & { type?: string; subtype?: string };

class CliProcess {
	readonly child: NodeChildProcess.ChildProcess;
	private readonly lines: CliEvent[] = [];
	private waiter: (() => void) | undefined;
	private buffer = "";
	private stderrText = "";
	private interrupted = false;
	exited = false;
	exitCode: number | null = null;
	exitSignal: NodeJS.Signals | null = null;
	spawnError: Error | undefined;

	constructor(bin: string, args: string[], env: Env, cwd: string) {
		const { childProcess } = node();
		this.child = childProcess.spawn(bin, args, {
			cwd,
			env: env as NodeJS.ProcessEnv,
			stdio: ["pipe", "pipe", "pipe"],
			// Own process group, so an abort can stop the CLI and anything it started.
			detached: process.platform !== "win32",
			windowsHide: true,
		});
		this.child.stdout?.setEncoding("utf8");
		this.child.stderr?.setEncoding("utf8");
		this.child.stdout?.on("data", (chunk: string) => this.onStdout(chunk));
		this.child.stderr?.on("data", (chunk: string) => {
			this.stderrText = tail(this.stderrText + chunk, STDERR_TAIL_CHARS);
		});
		this.child.stdin?.on("error", () => {});
		this.child.on("error", (error) => {
			this.spawnError = error;
			this.onExit(null, null);
		});
		this.child.on("exit", (code, signal) => this.onExit(code, signal));
	}

	get alive(): boolean {
		return !this.exited && this.spawnError === undefined;
	}

	get stderr(): string {
		return this.stderrText;
	}

	private onStdout(chunk: string): void {
		this.buffer += chunk;
		let index = this.buffer.indexOf("\n");
		while (index >= 0) {
			const line = this.buffer.slice(0, index).trim();
			this.buffer = this.buffer.slice(index + 1);
			if (line.startsWith("{")) {
				try {
					this.lines.push(JSON.parse(line) as CliEvent);
				} catch {
					// Not an event line; ignore it.
				}
			}
			index = this.buffer.indexOf("\n");
		}
		this.wake();
	}

	private onExit(code: number | null, signal: NodeJS.Signals | null): void {
		if (this.exited) return;
		this.exited = true;
		this.exitCode = code;
		this.exitSignal = signal;
		this.wake();
	}

	private wake(): void {
		const waiter = this.waiter;
		this.waiter = undefined;
		waiter?.();
	}

	/** The next event, or undefined once the process has exited and every event was read. */
	async next(): Promise<CliEvent | undefined> {
		while (this.lines.length === 0) {
			if (this.exited || this.interrupted) return undefined;
			await new Promise<void>((resolve) => {
				this.waiter = resolve;
			});
		}
		return this.lines.shift();
	}

	/** Wake a pending `next()` so the caller can observe an abort or timeout. */
	interrupt(): void {
		this.interrupted = true;
		this.wake();
	}

	send(message: unknown): void {
		this.child.stdin?.write(`${JSON.stringify(message)}\n`);
	}

	/** Close stdin so the CLI exits by itself; stop it if it lingers. */
	finish(): void {
		try {
			this.child.stdin?.end();
		} catch {}
		if (this.exited) return;
		const timer = setTimeout(() => this.kill(), EXIT_GRACE_MS);
		timer.unref?.();
		this.child.once("exit", () => clearTimeout(timer));
	}

	/** Stop the process tree: SIGTERM to the group, SIGKILL after a grace period. */
	kill(): void {
		if (this.exited || this.child.pid === undefined) return;
		const pid = this.child.pid;
		if (process.platform === "win32") {
			try {
				node().childProcess.spawn("taskkill", ["/pid", String(pid), "/T", "/F"], {
					stdio: "ignore",
					windowsHide: true,
				});
			} catch {
				this.child.kill();
			}
			return;
		}
		const signalGroup = (sig: NodeJS.Signals) => {
			try {
				process.kill(-pid, sig);
			} catch {
				try {
					this.child.kill(sig);
				} catch {}
			}
		};
		signalGroup("SIGTERM");
		const timer = setTimeout(() => {
			if (!this.exited) signalGroup("SIGKILL");
		}, KILL_GRACE_MS);
		timer.unref?.();
		this.child.once("exit", () => clearTimeout(timer));
	}
}

// ---------------------------------------------------------------------------------------------------------
// Warm spares: idle processes started ahead of time with the arguments of a recent request

interface Spare {
	key: string;
	process: CliProcess;
	timer: ReturnType<typeof setTimeout>;
}

const spares: Spare[] = [];
/** Request shapes that completed once: their next request likely follows, so a spare starts with each one. */
const provenKeys = new Set<string>();
let exitHookInstalled = false;

function killAllSpares(): void {
	for (const spare of spares.splice(0)) {
		clearTimeout(spare.timer);
		spare.process.kill();
	}
}

function takeSpare(key: string): CliProcess | undefined {
	for (let index = 0; index < spares.length; index += 1) {
		const spare = spares[index];
		if (spare.key !== key) continue;
		spares.splice(index, 1);
		clearTimeout(spare.timer);
		if (spare.process.alive) return spare.process;
		spare.process.kill();
	}
	return undefined;
}

function startSpare(key: string, bin: string, args: string[], env: Env, cwd: string, config: RuntimeConfig): void {
	if (config.warmSpares <= 0) return;
	if (spares.some((spare) => spare.key === key && spare.process.alive)) return;
	while (spares.length >= config.warmSpares) {
		const oldest = spares.shift();
		if (oldest) {
			clearTimeout(oldest.timer);
			oldest.process.kill();
		}
	}
	if (!exitHookInstalled) {
		exitHookInstalled = true;
		process.once("exit", killAllSpares);
	}
	const spawned = new CliProcess(bin, args, env, cwd);
	// An idle spare must never keep the host alive.
	spawned.child.unref();
	(spawned.child.stdin as { unref?: () => void } | null)?.unref?.();
	(spawned.child.stdout as { unref?: () => void } | null)?.unref?.();
	(spawned.child.stderr as { unref?: () => void } | null)?.unref?.();
	const spare: Spare = {
		key,
		process: spawned,
		timer: setTimeout(() => {
			const index = spares.indexOf(spare);
			if (index >= 0) spares.splice(index, 1);
			spawned.kill();
		}, config.warmTtlMs),
	};
	spare.timer.unref?.();
	spares.push(spare);
}

function adopt(process_: CliProcess): void {
	process_.child.ref();
	for (const stream of [process_.child.stdin, process_.child.stdout, process_.child.stderr])
		(stream as { ref?: () => void } | null)?.ref?.();
}

// ---------------------------------------------------------------------------------------------------------
// Request construction

type CliContentBlock =
	| { type: "text"; text: string }
	| { type: "image"; source: { type: "base64"; media_type: string; data: string } };

/** What the provider sends: exposed to `onPayload`, which may change `max_tokens`, `json_schema` or `effort`. */
export interface ClaudeCodePayload {
	model: string;
	system: string;
	content: CliContentBlock[];
	max_tokens: number;
	json_schema?: Record<string, unknown>;
	effort?: string;
}

function pushText(blocks: CliContentBlock[], text: string): void {
	if (!text) return;
	const last = blocks.at(-1);
	if (last?.type === "text") last.text += text;
	else blocks.push({ type: "text", text });
}

function pushContent(
	blocks: CliContentBlock[],
	content: string | readonly (TextContent | ImageContent)[],
	images: boolean,
): void {
	if (typeof content === "string") {
		pushText(blocks, content);
		return;
	}
	for (const part of content) {
		if (part.type === "text") pushText(blocks, part.text);
		else if (images)
			blocks.push({ type: "image", source: { type: "base64", media_type: part.mimeType, data: part.data } });
		else pushText(blocks, "[image omitted: this model takes text only]");
	}
}

/**
 * The transcript as one user message. A single user turn is sent as-is; a longer conversation (earlier
 * assistant replies, tool calls and results) is rendered as a text transcript the model continues.
 */
export function renderClaudeCodePrompt(messages: readonly Message[], images: boolean): CliContentBlock[] {
	const turns = messages.filter((message) => message.role !== "system");
	const blocks: CliContentBlock[] = [];
	if (turns.length === 1 && turns[0].role === "user") {
		pushContent(blocks, turns[0].content, images);
		if (blocks.length === 0) pushText(blocks, "(empty message)");
		return blocks;
	}
	pushText(blocks, "<conversation>\n");
	for (const message of turns) {
		if (message.role === "user") {
			pushText(blocks, "<user>\n");
			pushContent(blocks, message.content, images);
			pushText(blocks, "\n</user>\n");
		} else if (message.role === "assistant") {
			const parts: string[] = [];
			for (const part of message.content) {
				if (part.type === "text") parts.push(part.text);
				else if (part.type === "toolCall")
					parts.push(`[tool call ${part.name} (id ${part.id}): ${JSON.stringify(part.arguments)}]`);
			}
			pushText(blocks, `<assistant>\n${parts.join("\n")}\n</assistant>\n`);
		} else if (message.role === "toolResult") {
			pushText(
				blocks,
				`<tool_result name="${message.toolName}" id="${message.toolCallId}"${message.isError ? ' error="true"' : ""}>\n`,
			);
			pushContent(blocks, message.content, images);
			pushText(blocks, "\n</tool_result>\n");
		}
	}
	pushText(
		blocks,
		"</conversation>\n\nYou are the assistant in the conversation above. Write the assistant's next reply to the latest user message; reply with that text only.",
	);
	return blocks;
}

/** Tools the transcript declares; any tool makes the request unservable. */
function declaredTools(context: TranscriptContext): string[] {
	return getCurrentTools(context.messages).map((tool) => tool.name);
}

function isObjectSchema(schema: unknown): schema is Record<string, unknown> {
	return (
		typeof schema === "object" &&
		schema !== null &&
		!Array.isArray(schema) &&
		(schema as Record<string, unknown>).type === "object"
	);
}

function jsonInstruction(schema: unknown): string {
	return `\n\nReply with only one JSON value (no prose, no code fences) that satisfies this JSON schema: ${JSON.stringify(schema)}`;
}

interface PlannedRequest {
	args: string[];
	env: Env;
	message: { type: "user"; message: { role: "user"; content: CliContentBlock[] } };
	structured: boolean;
}

function planRequest(
	model: Model<Api>,
	payload: ClaudeCodePayload,
	baseEnv: Env,
	defaultMaxTokens: number,
	capabilities: ClaudeCliCapabilities,
	useSchemaFlag: boolean,
): PlannedRequest {
	const content = payload.content.map((block) => ({ ...block })) as CliContentBlock[];
	let system = payload.system.trim() || NEUTRAL_SYSTEM_PROMPT;
	if (system.length > MAX_SYSTEM_PROMPT_ARG_CHARS) {
		content.unshift({ type: "text", text: `<instructions>\n${system}\n</instructions>\n\n` });
		system = NEUTRAL_SYSTEM_PROMPT;
	}
	const schema = payload.json_schema;
	const structured = schema !== undefined && useSchemaFlag && isObjectSchema(schema);
	if (structured && !capabilities.flags.has("--json-schema"))
		throw new Error(
			`claude-code provider: this request needs --json-schema, which the installed Claude Code CLI (${capabilities.version}) does not list in \`claude --help\`.`,
		);
	if (schema !== undefined && !structured) pushText(content, jsonInstruction(schema));
	if (payload.effort && !capabilities.flags.has("--effort"))
		throw new Error(
			`claude-code provider: a thinking level needs --effort, which the installed Claude Code CLI (${capabilities.version}) does not list in \`claude --help\`.`,
		);
	const args = [
		"--print",
		"--output-format=stream-json",
		"--input-format=stream-json",
		"--verbose",
		"--include-partial-messages",
		"--tools=",
		"--strict-mcp-config",
		'--mcp-config={"mcpServers":{}}',
		"--setting-sources=",
		"--permission-prompts=none",
		"--disable-slash-commands",
		"--no-session-persistence",
		`--model=${payload.model || model.id}`,
		`--system-prompt=${system}`,
		payload.effort ? `--effort=${payload.effort}` : '--settings={"alwaysThinkingEnabled":false}',
		...(structured ? [`--json-schema=${JSON.stringify(schema)}`] : []),
	];
	const env = { ...baseEnv };
	if (Number.isSafeInteger(payload.max_tokens) && payload.max_tokens > 0 && payload.max_tokens !== defaultMaxTokens)
		env.CLAUDE_CODE_MAX_OUTPUT_TOKENS = String(payload.max_tokens);
	return { args, env, message: { type: "user", message: { role: "user", content } }, structured };
}

// ---------------------------------------------------------------------------------------------------------
// Result mapping

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

function num(value: unknown): number | undefined {
	return typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : undefined;
}

function record(value: unknown): Record<string, unknown> | undefined {
	return typeof value === "object" && value !== null && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

/** Usage from the CLI's result event; cost is the CLI's reported `total_cost_usd`, or unknown (NaN), never 0. */
export function usageFromResult(result: CliEvent): Usage {
	const usage = record(result.usage) ?? {};
	const details = record(usage.output_tokens_details);
	const creation = record(usage.cache_creation);
	const out = emptyUsage();
	out.input = num(usage.input_tokens) ?? 0;
	out.output = num(usage.output_tokens) ?? 0;
	out.cacheRead = num(usage.cache_read_input_tokens) ?? 0;
	out.cacheWrite = num(usage.cache_creation_input_tokens) ?? 0;
	const longWrite = num(creation?.ephemeral_1h_input_tokens);
	if (longWrite !== undefined && longWrite > 0) out.cacheWrite1h = longWrite;
	const thinking = num(details?.thinking_tokens);
	if (thinking !== undefined) out.reasoning = thinking;
	out.totalTokens = out.input + out.output + out.cacheRead + out.cacheWrite;
	out.cost.total = num(result.total_cost_usd) ?? Number.NaN;
	return out;
}

interface RateLimitInfo {
	status?: string;
	rateLimitType?: string;
	resetsAt?: number;
}

function rateLimitInfo(event: CliEvent): RateLimitInfo | undefined {
	const info = record(event.rate_limit_info);
	if (!info) return undefined;
	return {
		...(typeof info.status === "string" ? { status: info.status } : {}),
		...(typeof info.rateLimitType === "string" ? { rateLimitType: info.rateLimitType } : {}),
		...(num(info.resetsAt) !== undefined ? { resetsAt: num(info.resetsAt) } : {}),
	};
}

function resetText(info: RateLimitInfo | undefined): string {
	if (!info?.resetsAt) return "";
	const millis = info.resetsAt < 1e12 ? info.resetsAt * 1000 : info.resetsAt;
	return `; resets at ${new Date(millis).toISOString()}`;
}

type Failure =
	| { kind: "length" }
	| { kind: "schema_rejected"; message: string }
	| { kind: "error"; message: string; usageLimit?: boolean };

/** Diagnostic type on a response that failed because the subscription's usage limit is exhausted. */
export const USAGE_LIMIT_DIAGNOSTIC = "provider_usage_limit";

/** Classify an `is_error` result into a provider error the harness and the frames can act on. */
export function classifyFailure(result: CliEvent, rateLimit: RateLimitInfo | undefined): Failure {
	const text = typeof result.result === "string" && result.result ? result.result : "unknown error";
	const status = num(result.api_error_status);
	if (/exceeded the \d+ output token maximum/i.test(text)) return { kind: "length" };
	if (status === 400 && /input_schema/i.test(text)) return { kind: "schema_rejected", message: text };
	if (rateLimit?.status === "rejected" || /usage limit|limit reached|out of (?:extra )?usage/i.test(text)) {
		const window = rateLimit?.rateLimitType ? ` (${rateLimit.rateLimitType} window${resetText(rateLimit)})` : "";
		return {
			kind: "error",
			message: `Claude Code usage limit reached${window}: ${text}. The subscription's capacity is exhausted for now; this is not retried.`,
			usageLimit: true,
		};
	}
	if (status === 429 || /rate.?limit/i.test(text))
		return { kind: "error", message: `Claude Code rate limited (429): ${text}` };
	if (status === 529 || /overloaded/i.test(text))
		return { kind: "error", message: `Claude Code overloaded (529): ${text}` };
	if (/not logged in|please run \/login|invalid api key|authentication/i.test(text))
		return {
			kind: "error",
			message: `Claude Code CLI authentication failed: ${text}. Run \`claude auth login\` and try again.`,
		};
	return { kind: "error", message: `Claude Code CLI error${status ? ` (${status})` : ""}: ${text}` };
}

function mapStopReason(value: unknown): Extract<StopReason, "stop" | "length"> {
	return value === "max_tokens" ? "length" : "stop";
}

function tail(text: string, limit: number): string {
	return text.length > limit ? text.slice(text.length - limit) : text;
}

function diagnostic(type: string, message: string, details?: Record<string, string>): AssistantMessageDiagnostic {
	return { type, timestamp: Date.now(), error: { message }, ...(details ? { details } : {}) };
}

// ---------------------------------------------------------------------------------------------------------
// One request

interface Attempt {
	failure?: Exclude<Failure, { kind: "length" }>;
}

class RequestRun {
	private readonly stream: AssistantMessageEventStream;
	private readonly output: AssistantMessage;
	private readonly blocks = new Map<number, number>();
	private readonly streamedMessages = new Set<string>();
	private rateLimit: RateLimitInfo | undefined;
	/** Set once a result event reported usage; until then a failed call's spend is unknown. */
	usageKnown = false;

	constructor(stream: AssistantMessageEventStream, output: AssistantMessage) {
		this.stream = stream;
		this.output = output;
	}

	private addText(text: string): void {
		const index = this.output.content.push({ type: "text", text: "" }) - 1;
		this.stream.push({ type: "text_start", contentIndex: index, partial: this.output });
		(this.output.content[index] as TextContent).text = text;
		this.stream.push({ type: "text_delta", contentIndex: index, delta: text, partial: this.output });
		this.stream.push({ type: "text_end", contentIndex: index, content: text, partial: this.output });
	}

	private onStreamEvent(event: Record<string, unknown>, structured: boolean): void {
		const type = event.type;
		if (type === "message_start") {
			const message = record(event.message);
			this.blocks.clear();
			if (typeof message?.id === "string") {
				this.streamedMessages.add(message.id);
				this.output.responseId = message.id;
			}
			if (typeof message?.model === "string" && message.model !== this.output.model)
				this.output.responseModel = message.model;
			return;
		}
		const cliIndex = typeof event.index === "number" ? event.index : -1;
		if (type === "content_block_start") {
			const block = record(event.content_block);
			if (block?.type === "text" && !structured) {
				const index = this.output.content.push({ type: "text", text: "" }) - 1;
				this.blocks.set(cliIndex, index);
				this.stream.push({ type: "text_start", contentIndex: index, partial: this.output });
			} else if (block?.type === "thinking") {
				const index = this.output.content.push({ type: "thinking", thinking: "" }) - 1;
				this.blocks.set(cliIndex, index);
				this.stream.push({ type: "thinking_start", contentIndex: index, partial: this.output });
			}
			return;
		}
		const index = this.blocks.get(cliIndex);
		if (index === undefined) return;
		const target = this.output.content[index];
		if (type === "content_block_delta") {
			const delta = record(event.delta);
			if (delta?.type === "text_delta" && typeof delta.text === "string" && target.type === "text") {
				target.text += delta.text;
				this.stream.push({ type: "text_delta", contentIndex: index, delta: delta.text, partial: this.output });
			} else if (
				delta?.type === "thinking_delta" &&
				typeof delta.thinking === "string" &&
				target.type === "thinking"
			) {
				target.thinking += delta.thinking;
				this.stream.push({
					type: "thinking_delta",
					contentIndex: index,
					delta: delta.thinking,
					partial: this.output,
				});
			}
		} else if (type === "content_block_stop") {
			this.blocks.delete(cliIndex);
			if (target.type === "text")
				this.stream.push({ type: "text_end", contentIndex: index, content: target.text, partial: this.output });
			else if (target.type === "thinking")
				this.stream.push({
					type: "thinking_end",
					contentIndex: index,
					content: (target as ThinkingContent).thinking,
					partial: this.output,
				});
		}
	}

	/** A complete assistant message the CLI sent without partial events: emit its text at once. */
	private onAssistant(event: CliEvent, structured: boolean): void {
		const message = record(event.message);
		if (!message || structured) return;
		if (typeof message.id === "string" && this.streamedMessages.has(message.id)) return;
		if (message.model === "<synthetic>") return;
		for (const block of Array.isArray(message.content) ? message.content : []) {
			const part = record(block);
			if (part?.type === "text" && typeof part.text === "string" && part.text) this.addText(part.text);
		}
	}

	/**
	 * Drive one CLI process through one prompt. Resolves with the terminal state; text and thinking stream into
	 * the shared output as they arrive.
	 */
	async drive(
		child: CliProcess,
		plan: PlannedRequest,
		signal: AbortSignal | undefined,
		deadline: number,
		timeoutMs: number,
		onWarning: (message: string) => void,
	): Promise<Attempt> {
		let aborted = false;
		let timedOut = false;
		const stop = () => {
			child.kill();
			child.interrupt();
		};
		const onAbort = () => {
			aborted = true;
			stop();
		};
		signal?.addEventListener("abort", onAbort, { once: true });
		const timer = setTimeout(
			() => {
				timedOut = true;
				stop();
			},
			Math.max(1, deadline - Date.now()),
		);
		try {
			if (signal?.aborted) onAbort();
			else child.send(plan.message);
			for (;;) {
				const event = aborted || timedOut ? undefined : await child.next();
				if (aborted) throw Object.assign(new Error("Request was aborted"), { aborted: true });
				if (timedOut) throw new Error(`Claude Code CLI request timed out after ${Math.round(timeoutMs / 1000)}s`);
				if (!event) {
					if (child.spawnError) throw new Error(`Claude Code CLI failed to start: ${child.spawnError.message}`);
					const unknown = /unknown option '([^']+)'/.exec(child.stderr)?.[1];
					if (unknown)
						throw new Error(
							`claude-code provider: the installed Claude Code CLI rejected ${unknown}. Update Claude Code (\`claude update\`).`,
						);
					throw new Error(
						`Claude Code CLI exited (${child.exitSignal ?? `code ${child.exitCode}`}) before a result${child.stderr.trim() ? `: ${tail(child.stderr.trim(), 1_000)}` : ""}`,
					);
				}
				if (event.type === "system" && event.subtype === "init") {
					const source = event.apiKeySource;
					if (typeof source === "string" && source !== "none")
						onWarning(
							`the Claude Code CLI is using an API key (apiKeySource: ${source}) instead of the subscription login; calls may be billed to that key`,
						);
				} else if (event.type === "stream_event") {
					const inner = record(event.event);
					if (inner) this.onStreamEvent(inner, plan.structured);
				} else if (event.type === "assistant") {
					this.onAssistant(event, plan.structured);
				} else if (event.type === "rate_limit_event") {
					this.rateLimit = rateLimitInfo(event) ?? this.rateLimit;
					if (this.rateLimit?.status === "allowed_warning")
						onWarning(
							`Claude Code subscription usage is near its ${this.rateLimit.rateLimitType ?? ""} limit${resetText(this.rateLimit)}`,
						);
				} else if (event.type === "result") {
					return this.finish(event, plan);
				}
			}
		} finally {
			clearTimeout(timer);
			signal?.removeEventListener("abort", onAbort);
		}
	}

	private finish(result: CliEvent, plan: PlannedRequest): Attempt {
		this.output.usage = usageFromResult(result);
		this.usageKnown = true;
		if (result.is_error === true) {
			const failure = classifyFailure(result, this.rateLimit);
			if (failure.kind === "length") {
				this.output.stopReason = "length";
				this.output.rawStopReason = "max_output_tokens";
				return {};
			}
			return { failure };
		}
		if (plan.structured) {
			const value = result.structured_output;
			const text =
				value !== undefined && value !== null
					? JSON.stringify(value)
					: typeof result.result === "string"
						? result.result
						: "";
			this.addText(text);
		} else if (!this.output.content.some((part) => part.type === "text") && typeof result.result === "string") {
			if (result.result) this.addText(result.result);
		}
		this.output.stopReason = mapStopReason(result.stop_reason);
		if (typeof result.stop_reason === "string") this.output.rawStopReason = result.stop_reason;
		this.output.endTurn = result.stop_reason === "end_turn";
		return {};
	}
}

// ---------------------------------------------------------------------------------------------------------
// Stream entry points

function effortFor(model: Model<Api>, options: SimpleStreamOptions | undefined): string | undefined {
	const level = options?.reasoning;
	if (!level || !model.reasoning) return undefined;
	const mapped = model.thinkingLevelMap?.[level];
	return mapped === null ? undefined : (mapped ?? level);
}

/** One request against the CLI, as a Pi assistant-message stream. */
export function streamSimple(
	model: Model<Api>,
	context: TranscriptContext,
	options?: SimpleStreamOptions,
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
	const warnings: string[] = [];
	const warn = (message: string) => {
		if (!warnings.includes(message)) warnings.push(message);
	};
	let run: RequestRun | undefined;
	let spawned = false;
	const fail = (error: unknown, aborted: boolean) => {
		output.stopReason = aborted ? "aborted" : "error";
		output.errorMessage = error instanceof Error ? error.message : String(error);
		const diagnostics = warnings.map((message) => diagnostic("claude_code_warning", message));
		if ((error as { usageLimit?: boolean })?.usageLimit === true)
			diagnostics.push(diagnostic(USAGE_LIMIT_DIAGNOSTIC, output.errorMessage));
		if (diagnostics.length > 0) output.diagnostics = diagnostics;
		// Nothing was spent before a process got the prompt; after that, a call without a result event has
		// unknown usage, which is reported as an unknown cost rather than zero.
		if (spawned && !run?.usageKnown) output.usage.cost.total = Number.NaN;
		stream.push({ type: "error", reason: output.stopReason, error: output });
		stream.end(output);
	};

	(async () => {
		const signal = options?.signal;
		try {
			signal?.throwIfAborted();
			const tools = declaredTools(context);
			if (tools.length > 0)
				throw new Error(
					`${model.provider}/${model.id} ${TOOLS_UNSUPPORTED} (this lane declares tools: ${tools.slice(0, 8).join(", ")}${tools.length > 8 ? ", ..." : ""}).`,
				);
			const env = childEnv(options);
			const config = runtimeConfig(env, options);
			const bin = resolveClaudeCli(env);
			const cwd = emptyWorkDir();
			const capabilities = await ensureClaudeCli(bin, env, cwd);
			for (const message of capabilities.warnings) warn(message);
			signal?.throwIfAborted();

			const defaultMaxTokens = model.maxTokens;
			let payload: ClaudeCodePayload = {
				model: model.id,
				system: getCurrentSystemPrompt(context.messages),
				content: renderClaudeCodePrompt(context.messages, model.input.includes("image")),
				max_tokens: Math.min(options?.maxTokens ?? defaultMaxTokens, defaultMaxTokens),
				...(record((options as Record<string, unknown> | undefined)?.jsonSchema)
					? { json_schema: record((options as Record<string, unknown>).jsonSchema) }
					: {}),
				...(effortFor(model, options) ? { effort: effortFor(model, options) } : {}),
			};
			const replaced = await options?.onPayload?.(payload, model);
			if (replaced !== undefined) payload = replaced as ClaudeCodePayload;

			const release = await limiter.acquire(config.concurrency, signal);
			try {
				// The time limit starts once the call may run, not while it waits for a free slot.
				const deadline = Date.now() + config.timeoutMs;
				stream.push({ type: "start", partial: output });
				run = new RequestRun(stream, output);
				let useSchemaFlag = true;
				for (;;) {
					const plan = planRequest(model, payload, env, defaultMaxTokens, capabilities, useSchemaFlag);
					const key = JSON.stringify([bin, cwd, plan.args, plan.env]);
					const warm = takeSpare(key);
					const child = warm ?? new CliProcess(bin, plan.args, plan.env, cwd);
					if (warm) adopt(warm);
					spawned = true;
					// While this request runs, the next one of the same shape gets a process that is already up.
					if (provenKeys.has(key)) startSpare(key, bin, plan.args, plan.env, cwd, config);
					let attempt: Attempt;
					try {
						attempt = await run.drive(child, plan, signal, deadline, config.timeoutMs, warn);
					} finally {
						child.finish();
					}
					if (attempt.failure?.kind === "schema_rejected" && useSchemaFlag) {
						// The API refused the schema as a structured-output tool; ask for JSON in the prompt instead.
						warn(`--json-schema was rejected (${attempt.failure.message}); asked for JSON in the prompt instead`);
						useSchemaFlag = false;
						continue;
					}
					if (attempt.failure)
						throw Object.assign(new Error(attempt.failure.message), {
							usageLimit: attempt.failure.kind === "error" && attempt.failure.usageLimit === true,
						});
					if (!signal?.aborted) {
						provenKeys.add(key);
						startSpare(key, bin, plan.args, plan.env, cwd, config);
					}
					break;
				}
			} finally {
				release();
			}
			if (warnings.length > 0)
				output.diagnostics = warnings.map((message) => diagnostic("claude_code_warning", message));
			stream.push({
				type: "done",
				reason: output.stopReason === "length" ? "length" : "stop",
				message: output,
			});
			stream.end(output);
		} catch (error) {
			const aborted = signal?.aborted === true || (error as { aborted?: boolean })?.aborted === true;
			fail(error, aborted);
		}
	})();

	return stream;
}

export function stream(
	model: Model<Api>,
	context: TranscriptContext,
	options?: StreamOptions,
): AssistantMessageEventStream {
	return streamSimple(model, context, options);
}

/** Test hook: forget cached CLI paths, probes and warm spares. */
export function resetClaudeCodeCliState(): void {
	resolvedCli.clear();
	probes.clear();
	provenKeys.clear();
	killAllSpares();
	workDir = undefined;
}

/** Diagnostics for tests and status output. */
export function claudeCodeCliStats(): { inFlight: number; spares: number } {
	return { inFlight: limiter.inFlight, spares: spares.filter((spare) => spare.process.alive).length };
}
