import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { JsonValue } from "@ultron/chord";
import { afterEach, describe, expect, test } from "vitest";
import { SettingsManager } from "../src/core/settings-manager.ts";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import { NativeUsageLedger, nativeUsageLimitsFromEnv } from "../src/ultron/usage.ts";
import { ScriptedProvider, scriptedModelsJson } from "./support/scripted-provider.ts";
import { deferred, hostFixture, waitFor } from "./ultron-host-fixtures.ts";

const fixtures: Array<ReturnType<typeof hostFixture>> = [];
afterEach(async () => {
	for (const fixture of fixtures.splice(0)) await fixture.host.close();
});

function countingStore() {
	const state = { value: undefined as JsonValue | undefined, writes: 0 };
	return {
		state,
		read: async () => structuredClone(state.value),
		write: async (next: JsonValue) => {
			state.writes += 1;
			state.value = structuredClone(next);
		},
	};
}

describe("per-root max_total_turns / max_total_tokens", () => {
	test("settings set the limits and the environment overrides or removes them", () => {
		expect(nativeUsageLimitsFromEnv({})).not.toHaveProperty("maxTotalTurns");
		expect(nativeUsageLimitsFromEnv({}, { maxTotalTokens: 5000, maxTotalTurns: 12 })).toMatchObject({
			maxTotalTokens: 5000,
			maxTotalTurns: 12,
		});
		expect(
			nativeUsageLimitsFromEnv(
				{ ULTRON_MAX_TOTAL_TOKENS: "900", ULTRON_MAX_TOTAL_TURNS: "off" },
				{ maxTotalTokens: 5000, maxTotalTurns: 12 },
			),
		).toMatchObject({ maxTotalTokens: 900 });
		expect(nativeUsageLimitsFromEnv({ ULTRON_MAX_TOTAL_TURNS: "off" }, { maxTotalTurns: 12 })).not.toHaveProperty(
			"maxTotalTurns",
		);
		// Invalid values keep what the settings say; invalid settings are ignored.
		expect(nativeUsageLimitsFromEnv({ ULTRON_MAX_TOTAL_TURNS: "0" }, { maxTotalTurns: 4 })).toMatchObject({
			maxTotalTurns: 4,
		});
		expect(nativeUsageLimitsFromEnv({}, { maxTotalTurns: -3, maxTotalTokens: 1.5 })).not.toHaveProperty(
			"maxTotalTurns",
		);
	});

	test("settings.json rootBudget is read by the settings manager", () => {
		const settings = SettingsManager.inMemory({ rootBudget: { maxTotalTokens: 1000, maxTotalTurns: 0 } });
		expect(settings.getRootBudgetSettings()).toEqual({ maxTotalTokens: 1000 });
	});

	test("model turns of a root and its descendants count; the limit refuses model work with a clear error", async () => {
		const ledger = new NativeUsageLedger(undefined, { limits: { maxTotalTurns: 3 } });
		await ledger.recordTurn("turn:a", { totalTokens: 100 });
		await ledger.recordTurn("turn:a", { totalTokens: 50 });
		await expect(ledger.turnBudgetExhausted("turn:a")).resolves.toBeUndefined();
		await expect(ledger.reserve({ kind: "task", rootId: "turn:a", requestKey: "t1" })).resolves.toBeDefined();
		// A descendant's turn is charged to the same root.
		await ledger.recordTurn("turn:a", { totalTokens: null });
		const reason = await ledger.turnBudgetExhausted("turn:a");
		expect(reason).toContain("Usage turn limit reached for root turn:a: 3 of 3 model turns used (max_total_turns");
		await expect(ledger.reserve({ kind: "model", rootId: "turn:a", requestKey: "m1" })).rejects.toThrow(
			"Usage turn limit reached for root turn:a",
		);
		await expect(ledger.reserve({ kind: "task", rootId: "turn:a", requestKey: "t2" })).rejects.toThrow(
			"max_total_turns",
		);
		// Work that calls no model is still admitted, and other roots are untouched.
		await expect(
			ledger.reserve({ kind: "task", rootId: "turn:a", requestKey: "t3", modelBacked: false }),
		).resolves.toBeDefined();
		await expect(ledger.turnBudgetExhausted("turn:b")).resolves.toBeUndefined();
		await expect(ledger.reserve({ kind: "model", rootId: "turn:b", requestKey: "m1" })).resolves.toBeDefined();
		expect((await ledger.status("turn:a")).turns).toEqual({
			turns: 3,
			tokens: 150,
			maxTotalTurns: 3,
			maxTotalTokens: null,
		});
		expect((await ledger.status("turn:a")).limits).toMatchObject({ maxTotalTurns: 3, maxTotalTokens: null });
	});

	test("the token limit counts provider-reported tokens across the tree", async () => {
		const ledger = new NativeUsageLedger(undefined, { limits: { maxTotalTokens: 1000 } });
		await ledger.recordTurn("turn:a", { totalTokens: 600 });
		await expect(ledger.turnBudgetExhausted("turn:a")).resolves.toBeUndefined();
		await ledger.recordTurn("turn:a", { totalTokens: 450 });
		await expect(ledger.turnBudgetExhausted("turn:a")).resolves.toContain(
			"Usage token limit reached for root turn:a: 1050 of 1000 tokens used (max_total_tokens",
		);
		await expect(ledger.reserve({ kind: "model", rootId: "turn:a" })).rejects.toThrow("Usage token limit reached");
	});

	test("counts survive a reopen, and nothing is written per turn without a limit", async () => {
		const store = countingStore();
		const limited = new NativeUsageLedger(store, { limits: { maxTotalTurns: 2 } });
		await limited.recordTurn("turn:a", { totalTokens: 10 });
		await limited.recordTurn("turn:a", { totalTokens: 10 });
		const reopened = new NativeUsageLedger(store, { limits: { maxTotalTurns: 2 } });
		await expect(reopened.turnBudgetExhausted("turn:a")).resolves.toContain("2 of 2 model turns");

		const quiet = countingStore();
		const unlimited = new NativeUsageLedger(quiet);
		await unlimited.recordTurn("turn:a", { totalTokens: 10 });
		await expect(unlimited.turnBudgetExhausted("turn:a")).resolves.toBeUndefined();
		expect(quiet.state.writes).toBe(0);
		expect((await unlimited.status("turn:a")).turns).toEqual({
			turns: 0,
			tokens: 0,
			maxTotalTurns: null,
			maxTotalTokens: null,
		});
	});

	test("a descendant lane's turns are charged to the root turn that admitted its task", async () => {
		const gate = deferred();
		const lanes: string[] = [];
		const fixture = hostFixture({
			usage: new NativeUsageLedger(undefined, { limits: { maxTotalTurns: 10 } }),
			rootTurns: true,
			script: async (lane: string) => {
				lanes.push(lane);
				await gate.promise;
				return "done";
			},
		});
		fixtures.push(fixture);
		fixture.host.beginRootTurn("r1");
		await fixture.call("agents.spawn", { definition: "rlm-child@1", input: { prompt: "hold" } });
		await waitFor(() => lanes.length === 1);
		expect(fixture.host.usageRootForLane("main", "r2")).toBe("turn:r2");
		// The child keeps its admitting turn's root even after a new root turn begins.
		fixture.host.beginRootTurn("r2");
		expect(fixture.host.usageRootForLane(lanes[0]!, "child-run")).toBe("turn:r1");
		expect(fixture.host.usageRootForLane("unknown-lane", "x")).toBeUndefined();
		gate.resolve();
	});

	test("the real CLI stops a root turn at ULTRON_MAX_TOTAL_TURNS with the limit error, and the next turn starts fresh", async () => {
		const root = mkdtempSync(join(tmpdir(), "ultron-root-budget-"));
		const agentDir = join(root, "agent");
		const projectDir = join(root, "project");
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(projectDir, { recursive: true });
		// Never answers: every response is another tool call.
		const provider = new ScriptedProvider((request) => ({
			tool: "bash",
			args: { command: `echo round ${request.turn}` },
		}));
		await provider.start();
		writeFileSync(join(agentDir, "models.json"), scriptedModelsJson(provider.baseUrl));
		const client = new RpcClient({
			cliPath: resolve(__dirname, "../src/cli.ts"),
			cwd: projectDir,
			provider: "scripted",
			model: "scripted",
			args: ["--no-session"],
			env: {
				NODE_OPTIONS: `--import ${resolve(__dirname, "../src/experimental/source-resolver.ts")}`,
				ULTRON_CODING_AGENT_DIR: agentDir,
				ULTRON_SERVER_DIR: mkdtempSync(join("/tmp", "u-budget-")),
				ULTRON_TOOL_ROUNDS_NUDGE: "0",
				ULTRON_MAX_TOTAL_TURNS: "3",
			},
		});
		try {
			await client.start();
			// The scripted model calls `bash`, which is not a tool in REPL-only mode: the harness rejects those calls
			// before `before_tool`, so this also proves the request-level check stops the loop.
			const events = await client.promptAndWait("loop forever", undefined, 120_000);
			expect(provider.requests.length).toBe(3);
			const messages = JSON.stringify(events);
			expect(messages).toContain("Usage turn limit reached for root turn:");
			expect(messages).toContain("3 of 3 model turns used (max_total_turns");
			await client.promptAndWait("again", undefined, 120_000);
			expect(provider.requests.length).toBe(6);
		} finally {
			await client.stop();
			await provider.stop();
			rmSync(root, { recursive: true, force: true });
		}
	}, 180_000);
});
