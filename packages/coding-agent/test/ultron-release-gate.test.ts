import { afterEach, describe, expect, test } from "vitest";
import { createReleaseGateModule } from "../src/ultron/release-gate.ts";
import { createMemoryModuleStore, type HostModuleStore } from "../src/ultron/rlm/host-module.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";

const context = {} as never;
const FIXTURE = "f".repeat(64);
const OTHER_FIXTURE = "e".repeat(64);

function memoryStore(): NativeHostStore {
	let value: unknown;
	return {
		read: async () => value as never,
		write: async (next) => {
			value = structuredClone(next);
		},
	};
}

function waitingLane() {
	return {
		findEntries: async () => [],
		getActiveTools: async () => [],
		setModel: async () => {},
		prompt: () => new Promise<never>(() => {}),
		abort: async () => ({ ok: true }),
		steer: async () => ({ ok: true, value: {} }),
	};
}

type Change = { check: string; required: boolean; baseline: string; candidate: string };
type Attempt = {
	id: string;
	gate_digest: string;
	decision: "passed" | "blocked";
	reasons: string[];
	regressions: Change[];
	improvements: Change[];
	candidate: { results: Record<string, string> };
};

const hosts: NativeRlmHost[] = [];

afterEach(async () => {
	for (const host of hosts.splice(0)) await host.close();
});

function setup(store: HostModuleStore = createMemoryModuleStore()) {
	const lane = waitingLane();
	const host = new NativeRlmHost({ lane: async () => lane } as never, lane as never, {
		store: memoryStore(),
		definitionStore: memoryStore(),
		deterministic: async ({ input }) => input,
		modules: [createReleaseGateModule({ store, now: () => 5_000 })],
	});
	hosts.push(host);
	return <T = unknown>(type: string, payload: Record<string, unknown>) =>
		host.handle(type, payload, context) as Promise<T>;
}

const gate = {
	id: "release-core",
	checks: [
		{ name: "unit", required: true },
		{ name: "typecheck", required: true },
		{ name: "perf", required: false },
	],
	fixture_hash: FIXTURE,
};

function run(variant: string, results: Record<string, string>, fixture_hash = FIXTURE) {
	return { variant, fixture_hash, results };
}

const allPass = { unit: "passed", typecheck: "passed", perf: "passed" };

async function defined(store?: HostModuleStore) {
	const call = setup(store);
	await call("gates.define", gate);
	return call;
}

describe("release gate module (A15)", () => {
	test("passes when every required check passes in the candidate", async () => {
		const call = await defined();
		const attempt = await call<Attempt>("gates.compare", {
			gate_id: "release-core",
			baseline: run("main@abc", { unit: "passed", typecheck: "failed", perf: "passed" }),
			candidate: run("branch@def", allPass),
		});
		expect(attempt.decision).toBe("passed");
		expect(attempt.regressions).toEqual([]);
		expect(attempt.improvements).toEqual([
			{ check: "typecheck", required: true, baseline: "failed", candidate: "passed" },
		]);
	});

	test("a required regression blocks", async () => {
		const call = await defined();
		const attempt = await call<Attempt>("gates.compare", {
			gate_id: "release-core",
			baseline: run("main", allPass),
			candidate: run("branch", { ...allPass, unit: "failed" }),
		});
		expect(attempt.decision).toBe("blocked");
		expect(attempt.regressions).toEqual([{ check: "unit", required: true, baseline: "passed", candidate: "failed" }]);
		expect(attempt.reasons.join("\n")).toContain("Required check unit regressed");
	});

	test("an optional regression is reported but does not block", async () => {
		const call = await defined();
		const attempt = await call<Attempt>("gates.compare", {
			gate_id: "release-core",
			baseline: run("main", allPass),
			candidate: run("branch", { ...allPass, perf: "failed" }),
		});
		expect(attempt.decision).toBe("passed");
		expect(attempt.regressions).toEqual([
			{ check: "perf", required: false, baseline: "passed", candidate: "failed" },
		]);
		expect(attempt.reasons.join("\n")).toContain("not blocking");
	});

	test("a required check missing or failed in the candidate blocks even without a baseline pass", async () => {
		const call = await defined();
		const omitted = await call<Attempt>("gates.compare", {
			gate_id: "release-core",
			baseline: run("main", { unit: "failed", typecheck: "missing" }),
			candidate: run("branch", { unit: "passed" }),
		});
		expect(omitted.decision).toBe("blocked");
		expect(omitted.candidate.results).toEqual({ unit: "passed", typecheck: "missing", perf: "missing" });
		expect(omitted.reasons).toEqual(["Required check typecheck is missing in candidate"]);
		const explicit = await call<Attempt>("gates.compare", {
			gate_id: "release-core",
			baseline: run("main", allPass),
			candidate: run("branch", { ...allPass, typecheck: "missing" }),
		});
		expect(explicit.decision).toBe("blocked");
	});

	test("a gate definition is frozen once defined", async () => {
		const call = await defined();
		// Identical content, in any check order, is idempotent.
		const again = await call<{ digest: string }>("gates.define", { ...gate, checks: [...gate.checks].reverse() });
		const original = await call<{ gate: { digest: string } }>("gates.history", { gate_id: "release-core" });
		expect(again.digest).toBe(original.gate.digest);
		await expect(
			call("gates.define", {
				...gate,
				checks: gate.checks.map((check) => (check.name === "unit" ? { ...check, required: false } : check)),
			}),
		).rejects.toThrow("cannot be changed");
		await expect(call("gates.define", { ...gate, fixture_hash: OTHER_FIXTURE })).rejects.toThrow("cannot be changed");
		// A run cannot introduce checks the gate does not define.
		await expect(
			call("gates.compare", {
				gate_id: "release-core",
				baseline: run("main", allPass),
				candidate: run("branch", { ...allPass, bonus: "passed" }),
			}),
		).rejects.toThrow("does not define: bonus");
		await expect(
			call("gates.define", { ...gate, id: "optional-only", checks: [{ name: "a", required: false }] }),
		).rejects.toThrow("at least one required check");
	});

	test("a fixture mismatch blocks with a reason", async () => {
		const call = await defined();
		const attempt = await call<Attempt>("gates.compare", {
			gate_id: "release-core",
			baseline: run("main", allPass),
			candidate: run("branch", allPass, OTHER_FIXTURE),
		});
		expect(attempt.decision).toBe("blocked");
		expect(attempt.reasons).toEqual([expect.stringContaining("Fixture mismatch: candidate branch")]);
	});

	test("every attempt, blocked or passed, is recorded in history", async () => {
		const call = await defined();
		const blocked = await call<Attempt>("gates.compare", {
			gate_id: "release-core",
			baseline: run("main", allPass),
			candidate: run("try-1", { ...allPass, unit: "failed" }),
		});
		const passed = await call<Attempt>("gates.compare", {
			gate_id: "release-core",
			baseline: run("main", allPass),
			candidate: run("try-2", allPass),
		});
		const history = await call<{ gate: { digest: string }; attempts: Attempt[] }>("gates.history", {
			gate_id: "release-core",
		});
		expect(history.attempts.map((attempt) => [attempt.id, attempt.decision])).toEqual([
			[blocked.id, "blocked"],
			[passed.id, "passed"],
		]);
		expect(history.attempts.every((attempt) => attempt.gate_digest === history.gate.digest)).toBe(true);
	});

	test("rejects unknown fields and unknown gates", async () => {
		const call = await defined();
		await expect(call("gates.define", { ...gate, id: "g2", strict: true })).rejects.toThrow(
			"Unknown payload field: strict",
		);
		await expect(
			call("gates.define", { ...gate, id: "g3", checks: [{ name: "unit", required: true, weight: 2 }] }),
		).rejects.toThrow("Unknown payload field: weight");
		await expect(
			call("gates.compare", {
				gate_id: "release-core",
				baseline: run("main", allPass),
				candidate: run("branch", allPass),
				override: "passed",
			}),
		).rejects.toThrow("Unknown payload field: override");
		await expect(
			call("gates.compare", {
				gate_id: "release-core",
				baseline: { ...run("main", allPass), notes: "x" },
				candidate: run("branch", allPass),
			}),
		).rejects.toThrow("Unknown field baseline.notes");
		await expect(
			call("gates.compare", {
				gate_id: "release-core",
				baseline: run("main", allPass),
				candidate: run("branch", { ...allPass, unit: "skipped" }),
			}),
		).rejects.toThrow("passed, failed, or missing");
		await expect(call("gates.history", { gate_id: "nope" })).rejects.toThrow("Unknown release gate");
	});

	test("gates and attempts survive a new module instance, and tampered definitions are refused", async () => {
		const store = createMemoryModuleStore();
		const first = await defined(store);
		const attempt = await first<Attempt>("gates.compare", {
			gate_id: "release-core",
			baseline: run("main", allPass),
			candidate: run("branch", allPass),
		});
		const second = setup(store);
		const history = await second<{ attempts: Attempt[] }>("gates.history", { gate_id: "release-core" });
		expect(history.attempts.map((entry) => entry.id)).toEqual([attempt.id]);
		await expect(second("gates.define", { ...gate, fixture_hash: OTHER_FIXTURE })).rejects.toThrow(
			"cannot be changed",
		);

		const saved = (await store.read()) as { gates: Array<{ checks: Array<{ required: boolean }> }> };
		for (const check of saved.gates[0].checks) check.required = false;
		await store.write(saved as never);
		const third = setup(store);
		await expect(third("gates.list", {})).rejects.toThrow("does not match its digest");
	});
});
