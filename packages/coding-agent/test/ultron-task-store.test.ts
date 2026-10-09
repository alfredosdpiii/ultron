import { isJsonValue, type JsonValue } from "@ultron/chord";
import { describe, expect, test } from "vitest";
import {
	MAX_PERSISTED_FINISHED_TASKS,
	type NativeHostStore,
	NativeTaskJournal,
	persistedTasks,
	taskFingerprint,
} from "../src/ultron/rlm/task-store.ts";

const fingerprint = "a".repeat(64);
const interruptedResult = {
	status: "interrupted",
	error: "Owner ended; automatic replay is disabled",
	verification: "unverified",
} as const;

function memoryStore(initial?: unknown) {
	let value = initial;
	const backend: NativeHostStore = {
		read: async () => value as JsonValue | undefined,
		write: async (document) => {
			value = structuredClone(document);
		},
	};
	return { backend, value: () => value };
}

function deferred<T>() {
	let resolve!: (value: T | PromiseLike<T>) => void;
	let reject!: (reason?: unknown) => void;
	const promise = new Promise<T>((resolvePromise, rejectPromise) => {
		resolve = resolvePromise;
		reject = rejectPromise;
	});
	return { promise, resolve, reject };
}

function validTask(overrides: Record<string, unknown> = {}) {
	return {
		id: "task-1",
		key: "request-1",
		fingerprint,
		definition: "identity@1",
		state: "completed",
		result: { status: "succeeded", value: 42, verification: "unverified" },
		...overrides,
	};
}

function validDocument(tasks: unknown[] = [validTask()]) {
	return { version: 1, tasks };
}

describe("taskFingerprint", () => {
	test("is stable for strict JSON object key order", () => {
		expect(taskFingerprint({ b: 2, a: [true, null] })).toBe(taskFingerprint({ a: [true, null], b: 2 }));
	});

	test.each([
		["undefined", { value: undefined }],
		["nonfinite number", { value: Number.NaN }],
		["Date", new Date(0)],
		[
			"class instance",
			new (class Value {
				value = 1;
			})(),
		],
	])("rejects %s instead of hashing it", (_name, value) => {
		expect(() => taskFingerprint(value as never)).toThrow("strict JSON");
	});

	test("rejects cycles", () => {
		const value: Record<string, unknown> = {};
		value.self = value;
		expect(isJsonValue(value)).toBe(false);
		expect(() => taskFingerprint(value as never)).toThrow("strict JSON");
	});
});

describe("NativeTaskJournal", () => {
	test("the persisted document keeps every unfinished task and only the newest finished ones; memory keeps them all", async () => {
		const writes: Array<{ tasks: Array<{ id: string; state: string }> }> = [];
		const backend: NativeHostStore = {
			read: async () => undefined,
			write: async (document) => void writes.push(document as never),
		};
		const journal = new NativeTaskJournal(backend);
		const total = MAX_PERSISTED_FINISHED_TASKS + 30;
		const ids: string[] = [];
		for (let n = 0; n < total; n += 1) {
			const { task } = await journal.admit("identity@1", fingerprint, `request-${n}`);
			ids.push(task.id);
			await journal.transition(task.id, "running");
			// The last one stays running: it must be persisted whatever its age.
			if (n < total - 1)
				await journal.transition(task.id, "completed", { status: "succeeded", verification: "unverified" });
		}
		const last = writes.at(-1)!.tasks;
		expect(last).toHaveLength(MAX_PERSISTED_FINISHED_TASKS + 1);
		expect(last.filter((task) => task.state === "completed")).toHaveLength(MAX_PERSISTED_FINISHED_TASKS);
		expect(last.at(-1)).toMatchObject({ id: ids[total - 1], state: "running" });
		// The oldest finished ones are the dropped ones; the journal itself still lists every task.
		expect(last.map((task) => task.id)).toEqual(ids.slice(total - 1 - MAX_PERSISTED_FINISHED_TASKS));
		expect(await journal.list()).toHaveLength(total);
		// Below the bound nothing is dropped, and the helper is a pure function of the records.
		const few = [
			{ id: "a", key: "a", fingerprint, definition: "identity@1", state: "completed" as const },
			{ id: "b", key: "b", fingerprint, definition: "identity@1", state: "running" as const },
		];
		expect(persistedTasks(few)).toEqual(few);
	});

	test("shares one concurrent load across ready, reads, and writes", async () => {
		let readCount = 0;
		const readGate = deferred<JsonValue | undefined>();
		const backend: NativeHostStore = {
			read: async () => {
				readCount += 1;
				return readGate.promise;
			},
			write: async () => {},
		};
		const journal = new NativeTaskJournal(backend);
		const ready = journal.ready();
		const listed = journal.list();
		const admitted = journal.admit("identity@1", fingerprint, "request-1");
		await Promise.resolve();
		expect(readCount).toBe(1);
		readGate.resolve(undefined);
		await expect(ready).resolves.toBeUndefined();
		await expect(listed).resolves.toHaveLength(0);
		await expect(admitted).resolves.toMatchObject({ created: true, task: { state: "admitted" } });
		expect(readCount).toBe(1);
	});

	test("publishes a task only after its document write commits", async () => {
		const writeStarted = deferred<void>();
		const releaseWrite = deferred<void>();
		let value: unknown;
		const backend: NativeHostStore = {
			read: async () => value as JsonValue | undefined,
			write: async (document) => {
				writeStarted.resolve();
				await releaseWrite.promise;
				value = structuredClone(document);
			},
		};
		const journal = new NativeTaskJournal(backend);
		const admitted = journal.admit("identity@1", fingerprint, "request-1");
		await writeStarted.promise;
		let listed = false;
		const list = journal.list().then((tasks) => {
			listed = true;
			return tasks;
		});
		await Promise.resolve();
		expect(listed).toBe(false);
		releaseWrite.resolve();
		const result = await admitted;
		expect(result.created).toBe(true);
		expect(await list).toHaveLength(1);
	});

	test("poisons writes and reads already queued behind a failed write", async () => {
		const writeStarted = deferred<void>();
		const writeDecision = deferred<void>();
		let failWrites = false;
		let value: unknown;
		const backend: NativeHostStore = {
			read: async () => value as JsonValue | undefined,
			write: async (document) => {
				writeStarted.resolve();
				await writeDecision.promise;
				if (failWrites) throw new Error("backend failed");
				value = structuredClone(document);
			},
		};
		const journal = new NativeTaskJournal(backend);
		const first = journal.admit("identity@1", fingerprint, "request-1");
		await writeStarted.promise;
		const queuedAdmission = journal.admit("identity@1", fingerprint, "request-2");
		const queuedRead = journal.list();
		const queuedReady = journal.ready();
		failWrites = true;
		writeDecision.resolve();
		await expect(first).rejects.toThrow("durability is uncertain");
		await expect(queuedAdmission).rejects.toThrow("durability is uncertain");
		await expect(queuedRead).rejects.toThrow("durability is uncertain");
		await expect(queuedReady).rejects.toThrow("durability is uncertain");
	});

	test("uses one durable document and detects idempotency conflicts", async () => {
		const backend = memoryStore();
		const journal = new NativeTaskJournal(backend.backend);
		const first = await journal.admit("identity@1", fingerprint, "request-1");
		const duplicate = await journal.admit("identity@1", fingerprint, "request-1");
		expect(duplicate).toMatchObject({ created: false, task: { id: first.task.id } });
		await expect(
			journal.admit("identity@1", taskFingerprint({ definition: "identity@1", input: { answer: 7 } }), "request-1"),
		).rejects.toThrow("Idempotency key reused");
		expect(await journal.list()).toHaveLength(1);
	});

	test.each([
		["definition", "bad definition", fingerprint, "request-1"],
		["fingerprint", "identity@1", "not-a-fingerprint", "request-1"],
		["key", "identity@1", fingerprint, ""],
	])("rejects an invalid admission %s without coercion", async (_name, definition, taskHash, key) => {
		const journal = new NativeTaskJournal(memoryStore().backend);
		await expect(journal.admit(definition, taskHash, key)).rejects.toThrow();
		expect(await journal.list()).toEqual([]);
	});

	test("recovers unfinished tasks as interrupted and persists the recovery", async () => {
		const backend = memoryStore();
		const first = new NativeTaskJournal(backend.backend);
		const admitted = await first.admit("identity@1", fingerprint, "request-1");
		await first.transition(admitted.task.id, "running");
		const reopened = new NativeTaskJournal(backend.backend);
		expect(await reopened.list()).toMatchObject([
			{ id: admitted.task.id, state: "interrupted", result: { status: "interrupted" } },
		]);
		expect(backend.value()).toMatchObject({ tasks: [{ state: "interrupted", result: interruptedResult }] });
	});

	test("rejects invalid documents, duplicates, and state/result mismatches", async () => {
		const invalidDocuments = [
			validDocument([validTask(), validTask({ id: "task-1", key: "request-2" })]),
			validDocument([validTask(), validTask({ id: "task-2", key: "request-1" })]),
			validDocument([validTask({ state: "admitted", result: undefined })]),
			validDocument([validTask({ state: "running", result: { status: "succeeded", verification: "unverified" } })]),
			validDocument([validTask({ state: "completed", result: undefined })]),
			validDocument([validTask({ state: "completed", result: { status: "failed", verification: "unverified" } })]),
			{ version: 1, tasks: [validTask()], extra: true },
			validDocument([validTask({ result: { status: "succeeded", value: undefined, verification: "unverified" } })]),
		];
		for (const document of invalidDocuments) {
			const journal = new NativeTaskJournal(memoryStore(document).backend);
			await expect(journal.ready()).rejects.toThrow("Invalid task document");
		}
	});

	test("enforces forward transitions and matching terminal results", async () => {
		const journal = new NativeTaskJournal(memoryStore().backend);
		const admitted = await journal.admit("identity@1", fingerprint, "request-1");
		await expect(
			journal.transition(admitted.task.id, "completed", { status: "succeeded", verification: "unverified" }),
		).rejects.toThrow("Invalid task transition");
		await expect(
			journal.transition(admitted.task.id, "running", { status: "succeeded", verification: "unverified" }),
		).rejects.toThrow("Invalid task transition");
		await journal.transition(admitted.task.id, "running");
		await expect(journal.transition(admitted.task.id, "admitted")).rejects.toThrow("Invalid task transition");
		await expect(journal.transition(admitted.task.id, "completed")).rejects.toThrow("Invalid task transition");
		await expect(
			journal.transition(admitted.task.id, "completed", { status: "failed", verification: "unverified" }),
		).rejects.toThrow("state and result");
	});

	test("returns the committed terminal record when completion races cancellation", async () => {
		const journal = new NativeTaskJournal(memoryStore().backend);
		const admitted = await journal.admit("identity@1", fingerprint, "request-1");
		await journal.transition(admitted.task.id, "running");
		const cancelled = await journal.transition(admitted.task.id, "cancelled", {
			status: "cancelled",
			verification: "unverified",
		});
		const completion = await journal.transition(admitted.task.id, "completed", {
			status: "succeeded",
			value: 1,
			verification: "unverified",
		});
		expect(completion).toEqual(cancelled);
		expect((await journal.list())[0]).toEqual(cancelled);
	});
});
