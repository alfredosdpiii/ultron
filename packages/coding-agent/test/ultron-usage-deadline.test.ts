import { describe, expect, test } from "vitest";
import {
	budgetLine,
	DEFAULT_MAX_TOTAL_TOKENS,
	NativeUsageLedger,
	nativeUsageLimitsFromEnv,
} from "../src/ultron/usage.ts";

describe("root wall deadline", () => {
	test("later tasks with the default timeout are capped to the remaining root budget, not refused", async () => {
		const ledger = new NativeUsageLedger(undefined, { limits: { maxWallMs: 30 * 60 * 1000 } });
		const first = await ledger.reserve({ kind: "task", requestKey: "a", timeoutMs: 30 * 60 * 1000 });
		await new Promise((resolve) => setTimeout(resolve, 5));
		const second = await ledger.reserve({ kind: "task", requestKey: "b", timeoutMs: 30 * 60 * 1000 });
		expect(second.deadlineAt).toBe(first.deadlineAt);
		const untimed = await ledger.reserve({ kind: "task", requestKey: "c" });
		expect(untimed.deadlineAt).toBe(first.deadlineAt);
	});
});

describe("default token cap, exhaustion kinds and the budget line", () => {
	test("a root gets the default token cap unless one is set or turned off; goal roots are exempt from the default", async () => {
		expect(nativeUsageLimitsFromEnv({})).toMatchObject({ defaultMaxTotalTokens: DEFAULT_MAX_TOTAL_TOKENS });
		expect(nativeUsageLimitsFromEnv({}).maxTotalTokens).toBeUndefined();
		expect(nativeUsageLimitsFromEnv({ ULTRON_MAX_TOTAL_TOKENS: "500" })).toEqual(
			expect.objectContaining({ maxTotalTokens: 500 }),
		);
		expect(nativeUsageLimitsFromEnv({ ULTRON_MAX_TOTAL_TOKENS: "500" }).defaultMaxTotalTokens).toBeUndefined();
		expect(nativeUsageLimitsFromEnv({ ULTRON_MAX_TOTAL_TOKENS: "off" }).defaultMaxTotalTokens).toBeUndefined();

		const ledger = new NativeUsageLedger(undefined, { limits: { defaultMaxTotalTokens: 100 } });
		await ledger.recordTurn("turn:a", { totalTokens: 150 });
		await ledger.recordTurn("goal:g1", { totalTokens: 150 });
		expect(await ledger.budgetExhaustion("turn:a")).toMatchObject({ kind: "tokens" });
		expect(await ledger.budgetExhaustion("goal:g1")).toBeUndefined();
		expect(await ledger.remaining("goal:g1")).toEqual({ turnsLeft: null, tokensLeft: null, tokenCap: null });
		// An explicit cap binds goal roots too.
		const explicit = new NativeUsageLedger(undefined, { limits: { maxTotalTokens: 100 } });
		await explicit.recordTurn("goal:g1", { totalTokens: 150 });
		expect(await explicit.budgetExhaustion("goal:g1")).toMatchObject({ kind: "tokens" });
	});

	test("exhaustion says which limit: turns, tokens or cost", async () => {
		const turns = new NativeUsageLedger(undefined, { limits: { maxTotalTurns: 1 } });
		await turns.recordTurn("turn:a", { totalTokens: 1 });
		expect(await turns.budgetExhaustion("turn:a")).toMatchObject({ kind: "turns" });
		expect(await turns.turnBudgetExhausted("turn:a")).toMatch(/turn limit reached/);
		const cost = new NativeUsageLedger(undefined, { limits: { maxCostUsd: 0.01 } });
		await cost.recordTurn("turn:a", { totalTokens: 1, cost: 0.02 });
		expect(await cost.budgetExhaustion("turn:a")).toMatchObject({ kind: "cost" });
		expect(await turns.remaining("turn:b")).toMatchObject({ turnsLeft: 1 });
	});

	test("the budget line: turns whenever capped, tokens when set explicitly or past half the default", () => {
		const remaining = (turnsLeft: number | null, tokensLeft: number | null, tokenCap: number | null) => ({
			turnsLeft,
			tokensLeft,
			tokenCap,
		});
		expect(budgetLine(remaining(null, 9_000_000, 10_000_000), false)).toBeUndefined();
		expect(budgetLine(remaining(null, 4_000_000, 10_000_000), false)).toBe(
			"[budget left for this request: 4,000,000 tokens]",
		);
		expect(budgetLine(remaining(null, 900, 1000), true)).toBe("[budget left for this request: 900 tokens]");
		expect(budgetLine(remaining(3, null, null), false)).toBe("[budget left for this request: 3 model turns]");
		expect(budgetLine(remaining(null, null, null), true)).toBeUndefined();
	});
});
