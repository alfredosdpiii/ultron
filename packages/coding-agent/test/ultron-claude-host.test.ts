import { execFileSync } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, test } from "vitest";
import { AsyncEventDispatcher, type RuntimeEvent } from "../src/ultron/async-events.ts";
import { childClaudeArgs, childClaudeModel, foldStreamLine } from "../src/ultron/claude/child.ts";
import { EXTERNAL_ROOT_OPERATION, ExternalRootController } from "../src/ultron/claude/external-root.ts";
import { CLAUDE_RLM_TOOL, claudeRuntimeGuide, claudeSystemPrompt, runtimeGuide } from "../src/ultron/claude/guide.ts";
import { CellHints } from "../src/ultron/rlm/hints.ts";
import { createMemoryModuleStore } from "../src/ultron/rlm/host-module.ts";
import type { NativeExternalChildRun } from "../src/ultron/rlm/native-host.ts";
import { rlmRuntimePrompt } from "../src/ultron/rlm/prompt.ts";
import { waitNudgeMessage } from "../src/ultron/tool-round-nudge.ts";
import { hostFixture } from "./ultron-host-fixtures.ts";

/**
 * Claude Code as the root agent (ultron/claude): the runtime pieces that change when the root lives outside Ultron.
 * Subagents can run as processes of their own whose verdict reaches the host through the subagent's lane, a root can
 * itself be a subagent with a delegation allowance, root events go to an inbox instead of starting runs, and the
 * guide, hints and nudges say where completions arrive (the next rlm result) instead of promising a wake-up.
 */

const directories: string[] = [];
const fixtures: Array<ReturnType<typeof hostFixture>> = [];
afterEach(async () => {
	for (const fixture of fixtures.splice(0)) await fixture.host.close();
	for (const directory of directories.splice(0)) await rm(directory, { recursive: true, force: true });
});

async function repository(): Promise<string> {
	const root = await mkdtemp(join(tmpdir(), "ultron-claude-host-"));
	directories.push(root);
	execFileSync("git", ["init", "-q"], { cwd: root });
	return root;
}

describe("external subagents (Claude Code children)", () => {
	test("the runner gets the brief and nesting, its verdict comes back through its lane and is checked", async () => {
		const workspace = await repository();
		const runs: NativeExternalChildRun[] = [];
		const fixture = hostFixture({
			workspace,
			externalChild: async (run) => {
				runs.push(run);
				run.progress({ turns: 1, toolCalls: 1, text: "working", model: "claude-code/sonnet" });
				await writeFile(join(workspace, "out.txt"), "42\n");
				// The child's server relays rlm.finish; the parent host records it on the child's task.
				await fixture.host.handle(
					"rlm.finish",
					{
						status: "passed",
						summary: "wrote out.txt",
						evidence: ["out.txt:1 holds 42"],
						changed_files: ["out.txt"],
					},
					{} as never,
					{ lane: run.laneName },
				);
				return {
					text: "Done: out.txt",
					turns: 3,
					toolCalls: 2,
					model: "claude-code/sonnet",
					usage: { inputTokens: 900, outputTokens: 100, totalTokens: 1000, cost: 0.01 },
				};
			},
		});
		fixtures.push(fixture);
		const { rlm_child_id: id } = await fixture.call<{ rlm_child_id: string }>("rlm.spawn", {
			prompt: "write 42 to out.txt",
			kwargs: { name: "writer", depth: 1 },
		});
		const collected = await fixture.call<{
			results: Array<{
				result: { status: string; value: string; verdict: { status: string }; check: { outcome: string } };
			}>;
		}>("rlm.collect", { selectors: [id] });
		expect(runs).toHaveLength(1);
		expect(runs[0]).toMatchObject({ prompt: "write 42 to out.txt", level: 1, allowance: 1 });
		expect(runs[0]!.laneName).toContain(id);
		// A harness lane was never used for it.
		expect(fixture.prompts).toHaveLength(0);
		const result = collected.results[0]!.result;
		expect(result).toMatchObject({ status: "succeeded", value: "Done: out.txt", verdict: { status: "passed" } });
		expect(result.check.outcome).toBe("verified");
		const status = await fixture.call<{ tasks: Array<Record<string, unknown>> }>("agents.status", { graph: true });
		expect(status.tasks.find((task) => task.id === id)).toMatchObject({
			model: "claude-code/sonnet",
			turns: 3,
			toolCallCount: 2,
		});
	});

	test("a failing runner fails the task; an aborted one is cancelled", async () => {
		const fixture = hostFixture({
			externalChild: async (run) => {
				if (run.prompt === "fail") throw new Error("claude exited with code 1");
				await new Promise((_, reject) => run.signal.addEventListener("abort", () => reject(new Error("stopped"))));
				return { text: "", turns: 0, toolCalls: 0 };
			},
		});
		fixtures.push(fixture);
		const failed = await fixture.call<{ rlm_child_id: string }>("rlm.spawn", {
			prompt: "fail",
			kwargs: { name: "f" },
		});
		const [result] = (
			await fixture.call<{ results: Array<{ result: { status: string; error: string } }> }>("rlm.collect", {
				selectors: [failed.rlm_child_id],
			})
		).results;
		expect(result!.result).toMatchObject({ status: "failed", error: expect.stringContaining("exited with code 1") });
		const slow = await fixture.call<{ rlm_child_id: string }>("rlm.spawn", { prompt: "slow", kwargs: { name: "s" } });
		await fixture.call("agents.cancel", { id: slow.rlm_child_id });
		const [cancelled] = (
			await fixture.call<{ results: Array<{ result: { status: string } }> }>("rlm.collect", {
				selectors: [slow.rlm_child_id],
			})
		).results;
		expect(cancelled!.result.status).toBe("cancelled");
	});

	test("a root that is itself a subagent spawns only within its allowance and relays its verdict", async () => {
		const relayed: Array<Record<string, unknown>> = [];
		const noDepth = hostFixture({
			rootSpawn: { level: 1, allowance: 0 },
			rootFinish: async (payload) => {
				relayed.push(payload);
				return { recorded: true };
			},
		});
		fixtures.push(noDepth);
		await expect(noDepth.call("rlm.spawn", { prompt: "x", kwargs: { name: "x" } })).rejects.toThrow(
			/started with depth=0/,
		);
		expect(await noDepth.call("rlm.finish", { status: "blocked", summary: "no network" })).toEqual({
			recorded: true,
		});
		expect(relayed).toEqual([{ status: "blocked", summary: "no network" }]);
		const withDepth = hostFixture({ rootSpawn: { level: 1, allowance: 2 }, script: () => "ok" });
		fixtures.push(withDepth);
		await expect(withDepth.call("rlm.spawn", { prompt: "x", kwargs: { name: "x", depth: 2 } })).rejects.toThrow(
			/too deep here: at most 1/,
		);
		await expect(
			withDepth.call("rlm.spawn", { prompt: "x", kwargs: { name: "x", depth: 1 } }),
		).resolves.toMatchObject({
			name: "x",
		});
	});

	test("harness-lane children get the configured child model when the spawn names none", async () => {
		const fixture = hostFixture({ childModel: "cliproxyapi/glm-5.3-flash", script: () => "ok" });
		fixtures.push(fixture);
		const handle = await fixture.call<{ model: string }>("rlm.spawn", { prompt: "x", kwargs: { name: "x" } });
		expect(handle.model).toBe("cliproxyapi/glm-5.3-flash");
		const named = await fixture.call<{ model: string }>("rlm.spawn", {
			prompt: "y",
			kwargs: { name: "y", model: "other/m" },
		});
		expect(named.model).toBe("other/m");
	});
});

describe("Claude Code child processes", () => {
	test("models map to Claude Code aliases; other providers are refused", () => {
		expect(childClaudeModel(undefined, "sonnet")).toBe("sonnet");
		expect(childClaudeModel("claude-code/opus", "sonnet")).toBe("opus");
		expect(childClaudeModel("haiku", "sonnet")).toBe("haiku");
		expect(() => childClaudeModel("openai/gpt-5", "sonnet")).toThrow(/Claude Code processes/);
	});

	test("a child has only the REPL tool, no user settings, and its own system prompt", () => {
		const args = childClaudeArgs({ model: "sonnet", mcpConfig: "{}", systemPromptFile: "/tmp/p.md" });
		const flagValue = (flag: string) => args[args.indexOf(flag) + 1];
		expect(args[0]).toBe("-p");
		expect(flagValue("--tools")).toBe("");
		expect(flagValue("--allowedTools")).toBe(CLAUDE_RLM_TOOL);
		expect(flagValue("--setting-sources")).toBe("");
		expect(flagValue("--system-prompt-file")).toBe("/tmp/p.md");
		expect(args).toContain("--strict-mcp-config");
		expect(args).toContain("--no-session-persistence");
	});

	test("stream-json turns count responses, not content blocks", () => {
		const stats = { turns: 0, toolCalls: 0, text: "" };
		const assistant = (id: string, part: Record<string, unknown>) =>
			JSON.stringify({ type: "assistant", message: { id, content: [part] } });
		foldStreamLine(JSON.stringify({ type: "system", subtype: "init", model: "claude-sonnet-5" }), stats);
		foldStreamLine(assistant("m1", { type: "text", text: "Let me look." }), stats);
		foldStreamLine(assistant("m1", { type: "tool_use", id: "t1", name: CLAUDE_RLM_TOOL, input: {} }), stats);
		foldStreamLine(assistant("m2", { type: "text", text: "Done." }), stats);
		const result = foldStreamLine(JSON.stringify({ type: "result", result: "Done.", num_turns: 2 }), stats);
		expect(stats).toMatchObject({ turns: 2, toolCalls: 1, text: "Done.", model: "claude-code/claude-sonnet-5" });
		expect(result).toMatchObject({ result: "Done." });
	});
});

describe("the external root", () => {
	function controller(options: { running?: number; exhausted?: string; nudge?: number } = {}) {
		const calls: string[] = [];
		const events: string[] = [];
		const root = new ExternalRootController({
			execute: async (code, invocation) => {
				calls.push(invocation.operationId);
				if (code === "raise") throw new Error("ZeroDivisionError: division by zero");
				return { content: [{ type: "text", text: `ran ${code}` }] };
			},
			host: {
				beginRootTurn: (id) => events.push(`begin ${id}`),
				endRootTurn: (id) => events.push(`end ${id}`),
				rootIdOfRun: (id) => `turn:${id}`,
				pendingRootNotifications: () => options.running ?? 0,
			},
			usage: { turnBudgetExhausted: async () => options.exhausted },
			hints: { runEnded: (lane) => events.push(`hints ${lane}`) },
			fileHooks: { beginTurn: () => events.push("files") },
			lokiNotice: Promise.resolve("Loki created .loki/ and committed it."),
			lokiContext: "LOKI policy",
			toolRoundsNudge: options.nudge ?? 0,
			skillNudge: 0,
			asyncEvents: true,
		});
		return { root, calls, events };
	}
	const event: RuntimeEvent = {
		kind: "job_done",
		id: "job-1",
		status: "completed",
		summary: "exit 0",
		fetch: 'await rlm.job("job-1")',
		lane: "main",
	};

	test("cells run on the root lane; waiting events lead the next result", async () => {
		const { root, calls } = controller();
		root.sink([event]);
		const first = await root.runCell("x = 1");
		expect(calls[0]!.startsWith(EXTERNAL_ROOT_OPERATION)).toBe(true);
		expect(first.content[0]).toEqual({
			type: "text",
			text: expect.stringMatching(/^<runtime_event kind="job_done" id="job-1".*\/>\nran x = 1$/),
		});
		expect((await root.runCell("x")).content[0]).toEqual({ type: "text", text: "ran x" });
		const failed = await root.runCell("raise");
		expect(failed).toMatchObject({
			isError: true,
			content: [{ type: "text", text: expect.stringContaining("ZeroDivision") }],
		});
	});

	test("a user turn opens a root turn with Loki's note and waiting events; Stop closes it", async () => {
		const { root, events } = controller();
		expect(await root.sessionContext()).toBe("LOKI policy\n\nLoki created .loki/ and committed it.");
		root.sink([event]);
		const context = await root.beginTurn();
		expect(context).toContain("Runtime events since your last rlm call");
		// The setup note was already shown with the session context.
		expect(context).not.toContain("committed it");
		expect(root.turn).toBeDefined();
		const turn = root.turn!;
		await root.endTurn();
		expect(root.turn).toBeUndefined();
		expect(events).toEqual([`begin ${turn}`, "files", `end ${turn}`, "hints main"]);
	});

	test("the research brake and the wait nudge are appended to results, without promising a wake-up", async () => {
		const { root } = controller({ running: 1, nudge: 2 });
		await root.runCell("a");
		// The second round reaches the brake while a subagent still runs: the wait nudge comes with that result.
		const second = await root.runCell("b");
		const text = (second.content[0] as { text: string }).text;
		expect(text).toContain("still running");
		expect(text).toContain("rlm.collect");
		expect(text).not.toMatch(/events wake you/);
	});

	test("a spent budget refuses the cell", async () => {
		const { root, calls } = controller({ exhausted: "token budget of turn:x exhausted" });
		const result = await root.runCell("x");
		expect(result.isError).toBe(true);
		expect(calls).toHaveLength(0);
	});

	test("the dispatcher gives root events to the sink and never starts a run", async () => {
		const received: RuntimeEvent[][] = [];
		const dispatcher = new AsyncEventDispatcher({
			harness: {
				lane: async () => {
					throw new Error("the root lane must not be touched");
				},
				events: { on: () => () => {} },
			} as never,
			host: { rootIdOfRun: (id) => id, continueRootTurn: () => {} },
			rootSink: (events) => received.push(events),
			coalesceMs: 1,
		});
		dispatcher.publish(event);
		await dispatcher.drain();
		expect(received).toEqual([[event]]);
		await dispatcher.close();
	});
});

describe("guide and hint wording for a root nothing can wake", () => {
	test("the Claude guide says where completions arrive and drops ctx", () => {
		const guide = claudeRuntimeGuide();
		expect(guide).toContain("at the top of your next rlm result");
		expect(guide).toContain("Nothing wakes you after you reply");
		expect(guide).not.toContain("starts a new turn");
		expect(guide).not.toContain("or end your turn: each end arrives");
		expect(guide).not.toContain("`ctx` edits");
		// Ultron's own guide is unchanged.
		const native = rlmRuntimePrompt(["rlm"])!;
		expect(native).toContain("starts a new turn if yours ended");
		expect(native).toContain("`ctx` edits");
	});

	test("the system prompt stands alone: identity, the one tool, safety, environment", () => {
		const prompt = claudeSystemPrompt("claude", { cwd: "/work", platform: "linux", date: "2026-09-30" });
		expect(prompt).toContain(CLAUDE_RLM_TOOL);
		expect(prompt).toMatch(/destructive/);
		expect(prompt).toContain("Never commit or push unless asked");
		expect(prompt).toContain("- Working directory: /work");
		const child = claudeSystemPrompt("claude-child", { allowance: 0 });
		expect(child).toContain("rlm.finish");
		expect(child).toContain("`rlm.spawn` is refused for you");
		expect(runtimeGuide("ultron")).toContain("Pre-imported");
	});

	test("hints on the root point at the next result; child lanes keep the native wording", async () => {
		const hints = new CellHints({ store: createMemoryModuleStore(), rootDelivery: "next-call" });
		const detach = async (lane: string) => {
			hints.beginCell(lane, "out = await bash('''make''')");
			hints.observe(
				lane,
				"bash",
				{ command: "make", yield_after: 30 },
				{ running: true, job: { id: "job-9" } },
				Date.now(),
			);
			return hints.endCell(lane, { text: "ok" });
		};
		const root = await detach("main");
		expect(root).toContain("at the top of a later rlm result");
		expect(root).not.toContain("starts your next turn");
		expect(await detach("ultron.rlm-child.1")).toContain("starts your next turn");
		expect(waitNudgeMessage(2, true, true)).not.toMatch(/events wake you/);
		expect(waitNudgeMessage(2, true, false)).toMatch(/events wake you/);
	});
});
