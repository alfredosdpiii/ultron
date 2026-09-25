import { mkdirSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { createConnection, type Socket } from "node:net";
import { isAbsolute, join } from "node:path";
import type { Context } from "@earendil-works/chord";
import {
	isJsonValue,
	type JsonValue,
	parseServiceProviderUpdate,
	REMOTE_SERVICE_ERROR_CODES,
	RemoteServiceError,
	type RemoteServiceErrorCode,
	type ServiceCall,
	type ServiceProviderUpdate,
} from "@earendil-works/chord";
import { withAbortSignal } from "@earendil-works/chord/context";
import type { ThinkingLevel } from "@earendil-works/pi-agent-core";
import {
	AgentHarness,
	type AgentHarness as AgentHarnessInstance,
	type AgentHarnessTool,
	type AgentHarnessToolInvocation,
	type AgentLane,
	BACKGROUND_CONTEXT,
	createBashTool,
	createEditTool,
	createReadTool,
	createWriteTool,
	type JsonlSessionMetadata,
	JsonlSessionRepo,
	type Session,
	TODO_CONTEXT,
	value,
	withCancel,
} from "@earendil-works/pi-agent-core";
import { NodeExecutionEnv } from "@earendil-works/pi-agent-core/node";
import lockfile from "proper-lockfile";
import Type, { type Static } from "typebox";
import { Check } from "typebox/value";
import { isValidThinkingLevel } from "../cli/args.ts";
import { getAgentDir, getRlmRuntimePath } from "../config.ts";
import { executeBashWithOperations } from "../core/bash-executor.ts";
import { ModelRegistry } from "../core/model-registry.ts";
import { findInitialModel, resolveCliModel } from "../core/model-resolver.ts";
import { ModelRuntime } from "../core/model-runtime.ts";
import { DefaultResourceLoader } from "../core/resource-loader.ts";
import { SettingsManager } from "../core/settings-manager.ts";
import { buildSystemPrompt } from "../core/system-prompt.ts";
import { createLocalBashOperations } from "../core/tools/bash.ts";
import { initTheme } from "../modes/interactive/theme/theme.ts";
import { createFamilyModule } from "../ultron/family.ts";
import { createGrantModule } from "../ultron/grants.ts";
import { createInstanceModule } from "../ultron/instances.ts";
import { createNativeJevClient } from "../ultron/jev.ts";
import { JEV_DECISION_CAPACITY, JevDecisionLog, recordingJevClient } from "../ultron/jev-decisions.ts";
import type { RefinementBranch } from "../ultron/local-services.ts";
import { createPredictAdapter } from "../ultron/predict-adapter.ts";
import { createProgressModule } from "../ultron/progress.ts";
import { createReleaseGateModule } from "../ultron/release-gate.ts";
import { createSessionDefinitionStore } from "../ultron/rlm/definition-registry.ts";
import { createSessionModuleStore, type HostCaller } from "../ultron/rlm/host-module.ts";
import { type KernelExecutionResult, type KernelHostHandler, RlmKernel } from "../ultron/rlm/kernel.ts";
import { KernelPool, KernelPoolCapacityError } from "../ultron/rlm/kernel-pool.ts";
import { NativeRlmHost } from "../ultron/rlm/native-host.ts";
import { loadOrCreateSnapshotKey } from "../ultron/rlm/snapshot-auth.ts";
import { createSessionTaskStore } from "../ultron/rlm/task-store.ts";
import { createScheduleModule } from "../ultron/schedules.ts";
import { createSkillModule } from "../ultron/skills.ts";
import { ToolRoundNudger, toolRoundsNudgeFromEnv } from "../ultron/tool-round-nudge.ts";
import { createSessionUsageLedger, nativeUsageLimitsFromEnv } from "../ultron/usage.ts";
import { createWorkerServices } from "../ultron/worker-services.ts";
import { COORDINATOR_PROTOCOL_VERSION } from "./coordinator.ts";
import { LegacyExtensionAdapter } from "./legacy-extension-adapter.ts";
import { createSessionPluginFacetLoader } from "./plugins/bundled.ts";
import {
	consumeInternalProcessRole,
	encodeControlLine,
	isDirectInternalProcessEntry,
	MAX_CONTROL_LINE_BYTES,
} from "./process.ts";
import {
	createSessionWorkerServices,
	type SessionWorkerRuntime,
	type SessionWorkerServices,
	type WorkerServiceScope,
} from "./services/worker.ts";

export type { SessionWorkerRuntime } from "./services/worker.ts";

/** Host request handler that also receives the lane whose kernel issued the request. */
type RlmHostHandler = (
	type: string,
	payload: Record<string, unknown>,
	signal: AbortSignal | undefined,
	caller: HostCaller,
) => Promise<unknown> | unknown;

/** Worker adapter around the shared, bounded Python protocol implementation. */
export class UltronRlmKernel {
	private readonly kernel: RlmKernel;

	constructor(cwd: string, hostHandler: KernelHostHandler, snapshotPath?: string, snapshotKey?: Uint8Array) {
		this.kernel = new RlmKernel(
			{
				cwd,
				runtimePath: getRlmRuntimePath(),
				...(snapshotPath === undefined ? {} : { snapshotPath }),
				...(snapshotKey === undefined ? {} : { snapshotKey }),
			},
			hostHandler,
		);
	}

	snapshot(path?: string): Promise<KernelExecutionResult> {
		return this.kernel.snapshot(path);
	}

	shutdown(): Promise<void> {
		return this.kernel.shutdown();
	}

	async execute(code: string, context: Context): Promise<string> {
		const result = await this.kernel.execute(code, context.abortSignal);
		if (result.status === "error") {
			throw new Error(`${result.error?.ename ?? "PythonError"}: ${result.error?.evalue ?? "Execution failed"}`);
		}
		return [result.stdout, result.stderr, result.result].filter(Boolean).join("\n");
	}

	async resetScratch(): Promise<void> {
		const result = await this.kernel.resetScratch();
		if (result.status === "error") throw new Error("RLM scratch reset failed");
	}

	close(): Promise<void> {
		return this.kernel.shutdown();
	}
}

export type UltronRlmTool = AgentHarnessTool<{ env: NodeExecutionEnv }> & {
	close(): Promise<void>;
	/** Clear a lane's Python scratch before a new invocation of a retained instance. */
	resetScratch(lane: string): Promise<void>;
	/**
	 * Keep a lane's kernel alive against idle and capacity eviction (a retained instance). Returns false
	 * instead of pinning when pinned lanes would take more than the pool's pin share (half its capacity by
	 * default), so pins never exhaust room for new work.
	 */
	pin(lane: string, holder: string): boolean;
	unpin(lane: string, holder: string): void;
	/** Pool introspection, for tests and diagnostics. */
	readonly kernels: KernelPool<UltronRlmKernel>;
	/** Read-only kernel pool view for inspection: live kernels, pinned lanes, and eviction count. */
	poolStats(): RlmPoolStats;
};

export type RlmPoolStats = {
	live: number;
	maxLive: number;
	lanes: { lane: string; running: number; pinnedBy: string[]; idleMs: number }[];
	evictions: number;
};

export function createUltronRlmTool(
	cwd: string,
	hostHandler: RlmHostHandler,
	resolveLane: (invocation: AgentHarnessToolInvocation, context: Context) => Promise<string> = async () => "main",
	options: {
		readonly snapshotDir?: string;
		/** Host-held HMAC key for lane snapshots; see loadOrCreateSnapshotKey. */
		readonly snapshotKey?: Uint8Array;
		readonly maxLive?: number;
		readonly maxPinned?: number;
		readonly idleTtlMs?: number;
		readonly now?: () => number;
	} = {},
): UltronRlmTool {
	// Idle kernels are evicted after a snapshot, so a lane's declared state survives and a crowded session
	// cannot keep unbounded Python processes alive (A43). Running cells are never evicted.
	const snapshotPath = (lane: string): string | undefined =>
		options.snapshotDir === undefined
			? undefined
			: join(options.snapshotDir, `${lane.replace(/[^A-Za-z0-9._-]/g, "_")}.snapshot`);
	const maxLive = options.maxLive ?? 16;
	const kernels = new KernelPool<UltronRlmKernel>({
		create: (lane) =>
			new UltronRlmKernel(
				cwd,
				(type, payload, signal) => hostHandler(type, payload, signal, { lane }),
				snapshotPath(lane),
				options.snapshotKey,
			),
		maxLive,
		maxPinned: options.maxPinned ?? Math.floor(maxLive / 2),
		idleTtlMs: options.idleTtlMs ?? 30 * 60 * 1000,
		snapshotPath,
		...(options.now === undefined ? {} : { now: options.now }),
	});
	const sweeper = setInterval(() => void kernels.sweep().catch(() => {}), 60_000);
	sweeper.unref();
	let closed = false;
	const schema = Type.Object({
		code: Type.String({
			description: "Python code for persistent RLM computation, typed agents, and data processing",
		}),
	});
	return {
		close: async () => {
			closed = true;
			clearInterval(sweeper);
			await kernels.close();
		},
		// A lane without a live kernel has no scratch to clear; a restored snapshot keeps only declared state.
		resetScratch: async (lane) => kernels.live(lane)?.resetScratch(),
		pin: (lane, holder) => {
			if (closed) return false;
			try {
				kernels.pin(lane, holder);
				return true;
			} catch (error) {
				if (error instanceof KernelPoolCapacityError) return false;
				throw error;
			}
		},
		unpin: (lane, holder) => kernels.unpin(lane, holder),
		kernels,
		poolStats: () => ({
			...kernels.stats(),
			evictions: kernels.evictions.filter((record) => record.evicted).length,
		}),
		name: "rlm",
		label: "rlm",
		description:
			"Execute Python in Ultron's persistent RLM environment. Use ordinary Python to inspect data, retain intermediate values, invoke typed specialists, and compose optional workflows.",
		parameters: schema,
		async execute(
			_toolCallId,
			params: { code: string },
			_onUpdate,
			_toolContext,
			invocation: AgentHarnessToolInvocation,
			context,
		) {
			if (closed) throw new Error("Ultron RLM tool is closed");
			const lane = await resolveLane(invocation, context);
			context.abortSignal?.throwIfAborted();
			if (closed) throw new Error("Ultron RLM tool is closed");
			const result = await kernels.use(lane, (kernel) => kernel.execute(params.code, context));
			return { content: [{ type: "text", text: result || "(no result)" }], details: {} };
		},
	};
}

const StrictObject = <const T extends Parameters<typeof Type.Object>[0]>(properties: T) =>
	Type.Object(properties, { additionalProperties: false });
const OpaqueJsonValueSchema = Type.Unsafe<JsonValue>(Type.Unknown());
const ServiceCallSchema = Type.Unsafe<ServiceCall>(
	StrictObject({
		serviceId: Type.String({ minLength: 1 }),
		instance: Type.Optional(
			StrictObject({ key: Type.String({ minLength: 1 }), generation: Type.Integer({ minimum: 1 }) }),
		),
		member: Type.String({ minLength: 1 }),
		args: Type.Array(Type.Unknown()),
	}),
);
const RemoteServiceErrorCodeSchema = Type.Unsafe<RemoteServiceErrorCode>(
	Type.String({ pattern: `^(?:${REMOTE_SERVICE_ERROR_CODES.join("|")})$` }),
);

export const SESSION_WORKER_CONTROL_ADDRESS_ENV = "PI_SESSION_WORKER_CONTROL_ADDRESS";
export const SESSION_WORKER_CONTROL_TOKEN_ENV = "PI_SESSION_WORKER_CONTROL_TOKEN";
export const SESSION_WORKER_SESSION_KEY_ENV = "PI_SESSION_WORKER_SESSION_KEY_BASE64";
export const SESSION_WORKER_PEER_ID_ENV = "PI_SESSION_WORKER_PEER_ID";
export const SESSION_WORKER_API_KEY_ENV = "PI_SESSION_WORKER_API_KEY";

export const SessionWorkerMetadataSchema = StrictObject({
	id: Type.String({ minLength: 1 }),
	createdAt: Type.Integer(),
	storageVersion: Type.Integer(),
	cwd: Type.String(),
	path: Type.String(),
	modifiedAt: Type.Number(),
	parentSessionId: Type.Optional(Type.String()),
});

export const SessionWorkerOptionsSchema = StrictObject({
	sessionDir: Type.String({ minLength: 1 }),
	metadata: SessionWorkerMetadataSchema,
	provider: Type.Optional(Type.String({ minLength: 1 })),
	model: Type.Optional(Type.String({ minLength: 1 })),
	thinking: Type.Optional(Type.String({ minLength: 1 })),
	systemPrompt: Type.Optional(Type.String()),
	noTools: Type.Optional(Type.Union([Type.Literal("all"), Type.Literal("builtin")])),
	tools: Type.Optional(Type.Array(Type.String({ minLength: 1 }))),
	excludeTools: Type.Optional(Type.Array(Type.String({ minLength: 1 }))),
	pluginManifestPaths: Type.Array(Type.String({ minLength: 1 })),
});
export type SessionWorkerOptions = Static<typeof SessionWorkerOptionsSchema>;

type SessionWorkerRuntimeOptions = SessionWorkerOptions & { readonly apiKey?: string };

export const WorkerOperationScopeSchema = StrictObject({
	serverConnectionId: Type.String(),
	attachmentId: Type.String(),
});
export type WorkerOperationScope = WorkerServiceScope;

export const WorkerOperationRequestSchema = StrictObject({
	type: Type.Literal("operation"),
	requestId: Type.String({ minLength: 1 }),
	scope: WorkerOperationScopeSchema,
	call: ServiceCallSchema,
});
export type WorkerOperationRequest = Static<typeof WorkerOperationRequestSchema>;

export const WorkerOperationResponseSchema = Type.Union([
	StrictObject({
		type: Type.Literal("operation_result"),
		requestId: Type.String({ minLength: 1 }),
		scope: WorkerOperationScopeSchema,
		result: Type.Optional(OpaqueJsonValueSchema),
	}),
	StrictObject({
		type: Type.Literal("operation_error"),
		requestId: Type.String({ minLength: 1 }),
		scope: WorkerOperationScopeSchema,
		code: Type.Optional(RemoteServiceErrorCodeSchema),
		message: Type.String(),
	}),
]);
export type WorkerOperationResponse = Static<typeof WorkerOperationResponseSchema>;

export const SessionWorkerCommandSchema = Type.Union([
	Type.Object({ type: Type.Literal("shutdown") }),
	Type.Object({ type: Type.Literal("discover_workers") }),
	/** The server is going away: exit when idle, otherwise drop its demand and keep working detached. */
	StrictObject({
		type: Type.Literal("release"),
		requestId: Type.String({ minLength: 1 }),
		serverConnectionId: Type.String(),
	}),
	Type.Object({
		type: Type.Literal("session_demand"),
		serverConnectionId: Type.String(),
		requestId: Type.String(),
		attachmentId: Type.String(),
		attached: Type.Boolean(),
	}),
	WorkerOperationRequestSchema,
	StrictObject({
		type: Type.Literal("operation_cancel"),
		requestId: Type.String({ minLength: 1 }),
		scope: WorkerOperationScopeSchema,
	}),
]);
export type SessionWorkerCommand = Static<typeof SessionWorkerCommandSchema>;

export const SessionWorkerEventSchema = Type.Union([
	Type.Object({
		type: Type.Literal("worker_ready"),
		token: Type.String(),
		sessionKey: Type.String(),
		sessionId: Type.String(),
		pid: Type.Integer({ minimum: 1 }),
		metadata: SessionWorkerMetadataSchema,
		pluginManifestPaths: Type.Array(Type.String({ minLength: 1 })),
	}),
	Type.Object({
		type: Type.Literal("worker_failed"),
		token: Type.String(),
		sessionKey: Type.String(),
		message: Type.String(),
	}),
	Type.Object({
		type: Type.Literal("worker_released"),
		token: Type.String(),
		sessionKey: Type.String(),
		requestId: Type.String(),
		retained: Type.Boolean(),
	}),
	Type.Object({
		type: Type.Literal("demand_applied"),
		token: Type.String(),
		sessionKey: Type.String(),
		requestId: Type.String(),
		attachmentId: Type.String(),
		attached: Type.Boolean(),
	}),
	Type.Object({
		type: Type.Literal("demand_rejected"),
		token: Type.String(),
		sessionKey: Type.String(),
		requestId: Type.String(),
		message: Type.String(),
	}),
	Type.Object({
		type: Type.Literal("operation_response"),
		token: Type.String(),
		sessionKey: Type.String(),
		response: WorkerOperationResponseSchema,
	}),
	Type.Object({
		type: Type.Literal("service_update"),
		token: Type.String(),
		sessionKey: Type.String(),
		scope: WorkerOperationScopeSchema,
		subscriptionId: Type.String({ minLength: 1 }),
		update: Type.Unknown(),
	}),
]);
export type SessionWorkerEvent = Static<typeof SessionWorkerEventSchema>;

/** Worker-local reconciliation of server-generation demand and Harness activity. */
export class WorkerLifecycle {
	readonly #initialDemandGraceMs: number;
	readonly #orphanDemandGraceMs: number;
	readonly #onRetire: () => void;
	readonly #demands = new Map<string, { serverConnectionId: string; attachmentId: string; timer?: NodeJS.Timeout }>();
	readonly #activeOperations = new Set<string>();
	#currentServerConnectionId: string | undefined;
	#initialTimer: NodeJS.Timeout | undefined;
	#demandInitialized: boolean;
	#retirementHolds = 0;
	#activitySequence = 0;
	#retiring = false;

	constructor(options: {
		initialServerConnectionId?: string;
		initialDemandGraceMs: number;
		orphanDemandGraceMs: number;
		onRetire(): void;
	}) {
		this.#currentServerConnectionId = options.initialServerConnectionId;
		this.#initialDemandGraceMs = options.initialDemandGraceMs;
		this.#orphanDemandGraceMs = options.orphanDemandGraceMs;
		this.#onRetire = options.onRetire;
		this.#demandInitialized = false;
		this.#initialTimer = setTimeout(() => {
			this.#initialTimer = undefined;
			this.#demandInitialized = true;
			this.#reconcile();
		}, this.#initialDemandGraceMs);
		this.#initialTimer.unref();
	}

	serverConnected(serverConnectionId: string): void {
		this.#currentServerConnectionId = serverConnectionId;
		for (const demand of this.#demands.values()) {
			if (demand.serverConnectionId !== serverConnectionId || !demand.timer) continue;
			clearTimeout(demand.timer);
			delete demand.timer;
		}
	}

	/**
	 * The server's coordinator connection dropped without a release (it crashed or was killed). Its demand lapses
	 * after the short orphan grace, so an idle worker retires promptly while busy work keeps running, exactly as
	 * after a release. A server that never got to demand this worker lapses the same way instead of waiting out
	 * the initial demand grace.
	 */
	serverDisconnected(serverConnectionId: string): void {
		if (this.#currentServerConnectionId === serverConnectionId) {
			this.#currentServerConnectionId = undefined;
			if (this.#initialTimer) {
				clearTimeout(this.#initialTimer);
				this.#initialTimer = setTimeout(() => {
					this.#initialTimer = undefined;
					this.#demandInitialized = true;
					this.#reconcile();
				}, this.#orphanDemandGraceMs);
				this.#initialTimer.unref();
			}
		}
		for (const [key, demand] of this.#demands) {
			if (demand.serverConnectionId !== serverConnectionId || demand.timer) continue;
			demand.timer = setTimeout(() => {
				if (this.#demands.get(key) !== demand) return;
				this.#demands.delete(key);
				this.#reconcile();
			}, this.#orphanDemandGraceMs);
			demand.timer.unref();
		}
	}

	beginRequest(serverConnectionId: string, attachmentId: string): () => void {
		if (this.#retiring) throw new Error("Session worker is retiring");
		if (serverConnectionId !== this.#currentServerConnectionId) {
			throw new Error("Session worker received a request from a stale server generation");
		}
		const demand = this.#demands.get(demandKey(serverConnectionId, attachmentId));
		if (!demand || demand.timer) {
			throw new Error("Session worker request does not match the active attachment");
		}
		return this.holdRetirement();
	}

	holdRetirement(): () => void {
		this.#retirementHolds += 1;
		let released = false;
		return () => {
			if (released) return;
			released = true;
			this.#retirementHolds -= 1;
			this.#reconcile();
		};
	}

	setDemand(serverConnectionId: string, attachmentId: string, attached: boolean): void {
		if (this.#retiring) throw new Error("Session worker is retiring");
		if (serverConnectionId !== this.#currentServerConnectionId) {
			throw new Error("Session worker received demand from a stale server generation");
		}
		this.#demandInitialized = true;
		if (this.#initialTimer) {
			clearTimeout(this.#initialTimer);
			this.#initialTimer = undefined;
		}
		const key = demandKey(serverConnectionId, attachmentId);
		const previous = this.#demands.get(key);
		if (previous?.timer) clearTimeout(previous.timer);
		if (attached) this.#demands.set(key, { serverConnectionId, attachmentId });
		else this.#demands.delete(key);
		this.#reconcile();
	}

	/** True while a lane operation or held background activity is still running. */
	get busy(): boolean {
		return this.#activeOperations.size !== 0;
	}

	get retiring(): boolean {
		return this.#retiring;
	}

	/** Count detached work, such as a background task between lane runs, as an active operation. */
	holdActivity(): () => void {
		const key = `activity\0${++this.#activitySequence}`;
		this.#activeOperations.add(key);
		let released = false;
		return () => {
			if (released) return;
			released = true;
			this.#activeOperations.delete(key);
			this.#reconcile();
		};
	}

	/** Forget every demand owned by a departing server so the worker retires once its work is idle. */
	releaseServer(serverConnectionId: string): void {
		for (const [key, demand] of this.#demands) {
			if (demand.serverConnectionId !== serverConnectionId) continue;
			if (demand.timer) clearTimeout(demand.timer);
			this.#demands.delete(key);
		}
		this.#demandInitialized = true;
		if (this.#initialTimer) {
			clearTimeout(this.#initialTimer);
			this.#initialTimer = undefined;
		}
		this.#reconcile();
	}

	operationStarted(kind: "run" | "compaction" | "navigation", lane: string, operationId: string): void {
		this.#activeOperations.add(`${kind}\0${lane}\0${operationId}`);
	}

	operationStopped(kind: "run" | "compaction" | "navigation", lane: string, operationId: string): void {
		this.#activeOperations.delete(`${kind}\0${lane}\0${operationId}`);
		this.#reconcile();
	}

	close(): void {
		if (this.#initialTimer) clearTimeout(this.#initialTimer);
		for (const demand of this.#demands.values()) {
			if (demand.timer) clearTimeout(demand.timer);
		}
		this.#demands.clear();
	}

	#reconcile(): void {
		if (
			this.#retiring ||
			!this.#demandInitialized ||
			this.#retirementHolds !== 0 ||
			this.#activeOperations.size !== 0 ||
			this.#demands.size !== 0
		) {
			return;
		}
		this.#retiring = true;
		this.#onRetire();
	}
}

const DEFAULT_INITIAL_DEMAND_GRACE_MS = 10_000;
/**
 * How long a crashed server's demand outlives it. Reattachment does not depend on it: a busy worker is kept by its
 * own activity, and a server that discovers a worker holds it for the discovery grace while it attaches. It only
 * needs to cover the coordinator's replacement handshake (disconnect, connect, discover), which takes milliseconds,
 * so one second retires an abandoned idle worker promptly after a hard kill. An idle worker holds nothing that is not
 * durable, so a later restart simply starts a fresh one, as after a clean quit.
 */
const DEFAULT_ORPHAN_DEMAND_GRACE_MS = 1_000;
export const SESSION_WORKER_INITIAL_DEMAND_GRACE_ENV = "__PI_SESSION_WORKER_INITIAL_DEMAND_GRACE_MS";
export const SESSION_WORKER_ORPHAN_DEMAND_GRACE_ENV = "__PI_SESSION_WORKER_ORPHAN_DEMAND_GRACE_MS";
const DEFAULT_DISCOVERY_GRACE_MS = 5_000;
export const SESSION_WORKER_DISCOVERY_GRACE_ENV = "__PI_SESSION_WORKER_DISCOVERY_GRACE_MS";

const CoordinatorInputSchema = Type.Union([
	Type.Object({
		type: Type.Literal("peer_registered"),
		peerId: Type.String(),
		serverConnectionId: Type.Optional(Type.String()),
	}),
	Type.Object({ type: Type.Literal("server_connected"), serverConnectionId: Type.String() }),
	Type.Object({ type: Type.Literal("server_disconnected"), serverConnectionId: Type.String() }),
	Type.Object({ type: Type.Literal("message"), from: Type.Literal("server"), payload: Type.Unknown() }),
]);
type CoordinatorInput = Static<typeof CoordinatorInputSchema>;

interface WorkerControl {
	readonly initialServerConnectionId?: string;
	readonly messages: AsyncIterable<unknown>;
	readonly socket: Socket;
	send(event: SessionWorkerEvent): Promise<void>;
}

let failureControl: WorkerControl | undefined;

async function connectControl(): Promise<WorkerControl> {
	const address = process.env[SESSION_WORKER_CONTROL_ADDRESS_ENV];
	const token = process.env[SESSION_WORKER_CONTROL_TOKEN_ENV];
	const encodedSessionKey = process.env[SESSION_WORKER_SESSION_KEY_ENV];
	if (!address || !token || !encodedSessionKey) throw new Error("Session worker requires a control address");
	const peerId = process.env[SESSION_WORKER_PEER_ID_ENV];
	if (!peerId) throw new Error("Session worker requires a peer ID");
	const socket = createConnection(address);
	await new Promise<void>((resolve, reject) => {
		socket.once("connect", resolve);
		socket.once("error", reject);
	});
	const messages = createJsonLineMessages(socket);
	await writeJsonLine(socket, { type: "register_peer", protocol: COORDINATOR_PROTOCOL_VERSION, peerId });
	const registered = await messages[Symbol.asyncIterator]().next();
	if (
		registered.done ||
		!Check(CoordinatorInputSchema, registered.value) ||
		registered.value.type !== "peer_registered"
	) {
		throw new Error("Coordinator rejected the session worker registration");
	}
	return {
		...(registered.value.serverConnectionId === undefined
			? {}
			: { initialServerConnectionId: registered.value.serverConnectionId }),
		messages,
		socket,
		send: (event) => writeJsonLine(socket, { type: "send", to: "server", payload: event }),
	};
}

async function readCommands(
	control: WorkerControl,
	handlers: {
		onShutdown(): void;
		onDiscovery(): void;
		onRelease(command: Extract<SessionWorkerCommand, { type: "release" }>): Promise<void>;
		onDemand(command: Extract<SessionWorkerCommand, { type: "session_demand" }>): Promise<void>;
		onOperation(command: WorkerOperationRequest): void;
		onOperationCancel(command: Extract<SessionWorkerCommand, { type: "operation_cancel" }>): void;
		onServerConnected(serverConnectionId: string): void;
		onServerDisconnected(serverConnectionId: string): void;
	},
): Promise<void> {
	for await (const value of control.messages) {
		if (!Check(CoordinatorInputSchema, value)) {
			control.socket.destroy(new Error("Coordinator sent an invalid worker message"));
			return;
		}
		const message: CoordinatorInput = value;
		if (message.type === "server_connected") {
			handlers.onServerConnected(message.serverConnectionId);
			continue;
		}
		if (message.type === "server_disconnected") {
			handlers.onServerDisconnected(message.serverConnectionId);
			continue;
		}
		if (message.type !== "message" || !Check(SessionWorkerCommandSchema, message.payload)) continue;
		const command: SessionWorkerCommand = message.payload;
		if (command.type === "shutdown") handlers.onShutdown();
		else if (command.type === "discover_workers") handlers.onDiscovery();
		else if (command.type === "release") await handlers.onRelease(command);
		else if (command.type === "session_demand") await handlers.onDemand(command);
		else if (command.type === "operation_cancel") handlers.onOperationCancel(command);
		else handlers.onOperation(command);
	}
}

function createJsonLineMessages(socket: Socket): AsyncIterable<unknown> {
	const queued: unknown[] = [];
	const waiters: ((value: unknown) => void)[] = [];
	let buffered = "";
	socket.setEncoding("utf8");
	socket.on("data", (chunk: string) => {
		buffered += chunk;
		if (Buffer.byteLength(buffered) > MAX_CONTROL_LINE_BYTES) {
			socket.destroy(new Error("Session worker control message is too large"));
			return;
		}
		while (true) {
			const newline = buffered.indexOf("\n");
			if (newline === -1) return;
			const line = buffered.slice(0, newline);
			buffered = buffered.slice(newline + 1);
			try {
				const value: unknown = JSON.parse(line);
				const waiter = waiters.shift();
				if (waiter) waiter(value);
				else queued.push(value);
			} catch {
				socket.destroy(new Error("Session worker received invalid control JSON"));
				return;
			}
		}
	});
	return {
		[Symbol.asyncIterator]() {
			return {
				next: async () => {
					const value = queued.shift() ?? (await new Promise<unknown>((resolve) => waiters.push(resolve)));
					return { done: false as const, value };
				},
			};
		},
	};
}

function writeJsonLine(socket: Socket, message: unknown): Promise<void> {
	return new Promise((resolve, reject) => {
		socket.write(encodeControlLine(message), (error) => {
			if (error) reject(error);
			else resolve();
		});
	});
}

function toWorkerServiceUpdate(update: ServiceProviderUpdate): ServiceProviderUpdate {
	if (!isJsonValue(update)) throw new Error("Service produced a non-JSON update");
	return parseServiceProviderUpdate(update);
}

function demandKey(serverConnectionId: string, attachmentId: string): string {
	return `${serverConnectionId}\0${attachmentId}`;
}

function sameScope(left: WorkerOperationScope, right: WorkerOperationScope): boolean {
	return left.serverConnectionId === right.serverConnectionId && left.attachmentId === right.attachmentId;
}

function lifecycleDelay(name: string, fallback: number): number {
	const value = process.env[name];
	if (value === undefined) return fallback;
	const parsed = Number(value);
	if (!Number.isSafeInteger(parsed) || parsed < 0) throw new Error(`${name} must be a non-negative safe integer`);
	return parsed;
}

async function closeResources(resources: {
	harness?: AgentHarnessInstance;
	services?: SessionWorkerServices;
	session?: Session<JsonlSessionMetadata>;
	repo: JsonlSessionRepo;
	executionEnv: NodeExecutionEnv;
	releaseOwnership: () => Promise<void>;
	closeRlm?: () => Promise<void>;
}): Promise<void> {
	const errors: unknown[] = [];
	try {
		await resources.closeRlm?.();
	} catch (error) {
		errors.push(error);
	}
	try {
		await resources.services?.dispose();
	} catch (error) {
		errors.push(error);
	}
	try {
		if (resources.harness) await resources.harness.close(TODO_CONTEXT);
		else await resources.session?.close(TODO_CONTEXT);
	} catch (error) {
		errors.push(error);
	}
	try {
		await resources.repo.close(TODO_CONTEXT);
	} catch (error) {
		errors.push(error);
	}
	try {
		await resources.executionEnv.cleanup(TODO_CONTEXT);
	} catch (error) {
		errors.push(error);
	}
	try {
		await resources.releaseOwnership();
	} catch (error) {
		errors.push(error);
	}
	if (errors.length === 1) throw errors[0];
	if (errors.length > 1) throw new AggregateError(errors, "Session worker cleanup failed");
}

export type CreateSessionWorkerHarness = (
	session: Session<JsonlSessionMetadata>,
	options: SessionWorkerRuntimeOptions,
	executionEnv: NodeExecutionEnv,
) => Promise<SessionWorkerRuntime>;

async function run(options: SessionWorkerRuntimeOptions, createHarness: CreateSessionWorkerHarness): Promise<void> {
	const { sessionDir, metadata } = options;
	const sessionId = metadata.id;
	const control = await connectControl();
	const token = process.env[SESSION_WORKER_CONTROL_TOKEN_ENV]!;
	const sessionKey = Buffer.from(process.env[SESSION_WORKER_SESSION_KEY_ENV]!, "base64url").toString();
	failureControl = control;
	const pluginManifestPaths = options.pluginManifestPaths;
	const releaseOwnership = await lockfile.lock(metadata.path, {
		realpath: true,
		stale: 2_000,
		update: 1_000,
		retries: { retries: 320, factor: 1, minTimeout: 25, maxTimeout: 25, maxRetryTime: 8_000 },
	});
	const executionEnv = new NodeExecutionEnv({ cwd: metadata.cwd });
	const repo = new JsonlSessionRepo({ fileSystem: executionEnv, sessionsRoot: sessionDir });
	let session: Session<JsonlSessionMetadata> | undefined;
	let harness: AgentHarnessInstance | undefined;
	let lane: AgentLane | undefined;
	let services: SessionWorkerServices | undefined;
	let closeRlm: (() => Promise<void>) | undefined;
	let bindActivity: SessionWorkerRuntime["bindActivity"];
	try {
		session = await repo.open(metadata, TODO_CONTEXT);
		const runtime = await createHarness(session, options, executionEnv);
		harness = runtime.harness;
		closeRlm = runtime.closeRlm;
		bindActivity = runtime.bindActivity;
		lane = runtime.lane ?? (await harness.lane("main", TODO_CONTEXT));
		services = await createSessionWorkerServices({
			lane,
			harness,
			cwd: metadata.cwd,
			legacyExtensionCommands: runtime.legacyExtensionCommands,
			inspect: runtime.inspect,
			modelRuntime: runtime.modelRuntime,
			settingsManager: runtime.settingsManager,
			facetLoader: runtime.facetLoader,
			publish: (scope, subscriptionId, update) =>
				control.send({
					type: "service_update",
					token,
					sessionKey,
					scope,
					subscriptionId,
					update: toWorkerServiceUpdate(update),
				}),
		});
	} catch (error) {
		try {
			await closeResources({ harness, services, session, repo, executionEnv, releaseOwnership, closeRlm });
		} catch (cleanupError) {
			throw new AggregateError([error, cleanupError], "Session worker startup and cleanup failed");
		}
		throw error;
	}

	const activeRequests = new Map<
		string,
		{ readonly scope: WorkerOperationScope; readonly cancel: (reason?: unknown) => void }
	>();
	let lifecycle: WorkerLifecycle | undefined;
	let removeLifecycleListeners: (() => void)[] = [];
	let closing: Promise<void> | undefined;
	const close = (): Promise<void> => {
		if (closing) return closing;
		lifecycle?.close();
		services.removeSubscriptions(() => true);
		for (const request of activeRequests.values()) request.cancel(new Error("Session worker is closing"));
		activeRequests.clear();
		for (const remove of removeLifecycleListeners) remove();
		removeLifecycleListeners = [];
		closing = closeResources({ harness, services, repo, executionEnv, releaseOwnership, closeRlm });
		return closing;
	};
	const closeAndExit = (): void => {
		void close().then(
			() => process.exit(0),
			(error: unknown) => {
				console.error(error);
				process.exit(1);
			},
		);
	};

	lifecycle = new WorkerLifecycle({
		initialServerConnectionId: control.initialServerConnectionId,
		initialDemandGraceMs: lifecycleDelay(SESSION_WORKER_INITIAL_DEMAND_GRACE_ENV, DEFAULT_INITIAL_DEMAND_GRACE_MS),
		orphanDemandGraceMs: lifecycleDelay(SESSION_WORKER_ORPHAN_DEMAND_GRACE_ENV, DEFAULT_ORPHAN_DEMAND_GRACE_MS),
		onRetire: closeAndExit,
	});
	removeLifecycleListeners = [
		harness.events.on("run_start", (event) => lifecycle?.operationStarted("run", event.lane, event.runId)),
		harness.events.on("run_resume", (event) => lifecycle?.operationStarted("run", event.lane, event.runId)),
		harness.events.on("run_suspend", (event) => lifecycle?.operationStopped("run", event.lane, event.runId)),
		harness.events.on("run_end", (event) => lifecycle?.operationStopped("run", event.lane, event.runId)),
		harness.events.on("compaction_start", (event) =>
			lifecycle?.operationStarted("compaction", event.lane, event.runId),
		),
		harness.events.on("compaction_end", (event) =>
			lifecycle?.operationStopped("compaction", event.lane, event.runId),
		),
		harness.events.on("navigation_start", (event) =>
			lifecycle?.operationStarted("navigation", event.lane, event.runId),
		),
		harness.events.on("navigation_end", (event) =>
			lifecycle?.operationStopped("navigation", event.lane, event.runId),
		),
		harness.events.on("fault", closeAndExit),
	];
	const activeLifecycle = lifecycle;
	bindActivity?.(() => activeLifecycle.holdActivity());

	const handleOperation = async (request: WorkerOperationRequest): Promise<void> => {
		let releaseRequest = (): void => {};
		const cancellable = withCancel(BACKGROUND_CONTEXT);
		try {
			releaseRequest = lifecycle!.beginRequest(request.scope.serverConnectionId, request.scope.attachmentId);
			activeRequests.set(request.requestId, { scope: request.scope, cancel: cancellable.cancel });
			const result = await services.invoke(request.call, request.scope, cancellable.context);
			if (result !== undefined && !isJsonValue(result)) throw new Error("Service produced a non-JSON result");
			await control.send({
				type: "operation_response",
				token,
				sessionKey,
				response: {
					type: "operation_result",
					requestId: request.requestId,
					scope: request.scope,
					...(result === undefined ? {} : { result }),
				},
			});
		} catch (error) {
			let code: RemoteServiceErrorCode | undefined;
			if (error instanceof RemoteServiceError) {
				code = error.code;
			} else if (error instanceof Error && "code" in error) {
				const candidate = error.code;
				if (Check(RemoteServiceErrorCodeSchema, candidate)) code = candidate;
			}
			await control.send({
				type: "operation_response",
				token,
				sessionKey,
				response: {
					type: "operation_error",
					requestId: request.requestId,
					scope: request.scope,
					...(code === undefined ? {} : { code }),
					message: error instanceof Error ? error.message : String(error),
				},
			});
		} finally {
			if (activeRequests.get(request.requestId)?.cancel === cancellable.cancel) {
				activeRequests.delete(request.requestId);
			}
			releaseRequest();
		}
	};

	let ready = false;
	const discoveryGraceMs = lifecycleDelay(SESSION_WORKER_DISCOVERY_GRACE_ENV, DEFAULT_DISCOVERY_GRACE_MS);
	const announce = (): void => {
		if (!ready || lifecycle?.retiring) return;
		// A detached worker that a new server just discovered must not retire before that server attaches.
		const releaseDiscovery = lifecycle?.holdRetirement();
		if (releaseDiscovery) setTimeout(releaseDiscovery, discoveryGraceMs).unref();
		void control
			.send({
				type: "worker_ready",
				token,
				sessionKey,
				sessionId,
				pid: process.pid,
				metadata,
				pluginManifestPaths: [...pluginManifestPaths],
			})
			.catch(() => closeAndExit());
	};
	void readCommands(control, {
		onShutdown: closeAndExit,
		onDiscovery: announce,
		onRelease: async (command) => {
			const retained = lifecycle?.busy === true && !lifecycle.retiring;
			if (retained) {
				// Detached work outlives the departing server: its requests stop being cancellable by
				// that server's disconnect, and its demand no longer keeps this worker alive.
				for (const [requestId, request] of activeRequests) {
					if (request.scope.serverConnectionId === command.serverConnectionId) activeRequests.delete(requestId);
				}
				services.removeSubscriptions((scope) => scope.serverConnectionId === command.serverConnectionId);
			}
			await control
				.send({ type: "worker_released", token, sessionKey, requestId: command.requestId, retained })
				.catch(() => {});
			if (retained) lifecycle?.releaseServer(command.serverConnectionId);
			else closeAndExit();
		},
		onDemand: async (command) => {
			const releaseRetirement = lifecycle?.holdRetirement() ?? (() => {});
			try {
				try {
					if (!command.attached) {
						const matches = (scope: WorkerOperationScope): boolean =>
							scope.serverConnectionId === command.serverConnectionId &&
							scope.attachmentId === command.attachmentId;
						services.removeSubscriptions(matches);
					}
					lifecycle?.setDemand(command.serverConnectionId, command.attachmentId, command.attached);
				} catch (error) {
					await control.send({
						type: "demand_rejected",
						token,
						sessionKey,
						requestId: command.requestId,
						message: error instanceof Error ? error.message : String(error),
					});
					return;
				}
				await control.send({
					type: "demand_applied",
					token,
					sessionKey,
					requestId: command.requestId,
					attachmentId: command.attachmentId,
					attached: command.attached,
				});
			} finally {
				releaseRetirement();
			}
		},
		onOperation: (request) => {
			void handleOperation(request).catch(() => closeAndExit());
		},
		onOperationCancel: (command) => {
			const active = activeRequests.get(command.requestId);
			if (active !== undefined && sameScope(active.scope, command.scope)) {
				active.cancel(new DOMException("Service operation cancelled", "AbortError"));
			}
		},
		onServerConnected: (serverConnectionId) => lifecycle?.serverConnected(serverConnectionId),
		onServerDisconnected: (serverConnectionId) => {
			// A crashed server's calls stop waiting; lane operations they started are not aborted (see AgentController).
			const matches = (scope: WorkerOperationScope): boolean => scope.serverConnectionId === serverConnectionId;
			services.removeSubscriptions(matches);
			for (const request of activeRequests.values()) {
				if (matches(request.scope)) request.cancel(new Error("Server disconnected"));
			}
			lifecycle?.serverDisconnected(serverConnectionId);
		},
	}).catch(() => closeAndExit());
	control.socket.once("close", closeAndExit);
	control.socket.once("error", () => closeAndExit());
	process.once("SIGTERM", closeAndExit);
	process.once("SIGINT", closeAndExit);

	try {
		ready = true;
		await control.send({
			type: "worker_ready",
			token,
			sessionKey,
			sessionId,
			pid: process.pid,
			metadata,
			pluginManifestPaths: [...pluginManifestPaths],
		});
	} catch (error) {
		try {
			await close();
		} catch (cleanupError) {
			throw new AggregateError([error, cleanupError], "Session worker readiness and cleanup failed");
		}
		throw error;
	}
}

export async function runSessionWorkerWithHarness(
	args: readonly string[],
	createHarness: CreateSessionWorkerHarness,
): Promise<void> {
	try {
		if (args.length !== 1) throw new Error("Session worker requires one options argument");
		let options: unknown;
		try {
			options = JSON.parse(args[0]!);
		} catch (error) {
			throw new Error("Session worker received invalid options", { cause: error });
		}
		if (
			!Check(SessionWorkerOptionsSchema, options) ||
			!isAbsolute(options.sessionDir) ||
			!isAbsolute(options.metadata.cwd) ||
			!isAbsolute(options.metadata.path) ||
			(options.provider !== undefined && options.model === undefined)
		) {
			throw new Error("Session worker received invalid options");
		}
		const apiKey = process.env[SESSION_WORKER_API_KEY_ENV];
		await run(apiKey === undefined ? options : { ...options, apiKey }, createHarness);
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);
		const token = process.env[SESSION_WORKER_CONTROL_TOKEN_ENV];
		const encodedSessionKey = process.env[SESSION_WORKER_SESSION_KEY_ENV];
		if (token && encodedSessionKey) {
			const sessionKey = Buffer.from(encodedSessionKey, "base64url").toString();
			await failureControl?.send({ type: "worker_failed", token, sessionKey, message }).catch(() => {});
		}
		throw error;
	}
}

async function createCodingAgentHarness(
	session: Session<JsonlSessionMetadata>,
	options: SessionWorkerRuntimeOptions,
	executionEnv: NodeExecutionEnv,
): Promise<SessionWorkerRuntime> {
	const modelRuntime = await ModelRuntime.create({ refreshOnCreate: false, allowModelNetwork: false });
	const settingsManager = SettingsManager.create(session.metadata.cwd);
	// Extensions (tool renderers, headless UI contexts) read the theme; Pi always has one initialized.
	initTheme(settingsManager.getTheme(), false);
	const resourceLoader = new DefaultResourceLoader({
		cwd: session.metadata.cwd,
		agentDir: getAgentDir(),
		settingsManager,
		systemPrompt: options.systemPrompt,
	});
	await resourceLoader.reload();
	const loadedExtensions = resourceLoader.getExtensions();
	for (const registration of loadedExtensions.runtime.pendingProviderRegistrations) {
		modelRuntime.registerProvider(registration.name, registration.config);
	}
	for (const registration of loadedExtensions.runtime.pendingNativeProviderRegistrations) {
		modelRuntime.registerNativeProvider(registration.provider);
	}
	loadedExtensions.runtime.pendingProviderRegistrations = [];
	loadedExtensions.runtime.pendingNativeProviderRegistrations = [];
	await modelRuntime.refresh({ allowNetwork: false });
	let resolved: Awaited<ReturnType<typeof findInitialModel>> | ReturnType<typeof resolveCliModel>;
	if (options.model === undefined) {
		resolved = await findInitialModel({
			scopedModels: [],
			isContinuing: true,
			defaultProvider: settingsManager.getDefaultProvider(),
			defaultModelId: settingsManager.getDefaultModel(),
			defaultThinkingLevel: settingsManager.getDefaultThinkingLevel(),
			modelRuntime,
		});
	} else {
		resolved = resolveCliModel({
			cliProvider: options.provider,
			cliModel: options.model,
			modelRuntime,
		});
		if (resolved.error) throw new Error(`Session worker could not resolve model: ${resolved.error}`);
	}
	if (!resolved.model) throw new Error("Session worker could not resolve a model");
	if (options.apiKey !== undefined) await modelRuntime.setRuntimeApiKey(resolved.model.provider, options.apiKey);
	const thinkingLevel: ThinkingLevel =
		options.thinking === undefined
			? (resolved.thinkingLevel ?? "medium")
			: isValidThinkingLevel(options.thinking)
				? options.thinking
				: (() => {
						throw new Error(`Session worker received invalid thinking level: ${options.thinking}`);
					})();
	const registry = new ModelRegistry(modelRuntime);
	// Every Jev decision (triage, recall gate, retention policy) is recorded without its input for `jev.decisions`.
	const jevDecisionAddress = value<JsonValue>("ultron.jev.decisions", "root");
	const jevDecisions = new JevDecisionLog({
		read: async () => (await session.getValue(jevDecisionAddress, TODO_CONTEXT))?.value,
		write: (document) => session.setValue(jevDecisionAddress, document, TODO_CONTEXT),
	});
	const nativeJev = createNativeJevClient();
	const jev = nativeJev === undefined ? undefined : recordingJevClient(nativeJev, jevDecisions);
	const recordJevUnavailable = (kind: "triage" | "recall", prompt: unknown): void => {
		void jevDecisions.record(String(prompt ?? ""), {
			at: jevDecisions.now(),
			kind,
			status: "unavailable",
			durationMs: 0,
			reason: "Jev is not configured",
		});
	};
	let host: NativeRlmHost | undefined;
	let holdActivity: (() => () => void) | undefined;
	const hostHandler: RlmHostHandler = async (type, payload, signal, caller) => {
		if (type === "bash") {
			const command = payload.command;
			if (typeof command !== "string" || !command.trim()) throw new Error("bash command must be a non-empty string");
			const result = await executeBashWithOperations(
				command,
				options.metadata.cwd,
				createLocalBashOperations({ shellPath: settingsManager.getShellPath() }),
				{ signal },
			);
			return {
				output: result.output,
				exit_code: result.exitCode ?? null,
				cancelled: result.cancelled,
				truncated: result.truncated,
				full_output_path: result.fullOutputPath ?? null,
			};
		}
		if (type === "rlm.find_models")
			return registry.getAvailable().map((model) => ({ provider: model.provider, id: model.id, name: model.name }));
		if (type === "jev.triage") {
			if (!jev) {
				recordJevUnavailable("triage", payload.prompt);
				return { available: false, reason: "Jev is not configured" };
			}
			return { available: true, ...(await jev.triage(String(payload.prompt ?? ""), signal)) };
		}
		if (type === "jev.recall") {
			if (!jev) {
				recordJevUnavailable("recall", payload.prompt);
				return { available: false, gate: { retrieve: false, probability: 0 }, results: [] };
			}
			return { available: true, gate: await jev.memoryRecall(String(payload.prompt ?? ""), signal), results: [] };
		}
		if (!host) throw new Error("Ultron RLM host is not initialized");
		return host.handle(type, payload, signal ? withAbortSignal(signal, TODO_CONTEXT) : TODO_CONTEXT, caller);
	};
	// Snapshots are trusted host artifacts: keep them in the private profile directory, one folder per session.
	const snapshotDir = join(getAgentDir(), "rlm-snapshots", options.metadata.id);
	mkdirSync(snapshotDir, { recursive: true, mode: 0o700 });
	const rlmTool = createUltronRlmTool(
		options.metadata.cwd,
		hostHandler,
		async (invocation, context) => {
			const meta = await session.getValue(value<{ lane: string }>("pi.op.meta", invocation.operationId), context);
			if (!meta || typeof meta.value.lane !== "string") throw new Error("RLM invocation has no owning lane");
			// The invocation's operation is the main-lane run (root turn) its cells belong to; host requests from
			// the root kernel during this cell are charged to that turn's usage root.
			if (meta.value.lane === "main") host?.beginRootTurn(invocation.operationId);
			return meta.value.lane;
		},
		// The signing key stays in this process; the kernel running model code never receives it.
		{ snapshotDir, snapshotKey: loadOrCreateSnapshotKey(getAgentDir()) },
	);
	const tools = [createReadTool(), createEditTool(), createWriteTool(), createBashTool(), rlmTool];
	const loadedSkills = await Promise.all(
		resourceLoader.getSkills().skills.map(async (skill) => ({
			name: skill.name,
			description: skill.description,
			filePath: skill.filePath,
			disableModelInvocation: skill.disableModelInvocation,
			content: await readFile(skill.filePath, "utf8"),
		})),
	);
	const resources = {
		skills: loadedSkills,
		promptTemplates: resourceLoader.getPrompts().prompts.map((template) => ({
			name: template.name,
			description: template.description,
			content: template.content,
		})),
	};
	const selectedToolNames = ["read", "edit", "write", "bash", "rlm"];
	const contextFiles = resourceLoader.getAgentsFiles().agentsFiles;
	const systemPrompt =
		options.systemPrompt ??
		buildSystemPrompt({
			cwd: options.metadata.cwd,
			selectedTools: selectedToolNames,
			toolSnippets: {
				read: "Read file contents",
				edit: "Edit files with find/replace",
				write: "Write files",
				bash: "Execute shell commands",
				rlm: "Run Python RLM code and recursive agents",
			},
			contextFiles,
			skills: resourceLoader.getSkills().skills,
			appendSystemPrompt: resourceLoader.getAppendSystemPrompt().join("\n\n"),
		});
	const toolNames = tools.map((tool) => tool.name);
	const activeToolNames =
		options.noTools === "all"
			? []
			: (options.tools === undefined ? toolNames : toolNames.filter((name) => options.tools?.includes(name))).filter(
					(name) => options.excludeTools?.includes(name) !== true,
				);
	const effectiveActiveToolNames =
		options.noTools === "builtin" ? activeToolNames.filter((name) => name === "rlm") : activeToolNames;
	const harness = (
		await AgentHarness.create(
			{
				session,
				models: modelRuntime,
				model: resolved.model,
				thinkingLevel,
				tools,
				activeToolNames: effectiveActiveToolNames,
				toolContext: { env: executionEnv },
				resources,
				systemPrompt,
			},
			TODO_CONTEXT,
		)
	).harness;
	let legacyExtensions: LegacyExtensionAdapter | undefined;
	try {
		const lane = await harness.lane("main", TODO_CONTEXT);
		legacyExtensions = new LegacyExtensionAdapter({
			session,
			lane,
			harness,
			modelRuntime,
			resourceLoader,
			cwd: options.metadata.cwd,
			model: resolved.model,
			systemPrompt,
		});
		legacyExtensions.bind();
		const extensionTools = legacyExtensions.tools;
		await harness.setTools([...tools, ...extensionTools], TODO_CONTEXT);
		const extensionToolNames = extensionTools.map((tool) => tool.name);
		const extensionActiveToolNames =
			options.noTools === "all"
				? []
				: (options.tools === undefined
						? [...effectiveActiveToolNames, ...extensionToolNames]
						: [...effectiveActiveToolNames, ...extensionToolNames].filter((name) => options.tools?.includes(name))
					).filter((name) => options.excludeTools?.includes(name) !== true);
		const nativeServices = createWorkerServices({
			session,
			sessionId: options.metadata.id,
			cwd: options.metadata.cwd,
			jev: jev ?? undefined,
			hindsightUrl: hindsightUrl(process.env.ULTRON_HINDSIGHT_URL),
			bankId: process.env.ULTRON_HINDSIGHT_BANK || "ultron",
			extensionCommands: {
				list: async () => legacyExtensions?.commands ?? [],
				run: async (name, args) => legacyExtensions?.runCommand(name, args) ?? { notifications: [] },
			},
		});
		// Refinements follow the conversation branch: changes are anchored at the main lane's tip and read
		// back only when that anchor is on the branch now in use (A07).
		const mainBranch = async (context: Context): Promise<RefinementBranch> => {
			const [tip, entries] = await Promise.all([lane.getTipId(context), lane.findEntries(undefined, context)]);
			const onPath = new Set(entries.map((entry) => entry.id));
			return { anchor: tip, onBranch: (anchor) => onPath.has(anchor) };
		};
		const branchedServices = {
			handle: async (type: string, payload: Record<string, unknown>, context: Context) =>
				type.startsWith("refinements.")
					? nativeServices.handle(type, payload, context, await mainBranch(context))
					: nativeServices.handle(type, payload, context),
		};
		host = new NativeRlmHost(harness, lane, {
			store: createSessionTaskStore(session),
			definitionStore: createSessionDefinitionStore(session),
			// Budgets apply per root turn: each main-lane run opens a fresh wall, admission and cost window.
			usage: createSessionUsageLedger(session, { limits: nativeUsageLimitsFromEnv() }),
			rootTurns: true,
			pinLane: (lane, holder) => rlmTool.pin(lane, holder),
			unpinLane: (lane, holder) => rlmTool.unpin(lane, holder),
			services: branchedServices,
			beforeLaneReuse: (lane) => rlmTool.resetScratch(lane),
			predict: createPredictAdapter({ models: modelRuntime, model: () => lane.getModel(TODO_CONTEXT) }),
			holdActivity: () => holdActivity?.() ?? (() => {}),
			refinements: async (definitionId, context) => {
				const current = (await branchedServices.handle(
					"refinements.current",
					{ kind: "instruction", target: `instruction:${definitionId}` },
					context,
				)) as { id: string; version: number | null; content: unknown } | null;
				if (!current) return [];
				const text = typeof current.content === "string" ? current.content : JSON.stringify(current.content);
				return [{ id: current.id, version: current.version, text }];
			},
			modules: [
				createFamilyModule({ store: createSessionModuleStore(session, "family") }),
				createProgressModule({ store: createSessionModuleStore(session, "progress") }),
				createScheduleModule({ store: createSessionModuleStore(session, "schedules") }),
				createInstanceModule({ store: createSessionModuleStore(session, "instances") }),
				// Grants stay dormant (enforce: false) so nothing ever prompts or blocks by default.
				createGrantModule({ store: createSessionModuleStore(session, "grants") }),
				createReleaseGateModule({ store: createSessionModuleStore(session, "release-gates") }),
				createSkillModule({
					store: createSessionModuleStore(session, "skills"),
					// Re-read skill files so skills.refresh sees edits made during the session.
					loadSkills: () =>
						Promise.all(
							resourceLoader.getSkills().skills.map(async (skill) => ({
								name: skill.name,
								description: skill.description,
								filePath: skill.filePath,
								disableModelInvocation: skill.disableModelInvocation,
								content: await readFile(skill.filePath, "utf8"),
							})),
						),
				}),
			],
		});
		// Top-level work admitted after the turn ends (a schedule firing) gets a root of its own.
		const removeRootTurnListener = harness.events.on("run_end", (event) => {
			if (event.lane === "main") host?.endRootTurn(event.runId);
		});
		// Brake for open-ended research loops on the root agent (ULTRON_TOOL_ROUNDS_NUDGE, 0 disables).
		const nudger = new ToolRoundNudger(toolRoundsNudgeFromEnv(process.env.ULTRON_TOOL_ROUNDS_NUDGE), (message) =>
			lane.steer(message, undefined, BACKGROUND_CONTEXT),
		);
		const removeNudgeTurnListener = harness.events.on("turn_end", (event) => {
			if (event.lane !== "main") return;
			nudger.turnEnded(event.runId, event.message.content.filter((part) => part.type === "toolCall").length);
		});
		const removeNudgeRunListener = harness.events.on("run_end", (event) => {
			if (event.lane === "main") nudger.runEnded(event.runId);
		});
		const currentActiveToolNames = await lane.getActiveTools(TODO_CONTEXT);
		if (
			currentActiveToolNames.length !== extensionActiveToolNames.length ||
			currentActiveToolNames.some((name, index) => name !== extensionActiveToolNames[index])
		) {
			await lane.setActiveTools(extensionActiveToolNames, TODO_CONTEXT);
		}
		return {
			harness,
			closeRlm: async () => {
				removeRootTurnListener();
				removeNudgeTurnListener();
				removeNudgeRunListener();
				await legacyExtensions?.close();
				await rlmTool.close();
				await host?.close();
			},
			lane,
			modelRuntime,
			settingsManager,
			bindActivity: (hold) => {
				holdActivity = hold;
			},
			inspect: async (request, payload, context) => {
				if (request === "rlm.pool") return rlmTool.poolStats();
				if (request === "jev.decisions") {
					return {
						available: {
							jev: nativeJev !== undefined,
							hindsight: hindsightUrl(process.env.ULTRON_HINDSIGHT_URL) !== undefined,
						},
						capacity: JEV_DECISION_CAPACITY,
						decisions: await jevDecisions.list(),
					};
				}
				if (!host) throw new Error("Ultron RLM host is not initialized");
				return host.handle(request, payload, context);
			},
			legacyExtensionCommands: {
				list: async () => legacyExtensions?.commands ?? [],
				run: async (name, args) => legacyExtensions?.runCommand(name, args) ?? { notifications: [] },
			},
			facetLoader: createSessionPluginFacetLoader(options.pluginManifestPaths),
		};
	} catch (error) {
		try {
			await legacyExtensions?.close();
			await harness.close(TODO_CONTEXT);
		} catch (cleanupError) {
			throw new AggregateError([error, cleanupError], "Session worker model selection and cleanup failed");
		}
		throw error;
	}
}

/**
 * Hindsight memory is on by default against a local server, as in the Pi Jev extension.
 * ULTRON_HINDSIGHT_URL overrides the address; "off" (or "none"/"0") disables memory.
 */
export function hindsightUrl(configured: string | undefined): string | undefined {
	if (configured === undefined || configured.trim() === "") return "http://localhost:8888";
	return ["off", "none", "0", "false"].includes(configured.trim().toLowerCase()) ? undefined : configured.trim();
}

export function runSessionWorkerProcess(args: readonly string[]): Promise<void> {
	return runSessionWorkerWithHarness(args, createCodingAgentHarness);
}

if (isDirectInternalProcessEntry(import.meta.url)) {
	const role = consumeInternalProcessRole();
	if (role !== "session-worker") {
		throw new Error("Session worker entrypoint requires an internal session-worker invocation");
	}
	void runSessionWorkerProcess(process.argv.slice(2)).catch(() => process.exit(1));
}
