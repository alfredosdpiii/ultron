/**
 * `ultron mcp`: Ultron's REPL as an MCP server for Claude Code. One server process is one Claude Code session (or
 * one Claude Code subagent, `--child`), and it runs Ultron's own runtime on an Ultron session of its own: the same
 * persistent kernel, hints, file hooks (Loki), secret masking, host (subagents, typed agents, workflows, jobs),
 * frames, usage ledger and snapshots as Ultron's root lane. Claude Code is the root agent; the server exposes:
 *
 * - tool `rlm`: run a Python cell in the persistent kernel (ExternalRootController.runCell);
 * - prompt `ultron-guide` and resource `ultron://guide`: the runtime guide (also the server instructions, unless
 *   `--no-instructions`, which `ultron claude` passes because the guide is its system prompt);
 * - a control socket for hooks (`ultron hook`), the viewer (`ultron watch`) and subagent servers.
 *
 * The Ultron session of a Claude Code session is remembered by Claude Code's session id, so `claude --resume`
 * reopens the same tasks, kernel snapshots and ledger.
 */
import { randomBytes } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { type JsonlSessionMetadata, JsonlSessionRepo, type Session, TODO_CONTEXT } from "@ultron/agent-core";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import lockfile from "proper-lockfile";
import { ENV_SESSION_DIR, getAgentDir, VERSION } from "../../config.ts";
import { SettingsManager } from "../../core/settings-manager.ts";
import {
	createUltronRuntime,
	type ExternalRootOptions,
	type UltronRuntime,
} from "../../experimental/session-worker.ts";
import { codeSkillsToolSection } from "../code-skills.ts";
import { RLM_TOOL_DESCRIPTION } from "../rlm/prompt.ts";
import { runClaudeChild } from "./child.ts";
import { resolveClaudeBinary } from "./claude-cli.ts";
import {
	type ControlClient,
	type ControlRequest,
	connectControl,
	type ServerRecord,
	socketPathFor,
	startControlServer,
	unregisterSync,
} from "./control-socket.ts";
import { claudeRuntimeGuide, claudeSystemPrompt } from "./guide.ts";
import { type McpContent, serveMcp } from "./mcp-protocol.ts";
import { selfCommand } from "./self.ts";

export const DEFAULT_FRAME_MODEL = "claude-code/sonnet";
export const DEFAULT_CHILD_CLAUDE_MODEL = "sonnet";

export interface McpServerArgs {
	child: boolean;
	parentSocket?: string;
	parentName?: string;
	parentLane?: string;
	level: number;
	allowance: number;
	frameModel?: string;
	children?: "claude" | "ultron";
	childModel?: string;
	ultronChildModel?: string;
	socket?: string;
	instructions: boolean;
}

/** Parse `ultron mcp` flags; unknown flags are an error. */
export function parseMcpArgs(args: readonly string[]): McpServerArgs {
	const parsed: McpServerArgs = { child: false, level: 0, allowance: Number.POSITIVE_INFINITY, instructions: true };
	const value = (index: number, flag: string): string => {
		const next = args[index + 1];
		if (next === undefined) throw new Error(`${flag} needs a value`);
		return next;
	};
	const count = (text: string, flag: string): number => {
		const number = Number(text);
		if (!Number.isSafeInteger(number) || number < 0) throw new Error(`${flag} must be a non-negative integer`);
		return number;
	};
	for (let index = 0; index < args.length; index += 1) {
		const arg = args[index]!;
		switch (arg) {
			case "--child":
				parsed.child = true;
				break;
			case "--no-instructions":
				parsed.instructions = false;
				break;
			case "--parent-socket":
				parsed.parentSocket = value(index++, arg);
				break;
			case "--parent-name":
				parsed.parentName = value(index++, arg);
				break;
			case "--parent-lane":
				parsed.parentLane = value(index++, arg);
				break;
			case "--level":
				parsed.level = count(value(index++, arg), arg);
				break;
			case "--allowance":
				parsed.allowance = count(value(index++, arg), arg);
				break;
			case "--frame-model":
				parsed.frameModel = value(index++, arg);
				break;
			case "--children": {
				const mode = value(index++, arg);
				if (mode !== "claude" && mode !== "ultron") throw new Error("--children must be claude or ultron");
				parsed.children = mode;
				break;
			}
			case "--child-model":
				parsed.childModel = value(index++, arg);
				break;
			case "--ultron-child-model":
				parsed.ultronChildModel = value(index++, arg);
				break;
			case "--socket":
				parsed.socket = value(index++, arg);
				break;
			default:
				throw new Error(`unknown option for ultron mcp: ${arg}`);
		}
	}
	if (parsed.child && (parsed.parentSocket === undefined || parsed.parentLane === undefined))
		throw new Error("ultron mcp --child needs --parent-socket and --parent-lane");
	return parsed;
}

/** `provider/model` split at the first slash. */
function splitModel(model: string): { provider: string; model: string } | undefined {
	const slash = model.indexOf("/");
	return slash <= 0 || slash === model.length - 1
		? undefined
		: { provider: model.slice(0, slash), model: model.slice(slash + 1) };
}

/** Where the Ultron session of each Claude Code session is remembered. */
function sessionMapDir(): string {
	const dir = join(getAgentDir(), "claude-code", "sessions");
	mkdirSync(dir, { recursive: true, mode: 0o700 });
	return dir;
}

function rememberedSession(claudeSessionId: string): JsonlSessionMetadata | undefined {
	try {
		const path = join(sessionMapDir(), `${claudeSessionId.replace(/[^A-Za-z0-9._-]/g, "_")}.json`);
		return JSON.parse(readFileSync(path, "utf8")) as JsonlSessionMetadata;
	} catch {
		return undefined;
	}
}

function rememberSession(claudeSessionId: string, metadata: JsonlSessionMetadata): void {
	const path = join(sessionMapDir(), `${claudeSessionId.replace(/[^A-Za-z0-9._-]/g, "_")}.json`);
	writeFileSync(path, `${JSON.stringify(metadata)}\n`, { mode: 0o600 });
}

/** A root cell as the viewer shows it. */
export interface RootCellRecord {
	n: number;
	startedAt: number;
	endedAt?: number;
	code: string;
	status: "running" | "ok" | "error";
	output?: string;
}

const READ_ONLY_INSPECTIONS = new Set([
	"agents.status",
	"instances.list",
	"rlm.pool",
	"jev.decisions",
	"ctx.state",
	"rlm.frames",
	"progress.assess",
	"async.pending",
]);

type OpenedRuntime = {
	runtime: UltronRuntime;
	session: Session<JsonlSessionMetadata>;
	close(): Promise<void>;
};

async function openRuntime(
	args: McpServerArgs,
	cwd: string,
	claudeSessionId: string | undefined,
	external: ExternalRootOptions,
): Promise<OpenedRuntime> {
	const sessionDir = process.env[ENV_SESSION_DIR] ?? join(getAgentDir(), "experimental", "sessions");
	const executionEnv = new NodeExecutionEnv({ cwd });
	const repo = new JsonlSessionRepo({ fileSystem: executionEnv, sessionsRoot: sessionDir });
	let session: Session<JsonlSessionMetadata> | undefined;
	const remembered = claudeSessionId === undefined || args.child ? undefined : rememberedSession(claudeSessionId);
	const lock = (path: string) =>
		lockfile.lock(path, {
			realpath: true,
			stale: 2_000,
			update: 1_000,
			retries: { retries: 40, factor: 1, minTimeout: 50, maxTimeout: 50 },
		});
	let release: (() => Promise<void>) | undefined;
	if (remembered !== undefined && remembered.cwd === cwd) {
		session = await repo.open(remembered, TODO_CONTEXT).catch(() => undefined);
		// Another server (a second client resuming the same Claude Code session) holds it: start a fresh one.
		release = session === undefined ? undefined : await lock(remembered.path).catch(() => undefined);
		if (session !== undefined && release === undefined) {
			await session.close(TODO_CONTEXT).catch(() => {});
			session = undefined;
		}
	}
	if (session === undefined) {
		session = await repo.create({ cwd }, TODO_CONTEXT);
		await session
			.setName(args.child ? `claude subagent (${args.parentLane})` : "claude code", TODO_CONTEXT)
			.catch(() => {});
		if (claudeSessionId !== undefined && !args.child) rememberSession(claudeSessionId, session.metadata);
		release = await lock(session.metadata.path);
	}
	const metadata = session.metadata;
	let runtime: UltronRuntime;
	try {
		runtime = await createUltronRuntime(
			session,
			{
				sessionDir,
				metadata: {
					id: metadata.id,
					createdAt: metadata.createdAt,
					storageVersion: metadata.storageVersion,
					cwd: metadata.cwd,
					path: metadata.path,
					modifiedAt: metadata.modifiedAt,
					...(metadata.parentSessionId === undefined ? {} : { parentSessionId: metadata.parentSessionId }),
				},
				extensionMode: "print",
				pluginManifestPaths: [],
				externalRoot: external,
			},
			executionEnv,
		);
	} catch (error) {
		await session.close(TODO_CONTEXT).catch(() => {});
		await repo.close(TODO_CONTEXT).catch(() => {});
		await release?.().catch(() => {});
		throw error;
	}
	let closing: Promise<void> | undefined;
	return {
		runtime,
		session,
		close: () => {
			closing ??= (async () => {
				await runtime.closeRlm?.().catch(() => {});
				await runtime.harness.close(TODO_CONTEXT).catch(() => {});
				await repo.close(TODO_CONTEXT).catch(() => {});
				await executionEnv.cleanup(TODO_CONTEXT).catch(() => {});
				await release?.().catch(() => {});
			})();
			return closing;
		},
	};
}

/** Run `ultron mcp` on stdio until Claude Code disconnects. */
export async function runMcpServer(argv: readonly string[]): Promise<void> {
	const args = parseMcpArgs(argv);
	// Stdout carries only JSON-RPC: anything else the runtime prints goes to stderr.
	const protocolOut = process.stdout;
	const toStderr = process.stderr.write.bind(process.stderr) as typeof process.stdout.write;
	const protocolWrite = protocolOut.write.bind(protocolOut);
	const mcpOut = { write: (text: string) => protocolWrite(text) };
	process.stdout.write = toStderr;
	console.log = console.error;
	console.info = console.error;

	const cwd = process.cwd();
	const settings = SettingsManager.create(cwd, getAgentDir(), { projectTrusted: false });
	const claudeSettings = settings.getClaudeCodeSettings();
	const env = process.env;
	const claudeSessionId = env.CLAUDE_CODE_SESSION_ID?.trim() || undefined;
	const claudePid = Number(env.CLAUDE_PID) || undefined;
	const frameModel =
		args.frameModel ?? env.ULTRON_CLAUDE_FRAME_MODEL?.trim() ?? claudeSettings.frameModel ?? DEFAULT_FRAME_MODEL;
	const claude = resolveClaudeBinary(env);
	const childrenMode =
		args.children ??
		(env.ULTRON_CLAUDE_CHILDREN === "ultron" || env.ULTRON_CLAUDE_CHILDREN === "claude"
			? env.ULTRON_CLAUDE_CHILDREN
			: undefined) ??
		claudeSettings.children ??
		(claude === undefined ? "ultron" : "claude");
	const childModel =
		args.childModel ??
		env.ULTRON_CLAUDE_CHILD_MODEL?.trim() ??
		claudeSettings.childModel ??
		DEFAULT_CHILD_CLAUDE_MODEL;
	const defaultProvider = settings.getDefaultProvider();
	const defaultModel = settings.getDefaultModel();
	const ultronChildModel =
		args.ultronChildModel ??
		claudeSettings.ultronChildModel ??
		(defaultProvider && defaultModel ? `${defaultProvider}/${defaultModel}` : undefined);

	// Socket paths are short (about 100 bytes), so a subagent server's name does not grow with its ancestry.
	const name = args.child
		? `child-${process.pid}-${randomBytes(3).toString("hex")}`
		: (claudeSessionId ?? `mcp-${process.pid}-${Date.now().toString(36)}`);
	const socket = args.socket ?? socketPathFor(name);
	const record: ServerRecord = {
		name,
		pid: process.pid,
		socket,
		cwd,
		startedAt: Date.now(),
		...(claudeSessionId === undefined ? {} : { claudeSessionId }),
		...(claudePid === undefined ? {} : { claudePid }),
		...(args.child && args.parentName !== undefined ? { parent: args.parentName } : {}),
	};

	// Tokens of running Claude Code subagents: a verdict is accepted only from the child that holds its token.
	const childTokens = new Map<string, string>();
	let parent: ControlClient | undefined;
	const parentToken = env.ULTRON_PARENT_TOKEN;
	delete env.ULTRON_PARENT_TOKEN;
	if (args.child) parent = await connectControl(args.parentSocket!, 10_000);

	const self = selfCommand(env);
	const external: ExternalRootOptions = {
		...(splitModel(frameModel) === undefined ? {} : { preferredModel: splitModel(frameModel)! }),
		...(childrenMode === "ultron" && ultronChildModel !== undefined ? { childModel: ultronChildModel } : {}),
		...(childrenMode === "claude" && claude !== undefined
			? {
					externalChild: (run) =>
						runClaudeChild(
							{
								claude,
								self,
								cwd,
								parentSocket: socket,
								parentName: name,
								model: childModel,
								frameModel,
								registerChild: (token, lane) => {
									childTokens.set(token, lane);
									return () => childTokens.delete(token);
								},
							},
							run,
						),
				}
			: {}),
		...(args.child
			? {
					rootSpawn: { level: args.level, allowance: args.allowance },
					rootFinish: async (payload) => {
						if (!parent) throw new Error("rlm.finish: the parent server is not reachable");
						return parent.request("child.finish", { token: parentToken, payload }, 30_000);
					},
				}
			: {}),
	};

	const cells: RootCellRecord[] = [];
	let cellCount = 0;
	let opened: OpenedRuntime | undefined;
	let startupError: Error | undefined;
	const ready = openRuntime(args, cwd, claudeSessionId, external).then(
		(value) => {
			opened = value;
			return value;
		},
		(error: unknown) => {
			startupError = error instanceof Error ? error : new Error(String(error));
			console.error(`ultron mcp: runtime failed to start: ${startupError.message}`);
			throw startupError;
		},
	);
	// Tool calls and hooks report the failure; nothing else waits on it.
	ready.catch(() => {});
	const runtime = async (): Promise<UltronRuntime> => (await ready).runtime;

	const handleControl = async (request: ControlRequest): Promise<unknown> => {
		switch (request.op) {
			case "status": {
				const value = opened;
				return {
					name,
					pid: process.pid,
					cwd,
					child: args.child,
					startedAt: record.startedAt,
					ready: value !== undefined,
					...(startupError === undefined ? {} : { error: startupError.message }),
					...(claudeSessionId === undefined ? {} : { claudeSessionId }),
					...(value === undefined
						? {}
						: {
								sessionId: value.session.metadata.id,
								frameModel: value.runtime.model,
								children: childrenMode,
								turn: value.runtime.externalRoot?.turn ?? null,
								cells: value.runtime.externalRoot?.cells ?? 0,
								pendingEvents: value.runtime.externalRoot?.pendingEvents ?? 0,
							}),
				};
			}
			case "cells":
				return { cells: cells.slice(-50) };
			case "inspect": {
				const inspect = String(request.request ?? "");
				if (!READ_ONLY_INSPECTIONS.has(inspect)) throw new Error(`not a read-only inspection: ${inspect}`);
				const payload =
					typeof request.payload === "object" && request.payload !== null
						? (request.payload as Record<string, unknown>)
						: {};
				const value = await runtime();
				if (!value.inspect) throw new Error("inspection is not available");
				return value.inspect(inspect, payload, BACKGROUND_CONTEXT);
			}
			case "hook":
				return handleHook(String(request.event ?? ""), request.input);
			case "child.finish": {
				const lane = typeof request.token === "string" ? childTokens.get(request.token) : undefined;
				if (lane === undefined) throw new Error("rlm.finish: unknown subagent");
				const payload =
					typeof request.payload === "object" && request.payload !== null
						? (request.payload as Record<string, unknown>)
						: {};
				return (await runtime()).hostRequest("rlm.finish", payload, BACKGROUND_CONTEXT, { lane });
			}
			default:
				throw new Error(`unknown control op: ${request.op}`);
		}
	};

	/** Claude Code hook events: the hook command forwards its input here and prints what this returns. */
	const handleHook = async (event: string, input: unknown): Promise<Record<string, unknown>> => {
		const data = typeof input === "object" && input !== null ? (input as Record<string, unknown>) : {};
		const root = (await runtime()).externalRoot;
		if (!root) return {};
		if (event === "session-start") {
			const context = await root.sessionContext();
			return context === undefined
				? {}
				: { hookSpecificOutput: { hookEventName: "SessionStart", additionalContext: context } };
		}
		if (event === "user-prompt") {
			const context = await root.beginTurn(typeof data.prompt === "string" ? data.prompt : undefined);
			return context === undefined
				? {}
				: { hookSpecificOutput: { hookEventName: "UserPromptSubmit", additionalContext: context } };
		}
		if (event === "stop") {
			await root.endTurn(typeof data.last_assistant_message === "string" ? data.last_assistant_message : undefined);
			return {};
		}
		throw new Error(`unknown hook event: ${event}`);
	};

	const control = await startControlServer(record, handleControl);
	const cleanupSync = (): void => unregisterSync(record);
	process.once("exit", cleanupSync);
	// A signal shuts down like a disconnect: kernels are snapshotted, running subagents and jobs stop.
	const onSignal = (): void => {
		process.stdin.destroy();
	};
	process.once("SIGTERM", onSignal);
	process.once("SIGINT", onSignal);
	process.once("SIGHUP", onSignal);

	const toolDescription = (): string => `${RLM_TOOL_DESCRIPTION}${codeSkillsToolSection()}`;
	const guideText = (): string =>
		args.child ? claudeSystemPrompt("claude-child", { allowance: args.allowance }) : claudeRuntimeGuide();

	await serveMcp(
		{
			name: "ultron",
			version: VERSION,
			instructions: () => (args.instructions ? claudeRuntimeGuide() : undefined),
			tools: () => [
				{
					name: "rlm",
					title: "Ultron REPL",
					description: toolDescription(),
					inputSchema: {
						type: "object",
						properties: {
							code: {
								type: "string",
								description: "Python code for persistent RLM computation, typed agents, and data processing",
							},
						},
						required: ["code"],
						additionalProperties: false,
					},
				},
			],
			callTool: async (_name, toolArgs, signal) => {
				const code = toolArgs.code;
				if (typeof code !== "string")
					return { content: [{ type: "text", text: "rlm needs `code` (a string)" }], isError: true };
				let value: UltronRuntime;
				try {
					value = await runtime();
				} catch (error) {
					return {
						content: [
							{
								type: "text",
								text: `Ultron's runtime failed to start: ${error instanceof Error ? error.message : String(error)}`,
							},
						],
						isError: true,
					};
				}
				const root = value.externalRoot!;
				cellCount += 1;
				const cell: RootCellRecord = {
					n: cellCount,
					startedAt: Date.now(),
					code: code.slice(0, 400),
					status: "running",
				};
				cells.push(cell);
				if (cells.length > 200) cells.splice(0, cells.length - 200);
				const result = await root.runCell(code, signal);
				cell.endedAt = Date.now();
				cell.status = result.isError ? "error" : "ok";
				const first = result.content.find((part) => part.type === "text");
				if (first?.type === "text") cell.output = first.text.slice(-400);
				return { content: result.content as McpContent[], ...(result.isError ? { isError: true } : {}) };
			},
			prompts: () => [
				{
					name: "ultron-guide",
					title: "Ultron runtime guide",
					description: "How to work in Ultron's REPL",
					text: guideText,
				},
			],
			resources: () => [
				{
					uri: "ultron://guide",
					name: "guide",
					title: "Ultron runtime guide",
					description: "How to work in Ultron's REPL (the rlm tool)",
					mimeType: "text/markdown",
					text: guideText,
				},
			],
		},
		process.stdin,
		mcpOut,
	);

	// Claude Code closed the connection: shut down (running subagents and jobs stop with the host).
	await control.close().catch(() => {});
	parent?.close();
	if (opened) await opened.close();
	else await ready.then((value) => value.close()).catch(() => {});
	process.off("exit", cleanupSync);
	process.exit(0);
}
