import type { Context } from "@ultron/agent-core";
import type { JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { describe, expect, test } from "vitest";
import { createFamilyModule } from "../src/ultron/family.ts";
import {
	assertSessionFormatsReadable,
	readVersioned,
	UnsupportedFormatVersionError,
} from "../src/ultron/format-version.ts";
import { createGrantModule } from "../src/ultron/grants.ts";
import { createInstanceModule } from "../src/ultron/instances.ts";
import { JevDecisionLog } from "../src/ultron/jev-decisions.ts";
import { NativeLocalServices } from "../src/ultron/local-services.ts";
import { NativeMemoryService } from "../src/ultron/memory.ts";
import { createProgressModule } from "../src/ultron/progress.ts";
import { createReleaseGateModule } from "../src/ultron/release-gate.ts";
import { NativeDefinitionRegistry } from "../src/ultron/rlm/definition-registry.ts";
import type { NativeHostApi, NativeHostModule } from "../src/ultron/rlm/host-module.ts";
import { NativeTaskJournal } from "../src/ultron/rlm/task-store.ts";
import { createScheduleModule } from "../src/ultron/schedules.ts";
import { createSkillModule } from "../src/ultron/skills.ts";
import { NativeUsageLedger } from "../src/ultron/usage.ts";

const context = BACKGROUND_CONTEXT as Context;

/** A document store that counts writes, so a test can prove a newer document was never overwritten. */
function store(initial: JsonValue) {
	const state = { value: structuredClone(initial) as JsonValue | undefined, writes: 0 };
	return {
		state,
		read: async () => structuredClone(state.value),
		write: async (next: JsonValue) => {
			state.writes += 1;
			state.value = structuredClone(next);
		},
	};
}

const host = {
	callerTaskId: () => null,
	taskLane: () => null,
	tasks: async () => [],
	now: () => 0,
	usage: async () => null,
	pinLane: () => false,
	unpinLane: () => {},
} as unknown as NativeHostApi;

const memoryBackend = {
	namespace: "fake://bank",
	scopeTags: { session: ["ultron:session:s1"], project: ["ultron:project:p1"], global: ["ultron:global:g1"] },
};

type StoreCase = {
	namespace: string;
	field?: string;
	/** A valid document of the current shape, without its version field. */
	legacy: Record<string, JsonValue>;
	/** Opens the owner over the store and makes it load. */
	open: (backing: ReturnType<typeof store>) => Promise<unknown>;
};

const startModule = (module: NativeHostModule) => Promise.resolve(module.start?.(host));
const localServices = (backing: ReturnType<typeof store>, key: string) =>
	new NativeLocalServices({
		get: async (candidate) => (candidate === key ? backing.read() : undefined),
		set: async (candidate, next) => {
			if (candidate === key) await backing.write(next);
		},
		list: async () => [],
	});

const cases: StoreCase[] = [
	{
		namespace: "ultron.tasks/root",
		legacy: { tasks: [] },
		open: (backing) => new NativeTaskJournal(backing).list(),
	},
	{
		namespace: "ultron.definitions/root",
		legacy: { definitions: [] },
		open: (backing) => new NativeDefinitionRegistry(backing).ready(),
	},
	{
		namespace: "ultron.usage/root",
		legacy: { roots: {} },
		open: (backing) => new NativeUsageLedger(backing).ready(),
	},
	{
		namespace: "ultron.memory.state/root",
		legacy: { namespace: "fake://bank", operations: [] },
		open: (backing) =>
			new NativeMemoryService({
				store: backing,
				backend: memoryBackend,
				gate: async () => ({ retrieve: false }),
			}).list(),
	},
	{
		namespace: "ultron.jev.decisions/root",
		legacy: { decisions: [] },
		open: (backing) => new JevDecisionLog(backing).list(),
	},
	{
		namespace: "ultron.module/family",
		legacy: { messages: [] },
		open: (backing) =>
			createFamilyModule({ store: backing }).handle(
				{ type: "agent_message.list", payload: {}, caller: { lane: "main" }, context } as never,
				host,
			),
	},
	{
		namespace: "ultron.module/instances",
		legacy: { instances: [] },
		open: (backing) => startModule(createInstanceModule({ store: backing })),
	},
	{
		namespace: "ultron.module/release-gates",
		legacy: { gates: [], attempts: [] },
		open: (backing) => startModule(createReleaseGateModule({ store: backing })),
	},
	{
		namespace: "ultron.module/grants",
		legacy: { grants: [] },
		open: (backing) => startModule(createGrantModule({ store: backing })),
	},
	{
		namespace: "ultron.module/progress",
		legacy: { receipts: [], decisions: [] },
		open: (backing) => startModule(createProgressModule({ store: backing })),
	},
	{
		namespace: "ultron.module/schedules",
		legacy: { schedules: [], goals: [] },
		open: (backing) => startModule(createScheduleModule({ store: backing, tickIntervalMs: 0 })),
	},
	{
		namespace: "ultron.module/skills",
		field: "format",
		legacy: { generation: 0, catalog: [], changes: [], decisions: [], loads: [], invocations: [] },
		open: (backing) => startModule(createSkillModule({ store: backing, loadSkills: async () => [] })),
	},
	{
		namespace: "ultron.local/ultron.refinements",
		field: "formatVersion",
		legacy: { events: [] },
		open: (backing) => localServices(backing, "ultron.refinements").handle("refinements.list", {}, context),
	},
	{
		namespace: "ultron.local/ultron.experiments",
		field: "formatVersion",
		legacy: { runs: [] },
		open: (backing) => localServices(backing, "ultron.experiments").handle("experiments.list", {}, context),
	},
	{
		namespace: "ultron.local/ultron.artifact-refs",
		field: "formatVersion",
		legacy: { refs: {} },
		open: (backing) => localServices(backing, "ultron.artifact-refs").handle("artifacts.usage", {}, context),
	},
];

describe("versioned session values", () => {
	for (const item of cases) {
		const field = item.field ?? "version";

		test(`${item.namespace}: a newer format fails on load, naming the namespace and version, and is not rewritten`, async () => {
			const backing = store({ ...item.legacy, [field]: 2 });
			const failure = await item.open(backing).then(
				() => undefined,
				(error: unknown) => error,
			);
			expect(failure).toBeInstanceOf(UnsupportedFormatVersionError);
			expect((failure as UnsupportedFormatVersionError).namespace).toBe(item.namespace);
			expect((failure as UnsupportedFormatVersionError).version).toBe(2);
			expect((failure as Error).message).toContain(`${item.namespace} has format version 2`);
			expect(backing.state.writes).toBe(0);
			expect(backing.state.value).toEqual({ ...item.legacy, [field]: 2 });
		});

		test(`${item.namespace}: a document without a version is read as version 1`, async () => {
			await expect(item.open(store(item.legacy))).resolves.not.toThrow();
		});

		test(`${item.namespace}: the current version reads normally`, async () => {
			await expect(item.open(store({ ...item.legacy, [field]: 1 }))).resolves.not.toThrow();
		});
	}

	test("an artifact written in a newer format fails when read", async () => {
		const id = "a".repeat(64);
		const services = new NativeLocalServices({
			get: async (key) =>
				key === `ultron.artifacts/${id}`
					? { formatVersion: 3, id, bytes: 1, mediaType: "text/plain", label: "", text: "x" }
					: undefined,
			set: async () => {},
			list: async () => [],
		});
		await expect(services.handle("artifacts.read", { id }, context)).rejects.toThrow(
			`ultron.local/ultron.artifacts/${id} has format version 3`,
		);
	});

	test("readVersioned leaves non-object values to the caller's own validation", () => {
		expect(readVersioned("x", undefined)).toBeUndefined();
		expect(readVersioned("x", "text")).toBe("text");
		expect(readVersioned("x", [1])).toEqual([1]);
		expect(() => readVersioned("x", { version: "1" })).toThrow('x has format version "1"');
	});
});

describe("resume check", () => {
	function fakeSession(values: Record<string, JsonValue>) {
		const entry = (key: string) => {
			const [namespace, ...rest] = key.split("|");
			return { address: { namespace, key: rest.join("|"), kind: "value" as const }, value: values[key], seq: 1 };
		};
		return {
			getValue: async (address: { namespace: string; key: string }) =>
				Object.hasOwn(values, `${address.namespace}|${address.key}`)
					? entry(`${address.namespace}|${address.key}`)
					: undefined,
			scanValues: async (prefix: { namespace: string; key: string }) =>
				Object.keys(values)
					.filter((key) => key.startsWith(`${prefix.namespace}|${prefix.key}`))
					.map(entry),
		} as never;
	}

	test("a session with current and unversioned values resumes", async () => {
		await expect(
			assertSessionFormatsReadable(
				fakeSession({
					"ultron.tasks|root": { version: 1, tasks: [] },
					"ultron.usage|root": { roots: {} },
					"ultron.module|grants": { version: 1, grants: [] },
					"ultron.module|skills": { format: 1 },
					"ultron.local|ultron.refinements": { formatVersion: 1, events: [] },
				}),
				context,
			),
		).resolves.toBeUndefined();
	});

	test.each([
		["ultron.tasks|root", { version: 2, tasks: [] }, "ultron.tasks/root has format version 2"],
		["ultron.memory.state|root", { version: 7 }, "ultron.memory.state/root has format version 7"],
		["ultron.module|progress", { version: 2 }, "ultron.module/progress has format version 2"],
		["ultron.module|skills", { format: 2 }, "ultron.module/skills has format version 2"],
		["ultron.local|ultron.experiments", { formatVersion: 2 }, "ultron.local/ultron.experiments has format version 2"],
	])("%s in a newer format stops the resume", async (key, document, message) => {
		await expect(assertSessionFormatsReadable(fakeSession({ [key]: document }), context)).rejects.toThrow(message);
	});
});
