/**
 * Claude Code lanes in the session worker (`ultron --claude`, or any lane whose model is `claude-code/...` and has
 * tools): registers the claude-code provider's tool runner for this process, ends a lane's `claude -p` process with
 * its run, keeps each lane's Claude Code session id in the Ultron session (so `ultron --claude -c` resumes it), and
 * leaves context management to Claude Code (Ultron's compaction is declined on those lanes).
 *
 * The runner and its bridge server load on the first request that needs them, so a worker that never runs a
 * Claude Code lane pays nothing.
 */
import { existsSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import type { AgentHarness, JsonlSessionMetadata, Session } from "@ultron/agent-core";
import { TODO_CONTEXT, value } from "@ultron/agent-core";
import type { Api, Model } from "@ultron/ai";
import { CLAUDE_CODE_PROVIDER_ID, setClaudeCodeToolRunner } from "@ultron/ai/providers/claude-code";
import { AssistantMessageEventStream } from "@ultron/ai/utils/event-stream";
import { getPackageDir, isBunBinary, isBundledNode } from "../../config.ts";
import { DEFAULT_CLAUDE_MODEL } from "./claude-cli.ts";
import type { ClaudeLaneRecord, ClaudeRootRunner } from "./root-runner.ts";
import { type SelfCommand, selfCommand } from "./self.ts";
import type { BridgeServer } from "./tool-bridge.ts";

/** `ULTRON_ROOT=claude` (what `ultron --claude` sets): the root lane runs on Claude Code. */
export const ULTRON_ROOT_ENV = "ULTRON_ROOT";

export function claudeRootRequested(env: NodeJS.ProcessEnv = process.env): boolean {
	return env[ULTRON_ROOT_ENV]?.trim().toLowerCase() === "claude";
}

/** What the lanes are told about their tools under Claude Code (appended to the lane's own system prompt). */
export const CLAUDE_CODE_LANE_NOTE = `# Running on Claude Code
You run inside Claude Code (headless), driven by Ultron. Your tools are Ultron's, served over MCP: \`rlm\` is listed as \`mcp__ultron__rlm\` (any other tool likewise as \`mcp__ultron__<name>\`). Claude Code's own built-in tools are disabled; do everything through these. When background work you started finishes while you are idle, Ultron starts a new turn with a \`<runtime_event>\` message, as the runtime guide describes. Claude Code manages your context window.`;

const LANE_SESSIONS = "ultron.claude-code.lanes";

/**
 * `ultron --claude` (or ULTRON_ROOT=claude, or the `claudeCode.root` setting): the model the root starts on, as
 * `claude-code/<id>`, from `--model` (`claude-code/<id>`, or a bare Claude model or alias), ULTRON_CLAUDE_MODEL, or
 * claude-opus-5-5. Sets ULTRON_ROOT=claude for the session worker, and ULTRON_SELF_COMMAND (how the worker starts
 * `ultron mcp --bridge`) when unset. Throws for a model Claude Code does not serve.
 */
export function prepareClaudeRoot(
	selection: { provider?: string; model?: string },
	env: NodeJS.ProcessEnv = process.env,
): { provider?: string; model: string } {
	env[ULTRON_ROOT_ENV] = "claude";
	if (!env.ULTRON_SELF_COMMAND?.trim()) {
		try {
			const self = selfCommand(env);
			env.ULTRON_SELF_COMMAND = JSON.stringify([self.command, ...self.args]);
		} catch {
			// The worker works out its own command.
		}
	}
	const provider = selection.provider?.trim();
	const model = selection.model?.trim();
	if (!model) {
		if (provider && provider !== CLAUDE_CODE_PROVIDER_ID) throw new Error(claudeModelError(provider));
		return { model: `${CLAUDE_CODE_PROVIDER_ID}/${env.ULTRON_CLAUDE_MODEL?.trim() || DEFAULT_CLAUDE_MODEL}` };
	}
	if (provider) {
		if (provider !== CLAUDE_CODE_PROVIDER_ID) throw new Error(claudeModelError(`${provider}/${model}`));
		return { provider, model };
	}
	const slash = model.indexOf("/");
	if (slash > 0) {
		if (model.slice(0, slash) !== CLAUDE_CODE_PROVIDER_ID) throw new Error(claudeModelError(model));
		return { model };
	}
	if (/^(claude-|opus|sonnet|haiku|fable)/i.test(model)) return { model: `${CLAUDE_CODE_PROVIDER_ID}/${model}` };
	throw new Error(claudeModelError(model));
}

function claudeModelError(model: string): string {
	return `--claude runs the root agent on Claude Code, which does not serve ${model}. Use --model claude-code/<model> (for example claude-code/sonnet), or leave out --claude.`;
}

/** How this worker starts the Ultron CLI again (for `ultron mcp --bridge`). */
export function workerSelfCommand(env: NodeJS.ProcessEnv = process.env): SelfCommand {
	if (env.ULTRON_SELF_COMMAND?.trim()) return selfCommand(env);
	if (isBunBinary) return { command: process.execPath, args: [] };
	const packageDir = getPackageDir();
	if (isBundledNode) return { command: process.execPath, args: [join(packageDir, "dist", "bundle", "cli.js")] };
	if (import.meta.url.endsWith(".ts")) {
		const resolver = fileURLToPath(new URL("../../experimental/source-resolver.ts", import.meta.url));
		return {
			command: process.execPath,
			args: ["--import", resolver, fileURLToPath(new URL("../../cli.ts", import.meta.url))],
		};
	}
	const dist = join(packageDir, "dist", "cli.js");
	return {
		command: process.execPath,
		args: [existsSync(dist) ? dist : fileURLToPath(new URL("../../cli.js", import.meta.url))],
	};
}

export interface ClaudeCodeLanes {
	close(): Promise<void>;
}

/** Install Claude Code lanes for this worker's harness. */
export function installClaudeCodeLanes(input: {
	session: Session<JsonlSessionMetadata>;
	harness: AgentHarness<never> | AgentHarness<{ env: unknown }> | AgentHarness;
	cwd: string;
	env?: NodeJS.ProcessEnv;
}): ClaudeCodeLanes {
	const { session, cwd } = input;
	const harness = input.harness as AgentHarness;
	const env = input.env ?? process.env;
	const sessionId = session.metadata.id;
	let runner: ClaudeRootRunner | undefined;
	let bridge: BridgeServer | undefined;
	let ready: Promise<ClaudeRootRunner> | undefined;
	let closed = false;
	const laneOf = (key: string): string => (key.startsWith(`${sessionId}:`) ? key.slice(sessionId.length + 1) : key);
	const load = (): Promise<ClaudeRootRunner> => {
		ready ??= (async () => {
			const [{ ClaudeRootRunner }, { startBridgeServer }] = await Promise.all([
				import("./root-runner.ts"),
				import("./tool-bridge.ts"),
			]);
			if (closed) throw new Error("the session worker is closing");
			const server = await startBridgeServer();
			bridge = server;
			const self = workerSelfCommand(env);
			runner = new ClaudeRootRunner({
				cwd,
				env,
				systemPromptNote: CLAUDE_CODE_LANE_NOTE,
				bridgeCommand: (token) => ({
					command: self.command,
					args: [...self.args, "mcp", "--bridge", server.socket, "--token", token],
				}),
				registerBridge: (token, lane) => server.register(token, lane),
				load: async (key) =>
					(await session.getValue(value<ClaudeLaneRecord>(LANE_SESSIONS, laneOf(key)), TODO_CONTEXT))?.value,
				save: (key, record) =>
					session.setValue(value<ClaudeLaneRecord>(LANE_SESSIONS, laneOf(key)), record, TODO_CONTEXT),
			});
			return runner;
		})();
		return ready;
	};
	setClaudeCodeToolRunner((model: Model<Api>, context, options) => {
		if (runner) return runner.stream(model, context, options);
		const out = new AssistantMessageEventStream();
		void load().then(
			async (loaded) => {
				const inner = loaded.stream(model, context, options);
				for await (const event of inner) out.push(event);
				out.end(await inner.result());
			},
			(error: unknown) => {
				const message = {
					role: "assistant" as const,
					content: [],
					api: model.api,
					provider: model.provider,
					model: model.id,
					usage: {
						input: 0,
						output: 0,
						cacheRead: 0,
						cacheWrite: 0,
						totalTokens: 0,
						cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
					},
					stopReason: "error" as const,
					errorMessage: error instanceof Error ? error.message : String(error),
					timestamp: Date.now(),
				};
				out.push({ type: "start", partial: message });
				out.push({ type: "error", reason: "error", error: message });
				out.end(message);
			},
		);
		return out;
	});
	const removers = [
		// A lane's process lives as long as its run.
		harness.events.on("run_end", (event) => runner?.endRun(`${sessionId}:${event.lane}`)),
		// Claude Code manages the context of its lanes; Ultron's compaction would summarize a transcript the model
		// no longer reads from.
		harness.hooks.on("before_compaction", async (event, context) => {
			const lane = await harness.lane(event.lane, context);
			const model = await lane.getModel(context).catch(() => undefined);
			return model?.provider === CLAUDE_CODE_PROVIDER_ID ? { decline: true } : undefined;
		}),
	];
	return {
		close: async () => {
			closed = true;
			for (const remove of removers) remove();
			setClaudeCodeToolRunner(undefined);
			runner?.close();
			await bridge?.close();
		},
	};
}
