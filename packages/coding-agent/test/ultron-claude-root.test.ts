import { execFileSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
	type AgentMessage,
	BACKGROUND_CONTEXT,
	type Entry,
	type JsonlSessionMetadata,
	JsonlSessionRepo,
	type LaneSnapshot,
	type Session,
	TODO_CONTEXT,
} from "@ultron/agent-core";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import type { AssistantMessage, Message } from "@ultron/ai";
import { ProcessTerminal, TuiMainScreen } from "@ultron/tui";
import { afterAll, beforeAll, describe, expect, test, vi } from "vitest";
import { ENV_AGENT_DIR } from "../src/config.ts";
import { ExperimentalChatView } from "../src/experimental/client-tui-chat.ts";
import { claudeCodeUsageText, NativeFooter } from "../src/experimental/client-tui-footer.ts";
import { createUltronRuntime, type UltronRuntime } from "../src/experimental/session-worker.ts";
import { initTheme } from "../src/modes/interactive/theme/theme.ts";
import { rateLimitsFromEvent, renderInput, usageDiagnostic } from "../src/ultron/claude/root-runner.ts";
import { claudeRootRequested, prepareClaudeRoot } from "../src/ultron/claude/worker-root.ts";
import { readSessionLog } from "../src/ultron/session-log.ts";
import { buildSessionReport, type SessionReport } from "../src/ultron/session-report.ts";

/**
 * `ultron --claude`: Ultron's own runtime and UI with Claude Code (`claude -p`, stream-json, an MCP bridge back to
 * the worker) running the root lane. A fake `claude` (fixtures/fake-claude-agent.mjs) speaks the CLI's stream-json
 * and calls the bridge like Claude Code does, so these tests cover the arguments and isolation, the event mapping,
 * tool calls run by the harness on the lane's own kernel, session resume, steering, abort, usage limits and wake-ups
 * without the real CLI.
 */

const here = dirname(fileURLToPath(import.meta.url));
const fakeClaude = resolve(here, "fixtures/fake-claude-agent.mjs");
const cliPath = resolve(here, "../src/cli.ts");
const sourceResolverPath = resolve(here, "../src/experimental/source-resolver.ts");

function plain(lines: readonly string[]): string {
	return lines.join("\n").replace(/\u001b\[[0-9;]*m/g, "");
}

type Call = {
	phase: string;
	pid: number;
	argv?: string[];
	sessionId?: string;
	resumed?: boolean;
	prompt?: string;
	systemPrompt?: string | null;
	also?: string[];
	tools?: string[];
	env?: Record<string, string | null>;
	result?: unknown;
	cwd?: string;
};

describe("prepareClaudeRoot (--claude model selection)", () => {
	test("defaults to Opus 5.5 on Claude Code; claude-code/<id> and bare Claude names pass; others are refused", () => {
		const env: NodeJS.ProcessEnv = { ULTRON_SELF_COMMAND: '["node","cli.js"]' };
		expect(prepareClaudeRoot({}, env)).toEqual({ model: "claude-code/claude-opus-5-5" });
		expect(env.ULTRON_ROOT).toBe("claude");
		expect(claudeRootRequested(env)).toBe(true);
		expect(prepareClaudeRoot({ model: "claude-code/haiku" }, env)).toEqual({ model: "claude-code/haiku" });
		expect(prepareClaudeRoot({ model: "sonnet" }, env)).toEqual({ model: "claude-code/sonnet" });
		expect(prepareClaudeRoot({ provider: "claude-code", model: "opus" }, env)).toEqual({
			provider: "claude-code",
			model: "opus",
		});
		expect(prepareClaudeRoot({}, { ...env, ULTRON_CLAUDE_MODEL: "sonnet" })).toEqual({ model: "claude-code/sonnet" });
		expect(() => prepareClaudeRoot({ model: "openai/gpt-5" }, env)).toThrow(/does not serve openai\/gpt-5/);
		expect(() => prepareClaudeRoot({ model: "gpt-5" }, env)).toThrow(/--model claude-code\/<model>/);
		expect(() => prepareClaudeRoot({ provider: "openai" }, env)).toThrow(/does not serve openai/);
		// The worker needs to know how to start `ultron mcp --bridge`.
		const fresh: NodeJS.ProcessEnv = {};
		prepareClaudeRoot({}, fresh);
		expect(JSON.parse(String(fresh.ULTRON_SELF_COMMAND))).toEqual(expect.arrayContaining([process.execPath]));
	});
});

describe("input rendering and usage", () => {
	const user = (text: string): Message => ({ role: "user", content: text, timestamp: 0 });
	const assistant = (text: string, provider = "openai"): Message =>
		({
			role: "assistant",
			content: [
				{ type: "text", text },
				{ type: "toolCall", id: "c1", name: "rlm", arguments: { code: "print(1)" } },
			],
			provider,
			api: "x",
			model: "m",
			usage: {} as never,
			stopReason: "toolUse",
			timestamp: 0,
		}) as Message;
	test("a new Claude Code session of a conversation gets the history as a transcript, then the new message", () => {
		const blocks = renderInput(
			[
				user("first"),
				assistant("let me look"),
				{
					role: "toolResult",
					toolCallId: "c1",
					toolName: "rlm",
					content: [{ type: "text", text: "1" }],
					isError: false,
					timestamp: 0,
				},
				user("second"),
			],
			true,
		);
		const text = blocks.map((block) => (block.type === "text" ? block.text : "")).join("");
		expect(text).toContain("<conversation_so_far>");
		expect(text).toContain("<user>\nfirst\n</user>");
		expect(text).toContain("[rlm call c1]\n```python\nprint(1)\n```");
		expect(text).toContain("[rlm result c1]\n1");
		expect(text.endsWith("second")).toBe(true);
		// A continued session gets only what is new.
		expect(renderInput([user("again")], false)).toEqual([{ type: "text", text: "again" }]);
	});

	test("subscription windows from rate_limit_event, shown in the footer", () => {
		const limits = rateLimitsFromEvent({
			type: "rate_limit_event",
			rate_limit_info: {
				status: "allowed",
				rateLimitType: "five_hour",
				unifiedWindows: {
					five_hour: { utilization: 0.09, resetsAt: 1 },
					seven_day: { utilization: 0.62, resetsAt: 2 },
				},
			},
		});
		expect(limits).toEqual({
			status: "allowed",
			windows: { five_hour: { utilization: 0.09, resetsAt: 1 }, seven_day: { utilization: 0.62, resetsAt: 2 } },
		});
		const message = {
			role: "assistant",
			provider: "claude-code",
			content: [],
			diagnostics: [usageDiagnostic(limits!)],
		} as unknown as AgentMessage;
		expect(claudeCodeUsageText([{ type: "message", id: "e", message } as unknown as Entry])).toBe(
			"Claude Code 5h 9% · 7d 62%",
		);
		expect(claudeCodeUsageText([])).toBeUndefined();
	});
});

describe("ultron --claude: the root lane on Claude Code", () => {
	let work: string;
	let project: string;
	let sessionDir: string;
	let logDir: string;
	const opened: Array<{ runtime: UltronRuntime; repo: JsonlSessionRepo; env: NodeExecutionEnv }> = [];
	const saved = new Map<string, string | undefined>();

	beforeAll(() => {
		work = mkdtempSync(join(tmpdir(), "ultron-claude-root-"));
		project = join(work, "project");
		sessionDir = join(work, "sessions");
		logDir = join(work, "fake");
		const agentDir = join(work, "agent");
		const runtimeDir = join(work, "run");
		for (const dir of [project, sessionDir, agentDir, runtimeDir, logDir])
			mkdirSync(dir, { recursive: true, mode: 0o700 });
		execFileSync("git", ["init", "-q"], { cwd: project });
		writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ hindsightUrl: "off" }));
		chmodSync(fakeClaude, 0o755);
		// Set for the whole describe block (the config's unstubEnvs resets vi.stubEnv after each test).
		const set = (name: string, value: string) => {
			saved.set(name, process.env[name]);
			process.env[name] = value;
		};
		set(ENV_AGENT_DIR, agentDir);
		set("XDG_RUNTIME_DIR", runtimeDir);
		set("ULTRON_CLAUDE_CODE_BIN", fakeClaude);
		set("FAKE_CLAUDE_LOG", logDir);
		set("ULTRON_ROOT", "claude");
		set("ULTRON_SELF_COMMAND", JSON.stringify([process.execPath, "--import", sourceResolverPath, cliPath]));
		set("ULTRON_AUTO_MEMORY", "off");
		// A parent Claude Code's session variables must not reach the lane processes.
		set("CLAUDECODE", "1");
		set("CLAUDE_CODE_SESSION_ID", "parent-session");
	});

	afterAll(async () => {
		for (const { runtime, repo, env } of opened.splice(0)) {
			await runtime.closeRlm?.().catch(() => {});
			await runtime.harness.close(TODO_CONTEXT).catch(() => {});
			await repo.close(TODO_CONTEXT).catch(() => {});
			await env.cleanup(TODO_CONTEXT).catch(() => {});
		}
		for (const [name, value] of saved) {
			if (value === undefined) delete process.env[name];
			else process.env[name] = value;
		}
		// A Claude Code child or kernel may still be writing into it for a moment after the last test under load.
		rmSync(work, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
	});

	const calls = (): Call[] => {
		try {
			return readFileSync(join(logDir, "calls.jsonl"), "utf8")
				.split("\n")
				.filter(Boolean)
				.map((line) => JSON.parse(line) as Call);
		} catch {
			return [];
		}
	};

	async function open(
		metadata?: JsonlSessionMetadata,
	): Promise<{ runtime: UltronRuntime; session: Session<JsonlSessionMetadata> }> {
		const env = new NodeExecutionEnv({ cwd: project });
		const repo = new JsonlSessionRepo({ fileSystem: env, sessionsRoot: sessionDir });
		const session =
			metadata === undefined
				? await repo.create({ cwd: project }, TODO_CONTEXT)
				: await repo.open(metadata, TODO_CONTEXT);
		const runtime = await createUltronRuntime(
			session,
			{
				sessionDir,
				metadata: {
					id: session.metadata.id,
					createdAt: session.metadata.createdAt,
					storageVersion: session.metadata.storageVersion,
					cwd: session.metadata.cwd,
					path: session.metadata.path,
					modifiedAt: session.metadata.modifiedAt,
				},
				provider: "claude-code",
				model: "claude-opus-5-5",
				extensionMode: "print",
				pluginManifestPaths: [],
			},
			env,
		);
		opened.push({ runtime, repo, env });
		return { runtime, session };
	}

	async function close(runtime: UltronRuntime): Promise<void> {
		const index = opened.findIndex((entry) => entry.runtime === runtime);
		const [entry] = opened.splice(index, 1);
		await entry!.runtime.closeRlm?.();
		await entry!.runtime.harness.close(TODO_CONTEXT);
		await entry!.repo.close(TODO_CONTEXT);
		await entry!.env.cleanup(TODO_CONTEXT);
	}

	const messagesOf = async (runtime: UltronRuntime): Promise<AgentMessage[]> =>
		(await runtime.lane!.findEntries(undefined, BACKGROUND_CONTEXT))
			.flatMap((entry) => (entry.type === "message" ? [entry.message] : []))
			.reverse();

	let first: { runtime: UltronRuntime; session: Session<JsonlSessionMetadata> };

	test("a turn with an rlm cell: the harness runs the cell on the root kernel; text, tool call and usage map", async () => {
		first = await open();
		const lane = first.runtime.lane!;
		expect((await lane.getModel(TODO_CONTEXT))?.provider).toBe("claude-code");
		const run = await lane.prompt("CELL: x = 6 * 7\\nprint(x)", undefined, BACKGROUND_CONTEXT);
		expect(run, JSON.stringify(run).slice(0, 2000)).toMatchObject({ ok: true });
		const messages = await messagesOf(first.runtime);
		const assistants = messages.filter((message): message is AssistantMessage => message.role === "assistant");
		expect(assistants).toHaveLength(2);
		const [toolTurn, answer] = assistants as [AssistantMessage, AssistantMessage];
		expect(toolTurn.stopReason).toBe("toolUse");
		expect(toolTurn.content.find((part) => part.type === "text")).toMatchObject({ text: "Running a cell." });
		expect(toolTurn.content.find((part) => part.type === "toolCall")).toMatchObject({
			name: "rlm",
			arguments: { code: "x = 6 * 7\nprint(x)" },
		});
		// Claude Code streams no thinking text: the block is kept, redacted, with its signature.
		expect(toolTurn.content.find((part) => part.type === "thinking")).toMatchObject({ redacted: true, thinking: "" });
		const result = messages.find((message) => message.role === "toolResult");
		expect(result).toMatchObject({ toolName: "rlm", isError: false, content: [{ type: "text", text: "42" }] });
		expect(answer.content.find((part) => part.type === "text")).toMatchObject({ text: "Cell said: 42" });
		expect(answer.stopReason).toBe("stop");
		expect(answer.usage).toMatchObject({
			input: 1000,
			output: 40,
			cacheRead: 500,
			cacheWrite: 100,
			totalTokens: 1640,
		});
		// The CLI's reported cost lands on the turn's last response.
		expect(answer.usage.cost.total).toBeCloseTo(0.01);
		expect(toolTurn.usage.cost.total).toBe(0);
		expect(answer.diagnostics?.find((item) => item.type === "claude_code_usage")?.details).toMatchObject({
			windows: { five_hour: { utilization: 0.25 } },
		});

		const spawn = calls().find((call) => call.phase === "spawn")!;
		const argv = spawn.argv!;
		expect(argv).toEqual(
			expect.arrayContaining([
				"--print",
				"--output-format=stream-json",
				"--input-format=stream-json",
				"--include-partial-messages",
				"--tools=",
				"--strict-mcp-config",
				"--allowedTools=mcp__ultron__rlm",
				"--setting-sources=",
				"--permission-prompts=none",
				"--disable-slash-commands",
				"--model=claude-opus-5-5",
			]),
		);
		expect(argv.some((arg) => arg.startsWith("--session-id="))).toBe(true);
		expect(argv.some((arg) => arg.startsWith("--resume"))).toBe(false);
		const mcp = JSON.parse(argv.find((arg) => arg.startsWith("--mcp-config="))!.slice("--mcp-config=".length));
		expect(mcp.mcpServers.ultron.args).toEqual(expect.arrayContaining(["mcp", "--bridge", "--token"]));
		// Ultron's own system prompt (the native runtime guide), with the note on how tools are named under Claude Code.
		expect(spawn.systemPrompt).toContain("mcp__ultron__rlm");
		expect(spawn.systemPrompt).toContain("# Running on Claude Code");
		expect(spawn.env).toMatchObject({ CLAUDECODE: null, CLAUDE_CODE_SESSION_ID: null });
		expect(Number(spawn.env?.MCP_TOOL_TIMEOUT)).toBeGreaterThan(60 * 60 * 1000);
		expect(calls().find((call) => call.phase === "mcp-ready")?.tools).toEqual(["rlm"]);
		// The run's process ended with the run.
		await vi.waitFor(
			() => expect(calls().some((call) => call.phase === "exit" && call.pid === spawn.pid)).toBe(true),
			{
				timeout: 10_000,
			},
		);
	}, 120_000);

	test("the native TUI renders the turn like a native one; the footer names Claude Code and its subscription use", async () => {
		initTheme("dark");
		const transcript = (await first.runtime.lane!.findEntries(undefined, BACKGROUND_CONTEXT)).reverse();
		const model = { provider: "claude-code", modelId: "claude-opus-5-5" };
		const snapshot = {
			lane: "main",
			transcript,
			tipId: transcript.at(-1)?.id ?? null,
			configuration: { model, thinkingLevel: "medium", activeToolNames: ["rlm"] },
			stats: {
				messageCount: transcript.length,
				usage: (transcript.at(-1) as { message: AssistantMessage }).message.usage,
			},
			operation: null,
			queues: [],
			faulted: false,
		} as unknown as LaneSnapshot;
		const view = new ExperimentalChatView(new TuiMainScreen(new ProcessTerminal()), project);
		view.apply(snapshot);
		const chat = plain(view.transcript.render(100));
		expect(chat).toContain("CELL: x = 6 * 7");
		expect(chat).toContain("Running a cell.");
		expect(chat).toContain("rlm python · 2 lines");
		expect(chat).toMatch(/1 │ x = 6 \* 7/);
		expect(chat).toMatch(/2 │ print\(x\)/);
		expect(chat).toContain("Cell said: 42");
		view.dispose();
		const catalogModel = (await first.runtime.lane!.getModel(TODO_CONTEXT))!;
		const footer = new NativeFooter(project, {
			snapshot: () => snapshot,
			models: () => ({
				catalog: {
					revision: 1,
					availableModels: [
						{ ...model, name: catalogModel.name, reasoning: true, model: catalogModel },
						{ provider: "openai", modelId: "gpt-5", name: "GPT-5", reasoning: true },
					],
				},
				configuration: { model, thinkingLevel: "medium" },
				refresh: { status: "idle" },
			}),
			sessionName: () => undefined,
		});
		const lines = plain(footer.render(160));
		footer.dispose();
		expect(lines).toContain("claude-opus-5-5 (Claude Code) • medium");
		expect(lines).not.toContain("(claude-code)");
		expect(lines).toMatch(/\$0\.0\d\d \(sub\)/);
		expect(lines).not.toContain("(auto)");
		expect(lines).toContain("Claude Code 5h 25% · 7d 50%");
	});

	test("the next turn resumes the Claude Code session with only the new message; kernel state persists", async () => {
		const lane = first.runtime.lane!;
		const firstSession = calls().find((call) => call.phase === "spawn")!.sessionId;
		const run = await lane.prompt("CELL: print(x + 1)", undefined, BACKGROUND_CONTEXT);
		expect(run, JSON.stringify(run).slice(0, 2000)).toMatchObject({ ok: true });
		const spawns = calls().filter((call) => call.phase === "spawn");
		expect(spawns).toHaveLength(2);
		expect(spawns[1]).toMatchObject({ sessionId: firstSession, resumed: true });
		expect(spawns[1]!.argv).toContain(`--resume=${firstSession}`);
		const prompts = calls().filter((call) => call.phase === "prompt" && call.pid === spawns[1]!.pid);
		expect(prompts.map((call) => call.prompt)).toEqual(["CELL: print(x + 1)"]);
		const messages = await messagesOf(first.runtime);
		const last = messages.at(-1) as AssistantMessage;
		expect(last.content.find((part) => part.type === "text")).toMatchObject({ text: "Cell said: 43" });
	}, 120_000);

	test("a message steered in while a cell runs reaches Claude Code in the same turn", async () => {
		const lane = first.runtime.lane!;
		const before = calls().length;
		const run = lane.prompt("CELL: import time\\ntime.sleep(2)\\nprint('slept')", undefined, BACKGROUND_CONTEXT);
		await vi.waitFor(
			() =>
				expect(
					calls()
						.slice(before)
						.some((call) => call.phase === "mcp-ready"),
				).toBe(true),
			{ timeout: 30_000 },
		);
		await new Promise((wake) => setTimeout(wake, 1_000));
		await lane.steer("also mention bananas", undefined, BACKGROUND_CONTEXT);
		expect((await run).ok).toBe(true);
		const steered = calls()
			.slice(before)
			.find((call) => call.phase === "steered");
		expect(steered?.also).toEqual(["also mention bananas"]);
		const last = (await messagesOf(first.runtime)).at(-1) as AssistantMessage;
		expect(last.content.find((part) => part.type === "text")).toMatchObject({
			text: "Cell said: slept Also: also mention bananas",
		});
		// Still one Claude Code process for the run.
		expect(
			calls()
				.slice(before)
				.filter((call) => call.phase === "spawn"),
		).toHaveLength(1);
	}, 120_000);

	test("abort stops the claude process group; the next turn resumes the session", async () => {
		const lane = first.runtime.lane!;
		const before = calls().length;
		const run = lane.prompt("SLOW please", undefined, BACKGROUND_CONTEXT);
		await vi.waitFor(
			() =>
				expect(
					calls()
						.slice(before)
						.some((call) => call.phase === "slow"),
				).toBe(true),
			{
				timeout: 30_000,
			},
		);
		const pid = calls()
			.slice(before)
			.find((call) => call.phase === "spawn")!.pid;
		await lane.abort(BACKGROUND_CONTEXT);
		await run;
		await vi.waitFor(
			() => {
				expect(() => process.kill(pid, 0)).toThrow();
			},
			{ timeout: 10_000 },
		);
		const aborted = (await messagesOf(first.runtime)).at(-1) as AssistantMessage;
		expect(aborted.stopReason).toBe("aborted");
		const again = await lane.prompt("hello again", undefined, BACKGROUND_CONTEXT);
		expect(again.ok).toBe(true);
		const spawns = calls()
			.slice(before)
			.filter((call) => call.phase === "spawn");
		expect(spawns.at(-1)).toMatchObject({ resumed: true });
		const last = (await messagesOf(first.runtime)).at(-1) as AssistantMessage;
		expect(last.content.find((part) => part.type === "text")).toMatchObject({ text: "Echo: hello again" });
	}, 120_000);

	test("an exhausted subscription window fails the turn with a clear, non-retried error", async () => {
		const lane = first.runtime.lane!;
		const before = calls().length;
		await lane.prompt("USAGE_LIMIT now", undefined, BACKGROUND_CONTEXT);
		const last = (await messagesOf(first.runtime)).at(-1) as AssistantMessage;
		expect(last.stopReason).toBe("error");
		expect(last.errorMessage).toMatch(/Claude Code usage limit reached \(five_hour window; resets at /);
		expect(last.diagnostics?.some((item) => item.type === "provider_usage_limit")).toBe(true);
		expect(
			calls()
				.slice(before)
				.filter((call) => call.phase === "spawn"),
		).toHaveLength(1);
	}, 120_000);

	test("a completion while idle wakes the root: a new resumed turn with the runtime event", async () => {
		const lane = first.runtime.lane!;
		const before = calls().length;
		const run = await lane.prompt(
			"CELL: job = await bash('''sleep 1; echo job-finished''', yield_after=0)\\nprint(job)",
			undefined,
			BACKGROUND_CONTEXT,
		);
		expect(run, JSON.stringify(run).slice(0, 2000)).toMatchObject({ ok: true });
		await vi.waitFor(
			() =>
				expect(
					calls()
						.slice(before)
						.some((call) => call.phase === "prompt" && String(call.prompt).includes("<runtime_event")),
				).toBe(true),
			{ timeout: 30_000, interval: 200 },
		);
		const wake = calls()
			.slice(before)
			.find((call) => call.phase === "prompt" && String(call.prompt).includes("<runtime_event"))!;
		const spawn = calls().find((call) => call.phase === "spawn" && call.pid === wake.pid)!;
		expect(spawn.resumed).toBe(true);
		expect(wake.prompt).toContain("job_done");
		// The wake-up turn runs on its own; later tests prompt the same lane, so let it finish first.
		await first.runtime.lane!.waitForIdle(BACKGROUND_CONTEXT);
	}, 120_000);

	test("a subagent (rlm.spawn) is a lane of its own on Claude Code: its own claude -p process and session", async () => {
		const lane = first.runtime.lane!;
		const rootSession = calls().find((call) => call.phase === "spawn")!.sessionId;
		const before = calls().length;
		const run = await lane.prompt(
			"CELL: h = await rlm.spawn('Echo the word kiwi', name='kid')\\nr = await rlm.collect([h])\\nprint(r)",
			undefined,
			BACKGROUND_CONTEXT,
		);
		expect(run, JSON.stringify(run).slice(0, 2000)).toMatchObject({ ok: true });
		const childPrompt = calls()
			.slice(before)
			.find((call) => call.phase === "prompt" && String(call.prompt).startsWith("Echo the word kiwi"))!;
		expect(childPrompt).toBeDefined();
		const childSpawn = calls().find((call) => call.phase === "spawn" && call.pid === childPrompt.pid)!;
		expect(childSpawn.sessionId).not.toBe(rootSession);
		expect(childSpawn.resumed).toBe(false);
		expect(childSpawn.argv).toContain("--allowedTools=mcp__ultron__rlm");
		const last = (await messagesOf(first.runtime)).at(-1) as AssistantMessage;
		expect(last.content.find((part) => part.type === "text")).toMatchObject({
			text: expect.stringContaining("Echo: Echo the word kiwi"),
		});
	}, 120_000);

	test("the session report names the mode, counts the cells and prices Claude Code as a subscription", async () => {
		const report = (await first.runtime.inspect!("usage.report", {}, BACKGROUND_CONTEXT)) as SessionReport;
		expect(report.mode).toBe("ultron --claude");
		// The turns so far: two cells, a steered turn, and the one that spawned a subagent.
		expect(report.turns?.count).toBeGreaterThanOrEqual(3);
		expect(report.turns?.running).toBe(0);
		expect(report.root.models).toEqual([
			{ model: "claude-code/claude-opus-5-5", responses: report.usage.lanes.root.responses },
		]);
		expect(report.cells?.source).toBe("transcript");
		expect(report.cells?.root.count).toBeGreaterThanOrEqual(3);
		expect(report.cells?.root.apis).toMatchObject({ "rlm.spawn": 1, "rlm.collect": 1 });
		expect(report.depth.verdict).toBe("depth 1: 0 frames, 1 sub-agent");
		expect(report.depth.subagents.byModel).toMatchObject([{ model: "claude-code/claude-opus-5-5", count: 1 }]);
		expect(report.usage.lanes.subagents.responses).toBeGreaterThan(0);
		// Claude Code's reported cost is a notional figure of the user's plan: never an API charge, never unknown.
		expect(report.usage.total.cost.reportedUsd).toBeNull();
		expect(report.usage.total.cost.unpricedResponses).toBe(0);
		expect(report.usage.total.cost.subscriptionUsd).toBeGreaterThan(0);
		expect(report.guardrails.countersSince).toBeNull();
		expect(report.unrecorded.turns).toBeUndefined();
		// The same report from the file alone.
		const offline = buildSessionReport(await readSessionLog(first.session.metadata.path));
		expect(offline.mode).toBe("ultron --claude");
		expect(offline.depth).toEqual(report.depth);
		expect(offline.usage).toEqual(report.usage);
	}, 60_000);

	test("a worktree subagent runs its Claude Code process and its cells in its own worktree", async () => {
		const git = (...args: string[]) => execFileSync("git", args, { cwd: project, encoding: "utf8" });
		git("init", "-q");
		git("config", "user.email", "test@example.com");
		git("config", "user.name", "Test");
		writeFileSync(join(project, "wt.txt"), "base");
		git("add", "wt.txt");
		git("commit", "-q", "-m", "base");
		try {
			const lane = first.runtime.lane!;
			const before = calls().length;
			const run = await lane.prompt(
				"CELL: h = await rlm.spawn(\"CELL: import os; print('cwd=' + os.getcwd()); open('wt.txt', 'w').write('child')\", name='wt', worktree=True)\\nr = await rlm.collect([h])\\nprint(r[0]['result']['worktree']['changed_files'])",
				undefined,
				BACKGROUND_CONTEXT,
			);
			expect(run, JSON.stringify(run).slice(0, 2000)).toMatchObject({ ok: true });
			const childPrompt = calls()
				.slice(before)
				.find((call) => call.phase === "prompt" && String(call.prompt).startsWith("CELL: import os"))!;
			expect(childPrompt.prompt).toContain("[Worktree] You work in your own Git worktree");
			const childSpawn = calls().find((call) => call.phase === "spawn" && call.pid === childPrompt.pid)!;
			expect(childSpawn.cwd).toContain(`${join(".git", "ultron-worktrees")}`);
			const cell = calls().find((call) => call.phase === "tool-result" && call.pid === childPrompt.pid)!;
			expect(JSON.stringify(cell.result)).toContain(`cwd=${childSpawn.cwd}`);
			// The parent's file is untouched; the child's change is on its branch.
			expect(readFileSync(join(project, "wt.txt"), "utf8")).toBe("base");
			const last = (await messagesOf(first.runtime)).at(-1) as AssistantMessage;
			expect(last.content.find((part) => part.type === "text")).toMatchObject({
				text: expect.stringContaining("['wt.txt']"),
			});
		} finally {
			rmSync(join(project, ".git"), { recursive: true, force: true });
			rmSync(join(project, "wt.txt"), { force: true });
		}
	}, 120_000);

	test("after another model answered (/model), Claude Code starts a fresh session with the conversation so far", async () => {
		const lane = first.runtime.lane!;
		const rootSession = calls().find((call) => call.phase === "spawn")!.sessionId;
		await lane.appendMessage(
			{ role: "user", content: "what model are you?", timestamp: Date.now() },
			BACKGROUND_CONTEXT,
		);
		await lane.appendMessage(
			{
				role: "assistant",
				content: [{ type: "text", text: "I am another model." }],
				api: "openai-responses",
				provider: "openai",
				model: "gpt-5",
				usage: {
					input: 1,
					output: 1,
					cacheRead: 0,
					cacheWrite: 0,
					totalTokens: 2,
					cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
				},
				stopReason: "stop",
				timestamp: Date.now(),
			},
			BACKGROUND_CONTEXT,
		);
		const before = calls().length;
		expect((await lane.prompt("back to claude", undefined, BACKGROUND_CONTEXT)).ok).toBe(true);
		const spawn = calls()
			.slice(before)
			.find((call) => call.phase === "spawn")!;
		expect(spawn.resumed).toBe(false);
		expect(spawn.sessionId).not.toBe(rootSession);
		const prompt = calls()
			.slice(before)
			.find((call) => call.phase === "prompt")!.prompt!;
		expect(prompt).toContain("<conversation_so_far>");
		expect(prompt).toContain("I am another model.");
		expect(prompt.endsWith("back to claude")).toBe(true);
		// The next turn resumes that new session.
		const next = calls().length;
		expect((await lane.prompt("and again", undefined, BACKGROUND_CONTEXT)).ok).toBe(true);
		expect(
			calls()
				.slice(next)
				.find((call) => call.phase === "spawn"),
		).toMatchObject({
			sessionId: spawn.sessionId,
			resumed: true,
		});
	}, 120_000);

	test("a logged-out Claude Code CLI fails the turn with a clear message, before any session starts", async () => {
		const lane = first.runtime.lane!;
		// Another path for the same fake, so the CLI check is not the cached one.
		const loggedOut = join(work, "claude-logged-out");
		symlinkSync(fakeClaude, loggedOut);
		const previous = process.env.ULTRON_CLAUDE_CODE_BIN;
		process.env.ULTRON_CLAUDE_CODE_BIN = loggedOut;
		process.env.FAKE_CLAUDE_AUTH = "logged_out";
		const before = calls().length;
		try {
			await lane.prompt("hello?", undefined, BACKGROUND_CONTEXT);
		} finally {
			process.env.ULTRON_CLAUDE_CODE_BIN = previous;
			delete process.env.FAKE_CLAUDE_AUTH;
		}
		const last = (await messagesOf(first.runtime)).at(-1) as AssistantMessage;
		expect(last.stopReason).toBe("error");
		expect(last.errorMessage).toMatch(/not logged in\. Run `claude auth login`/);
		expect(
			calls()
				.slice(before)
				.some((call) => call.phase === "spawn"),
		).toBe(false);
	}, 120_000);

	test("Ultron's own session resume continues the same Claude Code session", async () => {
		const sessionId = calls()
			.filter((call) => call.phase === "spawn")
			.at(-1)!.sessionId;
		const metadata = first.session.metadata;
		await vi.waitFor(
			async () => expect((await first.runtime.lane!.inspectExecution(BACKGROUND_CONTEXT)).current).toBeNull(),
			{
				timeout: 30_000,
			},
		);
		await close(first.runtime);
		const reopened = await open(metadata);
		const before = calls().length;
		const run = await reopened.runtime.lane!.prompt("after restart", undefined, BACKGROUND_CONTEXT);
		expect(run, JSON.stringify(run).slice(0, 2000)).toMatchObject({ ok: true });
		const spawn = calls()
			.slice(before)
			.find((call) => call.phase === "spawn")!;
		expect(spawn).toMatchObject({ sessionId, resumed: true });
		// The message of the failed (logged-out) turn never reached Claude Code, so it comes along.
		expect(
			calls()
				.slice(before)
				.find((call) => call.phase === "prompt")?.prompt,
		).toBe("hello?\n\nafter restart");
	}, 120_000);
});
