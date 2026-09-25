import { describe, expect, test } from "vitest";
import { NativeUsageLedger } from "../src/ultron/usage.ts";

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
