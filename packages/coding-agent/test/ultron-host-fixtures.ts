import type { JsonValue } from "@earendil-works/chord";
import type { Context } from "@earendil-works/pi-agent-core";
import type { NativeDefinitionAdapter, NativeDefinitionStore } from "../src/ultron/rlm/definition-registry.ts";
import type { NativeHostModule } from "../src/ultron/rlm/host-module.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";
import type { NativeUsageLedgerLike } from "../src/ultron/usage.ts";

/** Shared fake-lane fixtures for the host contract acceptance suites. */

export const context = {} as never;

export function memoryStore(): NativeHostStore & { document(): unknown } {
	let value: JsonValue | undefined;
	return {
		read: async () => structuredClone(value),
		write: async (next) => {
			value = structuredClone(next);
		},
		document: () => structuredClone(value),
	};
}

export function memoryDefinitionStore(): NativeDefinitionStore {
	let value: JsonValue | undefined;
	return {
		read: async () => structuredClone(value),
		write: async (next) => {
			value = structuredClone(next);
		},
	};
}

export type Deferred<T> = { promise: Promise<T>; resolve: (value: T) => void; reject: (error: unknown) => void };

export function deferred<T = void>(): Deferred<T> {
	let resolve!: (value: T) => void;
	let reject!: (error: unknown) => void;
	const promise = new Promise<T>((onResolve, onReject) => {
		resolve = onResolve;
		reject = onReject;
	});
	return { promise, resolve, reject };
}

/** Resolves when the context's abort signal fires; never resolves otherwise. */
export function aborted(context: Context): Promise<never> {
	return new Promise((_resolve, reject) => {
		const signal = context.abortSignal;
		if (!signal) return;
		if (signal.aborted) reject(signal.reason);
		signal.addEventListener("abort", () => reject(signal.reason), { once: true });
	});
}

/**
 * Lane script: receives the lane name, the prompt text, and the task context the host passed to
 * `lane.prompt` (it carries the task's abort signal). Returns the assistant text at the completed tip.
 */
export type LaneScript = (lane: string, prompt: string, context: Context) => Promise<string> | string;

export function scriptedHarness(script: LaneScript) {
	const laneCalls: string[] = [];
	const prompts: Array<{ lane: string; prompt: string }> = [];
	const aborts: string[] = [];
	const lanes = new Map<string, object>();
	const harness = {
		lane: async (name: string) => {
			laneCalls.push(name);
			let lane = lanes.get(name);
			if (!lane) {
				let turn = 0;
				let text = "";
				lane = {
					getActiveTools: async () => [],
					setModel: async () => {},
					steer: async () => ({ ok: true, value: {} }),
					abort: async () => {
						aborts.push(name);
						return { ok: true };
					},
					prompt: async (prompt: string, _options: unknown, laneContext: Context) => {
						prompts.push({ lane: name, prompt });
						const reply = await script(name, prompt, laneContext);
						turn += 1;
						text = reply;
						return { ok: true, value: { status: "completed", tipId: `${name}#${turn}` } };
					},
					findEntries: async () => [
						{
							id: `${name}#${turn}`,
							type: "message",
							message: { role: "assistant", content: [{ type: "text", text }] },
						},
					],
				};
				lanes.set(name, lane);
			}
			return lane;
		},
	};
	return { harness, laneCalls, prompts, aborts };
}

export type HostFixtureOptions = {
	script?: LaneScript;
	store?: NativeHostStore;
	usage?: NativeUsageLedgerLike;
	deterministic?: NativeDefinitionAdapter;
	predict?: NativeDefinitionAdapter;
	modules?: readonly NativeHostModule[];
	definitionStore?: NativeDefinitionStore;
	rootTurns?: boolean;
	now?: () => number;
};

export function hostFixture(options: HostFixtureOptions = {}) {
	const fake = scriptedHarness(options.script ?? (() => "unused"));
	const store = options.store ?? memoryStore();
	const host = new NativeRlmHost(fake.harness as never, {} as never, {
		store,
		definitionStore: options.definitionStore ?? memoryDefinitionStore(),
		usage: options.usage,
		deterministic: options.deterministic,
		predict: options.predict,
		modules: options.modules,
		...(options.rootTurns === undefined ? {} : { rootTurns: options.rootTurns }),
		...(options.now === undefined ? {} : { now: options.now }),
	});
	return {
		host,
		store,
		...fake,
		call: <T = unknown>(type: string, payload: Record<string, unknown> = {}, lane?: string): Promise<T> =>
			host.handle(type, payload, context, lane === undefined ? undefined : { lane }) as Promise<T>,
	};
}

export type TaskRecordView = {
	id: string;
	key: string;
	fingerprint: string;
	definition: string;
	state: string;
	parentId?: string;
	result?: { status: string; value?: unknown; error?: string; verification: string };
};

/** Durable journal contents, read the same way a reopened owner would. */
export async function journal(fixture: ReturnType<typeof hostFixture>): Promise<TaskRecordView[]> {
	await fixture.host.handle("ping", {}, context);
	return (await fixture.host.api.tasks()) as TaskRecordView[];
}

export function definition(
	id: string,
	strategy: "deterministic" | "predict" | "rlm",
	overrides: Record<string, unknown> = {},
): Record<string, unknown> {
	return {
		id,
		version: "1",
		strategy,
		instructions: `Run ${id}.`,
		inputSchema: {
			type: "object",
			properties: { n: { type: "integer" } },
			required: ["n"],
			additionalProperties: false,
		},
		outputSchema: {
			type: "object",
			properties: { doubled: { type: "integer" } },
			required: ["doubled"],
			additionalProperties: false,
		},
		maxRepairs: 0,
		inputDescription: "{n:integer}",
		outputDescription: "{doubled:integer}",
		...overrides,
	};
}

export async function waitFor(condition: () => boolean | Promise<boolean>, timeoutMs = 2_000): Promise<void> {
	const started = Date.now();
	while (!(await condition())) {
		if (Date.now() - started > timeoutMs) throw new Error("waitFor timed out");
		await new Promise((resolve) => setTimeout(resolve, 2));
	}
}
