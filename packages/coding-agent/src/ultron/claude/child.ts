/**
 * Claude Code as an Ultron subagent (the recursive design): an `rlm.spawn` child of an `ultron mcp` root runs as
 * its own `claude -p` process whose only tool is the `rlm` tool of its own `ultron mcp --child` server, so it has its
 * own kernel (and may delegate further when its parent passed `depth=`). The child's `rlm.finish` verdict is relayed
 * to the parent server over the parent's control socket and recorded on the parent's task exactly as for a native
 * subagent; its final reply is the task's result, its reported usage is charged to the task.
 *
 *   parent `ultron mcp` ──spawns──> claude -p --strict-mcp-config --mcp-config {ultron mcp --child ...}
 *        ▲                                  │ tools: mcp__ultron__rlm only
 *        └──── control socket: child.finish ┴── ultron mcp --child (its own kernel, frames, children)
 */
import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { NativeExternalChildResult, NativeExternalChildRun } from "../rlm/native-host.ts";
import { CLAUDE_RLM_TOOL, claudeSystemPrompt } from "./guide.ts";
import type { SelfCommand } from "./self.ts";

export interface ClaudeChildOptions {
	readonly claude: string;
	readonly self: SelfCommand;
	readonly cwd: string;
	/** The parent server's control socket and registry name. */
	readonly parentSocket: string;
	readonly parentName: string;
	/** Claude model alias for children (`claude-opus-5-5` by default); `rlm.spawn(model="claude-code/opus")` overrides it. */
	readonly model: string;
	/** Frame model for the child's own `rlm.map`/`rlm.infer`. */
	readonly frameModel?: string;
	/** Registers a child's one-time token, so only that child can give its task's verdict. */
	readonly registerChild: (token: string, laneName: string) => () => void;
	readonly env?: NodeJS.ProcessEnv;
}

/** Environment a child Claude Code must not inherit from the session that started its parent server. */
const PARENT_ONLY_ENV = [
	"CLAUDE_CODE_SESSION_ID",
	"CLAUDE_PID",
	"CLAUDE_CODE_MESSAGING_SOCKET",
	"CLAUDE_CODE_MESSAGING_TOKEN",
	"CLAUDE_CODE_SSE_PORT",
	"CLAUDE_PROJECT_DIR",
	"CLAUDE_ENV_FILE",
];

/** `claude-code/opus` -> `opus`; another provider's model cannot drive a Claude Code child. */
export function childClaudeModel(requested: string | undefined, fallback: string): string {
	if (requested === undefined || requested.trim() === "") return fallback;
	const trimmed = requested.trim();
	if (trimmed.startsWith("claude-code/")) return trimmed.slice("claude-code/".length);
	if (!trimmed.includes("/")) return trimmed;
	throw new Error(
		`rlm.spawn model=${trimmed}: subagents here are Claude Code processes; pass claude-code/<claude-opus-5-5|opus|sonnet|haiku> or leave model unset`,
	);
}

/** The `claude` arguments of a child (the brief goes in on stdin). */
export function childClaudeArgs(options: { model: string; mcpConfig: string; systemPromptFile: string }): string[] {
	return [
		"-p",
		"--output-format",
		"stream-json",
		"--verbose",
		"--no-session-persistence",
		"--model",
		options.model,
		"--strict-mcp-config",
		"--mcp-config",
		options.mcpConfig,
		"--tools",
		"",
		"--allowedTools",
		CLAUDE_RLM_TOOL,
		"--setting-sources",
		"",
		"--disable-slash-commands",
		"--system-prompt-file",
		options.systemPromptFile,
	];
}

type StreamStats = { turns: number; toolCalls: number; text: string; model?: string; messages?: Set<string> };

/** Fold one stream-json line of `claude -p` into the running stats; returns the final result message if it is one. */
export function foldStreamLine(line: string, stats: StreamStats): Record<string, unknown> | undefined {
	let message: Record<string, unknown>;
	try {
		message = JSON.parse(line) as Record<string, unknown>;
	} catch {
		return undefined;
	}
	if (message.type === "system" && message.subtype === "init" && typeof message.model === "string")
		stats.model = `claude-code/${message.model}`;
	if (message.type === "assistant") {
		const body = message.message as { id?: unknown; content?: unknown } | undefined;
		const content = body?.content;
		if (Array.isArray(content)) {
			// Claude Code streams one message per content block; blocks of one response share its id.
			stats.messages ??= new Set();
			const id = typeof body?.id === "string" ? body.id : `line-${stats.messages.size}`;
			if (!stats.messages.has(id)) {
				stats.messages.add(id);
				stats.turns += 1;
			}
			for (const part of content as Array<Record<string, unknown>>) {
				if (part.type === "tool_use") stats.toolCalls += 1;
				if (part.type === "text" && typeof part.text === "string" && part.text.trim()) stats.text = part.text;
			}
		}
	}
	return message.type === "result" ? message : undefined;
}

/** Run one `rlm.spawn` subagent as `claude -p` attached to its own `ultron mcp --child` server. */
export function runClaudeChild(
	options: ClaudeChildOptions,
	run: NativeExternalChildRun,
): Promise<NativeExternalChildResult> {
	const model = childClaudeModel(run.model, options.model);
	const dir = mkdtempSync(join(tmpdir(), "ultron-child-"));
	const token = randomBytes(24).toString("base64url");
	const unregister = options.registerChild(token, run.laneName);
	const systemPromptFile = join(dir, "system-prompt.md");
	writeFileSync(
		systemPromptFile,
		claudeSystemPrompt("claude-child", {
			cwd: options.cwd,
			platform: process.platform,
			date: new Date().toISOString().slice(0, 10),
			allowance: run.allowance,
		}),
		{ mode: 0o600 },
	);
	const serverArgs = [
		...options.self.args,
		"mcp",
		"--child",
		// The system prompt already carries the guide.
		"--no-instructions",
		"--parent-socket",
		options.parentSocket,
		"--parent-name",
		options.parentName,
		"--parent-lane",
		run.laneName,
		"--level",
		String(run.level),
		"--allowance",
		String(run.allowance),
		"--child-model",
		model,
		...(options.frameModel === undefined ? [] : ["--frame-model", options.frameModel]),
	];
	const mcpConfig = JSON.stringify({
		mcpServers: {
			ultron: {
				type: "stdio",
				command: options.self.command,
				args: serverArgs,
				// The token stays out of process listings.
				env: { ULTRON_PARENT_TOKEN: token },
			},
		},
	});
	const env: NodeJS.ProcessEnv = { ...(options.env ?? process.env) };
	for (const name of PARENT_ONLY_ENV) delete env[name];
	const child = spawn(options.claude, childClaudeArgs({ model, mcpConfig, systemPromptFile }), {
		cwd: options.cwd,
		env,
		stdio: ["pipe", "pipe", "pipe"],
		detached: process.platform !== "win32",
	});
	const stats: StreamStats = { turns: 0, toolCalls: 0, text: "" };
	let result: Record<string, unknown> | undefined;
	let stderr = "";
	let buffered = "";
	const kill = (): void => {
		if (child.exitCode !== null || child.pid === undefined) return;
		try {
			if (process.platform === "win32") child.kill("SIGTERM");
			else process.kill(-child.pid, "SIGTERM");
		} catch {
			// Already gone.
		}
		setTimeout(() => {
			if (child.exitCode !== null || child.pid === undefined) return;
			try {
				if (process.platform === "win32") child.kill("SIGKILL");
				else process.kill(-child.pid, "SIGKILL");
			} catch {
				// Already gone.
			}
		}, 5_000).unref();
	};
	const onAbort = (): void => kill();
	run.signal.addEventListener("abort", onAbort, { once: true });
	child.stdout.setEncoding("utf8");
	child.stdout.on("data", (chunk: string) => {
		buffered += chunk;
		let newline = buffered.indexOf("\n");
		while (newline !== -1) {
			const line = buffered.slice(0, newline);
			buffered = buffered.slice(newline + 1);
			newline = buffered.indexOf("\n");
			if (!line.trim()) continue;
			const final = foldStreamLine(line, stats);
			if (final) result = final;
			run.progress({
				turns: stats.turns,
				toolCalls: stats.toolCalls,
				...(stats.text ? { text: stats.text } : {}),
				...(stats.model === undefined ? {} : { model: stats.model }),
			});
		}
	});
	child.stderr.setEncoding("utf8");
	child.stderr.on("data", (chunk: string) => {
		stderr = `${stderr}${chunk}`.slice(-4000);
	});
	child.stdin.end(run.prompt);
	return new Promise<NativeExternalChildResult>((resolve, reject) => {
		const done = (): void => {
			run.signal.removeEventListener("abort", onAbort);
			unregister();
			rmSync(dir, { recursive: true, force: true });
		};
		child.once("error", (error) => {
			done();
			reject(error);
		});
		child.once("close", (code) => {
			if (buffered.trim()) {
				const final = foldStreamLine(buffered, stats);
				if (final) result = final;
			}
			done();
			if (run.signal.aborted) {
				reject(run.signal.reason instanceof Error ? run.signal.reason : new Error("subagent cancelled"));
				return;
			}
			if (result === undefined) {
				reject(
					new Error(
						`claude exited with code ${code ?? "?"} without a result${stderr ? `: ${stderr.trim()}` : ""}`,
					),
				);
				return;
			}
			const text = typeof result.result === "string" ? result.result : stats.text;
			if (result.is_error === true && !text.trim()) {
				reject(new Error(`claude child failed: ${String(result.subtype ?? "error")}`));
				return;
			}
			const usage = (result.usage ?? {}) as Record<string, unknown>;
			const number = (value: unknown) => (typeof value === "number" && Number.isFinite(value) ? value : 0);
			const input =
				number(usage.input_tokens) +
				number(usage.cache_creation_input_tokens) +
				number(usage.cache_read_input_tokens);
			const output = number(usage.output_tokens);
			resolve({
				text,
				turns: typeof result.num_turns === "number" ? result.num_turns : stats.turns,
				toolCalls: stats.toolCalls,
				...(stats.model === undefined ? {} : { model: stats.model }),
				usage: {
					inputTokens: input,
					outputTokens: output,
					totalTokens: input + output,
					cost: typeof result.total_cost_usd === "number" ? result.total_cost_usd : null,
					...(typeof result.duration_ms === "number" ? { wallMs: result.duration_ms } : {}),
				},
			});
		});
	});
}
