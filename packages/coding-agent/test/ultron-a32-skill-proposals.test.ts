import type { Context, JsonValue } from "@earendil-works/chord";
import { describe, expect, test } from "vitest";
import {
	type DurableDocumentStorage,
	NativeLocalServices,
	type NativeLocalServicesOptions,
	type RefinementRecord,
} from "../src/ultron/local-services.ts";

const context = {} as Context;

function documents(): DurableDocumentStorage {
	const values = new Map<string, JsonValue>();
	return {
		get: async (key) => structuredClone(values.get(key)),
		set: async (key, value) => {
			values.set(key, structuredClone(value));
		},
		list: async (prefix) =>
			[...values].filter(([key]) => key.startsWith(prefix)).map(([key, value]) => ({ key, value })),
	};
}

/** Skill schema check wired as the activation validator: a proposal must be a complete procedure. */
const validateSkill: NativeLocalServicesOptions["validate"] = (kind, content) => {
	if (kind !== "skill") return true;
	if (content === null || typeof content !== "object" || Array.isArray(content)) return false;
	return ["problem", "procedure", "input", "output", "tests"].every(
		(key) => typeof content[key] === "string" || Array.isArray(content[key]),
	);
};

const procedure = {
	problem: "Flaky vitest runs are misreported as regressions.",
	procedure: "Re-run the failing file three times in isolation before bisecting.",
	input: "failing test file path",
	output: "flaky | deterministic verdict with run logs",
	tests: ["test/flaky-triage.test.ts"],
};
// The repeated-procedure evidence: several independent runs where the same procedure succeeded.
const repeatedRuns = [
	{ run: "run-101", outcome: "passed", procedure: "rerun-isolated" },
	{ run: "run-117", outcome: "passed", procedure: "rerun-isolated" },
	{ run: "run-130", outcome: "passed", procedure: "rerun-isolated" },
];

function services(store: DurableDocumentStorage, options: NativeLocalServicesOptions = {}) {
	const host = new NativeLocalServices(store, { validate: validateSkill, ...options });
	const call = <T = RefinementRecord>(type: string, payload: Record<string, JsonValue>) =>
		host.handle(type, payload, context) as Promise<T>;
	return { host, call };
}

const skill = (overrides: Record<string, JsonValue> = {}) => ({
	kind: "skill",
	target: "skill:flaky-test-triage",
	baseVersion: 0,
	content: procedure,
	evidence: repeatedRuns,
	scope: "project",
	...overrides,
});

describe("A32 skill proposals require evidence, validation, and applicable approval; rejection/rollback holds", () => {
	test("a repeated-procedure proposal activates only with evidence and records that approval is disabled", async () => {
		const store = documents();
		const { call } = services(store);
		const unsupported = await call("refinements.propose", skill({ evidence: [] }));
		await expect(call("refinements.activate", { id: unsupported.id })).rejects.toThrow("nonempty evidence");
		await call("refinements.reject", { id: unsupported.id });

		const proposed = await call("refinements.propose", skill());
		expect(proposed).toMatchObject({ state: "proposed", approval: null, evidence: repeatedRuns });
		const active = await call("refinements.activate", { id: proposed.id });
		// Approval is off by default (no-friction amendment); the record says so explicitly.
		expect(active).toMatchObject({ state: "active", version: 1, approval: "not_required" });

		const reopened = services(store).call;
		await expect(
			reopened("refinements.current", { kind: "skill", target: "skill:flaky-test-triage" }),
		).resolves.toMatchObject({ id: proposed.id, approval: "not_required", version: 1 });
	});

	test("an equivalent pending or active proposal is a duplicate; a real change is not", async () => {
		const { call } = services(documents());
		const first = await call("refinements.propose", skill());
		await expect(call("refinements.propose", skill())).rejects.toThrow(`Duplicate refinement proposal: ${first.id}`);
		// Key order and fresh evidence do not make the same procedure new while it is pending.
		const reordered = Object.fromEntries(Object.entries(procedure).reverse());
		await expect(
			call("refinements.propose", skill({ content: reordered, evidence: [{ run: "run-200" }] })),
		).rejects.toThrow("Duplicate refinement proposal");
		await call("refinements.activate", { id: first.id });
		await expect(call("refinements.propose", skill())).rejects.toThrow("(active)");

		// A changed procedure against the active version, and the same one in another scope, are not duplicates.
		const revised = await call(
			"refinements.propose",
			skill({ baseVersion: 1, content: { ...procedure, procedure: "Re-run five times." } }),
		);
		expect(revised.state).toBe("proposed");
		await expect(call("refinements.propose", skill({ scope: "session" }))).resolves.toMatchObject({
			state: "proposed",
		});
	});

	test("invalid skill content fails validation, and enabled approval cannot override it", async () => {
		const store = documents();
		const { call } = services(store, { requireApproval: true });
		const invalid = await call("refinements.propose", skill({ content: { problem: "only a problem statement" } }));
		await expect(call("refinements.activate", { id: invalid.id })).rejects.toThrow("Activation requires approval");
		await expect(call("refinements.approve", { id: invalid.id })).resolves.toMatchObject({ approval: "approved" });
		await expect(call("refinements.activate", { id: invalid.id })).rejects.toThrow("validation failed");
		await expect(call("refinements.get", { id: invalid.id })).resolves.toMatchObject({
			state: "proposed",
			version: null,
		});

		const valid = await call("refinements.propose", skill());
		await expect(call("refinements.activate", { id: valid.id })).rejects.toThrow("Activation requires approval");
		await call("refinements.approve", { id: valid.id });
		await expect(call("refinements.activate", { id: valid.id })).resolves.toMatchObject({
			state: "active",
			approval: "approved",
		});
		// Approval is only for proposed records and never re-opens a decided one.
		await expect(call("refinements.approve", { id: valid.id })).rejects.toThrow("Only a proposed");
		// The approval is durable: a restart with the control on still sees it.
		await expect(
			services(store, { requireApproval: true }).call("refinements.get", { id: valid.id }),
		).resolves.toMatchObject({ approval: "approved" });
	});

	test("rejection and rollback hold across restart; a declined proposal needs new evidence to resurface", async () => {
		const store = documents();
		let { call } = services(store);
		const declined = await call("refinements.propose", skill({ target: "skill:auto-rebase" }));
		await call("refinements.reject", { id: declined.id });

		const v1 = await call("refinements.propose", skill());
		await call("refinements.activate", { id: v1.id });
		const v2 = await call(
			"refinements.propose",
			skill({ baseVersion: 1, content: { ...procedure, procedure: "Re-run five times." } }),
		);
		await call("refinements.activate", { id: v2.id });
		await call("refinements.rollback", { id: v2.id });

		call = services(store).call; // restart
		await expect(call("refinements.get", { id: declined.id })).resolves.toMatchObject({ state: "rejected" });
		await expect(call("refinements.activate", { id: declined.id })).rejects.toThrow("Only a proposed");
		await expect(call("refinements.propose", skill({ target: "skill:auto-rebase" }))).rejects.toThrow("(rejected)");
		// New supporting evidence may resurface it as a new, separately tracked proposal.
		const resurfaced = await call(
			"refinements.propose",
			skill({ target: "skill:auto-rebase", evidence: [...repeatedRuns, { run: "run-150", outcome: "passed" }] }),
		);
		expect(resurfaced.id).not.toBe(declined.id);

		const current = await call("refinements.current", { kind: "skill", target: "skill:flaky-test-triage" });
		expect(current).toMatchObject({ id: v1.id, version: 1, state: "active" });
		const rolledBack = await call("refinements.get", { id: v2.id });
		expect(rolledBack).toMatchObject({ state: "rolled_back", version: 2 });
		expect(rolledBack.history.map((entry) => entry.state)).toEqual(["proposed", "active", "rolled_back"]);
		expect((await call("refinements.get", { id: v1.id })).history.map((entry) => entry.state)).toEqual([
			"proposed",
			"active",
			"superseded",
			"active",
		]);
		await expect(call("refinements.activate", { id: v2.id })).rejects.toThrow("Only a proposed");
		await expect(call("refinements.rollback", { id: v2.id })).rejects.toThrow("latest active");
	});
});
