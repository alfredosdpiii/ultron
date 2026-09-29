import { createHash, randomUUID } from "node:crypto";
import { type Session, value } from "@ultron/agent-core";
import { isJsonValue, type JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import Type from "typebox";
import { Check } from "typebox/value";
import { readVersioned } from "../format-version.ts";

/** Atomic document replacement. One live task host per store, enforced by the worker owner lock. */
export interface NativeHostStore {
	read(): Promise<JsonValue | undefined>;
	write(document: JsonValue): Promise<void>;
}

export function createSessionTaskStore(session: Pick<Session, "getValue" | "setValue">): NativeHostStore {
	const address = value<JsonValue>("ultron.tasks", "root");
	return {
		read: async () => (await session.getValue(address, BACKGROUND_CONTEXT))?.value,
		write: (document) => session.setValue(address, document, BACKGROUND_CONTEXT),
	};
}

const resultSchema = Type.Object(
	{
		status: Type.Union([
			Type.Literal("succeeded"),
			Type.Literal("failed"),
			Type.Literal("cancelled"),
			Type.Literal("interrupted"),
		]),
		value: Type.Optional(Type.Unknown()),
		error: Type.Optional(Type.String()),
		verification: Type.Literal("unverified"),
		/** A subagent's checked verdict (rlm.finish), or null when it ended without one. */
		verdict: Type.Optional(Type.Unknown()),
		/** The host's check of that verdict against the files that changed while the subagent ran. */
		check: Type.Optional(Type.Unknown()),
		/** Set when a subagent ended without a valid verdict: its reply is all there is. */
		unverified: Type.Optional(Type.Literal(true)),
	},
	{ additionalProperties: false },
);
const taskSchema = Type.Object(
	{
		id: Type.String({ minLength: 1 }),
		key: Type.String({ minLength: 1 }),
		fingerprint: Type.String({ pattern: "^[a-f0-9]{64}$" }),
		definition: Type.String({ pattern: "^[a-z][a-z0-9-]*@[0-9]+$" }),
		state: Type.Union([
			Type.Literal("admitted"),
			Type.Literal("running"),
			Type.Literal("completed"),
			Type.Literal("failed"),
			Type.Literal("cancelled"),
			Type.Literal("interrupted"),
		]),
		result: Type.Optional(resultSchema),
		/** Spawning task, absent for tasks started by the root lane. */
		parentId: Type.Optional(Type.String({ minLength: 1 })),
	},
	{ additionalProperties: false },
);
const documentSchema = Type.Object(
	{ version: Type.Literal(1), tasks: Type.Array(taskSchema) },
	{ additionalProperties: false },
);

export type NativeTaskState = "admitted" | "running" | "completed" | "failed" | "cancelled" | "interrupted";
export type NativeResult = {
	status: "succeeded" | "failed" | "cancelled" | "interrupted";
	value?: JsonValue;
	error?: string;
	verification: "unverified";
	/** A subagent's checked verdict (`rlm.finish`); null when it ended without one. */
	verdict?: JsonValue;
	/** The host's check of the verdict against the files that changed while the subagent ran. */
	check?: JsonValue;
	/** A subagent ended without a valid verdict: its reply text is all there is. */
	unverified?: true;
};
export type NativeTask = {
	id: string;
	key: string;
	fingerprint: string;
	definition: string;
	state: NativeTaskState;
	result?: NativeResult;
	parentId?: string;
};

type NativeDocument = { version: 1; tasks: NativeTask[] };
type TerminalState = Exclude<NativeTaskState, "admitted" | "running">;
type ResultStatus = NativeResult["status"];

const DURABILITY_ERROR = "Task store durability is uncertain; reopen the owner";
const DEFINITION_PATTERN = /^[a-z][a-z0-9-]*@[0-9]+$/;
const FINGERPRINT_PATTERN = /^[a-f0-9]{64}$/;
const TERMINAL_STATES = new Set<TerminalState>(["completed", "failed", "cancelled", "interrupted"]);
const TASK_STATES = new Set<NativeTaskState>([
	"admitted",
	"running",
	"completed",
	"failed",
	"cancelled",
	"interrupted",
]);

function isTerminalState(state: NativeTaskState): state is TerminalState {
	return TERMINAL_STATES.has(state as TerminalState);
}

function resultStatusForState(state: TerminalState): ResultStatus {
	return state === "completed" ? "succeeded" : state;
}

function invalidDocument(): Error {
	return new Error("Invalid task document");
}

function validateResult(result: unknown): asserts result is NativeResult {
	if (!isJsonValue(result) || !Check(resultSchema, result)) throw new Error("Invalid task result");
}

function validateAdmission(definition: unknown, fingerprint: unknown, key: unknown): asserts definition is string {
	if (typeof definition !== "string" || !DEFINITION_PATTERN.test(definition))
		throw new Error("definition must be id@version");
	if (typeof fingerprint !== "string" || !FINGERPRINT_PATTERN.test(fingerprint))
		throw new Error("fingerprint must be a lowercase SHA-256 hex string");
	if (typeof key !== "string" || key.length === 0) throw new Error("idempotency key must be nonempty");
}

function validateDocument(saved: JsonValue): NativeDocument {
	if (!Check(documentSchema, saved)) throw invalidDocument();
	const parsed = saved as NativeDocument;
	const ids = new Set<string>();
	const keys = new Set<string>();
	for (const task of parsed.tasks) {
		if (ids.has(task.id) || keys.has(task.key)) throw invalidDocument();
		ids.add(task.id);
		keys.add(task.key);
		const hasResult = Object.hasOwn(task, "result");
		if (task.state === "admitted" || task.state === "running") {
			if (hasResult) throw invalidDocument();
			continue;
		}
		if (!hasResult || task.result === undefined || task.result.status !== resultStatusForState(task.state))
			throw invalidDocument();
	}
	return parsed;
}

export function taskFingerprint(request: JsonValue): string {
	if (!isJsonValue(request)) throw new Error("Task fingerprint input must be strict JSON");
	function canonical(item: JsonValue): string {
		if (Array.isArray(item)) return `[${item.map(canonical).join(",")}]`;
		if (item !== null && typeof item === "object") {
			return `{${Object.keys(item)
				.sort()
				.map((key) => `${JSON.stringify(key)}:${canonical(item[key])}`)
				.join(",")}}`;
		}
		return JSON.stringify(item);
	}
	return createHash("sha256").update(canonical(request)).digest("hex");
}

/** Serialized, commit-before-publish state. Failed writes poison the owner; reopening never replays effects. */
export class NativeTaskJournal {
	private readonly store: NativeHostStore;
	private records: NativeTask[] = [];
	private loading?: Promise<void>;
	private tail: Promise<void> = Promise.resolve();
	private broken = false;

	constructor(store: NativeHostStore) {
		this.store = store;
	}

	private assertHealthy(): void {
		if (this.broken) throw new Error(DURABILITY_ERROR);
	}

	private async load(): Promise<void> {
		let saved: JsonValue | undefined;
		try {
			saved = await this.store.read();
		} catch {
			throw new Error("Task store could not be read");
		}
		if (saved === undefined) return;
		if (!isJsonValue(saved)) throw invalidDocument();
		const parsed = validateDocument(readVersioned("ultron.tasks/root", saved));
		const recovered: NativeTask[] = parsed.tasks.map(
			(task): NativeTask =>
				task.state !== "admitted" && task.state !== "running"
					? structuredClone(task)
					: {
							...task,
							state: "interrupted",
							result: {
								status: "interrupted",
								error: "Owner ended; automatic replay is disabled",
								verification: "unverified",
							},
						},
		);
		if (parsed.tasks.some((task) => task.state === "admitted" || task.state === "running"))
			await this.write(recovered);
		else this.records = recovered;
	}

	private ensureLoaded(): Promise<void> {
		this.loading ??= this.load();
		return this.loading;
	}

	private async write(next: NativeTask[]): Promise<void> {
		try {
			await this.store.write(structuredClone({ version: 1, tasks: next }) as JsonValue);
		} catch {
			this.broken = true;
			throw new Error(DURABILITY_ERROR);
		}
		this.records = next;
	}

	async ready(): Promise<void> {
		await this.ensureLoaded();
		await this.tail;
		this.assertHealthy();
	}

	private enqueue<T>(change: () => Promise<T>): Promise<T> {
		const pending = this.tail.then(async () => {
			this.assertHealthy();
			await this.ensureLoaded();
			this.assertHealthy();
			return change();
		});
		this.tail = pending.then(
			() => {},
			() => {},
		);
		return pending;
	}

	async list(): Promise<NativeTask[]> {
		return this.enqueue(async () => structuredClone(this.records));
	}

	admit(
		definition: string,
		fingerprint: string,
		key: string,
		signal?: AbortSignal,
		parentId?: string,
	): Promise<{ task: NativeTask; created: boolean }> {
		return this.enqueue(async () => {
			signal?.throwIfAborted();
			validateAdmission(definition, fingerprint, key);
			const previous = this.records.find((task) => task.key === key);
			if (previous) {
				if (previous.fingerprint !== fingerprint) throw new Error("Idempotency key reused for a different task");
				return { task: structuredClone(previous), created: false };
			}
			const task: NativeTask = {
				id: `ultron-task-${randomUUID()}`,
				definition,
				fingerprint,
				key,
				state: "admitted",
				...(parentId === undefined ? {} : { parentId }),
			};
			await this.write([...this.records, task]);
			return { task: structuredClone(task), created: true };
		});
	}

	transition(id: string, state: NativeTaskState, result?: NativeResult): Promise<NativeTask> {
		return this.enqueue(async () => {
			if (typeof id !== "string" || id.length === 0) throw new Error("task ID must be nonempty");
			if (!TASK_STATES.has(state)) throw new Error("Invalid task state");
			const current = this.records.find((task) => task.id === id);
			if (!current) throw new Error("Unknown Ultron task");
			if (isTerminalState(current.state)) return structuredClone(current);
			if (state === "running") {
				if (current.state !== "admitted" || result !== undefined) throw new Error("Invalid task transition");
			} else {
				const canFinish = current.state === "running" || (current.state === "admitted" && state === "cancelled");
				if (!canFinish || !isTerminalState(state) || result === undefined)
					throw new Error("Invalid task transition");
				validateResult(result);
				if (result.status !== resultStatusForState(state as TerminalState))
					throw new Error("Task state and result do not match");
			}
			const next: NativeTask = { ...current, state, ...(result === undefined ? {} : { result }) };
			await this.write(this.records.map((task) => (task.id === id ? next : task)));
			return structuredClone(next);
		});
	}
}
