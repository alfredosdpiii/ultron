/**
 * Per-root tree limits on the real CLI: every model response on every lane (the root's own, sub-agents, frames)
 * is charged, tokens and cost, to the root that admitted it. Once a tree's turn or token limit is spent, each lane
 * still running gets one final request with tool choice "none" (so the run ends with an answer) and then nothing more;
 * a spent cost cap allows nothing more at all. Frame spend counts once in the root's totals: a frame's own `Budget` is a nested cap on top.
 */
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { describe, expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import { ScriptedProvider, type ScriptedReply, type ScriptedRequest } from "./support/scripted-provider.ts";
import { tempServerDir } from "./support/server-dir.ts";

/** models.json for a scripted model priced at $1 per million input and output tokens. */
function pricedModelsJson(baseUrl: string): string {
	return JSON.stringify({
		providers: {
			scripted: {
				baseUrl,
				api: "openai-completions",
				apiKey: "scripted-key",
				models: [
					{
						id: "scripted",
						name: "scripted",
						reasoning: false,
						input: ["text"],
						cost: { input: 1, output: 1, cacheRead: 0, cacheWrite: 0 },
						contextWindow: 128000,
						maxTokens: 4096,
					},
				],
			},
		},
	});
}

async function runFlow(
	script: (request: ScriptedRequest) => ScriptedReply,
	env: Record<string, string>,
	body: (client: RpcClient, provider: ScriptedProvider) => Promise<void>,
): Promise<void> {
	const root = mkdtempSync(join(tmpdir(), "ultron-tree-budget-"));
	const agentDir = join(root, "agent");
	const projectDir = join(root, "project");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(projectDir, { recursive: true });
	const provider = new ScriptedProvider(script);
	await provider.start();
	writeFileSync(join(agentDir, "models.json"), pricedModelsJson(provider.baseUrl));
	const client = new RpcClient({
		cliPath: resolve(__dirname, "../src/cli.ts"),
		cwd: projectDir,
		provider: "scripted",
		model: "scripted",
		args: ["--no-session"],
		env: {
			NODE_OPTIONS: `--import ${resolve(__dirname, "../src/experimental/source-resolver.ts")}`,
			ULTRON_CODING_AGENT_DIR: agentDir,
			ULTRON_SERVER_DIR: tempServerDir("u-tree-budget-"),
			ULTRON_TOOL_ROUNDS_NUDGE: "0",
			ULTRON_ASYNC_EVENTS: "off",
			...env,
		},
	});
	try {
		await client.start();
		await body(client, provider);
	} finally {
		await client.stop();
		await provider.stop();
		rmSync(root, { recursive: true, force: true });
	}
}

/** A request sent with tools turned off: a lane's one final request after its tree's limit. */
const isFinal = (request: ScriptedRequest) => request.raw.includes('"tool_choice":"none"');

/** Provider-reported tokens of one scripted request (input estimate plus 8 output tokens). */
function reportedTokens(request: ScriptedRequest): number {
	return Math.ceil(request.raw.length / 4) + 8;
}

const isChild = (request: ScriptedRequest) => request.firstUser.startsWith("CHILD");

/** The root spawns one sub-agent that never stops calling its REPL, then answers with what the spawn returned. */
function runawayChild(request: ScriptedRequest): ScriptedReply {
	if (isChild(request)) return { tool: "rlm", args: { code: `print("child round ${request.turn}")` } };
	if (request.turn === 0)
		return {
			tool: "rlm",
			args: {
				code: 'h = await rlm.spawn("CHILD: loop", name="runaway")\nr = (await rlm.collect([h]))[0]\nprint("CHILD RESULT", r)',
			},
		};
	return { text: "ROOT ANSWERED" };
}

function toolResults(events: unknown[]): string {
	return JSON.stringify(events.filter((event) => (event as { type?: string }).type === "tool_execution_end"));
}

describe("sub-agent spend counts against the root's limits", () => {
	test("a child's tokens count toward the root's token limit; the child fails with the limit and the root stops", async () => {
		await runFlow(runawayChild, { ULTRON_MAX_TOTAL_TOKENS: "40000" }, async (client, provider) => {
			const events = await client.promptAndWait("ROOT: delegate", undefined, 120_000);
			const child = provider.requests.filter(isChild);
			const root = provider.requests.filter((request) => !isChild(request));
			// Stopped by the tree's tokens: up to the request that crossed the limit everything ran with tools; after
			// it, only one final request per lane (the child's and the root's), each with tools off.
			const totals = provider.requests.map((_, index) =>
				provider.requests.slice(0, index + 1).reduce((sum, request) => sum + reportedTokens(request), 0),
			);
			const crossed = totals.findIndex((total) => total >= 40_000);
			expect(crossed).toBeGreaterThan(0);
			expect(provider.requests.slice(0, crossed + 1).some(isFinal)).toBe(false);
			const after = provider.requests.slice(crossed + 1);
			expect(after.map((request) => (isChild(request) ? "child" : "root"))).toEqual(["child", "root"]);
			expect(after.every(isFinal)).toBe(true);
			expect(child.length).toBeGreaterThan(2);
			expect(child.length).toBeLessThan(20);
			// The child answered its final request with a tool call (this scripted model ignores tool choice), so its
			// task failed with the limit message.
			const results = toolResults(events);
			expect(results).toContain("'status': 'failed'");
			expect(results).toMatch(/'error': 'Usage token limit reached for root turn:[^:]+: \d+ of 40000 tokens used/);
			// The root's final request ends its run with an answer instead of an error.
			expect(root).toHaveLength(2);
			expect(isFinal(root[1]!)).toBe(true);
			expect(root[1]!.raw).toContain("Tools are off for your last reply");
			expect(await client.getLastAssistantText()).toBe("ROOT ANSWERED");
		});
	}, 180_000);

	test("the cost cap counts every response in the tree and stops a runaway child", async () => {
		await runFlow(runawayChild, { ULTRON_MAX_COST_USD: "0.02" }, async (client, provider) => {
			const events = await client.promptAndWait("ROOT: delegate", undefined, 120_000);
			const child = provider.requests.filter(isChild);
			// $1 per million tokens: the tree stops at the first request that reaches $0.02.
			const spent = provider.requests.reduce((sum, request) => sum + reportedTokens(request), 0) / 1e6;
			expect(spent).toBeGreaterThanOrEqual(0.02);
			expect(spent - reportedTokens(provider.requests.at(-1)!) / 1e6).toBeLessThan(0.02);
			// The guarantee is the two lines above: nothing is sent once the cap is crossed. How many requests it takes
			// to get there depends on request sizes, which vary with timing (runtime hints, elapsed times), so the
			// count only has a loose bound, as in the token-limit test.
			expect(child.length).toBeLessThan(20);
			expect(toolResults(events)).toMatch(
				/'error': 'Usage cost cap reached for root turn:[^:]+: spent \$[\d.]+ of \$0\.02/,
			);
			// A cost cap is money: no final request either.
			expect(provider.requests.some(isFinal)).toBe(false);
			expect(provider.requests.filter((request) => !isChild(request))).toHaveLength(1);
		});
	}, 180_000);

	test("the turn limit counts the child's turns and blocks the root afterwards", async () => {
		await runFlow(runawayChild, { ULTRON_MAX_TOTAL_TURNS: "5" }, async (client, provider) => {
			const events = await client.promptAndWait("ROOT: delegate", undefined, 120_000);
			// Five turns with tools (the root's spawn and four child rounds), then one final request per lane.
			expect(provider.requests.map((r) => `${isChild(r) ? "C" : "R"}${isFinal(r) ? "*" : ""}`).join(" ")).toBe(
				"R C C C C C* R*",
			);
			expect(toolResults(events)).toContain("'error': 'Usage turn limit reached for root turn:");
			expect(await client.getLastAssistantText()).toBe("ROOT ANSWERED");
		});
	}, 180_000);

	test("agents.status shows the tree's spend; a frame's responses count once, beside its own budget", async () => {
		await runFlow(
			(request) => {
				if (request.system.startsWith("You are an inference frame")) return { text: "42" };
				if (isChild(request)) return { text: "child done" };
				if (request.turn === 0)
					return {
						tool: "rlm",
						args: {
							code: [
								"import json",
								'v = await rlm.infer("the number", context=["forty-two"], contract={"type": "integer"})',
								'h = await rlm.spawn("CHILD: one answer", name="one")',
								"await rlm.collect([h])",
								"s = await agents.status()",
								'print("SPEND", json.dumps(s["spend"]), "VALUE", v)',
							].join("\n"),
						},
					};
				return { text: `ROOT SAW ${request.lastToolResult}` };
			},
			{ ULTRON_MAX_TOTAL_TOKENS: "1000000" },
			async (client, provider) => {
				await client.promptAndWait("ROOT: infer and delegate", undefined, 120_000);
				const text = (await client.getLastAssistantText()) ?? "";
				const match = /SPEND (\{.*?\}) VALUE 42/.exec(text);
				expect(match, text).not.toBeNull();
				const spend = JSON.parse(match![1]!) as { turns: number; tokens: number; costUsd: number };
				// Everything before the final answer: the root's first turn, the frame and the child.
				const before = provider.requests.slice(0, -1);
				expect(before.filter((request) => request.system.startsWith("You are an inference frame"))).toHaveLength(1);
				expect(before.filter(isChild)).toHaveLength(1);
				expect(spend.turns).toBe(3);
				// Each response is counted exactly once: the frame's and child's settled model calls are not added again.
				expect(spend.tokens).toBe(before.reduce((sum, request) => sum + reportedTokens(request), 0));
				expect(spend.costUsd).toBeCloseTo(spend.tokens / 1e6, 9);
			},
		);
	}, 180_000);
});
