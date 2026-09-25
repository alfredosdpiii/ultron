import { createHash, randomUUID } from "node:crypto";
import { isJsonValue, type JsonValue } from "@ultron/chord";
import type { HostModuleStore, NativeHostApi, NativeHostModule } from "./rlm/host-module.ts";

/**
 * Retained instances (A36, A40): a completed RLM task can be kept so later invocations run on the
 * same lane, continuing its conversation and declared Python `state`, while each invocation gets
 * fresh scratch and its own task record. Earlier terminal results are never rewritten.
 *
 * An open instance pins its lane's Python kernel (holder `instance:<id>`) so an idle retained agent keeps
 * its process; closing unpins it, and a restarted owner re-pins every open instance. Pinning is best
 * effort: when the pool's pin capacity is spent, retain still succeeds and reports `pinned: false`.
 */
export interface InstanceRecord {
	id: string;
	task_id: string;
	definition: string;
	lane: string;
	owner: string | null;
	state: "open" | "closed";
	created_at: number;
	closed_at: number | null;
	invocations: { task_id: string; at: number; input_sha256: string }[];
}

interface InstanceDocument {
	version: 1;
	instances: InstanceRecord[];
}

const TERMINAL = new Set(["completed", "failed", "cancelled", "interrupted"]);

export function createInstanceModule(options: { store: HostModuleStore; now?: () => number }): NativeHostModule {
	let document: InstanceDocument | undefined;
	let tail: Promise<unknown> = Promise.resolve();
	const pinned = new Set<string>();
	const holder = (instance: InstanceRecord) => `instance:${instance.id}`;
	const pin = (instance: InstanceRecord, host: NativeHostApi): boolean => {
		if (pinned.has(instance.id)) return true;
		let ok = false;
		try {
			ok = host.pinLane?.(instance.lane, holder(instance)) ?? false;
		} catch {
			ok = false;
		}
		if (ok) pinned.add(instance.id);
		return ok;
	};
	const unpin = (instance: InstanceRecord, host: NativeHostApi): void => {
		if (!pinned.delete(instance.id)) return;
		host.unpinLane?.(instance.lane, holder(instance));
	};

	const load = async (): Promise<InstanceDocument> => {
		if (document) return document;
		const saved = await options.store.read();
		document = saved === undefined ? { version: 1, instances: [] } : parse(saved);
		return document;
	};
	const commit = async (next: InstanceDocument): Promise<void> => {
		await options.store.write(structuredClone(next) as unknown as JsonValue);
		document = next;
	};
	const serialize = <T>(operation: () => Promise<T>): Promise<T> => {
		const result = tail.then(operation, operation);
		tail = result.catch(() => {});
		return result;
	};

	return {
		prefixes: ["instances."],
		start(host) {
			return serialize(async () => {
				for (const instance of (await load()).instances) if (instance.state === "open") pin(instance, host);
			});
		},
		handle(request, host) {
			return serialize(async () => {
				const current = await load();
				const caller = host.callerTaskId(request.caller);
				const now = options.now?.() ?? host.now();
				const payload = request.payload;
				switch (request.type) {
					case "instances.retain": {
						fields(payload, ["task_id"]);
						const taskId = text(payload.task_id, "task_id");
						const existing = current.instances.find((instance) => instance.task_id === taskId);
						if (existing) {
							assertOwner(existing, caller);
							return { ...existing, pinned: existing.state === "open" && pin(existing, host) };
						}
						const task = (await host.tasks()).find((candidate) => candidate.id === taskId);
						if (!task) throw new Error("Unknown Ultron task");
						if ((task.parentId ?? null) !== caller) throw new Error("Only the task's parent can retain it");
						if (task.state !== "completed") throw new Error("Only a completed task can be retained");
						if (host.strategy(task.definition) !== "rlm") throw new Error("Only RLM tasks have a lane to retain");
						const record: InstanceRecord = {
							id: `ultron-instance-${randomUUID()}`,
							task_id: taskId,
							definition: task.definition,
							// Lane names are deterministic, so a retained lane survives an owner restart.
							lane: host.taskLane(taskId) ?? `ultron.${task.definition.split("@")[0]}.${taskId}`,
							owner: caller,
							state: "open",
							created_at: now,
							closed_at: null,
							invocations: [],
						};
						await commit({ ...current, instances: [...current.instances, record] });
						return { ...record, pinned: pin(record, host) };
					}
					case "instances.invoke": {
						fields(payload, ["id", "input", "key"]);
						const instance = find(current, payload.id);
						assertOwner(instance, caller);
						if (instance.state !== "open") throw new Error("Instance is closed");
						if (!isJsonValue(payload.input)) throw new Error("input must be JSON");
						const tasks = await host.tasks();
						const last = instance.invocations.at(-1)?.task_id ?? instance.task_id;
						const lastState = tasks.find((task) => task.id === last)?.state;
						if (lastState !== undefined && !TERMINAL.has(lastState))
							throw new Error("Instance is busy with a previous invocation");
						const key = payload.key == null ? undefined : `instance:${instance.id}:${text(payload.key, "key")}`;
						const task = await host.spawn(
							{ definition: instance.definition, input: payload.input, lane: instance.lane, key },
							caller,
							request.context,
						);
						if (!instance.invocations.some((invocation) => invocation.task_id === task.id)) {
							const next = structuredClone(current);
							find(next, instance.id).invocations.push({
								task_id: task.id,
								at: now,
								input_sha256: createHash("sha256").update(JSON.stringify(payload.input)).digest("hex"),
							});
							await commit(next);
						}
						return { instance_id: instance.id, task_id: task.id };
					}
					case "instances.close": {
						fields(payload, ["id"]);
						const instance = find(current, payload.id);
						assertOwner(instance, caller);
						if (instance.state === "closed") return instance;
						const next = structuredClone(current);
						Object.assign(find(next, instance.id), { state: "closed", closed_at: now });
						await commit(next);
						unpin(instance, host);
						return find(next, instance.id);
					}
					case "instances.get": {
						fields(payload, ["id"]);
						const instance = find(current, payload.id);
						assertOwner(instance, caller);
						return instance;
					}
					case "instances.list":
						fields(payload, []);
						return current.instances.filter((instance) => caller === null || instance.owner === caller);
					default:
						throw new Error(`Unknown instance request: ${request.type}`);
				}
			});
		},
	};
}

function assertOwner(instance: InstanceRecord, caller: string | null): void {
	// Root may inspect and drive every instance; a task only its own.
	if (caller !== null && instance.owner !== caller) throw new Error("Instance belongs to another task");
}

function find(document: InstanceDocument, id: unknown): InstanceRecord {
	const instance = document.instances.find((candidate) => candidate.id === text(id, "id"));
	if (!instance) throw new Error("Unknown instance");
	return instance;
}

function fields(payload: Record<string, unknown>, allowed: string[]): void {
	for (const key of Object.keys(payload)) if (!allowed.includes(key)) throw new Error(`Unknown payload field: ${key}`);
}

function text(value: unknown, name: string): string {
	if (typeof value !== "string" || !value.trim()) throw new Error(`${name} must be a nonempty string`);
	return value;
}

function parse(saved: JsonValue): InstanceDocument {
	if (
		typeof saved !== "object" ||
		saved === null ||
		Array.isArray(saved) ||
		saved.version !== 1 ||
		!Array.isArray(saved.instances)
	)
		throw new Error("Invalid instance document");
	return structuredClone(saved) as unknown as InstanceDocument;
}
