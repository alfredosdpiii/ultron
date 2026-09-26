import { type Context, type Session, value } from "@ultron/agent-core";
import type { JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import type { NativeResult, NativeTask } from "./task-store.ts";

/** Lane whose Python kernel issued a host request. The root agent uses lane "main". */
export interface HostCaller {
	readonly lane: string;
}

export const ROOT_CALLER: HostCaller = { lane: "main" };

/** Task request accepted by {@link NativeHostApi.spawn}; mirrors `agents.spawn`. */
export interface HostTaskRequest {
	readonly definition: string;
	readonly input: JsonValue;
	readonly model?: string;
	readonly key?: string;
	readonly timeoutMs?: number;
	/** Existing lane to run on (a retained instance). Only host modules may set this. */
	readonly lane?: string;
}

/**
 * Host operations available to modules. Identity is derived by the host from the calling
 * lane; modules must never trust sender or task identity supplied in a payload.
 */
export interface NativeHostApi {
	/** Task that owns the calling lane, or null for the root lane. */
	callerTaskId(caller: HostCaller): string | null;
	/** Lane a live task ran on in this owner process, or null (deterministic/predict tasks have none). */
	taskLane(taskId: string): string | null;
	/** Execution strategy of a registered definition; throws for unknown definitions. */
	strategy(definition: string): "deterministic" | "predict" | "rlm";
	tasks(): Promise<NativeTask[]>;
	/** Admit a task as a child of `parentTaskId` (null for root). Budget and idempotency rules apply. */
	spawn(request: HostTaskRequest, parentTaskId: string | null, context: Context): Promise<NativeTask>;
	/** Resolves with the durable terminal result. */
	result(taskId: string): Promise<NativeResult>;
	cancel(taskId: string, reason: string): Promise<NativeResult>;
	/** Queue a steering message into the lane of a running task. Returns false when the task has no live lane. */
	steer(taskId: string, message: string, context: Context): Promise<boolean>;
	/** Usage status of the caller's root (the current root turn for the root lane). */
	usage(): Promise<JsonValue | null>;
	/**
	 * Keep a lane's Python kernel alive against idle and capacity eviction for `holder`. Returns false
	 * when the worker has no kernel pool or its pin capacity is spent; the lane then stays evictable.
	 */
	pinLane?(lane: string, holder: string): boolean;
	unpinLane?(lane: string, holder: string): void;
	/**
	 * Usage root that work started from `caller` belongs to: the running root turn for the root lane, the task's
	 * root for a task lane; undefined when there is none (no turn running, or a host without root turns).
	 */
	rootOf?(caller: HostCaller): string | undefined;
	now(): number;
}

export interface HostModuleRequest {
	readonly type: string;
	readonly payload: Record<string, unknown>;
	readonly caller: HostCaller;
	readonly context: Context;
}

/** Durable per-module document, stored in the session value store. */
export interface HostModuleStore {
	read(): Promise<JsonValue | undefined>;
	write(document: JsonValue): Promise<void>;
}

/**
 * A host module owns every request type that starts with one of its prefixes (for example
 * "progress."). Modules are created per session worker and closed with the host.
 */
export interface NativeHostModule {
	readonly prefixes: readonly string[];
	handle(request: HostModuleRequest, host: NativeHostApi): Promise<unknown>;
	/** Called once after the host has loaded its task journal. */
	start?(host: NativeHostApi): Promise<void> | void;
	close?(): Promise<void> | void;
}

/** One durable document per module, stored beside the task journal in the session value store. */
export function createSessionModuleStore(
	session: Pick<Session, "getValue" | "setValue">,
	moduleName: string,
): HostModuleStore {
	const address = value<JsonValue>("ultron.module", moduleName);
	return {
		read: async () => (await session.getValue(address, BACKGROUND_CONTEXT))?.value,
		write: (document) => session.setValue(address, document, BACKGROUND_CONTEXT),
	};
}

/** In-memory store for tests and sessionless use. */
export function createMemoryModuleStore(initial?: JsonValue): HostModuleStore {
	let document = initial === undefined ? undefined : structuredClone(initial);
	return {
		read: async () => (document === undefined ? undefined : structuredClone(document)),
		write: async (next) => {
			document = structuredClone(next);
		},
	};
}
