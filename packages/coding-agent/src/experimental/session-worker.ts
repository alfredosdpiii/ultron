import { mkdirSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { createConnection, type Socket } from "node:net";
import { dirname, isAbsolute, join } from "node:path";
import type { ThinkingLevel } from "@ultron/agent-core";
import {
	AgentHarness,
	type AgentHarness as AgentHarnessInstance,
	type AgentHarnessStreamOptions,
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
} from "@ultron/agent-core";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import type { Api, Model } from "@ultron/ai";
import { CLAUDE_CODE_PROVIDER_ID } from "@ultron/ai/providers/claude-code";
import type { Context } from "@ultron/chord";
import {
	isJsonValue,
	type JsonValue,
	parseServiceProviderUpdate,
	REMOTE_SERVICE_ERROR_CODES,
	RemoteServiceError,
	type RemoteServiceErrorCode,
	type ServiceCall,
	type ServiceProviderUpdate,
} from "@ultron/chord";
import { withAbortSignal } from "@ultron/chord/context";
import lockfile from "proper-lockfile";
import Type, { type Static } from "typebox";
import { Check } from "typebox/value";
import { isValidThinkingLevel } from "../cli/args.ts";
import { getAgentDir, getBundledLokiPath, getRlmRuntimePath } from "../config.ts";
import { DEFAULT_HINDSIGHT_URL } from "../core/defaults.ts";
import { createEventBus } from "../core/event-bus.ts";
import { configureHttpDispatcher } from "../core/http-dispatcher.ts";
import { ModelRegistry } from "../core/model-registry.ts";
import { findInitialModel, resolveCliModel } from "../core/model-resolver.ts";
import { ModelRuntime } from "../core/model-runtime.ts";
import { DefaultResourceLoader, type ResourceLoader } from "../core/resource-loader.ts";
import { SettingsManager } from "../core/settings-manager.ts";
import { buildSystemPrompt } from "../core/system-prompt.ts";
import { bashToolSystemPromptContribution, createLocalBashOperations } from "../core/tools/bash.ts";
import { editToolSystemPromptContribution } from "../core/tools/edit.ts";
import { readToolSystemPromptContribution } from "../core/tools/read.ts";
import { writeToolSystemPromptContribution } from "../core/tools/write.ts";
import { initTheme } from "../modes/interactive/theme/theme.ts";
import {
	AsyncEventDispatcher,
	asyncEventsEnabled,
	boundedSummary,
	maxEventRunsFromEnv,
	type RuntimeEvent,
} from "../ultron/async-events.ts";
import {
	AutoMemory,
	autoMemoryModeFromEnv,
	autoMemoryScopeFromEnv,
	createLegacyRecall,
	legacyBankFromEnv,
} from "../ultron/auto-memory.ts";
import { DEFAULT_CLAUDE_MODEL } from "../ultron/claude/claude-cli.ts";
import { EXTERNAL_ROOT_OPERATION, ExternalRootController } from "../ultron/claude/external-root.ts";
import { claudeRootRequested, installClaudeCodeLanes } from "../ultron/claude/worker-root.ts";
import { CodeSkills, codeSkillsDir, codeSkillsToolSection } from "../ultron/code-skills.ts";
import { CONTEXT_EDIT_EVENT, CONTEXT_ENTRY_PROJECTORS, ContextControl } from "../ultron/context-control.ts";
import { createFamilyModule } from "../ultron/family.ts";
import { BEFORE_WRITE_REQUEST, FileHooks, type GuardStats, type ProposedWrite } from "../ultron/file-hooks.ts";
import { assertSessionFormatsReadable } from "../ultron/format-version.ts";
import { createGrantModule } from "../ultron/grants.ts";
import { createInstanceModule } from "../ultron/instances.ts";
import { createNativeJevClient, JEV_RECALL_THRESHOLD } from "../ultron/jev.ts";
import { JEV_DECISION_CAPACITY, JevDecisionLog, recordingJevClient } from "../ultron/jev-decisions.ts";
import type { RefinementBranch } from "../ultron/local-services.ts";
import { lokiLog, lokiMode, setupLoki } from "../ultron/loki.ts";
import { createPredictAdapter } from "../ultron/predict-adapter.ts";
import { createProgressModule } from "../ultron/progress.ts";
import { createReleaseGateModule } from "../ultron/release-gate.ts";
import { createSessionDefinitionStore } from "../ultron/rlm/definition-registry.ts";
import {
	ExtensionToolCalls,
	type ExtensionToolInfo,
	extensionToolMode,
	modelExtensionToolNames,
	nativeExtensionToolAllowlist,
	type ToolCallEnd,
	toolCallSummary,
} from "../ultron/rlm/extension-tools.ts";
import { CellHints, hintMaxPerTag, hintsEnabled, readHandleBytes } from "../ultron/rlm/hints.ts";
import { runHostBash } from "../ultron/rlm/host-bash.ts";
import { createSessionModuleStore, type HostCaller } from "../ultron/rlm/host-module.ts";
import { createInferenceRuntime, createSessionFrameStore } from "../ultron/rlm/inference.ts";
import { type KernelExecutionResult, type KernelHostHandler, RlmKernel } from "../ultron/rlm/kernel.ts";
import { KernelPool, KernelPoolCapacityError } from "../ultron/rlm/kernel-pool.ts";
import { type DetachedTaskEnd, type NativeExternalChildRunner, NativeRlmHost } from "../ultron/rlm/native-host.ts";
import { maskCellOutput } from "../ultron/rlm/output-secrets.ts";
import { truncateToolOutput } from "../ultron/rlm/output-truncation.ts";
import {
	defaultBuiltinToolNames,
	NATIVE_FILE_TOOLS,
	RLM_TOOL_DESCRIPTION,
	RLM_TOOL_SNIPPET,
	rlmRuntimePrompt,
	rlmToolGuidelines,
} from "../ultron/rlm/prompt.ts";
import { jobSummary, type ShellJobEnd, ShellJobs } from "../ultron/rlm/shell-jobs.ts";
import { loadSnapshotKey } from "../ultron/rlm/snapshot-auth.ts";
import { createSessionTaskStore } from "../ultron/rlm/task-store.ts";
import { verdictTag } from "../ultron/rlm/verdict.ts";
import { CellImages, VIEW_IMAGE_REQUEST, type ViewImageOptions } from "../ultron/rlm/view-image.ts";
import { createScheduleModule } from "../ultron/schedules.ts";
import { createSkillModule } from "../ultron/skills.ts";
import {
	SkillExtractionNudger,
	skillNudgeFromEnv,
	ToolRoundNudger,
	toolRoundsNudgeFromEnv,
} from "../ultron/tool-round-nudge.ts";
import { createSessionUsageLedger, nativeUsageLimitsFromEnv } from "../ultron/usage.ts";
import { AUTOMATIC_KEEP_THRESHOLD, createWorkerServices } from "../ultron/worker-services.ts";
import { COORDINATOR_PROTOCOL_VERSION } from "./coordinator.ts";
import { LegacyExtensionAdapter } from "./legacy-extension-adapter.ts";
import { createSessionPluginFacetLoader } from "./plugins/bundled.ts";
import {
	consumeInternalProcessRole,
	encodeControlLine,
	isDirectInternalProcessEntry,
	MAX_CONTROL_LINE_BYTES,
} from "./process.ts";
import { ExtensionUIBridge } from "./services/extension-ui-provider.ts";
import {
	createSessionWorkerServices,
	type SessionWorkerRuntime,
	type SessionWorkerServices,
	type WorkerServiceScope,
} from "./services/worker.ts";
import { workerProjectTrusted } from "./services/worker-settings.ts";
import { forkedSessionStart } from "./session-start.ts";
import { traceStartup } from "./startup-trace.ts";

export type { SessionWorkerRuntime } from "./services/worker.ts";

/** Host request handler that also receives the lane whose kernel issued the request. */
type RlmHostHandler = (
	type: string,
	payload: Record<string, unknown>,
	signal: AbortSignal | undefined,
	caller: HostCaller,
) => Promise<unknown> | unknown;

/** A cell that raised: the message is the (truncated) output and traceback; `ename` is the exception type. */
export class RlmCellError extends Error {
	readonly ename: string;
	constructor(message: string, ename: string) {
		super(message);
		this.name = "RlmCellError";
		this.ename = ename;
	}
}

/** Worker adapter around the shared, bounded Python protocol implementation. */
/** One cell's tool result: its text and the images `view_image` attached. */
export type UltronRlmCellOutput = { text: string; images: CellImages };

export class UltronRlmKernel {
	private readonly kernel: RlmKernel;
	private readonly hostHandler: KernelHostHandler;

	constructor(
		cwd: string,
		hostHandler: KernelHostHandler,
		snapshotPath?: string,
		snapshotKey?: Uint8Array,
		env: Record<string, string> = { ULTRON_CODE_SKILLS_DIR: codeSkillsDir() },
	) {
		this.kernel = new RlmKernel(
			{
				cwd,
				runtimePath: getRlmRuntimePath(),
				// Active code skills import as `from code_skills import <name>` in every kernel.
				env,
				...(snapshotPath === undefined ? {} : { snapshotPath }),
				...(snapshotKey === undefined ? {} : { snapshotKey }),
			},
			hostHandler,
		);
		this.hostHandler = hostHandler;
	}

	snapshot(path?: string): Promise<KernelExecutionResult> {
		return this.kernel.snapshot(path);
	}

	shutdown(): Promise<void> {
		return this.kernel.shutdown();
	}

	memoryUsage(): { bytes: number; capBytes: number | null } | undefined {
		return this.kernel.memoryUsage();
	}

	async execute(code: string, context: Context): Promise<string> {
		return (await this.executeCell(code, context)).text;
	}

	/** Run a cell; images it attaches with `view_image` are normalized with `imageOptions` and returned beside the text. */
	async executeCell(
		code: string,
		context: Context,
		imageOptions: ViewImageOptions = {},
	): Promise<UltronRlmCellOutput> {
		const images = new CellImages(imageOptions);
		const result = await this.kernel.execute(code, context.abortSignal, (type, payload, signal) =>
			type === VIEW_IMAGE_REQUEST ? images.attach(payload) : this.hostHandler(type, payload, signal),
		);
		// The streams end with print()'s newline; the parts are joined by one, so drop it to avoid blank lines.
		const stdout = result.stdout.replace(/\n$/, "");
		const stderr = result.stderr.replace(/\n$/, "");
		if (result.status === "error") {
			// Show what the cell printed before it failed, then the traceback (its last line is `ename: evalue`).
			const summary = `${result.error?.ename ?? "PythonError"}: ${result.error?.evalue ?? "Execution failed"}`;
			const traceback = (result.error?.traceback ?? []).join("\n");
			const failure = !traceback ? summary : traceback.endsWith(summary) ? traceback : `${traceback}\n${summary}`;
			throw new RlmCellError(
				truncateToolOutput(maskCellOutput([stdout, stderr, failure].filter(Boolean).join("\n"))),
				result.error?.ename ?? "PythonError",
			);
		}
		// Secrets are masked before the cut, so a truncation boundary cannot split one past recognition.
		return {
			text: truncateToolOutput(maskCellOutput([stdout, stderr, result.result].filter(Boolean).join("\n"))),
			images,
		};
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
	lanes: { lane: string; running: number; pinnedBy: string[]; idleMs: number; memoryBytes?: number }[];
	evictions: number;
	/** Summed tree memory of the live kernels, when readable (Linux). */
	memoryBytes?: number;
	/** Per-kernel tree memory cap, when one is set. */
	memoryCapBytes?: number;
	/** File-write guards (Loki, extensions): checks, blocks and time, per turn and in all. */
	guards?: GuardStats[];
};

/** The `writes` of a `files.before_write` request, validated. */
function proposedWrites(payload: Record<string, unknown>): ProposedWrite[] {
	const writes = payload.writes;
	if (!Array.isArray(writes) || writes.length === 0 || writes.length > 64)
		throw new Error("files.before_write needs 1 to 64 writes");
	return writes.map((write) => {
		const path = (write as { path?: unknown } | null)?.path;
		const content = (write as { content?: unknown } | null)?.content;
		if (typeof path !== "string" || !path || typeof content !== "string")
			throw new Error("files.before_write writes need a path and text content");
		return { path, content };
	});
}

/** Tree memory is read from /proc; inspection polls reuse a reading this recent. */
const POOL_MEMORY_TTL_MS = 3000;

export { RLM_TOOL_DESCRIPTION } from "../ultron/rlm/prompt.ts";

function sectionIfPresent(name: string, content: string | undefined): Record<string, string> {
	return content === undefined ? {} : { [name]: content };
}

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
		/** Situational hints appended to cell results (see hints.ts). */
		readonly hints?: CellHints;
		/** The model a lane's tool results go to: `view_image` resizes for it and notes when it takes no images. */
		readonly resolveModel?: (lane: string, context: Context) => Promise<Model<Api> | undefined>;
		/** Pi's `images.autoResize` setting, read per cell. Default true. */
		readonly autoResizeImages?: () => boolean;
		/**
		 * Before-write and after-cell file hooks (file-hooks.ts): the kernels' `edit()`/`write()` ask them before
		 * writing, and each cell's other file changes are given to them in the background.
		 */
		readonly fileHooks?: FileHooks;
	} = {},
): UltronRlmTool {
	const hints = options.hints;
	const fileHooks = options.fileHooks;
	// Host requests are observed per lane so the hints can see what a cell waited on, polled or detached.
	const observedHandler = (lane: string): KernelHostHandler => {
		const handler: KernelHostHandler =
			hints === undefined || !hints.enabled
				? (type, payload, signal) => hostHandler(type, payload, signal, { lane })
				: async (type, payload, signal) => {
						const startedAt = Date.now();
						let result: unknown;
						try {
							result = await hostHandler(type, payload, signal, { lane });
							return result;
						} finally {
							hints.observe(lane, type, payload, result, startedAt);
						}
					};
		if (fileHooks === undefined) return handler;
		return async (type, payload, signal) =>
			type === BEFORE_WRITE_REQUEST
				? { results: await fileHooks.beforeWrite(proposedWrites(payload), { lane, ...(signal ? { signal } : {}) }) }
				: handler(type, payload, signal);
	};
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
				observedHandler(lane),
				snapshotPath(lane),
				options.snapshotKey,
				// The kernel's write skills ask the host before writing only when there are hooks to ask.
				fileHooks === undefined ? undefined : { ULTRON_CODE_SKILLS_DIR: codeSkillsDir(), ULTRON_FILE_HOOKS: "1" },
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
	let memoryReading: { at: number; laneKey: string; byLane: Map<string, number>; cap: number | undefined } | undefined;
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
		poolStats: () => {
			const stats = kernels.stats();
			const now = Date.now();
			const laneKey = stats.lanes.map((lane) => lane.lane).join("\n");
			// Re-read when the reading is old or the set of live kernels changed.
			if (
				memoryReading === undefined ||
				memoryReading.laneKey !== laneKey ||
				now - memoryReading.at > POOL_MEMORY_TTL_MS
			) {
				const byLane = new Map<string, number>();
				let cap: number | undefined;
				for (const lane of stats.lanes) {
					const usage = kernels.live(lane.lane)?.memoryUsage();
					if (usage === undefined) continue;
					byLane.set(lane.lane, usage.bytes);
					if (usage.capBytes !== null) cap = usage.capBytes;
				}
				memoryReading = { at: now, laneKey, byLane, cap };
			}
			const reading = memoryReading;
			const lanes = stats.lanes.map((lane) => {
				const bytes = reading.byLane.get(lane.lane);
				return bytes === undefined ? lane : { ...lane, memoryBytes: bytes };
			});
			const measured = lanes.filter((lane) => "memoryBytes" in lane);
			return {
				...stats,
				lanes,
				evictions: kernels.evictions.filter((record) => record.evicted).length,
				...(measured.length === 0
					? {}
					: { memoryBytes: [...reading.byLane.values()].reduce((sum, bytes) => sum + bytes, 0) }),
				...(reading.cap === undefined ? {} : { memoryCapBytes: reading.cap }),
				...(fileHooks === undefined || fileHooks.stats().length === 0 ? {} : { guards: fileHooks.stats() }),
			};
		},
		name: "rlm",
		label: "rlm",
		// Read per request, so a skill activated mid-session is listed on the next model call.
		get description() {
			return RLM_TOOL_DESCRIPTION + codeSkillsToolSection();
		},
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
			const model = await options.resolveModel?.(lane, context).catch(() => undefined);
			const imageOptions: ViewImageOptions = {
				autoResizeImages: options.autoResizeImages?.() ?? true,
				...(model === undefined ? {} : { model }),
			};
			// Files a cell changes by other means than checked writes are handed to the hooks after it, in the
			// background; what they find arrives with this lane's next cell result.
			const run = async () => {
				fileHooks?.cellStarted();
				try {
					return await kernels.use(lane, (kernel) => kernel.executeCell(params.code, context, imageOptions));
				} finally {
					fileHooks?.cellEnded(lane);
				}
			};
			const withFindings = (text: string): string => {
				const findings = fileHooks?.takePending(lane);
				return findings ? `${text}\n${findings}` : text;
			};
			if (hints === undefined) {
				let output: UltronRlmCellOutput;
				try {
					output = await run();
				} catch (error) {
					if (error instanceof Error) error.message = withFindings(error.message);
					throw error;
				}
				return { content: output.images.content(withFindings(output.text || "(no result)")), details: {} };
			}
			hints.beginCell(lane, params.code);
			let output: UltronRlmCellOutput;
			try {
				output = await run();
			} catch (error) {
				const message = error instanceof Error ? error.message : String(error);
				const hint = await hints
					.endCell(lane, { text: message, ename: error instanceof RlmCellError ? error.ename : "Error" })
					.catch(() => undefined);
				if (error instanceof Error) error.message = withFindings(hint ? `${message}\n${hint}` : message);
				throw error;
			}
			const result = output.text;
			const hint = await hints.endCell(lane, { text: result }).catch(() => undefined);
			const text = withFindings(result || "(no result)");
			return { content: output.images.content(hint ? `${text}\n${hint}` : text), details: {} };
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
	/** Pi `-e` extension paths (absolute, or package sources), loaded in addition to discovered extensions. */
	extensionPaths: Type.Optional(Type.Array(Type.String({ minLength: 1 }))),
	/** Pi `--no-extensions`: skip extension discovery; explicit `extensionPaths` still load. */
	noExtensions: Type.Optional(Type.Boolean()),
	/**
	 * Pi's extension mode for the client this worker is started for: `ctx.mode`, and whether `ctx.hasUI` is true
	 * (and UI queued) before that client attaches. Absent, extensions see "tui" and UI only while a client serves it.
	 */
	extensionMode: Type.Optional(
		Type.Union([Type.Literal("tui"), Type.Literal("rpc"), Type.Literal("print"), Type.Literal("json")]),
	),
	pluginManifestPaths: Type.Array(Type.String({ minLength: 1 })),
});
export type SessionWorkerOptions = Static<typeof SessionWorkerOptionsSchema>;

/**
 * Ultron's runtime for a root agent outside it (Claude Code over MCP, see ultron/claude/external-root.ts): the root
 * lane never runs; the harness model serves frames (and harness-lane children), and root events go to an inbox.
 */
export type ExternalRootOptions = {
	/** The harness model when it resolves (frames run on it); otherwise the profile's default model. */
	readonly preferredModel?: { readonly provider: string; readonly model: string };
	/** Model for `rlm.spawn` subagents on harness lanes (they need tool calls, which the frame model may lack). */
	readonly childModel?: string;
	/** Runs `rlm.spawn` subagents as processes of their own (Claude Code child agents). */
	readonly externalChild?: NativeExternalChildRunner;
	/** The root is itself a subagent: levels above it and how many more it may create. */
	readonly rootSpawn?: { readonly level: number; readonly allowance: number };
	/** Where a root that is itself a subagent sends its `rlm.finish` verdict. */
	readonly rootFinish?: (payload: Record<string, unknown>, context: Context) => Promise<unknown>;
};

type SessionWorkerRuntimeOptions = SessionWorkerOptions & {
	readonly apiKey?: string;
	readonly externalRoot?: ExternalRootOptions;
};

/** The runtime with, for an external root, its controller and the model frames run on. */
export type UltronRuntime = SessionWorkerRuntime & {
	readonly externalRoot?: ExternalRootController;
	readonly model: string;
	/** A host request on behalf of a lane (a relayed subagent verdict names the subagent's lane). */
	readonly hostRequest: (
		type: string,
		payload: Record<string, unknown>,
		context: Context,
		caller: HostCaller,
	) => Promise<unknown>;
};

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
	/** Sent before a worker retires on its own (no demand left), so its exit is not reported as unexpected. */
	Type.Object({
		type: Type.Literal("worker_retiring"),
		token: Type.String(),
		sessionKey: Type.String(),
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
			session,
			cwd: metadata.cwd,
			legacyExtensionCommands: runtime.legacyExtensionCommands,
			extensionUI: runtime.extensionUI,
			resourceSourceInfo: runtime.resourceSourceInfo,
			inspect: runtime.inspect,
			extensionSessionEvents: runtime.extensionSessionEvents,
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
		// Announce the deliberate retirement before exiting; the coordinator delivers it before the disconnect.
		onRetire: () => {
			void control
				.send({ type: "worker_retiring", token, sessionKey })
				.catch(() => {})
				.finally(closeAndExit);
		},
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
		traceStartup("worker.ready");
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

/**
 * Provider request options from the profile's settings, as Pi's session applies them to every model request
 * (sdk.ts buildRequestOptions): the request timeout defaults to Pi's HTTP idle timeout (5 min), and provider-level
 * retries follow `retry.provider`. Without them the worker's requests carried no timeout at all.
 */
export function providerRequestOptions(settingsManager: SettingsManager): AgentHarnessStreamOptions {
	const provider = settingsManager.getProviderRetrySettings();
	const idleTimeoutMs = settingsManager.getHttpIdleTimeoutMs();
	return {
		timeoutMs: provider.timeoutMs ?? (idleTimeoutMs === 0 ? 2147483647 : idleTimeoutMs),
		...(provider.maxRetries === undefined ? {} : { maxRetries: provider.maxRetries }),
		maxRetryDelayMs: provider.maxRetryDelayMs,
	};
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

/**
 * Ultron's runtime on one session: the harness with the rlm tool, the host (tasks, subagents, frames, modules),
 * hints, file hooks, completion events, memory and budgets. The session worker drives its root lane; with
 * `options.externalRoot` an outside agent (Claude Code over MCP) drives the root through `externalRoot` instead.
 */
export async function createUltronRuntime(
	session: Session<JsonlSessionMetadata>,
	options: SessionWorkerRuntimeOptions,
	executionEnv: NodeExecutionEnv,
): Promise<UltronRuntime> {
	const external = options.externalRoot;
	traceStartup("worker.harness");
	const modelRuntime = await ModelRuntime.create({ refreshOnCreate: false, allowModelNetwork: false });
	// A project the user distrusted with `/trust` (or `defaultProjectTrust: "never"`) loads no project settings or resources.
	const settingsManager = SettingsManager.create(session.metadata.cwd, getAgentDir(), {
		projectTrusted: workerProjectTrusted(session.metadata.cwd, getAgentDir()),
	});
	// Model requests leave from this process: use Pi's HTTP stack (npm undici fetch, Pi's idle timeout, env proxy)
	// rather than Node's bundled fetch, so requests match what stock Pi sends.
	configureHttpDispatcher(settingsManager.getHttpIdleTimeoutMs());
	// Extensions (tool renderers, headless UI contexts) read the theme; Pi always has one initialized.
	initTheme(settingsManager.getTheme(), false);
	// Loki guardrails (loki.ts): set up in parallel with resource loading. The Loki Pi extension, if a project has it,
	// defers to the built-in integration instead of checking every write twice.
	const lokiSettings = settingsManager.getLokiSettings();
	if (lokiMode(process.env, lokiSettings) !== "off") process.env.ULTRON_LOKI_BUILTIN = "1";
	const lokiRecord = lokiLog(process.env.ULTRON_LOKI_LOG);
	const lokiSetup = setupLoki({
		cwd: session.metadata.cwd,
		env: process.env,
		settings: lokiSettings,
		bundledEngine: getBundledLokiPath(),
		...(lokiRecord === undefined ? {} : { record: lokiRecord }),
	});
	// Pi's shared extension event bus (`pi.events`); the worker also publishes context edits on it.
	const extensionEvents = createEventBus();
	const resourceLoader = new DefaultResourceLoader({
		eventBus: extensionEvents,
		cwd: session.metadata.cwd,
		agentDir: getAgentDir(),
		settingsManager,
		systemPrompt: options.systemPrompt,
		...(options.extensionPaths === undefined ? {} : { additionalExtensionPaths: [...options.extensionPaths] }),
		...(options.noExtensions === true ? { noExtensions: true } : {}),
	});
	await resourceLoader.reload();
	traceStartup("worker.resources-loaded");
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
	let resolved: Awaited<ReturnType<typeof findInitialModel>> | ReturnType<typeof resolveCliModel> | undefined;
	// An external root prefers its frame model (claude-code/claude-opus-5-5 by default) and falls back to the default.
	if (external?.preferredModel !== undefined) {
		const preferred = resolveCliModel({
			cliProvider: external.preferredModel.provider,
			cliModel: external.preferredModel.model,
			modelRuntime,
		});
		if (!preferred.error && preferred.model) resolved = preferred;
	}
	if (resolved !== undefined) {
		// Resolved above.
	} else if (options.model === undefined) {
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
	let rlmHarness: AgentHarnessInstance<{ env: NodeExecutionEnv }> | undefined;
	let holdActivity: (() => () => void) | undefined;
	const hostHandler: RlmHostHandler = async (type, payload, signal, caller) => {
		// A plain `bash` with a yield_after (ULTRON_BASH_YIELD_AFTER) runs as a shell job that detaches when slow.
		if (type === "bash" && payload.yield_after !== undefined && host)
			return host.handle(
				"shell.bash",
				payload,
				signal ? withAbortSignal(signal, TODO_CONTEXT) : TODO_CONTEXT,
				caller,
			);
		if (type === "bash") {
			return runHostBash(
				payload,
				options.metadata.cwd,
				createLocalBashOperations({ shellPath: settingsManager.getShellPath() }),
				signal,
			);
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
	const snapshotKey = loadSnapshotKey(getAgentDir());
	if (snapshotKey.warning) console.error(snapshotKey.warning);
	mkdirSync(snapshotDir, { recursive: true, mode: 0o700 });
	// Situational hints on cell results (ULTRON_HINTS=off disables them); mutes and counts are a session value.
	const cellHints = new CellHints({
		store: createSessionModuleStore(session, "hints"),
		enabled: hintsEnabled(),
		maxPerTag: hintMaxPerTag(),
		asyncEvents: asyncEventsEnabled(process.env.ULTRON_ASYNC_EVENTS),
		readHandleBytes: readHandleBytes(),
		...(external === undefined ? {} : { rootDelivery: "next-call" as const }),
	});
	// Before-write and after-cell file hooks: Loki and extensions' `before_file_write`/`after_cell_changes` handlers.
	const fileHooks = new FileHooks({
		cwd: options.metadata.cwd,
		...(lokiRecord === undefined ? {} : { onRecord: lokiRecord }),
	});
	const rlmTool = createUltronRlmTool(
		options.metadata.cwd,
		hostHandler,
		async (invocation, context) => {
			// A cell of an external root (Claude Code over MCP) runs on the root lane; its turn is opened by the controller.
			if (external !== undefined && invocation.operationId.startsWith(EXTERNAL_ROOT_OPERATION)) return "main";
			const meta = await session.getValue(value<{ lane: string }>("pi.op.meta", invocation.operationId), context);
			if (!meta || typeof meta.value.lane !== "string") throw new Error("RLM invocation has no owning lane");
			// The invocation's operation is the main-lane run (root turn) its cells belong to; host requests from
			// the root kernel during this cell are charged to that turn's usage root.
			if (meta.value.lane === "main") host?.beginRootTurn(invocation.operationId);
			return meta.value.lane;
		},
		// The signing key stays in this process; the kernel running model code never receives it. It lives in the
		// profile file unless ULTRON_RLM_SNAPSHOT_KEY_STORE opts into the OS keyring.
		{
			snapshotDir,
			snapshotKey: snapshotKey.key,
			hints: cellHints,
			// The lane's current model (it can change mid-session); the harness exists by the time a cell runs.
			// An external root's own model is not Ultron's to know (its images go to Claude Code as they are).
			resolveModel: async (lane, context) =>
				external !== undefined && lane === "main"
					? undefined
					: (await rlmHarness?.lane(lane, context))?.getModel(context),
			autoResizeImages: () => settingsManager.getImageAutoResize(),
			fileHooks,
		},
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
	const toolNames = tools.map((tool) => tool.name);
	// Every built-in stays registered so `--tools read,bash` can pick it, but by default the model gets only the
	// RLM REPL, whose kernel has shell and edits as Python skills (ULTRON_TOOLS=native restores Pi's set).
	const defaultToolNames = defaultBuiltinToolNames();
	const activeToolNames =
		options.noTools === "all"
			? []
			: (options.tools === undefined
					? toolNames.filter((name) => defaultToolNames.includes(name))
					: toolNames.filter((name) => options.tools?.includes(name))
				).filter((name) => options.excludeTools?.includes(name) !== true);
	const effectiveActiveToolNames =
		options.noTools === "builtin" ? activeToolNames.filter((name) => name === "rlm") : activeToolNames;
	const contextFiles = resourceLoader.getAgentsFiles().agentsFiles;
	// Extension tools live in the REPL by default (ULTRON_EXTENSION_TOOLS=native, ULTRON_TOOLS=native or the
	// `extensionTools` setting restore them as model tools; ULTRON_NATIVE_EXTENSION_TOOLS keeps some native).
	const extensionToolSettings = settingsManager.getExtensionToolsSettings();
	const extensionMode = extensionToolMode(process.env, extensionToolSettings);
	const extensionAllowlist = nativeExtensionToolAllowlist(process.env, extensionToolSettings);
	let legacyExtensions: LegacyExtensionAdapter | undefined;
	/** Extension tools as registered now: live once the adapter exists, else as the extensions loaded them. */
	const currentExtensionTools = (): ExtensionToolInfo[] =>
		legacyExtensions?.replTools ?? loadedExtensionTools(resourceLoader);
	const nativeExtensionTools = (names: readonly string[]): string[] =>
		modelExtensionToolNames(names, {
			mode: extensionMode,
			allowlist: extensionAllowlist,
			...(options.tools === undefined ? {} : { explicit: options.tools }),
		});
	const renderSystemPrompt = (extensionTools: readonly ExtensionToolInfo[]): string =>
		buildSystemPrompt({
			cwd: options.metadata.cwd,
			// Only the tools the model can call, as Pi lists them: `--tools`/`--exclude-tools` drop their snippets too.
			selectedTools: effectiveActiveToolNames,
			// Pi's own tool snippets and guidelines, and the profile's SYSTEM.md, as Pi's session builds them.
			customPrompt: resourceLoader.getSystemPrompt(),
			toolSnippets: {
				read: readToolSystemPromptContribution.snippet,
				edit: editToolSystemPromptContribution.snippet,
				write: writeToolSystemPromptContribution.snippet,
				bash: bashToolSystemPromptContribution.snippet,
				rlm: RLM_TOOL_SNIPPET,
			},
			toolGuidelines: {
				read: [...readToolSystemPromptContribution.guidelines],
				edit: [...editToolSystemPromptContribution.guidelines],
				write: [...writeToolSystemPromptContribution.guidelines],
				rlm: rlmToolGuidelines(effectiveActiveToolNames),
			},
			// The REPL runtime guide (kernel, skills, delegation) as its own section after Pi's tool list and rules.
			sections: {
				...sectionIfPresent(
					"runtime",
					rlmRuntimePrompt(effectiveActiveToolNames, {
						asyncEvents: asyncEventsEnabled(process.env.ULTRON_ASYNC_EVENTS),
						extensionTools,
						mcpServers: mcpServerNames(extensionTools),
						nativeExtensionTools: nativeExtensionTools(extensionTools.map((tool) => tool.name)),
					}),
				),
				// Loki's short policy note, kept apart from the runtime guide.
				...sectionIfPresent("loki", loki.context),
			},
			contextFiles,
			skills: resourceLoader.getSkills().skills,
			appendSystemPrompt: resourceLoader.getAppendSystemPrompt().join("\n\n"),
			// REPL-only mode: Pi's docs pointers and custom-tools note describe docs and tools the model lacks
			// (extension tools are REPL skills, listed in the guide).
			includeHarnessDocs:
				!effectiveActiveToolNames.includes("rlm") ||
				NATIVE_FILE_TOOLS.some((name) => effectiveActiveToolNames.includes(name)),
		});
	const loki = await lokiSetup;
	if (loki.guard) fileHooks.add(loki.guard);
	// Rendered again only when the extension tool list changes (the guide lists them), so the prompt-cache prefix
	// stays stable.
	let promptCache: { key: string; text: string } | undefined;
	const currentSystemPrompt = (): string => {
		if (options.systemPrompt !== undefined) return options.systemPrompt;
		const extensionTools = currentExtensionTools();
		const key = JSON.stringify(extensionTools.map((tool) => [tool.name, tool.description]));
		if (promptCache?.key !== key) promptCache = { key, text: renderSystemPrompt(extensionTools) };
		return promptCache.text;
	};
	const systemPrompt = currentSystemPrompt();
	traceStartup("worker.harness-create");
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
				systemPrompt: () => currentSystemPrompt(),
				entryProjectors: CONTEXT_ENTRY_PROJECTORS,
				streamOptions: providerRequestOptions(settingsManager),
				retry: settingsManager.getRetrySettings(),
			},
			TODO_CONTEXT,
		)
	).harness;
	rlmHarness = harness;
	// Lanes on a claude-code model with tools run through Claude Code (`ultron --claude`, see
	// ultron/claude/worker-root.ts); an external root (`ultron mcp`) already is Claude Code.
	const claudeLanes =
		external === undefined ? installClaudeCodeLanes({ session, harness, cwd: options.metadata.cwd }) : undefined;
	// A worker started for an interactive client (Pi's TUI or RPC mode) queues extension UI for it until it attaches.
	const extensionUI = new ExtensionUIBridge({
		expectClient: options.extensionMode === "tui" || options.extensionMode === "rpc",
	});
	try {
		const lane = await harness.lane("main", TODO_CONTEXT);
		// `ultron --claude` (ULTRON_ROOT=claude): the root lane runs on Claude Code, also in a resumed session that
		// was on another model.
		if (external === undefined && claudeRootRequested()) {
			const current = await lane.getModel(TODO_CONTEXT).catch(() => undefined);
			if (current?.provider !== CLAUDE_CODE_PROVIDER_ID) {
				const target =
					resolved.model.provider === CLAUDE_CODE_PROVIDER_ID
						? resolved.model
						: modelRuntime.getModel(
								CLAUDE_CODE_PROVIDER_ID,
								process.env.ULTRON_CLAUDE_MODEL?.trim() || DEFAULT_CLAUDE_MODEL,
							);
				if (target) await lane.setModel({ provider: target.provider, modelId: target.id }, TODO_CONTEXT);
			}
		}
		// A Session the server just created as a fork starts with Pi's `session_start` reason "fork", once.
		const forked = await session.getValue(forkedSessionStart, TODO_CONTEXT);
		if (forked !== undefined) await session.deleteValue(forkedSessionStart, TODO_CONTEXT);
		legacyExtensions = new LegacyExtensionAdapter({
			ui: extensionUI,
			...(options.extensionMode === undefined ? {} : { mode: options.extensionMode }),
			session,
			lane,
			harness,
			modelRuntime,
			resourceLoader,
			cwd: options.metadata.cwd,
			model: resolved.model,
			systemPrompt,
			// Extension tools that live in the REPL stay out of the model's tool list whatever an extension activates.
			filterActiveTools: (names) => {
				const registered = new Set(legacyExtensions?.replTools.map((tool) => tool.name) ?? []);
				const native = new Set(nativeExtensionTools([...registered]));
				return names.filter((name) => !registered.has(name) || native.has(name));
			},
		});
		legacyExtensions.bind(
			forked === undefined
				? { reason: "startup" }
				: { reason: "fork", previousSessionFile: forked.value.previousSessionFile },
		);
		const extensionTools = legacyExtensions.tools;
		await harness.setTools([...tools, ...extensionTools], TODO_CONTEXT);
		for (const guard of legacyExtensions.fileGuards()) fileHooks.add(guard);
		// The first snapshot, so the first cell's file changes are seen too.
		void fileHooks.start();
		const removeLokiListeners = installLokiNotice(harness, fileHooks, loki.notice);
		// By default the model's tool list stays [rlm]: extension tools are Python skills (`tools`, `mcp`).
		const extensionToolNames = nativeExtensionTools(extensionTools.map((tool) => tool.name));
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
			hindsightUrl: hindsightUrl(process.env.ULTRON_HINDSIGHT_URL, settingsManager.getHindsightUrl()),
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
		// Model-owned context (`ctx`) and collapse on return; edits are published to extensions on `pi.events`.
		const contextControl = new ContextControl({
			harness,
			rootLane: lane,
			observe: (event) => extensionEvents.emit(CONTEXT_EDIT_EVENT, event),
		});
		const removeContextControl = contextControl.install();
		traceStartup("worker.host");
		// Budgets apply per root turn; the turn and token limits (rootBudget settings, ULTRON_MAX_TOTAL_*) count
		// every model turn of a root and its descendants.
		const usage = createSessionUsageLedger(session, {
			limits: nativeUsageLimitsFromEnv(process.env, settingsManager.getRootBudgetSettings()),
		});
		// A session written by a newer Ultron fails here, naming the value and its format version, before any of
		// it is loaded or rewritten.
		await assertSessionFormatsReadable(session, BACKGROUND_CONTEXT);
		// Bounded inference: handles are content-addressed beside the session file, frame traces are session values.
		const inference = createInferenceRuntime({
			contextDir: join(dirname(options.metadata.path), "rlm-context", options.metadata.id),
			traces: createSessionFrameStore(session),
			usage,
			// Read per frame: `/settings → Models` applies to the next frame without a restart.
			modelSettings: () => ({
				rlm: settingsManager.getRlmModelSettings(),
				reviewModel: settingsManager.getReviewModel(),
			}),
		});
		const removeInferenceHooks = inference.install(harness);
		// Host-owned shell jobs (`bash(cmd, yield_after=...)`) and completion events for detached work
		// (ULTRON_ASYNC_EVENTS=off disables the events; jobs still work and can be waited on).
		let events: AsyncEventDispatcher | undefined;
		const shellJobs = new ShellJobs({
			cwd: options.metadata.cwd,
			dir: join(dirname(options.metadata.path), "rlm-jobs", options.metadata.id),
			operations: () => createLocalBashOperations({ shellPath: settingsManager.getShellPath() }),
			store: createSessionModuleStore(session, "jobs"),
			holdActivity: () => holdActivity?.() ?? (() => {}),
			onEnd: (end) => {
				const event = jobEvent(end);
				if (event) events?.publish(event);
			},
		});
		// Extension tool calls from the REPL (`tools.call`, `mcp.call`), and every non-rlm tool the model calls
		// directly, recorded for the graph; a detached REPL call's end is announced like a job's.
		const adapter = legacyExtensions;
		const toolCalls = new ExtensionToolCalls({
			runner: () => ({
				tools: () => adapter.replTools,
				execute: (name, toolCallId, params, signal, onUpdate) =>
					adapter.executeTool(name, toolCallId, params, signal, onUpdate),
			}),
			store: createSessionModuleStore(session, "tool-calls"),
			holdActivity: () => holdActivity?.() ?? (() => {}),
			onEnd: (end) => {
				const event = toolEvent(end);
				if (event) events?.publish(event);
			},
		});
		const removeToolCallListeners = recordNativeToolCalls(harness, toolCalls);
		host = new NativeRlmHost(harness, lane, {
			store: createSessionTaskStore(session),
			definitionStore: createSessionDefinitionStore(session),
			// Budgets apply per root turn: each main-lane run opens a fresh wall, admission and cost window.
			usage,
			frames: inference.executor,
			rootTurns: true,
			// Subagents' declared file changes are checked against snapshots of the session's working directory.
			workspace: options.metadata.cwd,
			// Read per spawn: `ultron claude`'s child model, else `rlm.childModel` (/settings → Models).
			childModel: () => external?.childModel ?? settingsManager.getRlmModelSettings().childModel,
			...(external?.externalChild === undefined ? {} : { externalChild: external.externalChild }),
			...(external?.rootSpawn === undefined ? {} : { rootSpawn: external.rootSpawn }),
			...(external?.rootFinish === undefined ? {} : { rootFinish: external.rootFinish }),
			pinLane: (lane, holder) => rlmTool.pin(lane, holder),
			unpinLane: (lane, holder) => rlmTool.unpin(lane, holder),
			onTaskEnd: (task, info) => contextControl.taskEnded(task, info),
			onDetachedEnd: (end) => {
				if (!end.awaited) events?.publish(taskEvent(end));
			},
			statusExtras: (caller) => ({
				jobs: shellJobs
					.list()
					.filter((job) => caller.lane === "main" || job.lane === caller.lane)
					.slice(0, 20)
					.map((job) => ({
						id: job.id,
						lane: job.lane,
						command: job.command.slice(0, 200),
						status: job.status,
						exitCode: job.exitCode,
						startedAt: job.startedAt,
						endedAt: job.endedAt,
						outputBytes: job.outputBytes,
						// Last output, bounded, for the TUI graph's job details.
						tail: job.tail.slice(-160),
					})),
				// Extension and native tool calls, for the graph's tool nodes.
				toolCalls: toolCalls
					.list()
					.filter((call) => caller.lane === "main" || call.lane === caller.lane)
					.slice(0, 40)
					.map((call) => ({
						id: call.id,
						lane: call.lane,
						source: call.source,
						name: call.name,
						label: call.label,
						status: call.status,
						startedAt: call.startedAt,
						endedAt: call.endedAt,
						input: call.input,
						...(call.preview === undefined ? {} : { preview: call.preview }),
						...(call.error === undefined ? {} : { error: call.error }),
					})),
			}),
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
				shellJobs.module,
				toolCalls.module,
				cellHints.module,
				contextControl.module,
				inference.module,
				createFamilyModule({ store: createSessionModuleStore(session, "family") }),
				createProgressModule({ store: createSessionModuleStore(session, "progress") }),
				createScheduleModule({ store: createSessionModuleStore(session, "schedules") }),
				createInstanceModule({ store: createSessionModuleStore(session, "instances") }),
				// Grants stay dormant (enforce: false) so nothing ever prompts or blocks by default.
				createGrantModule({ store: createSessionModuleStore(session, "grants") }),
				createReleaseGateModule({ store: createSessionModuleStore(session, "release-gates") }),
				createSkillModule({
					store: createSessionModuleStore(session, "skills"),
					// Tested Python skills: each proposal's test runs in a fresh kernel with no host capabilities.
					code: new CodeSkills({
						...(jev ? { jev } : {}),
						createTestKernel: (cwd, env) =>
							new UltronRlmKernel(
								cwd,
								() => {
									throw new Error("Host requests are unavailable while a code skill test runs");
								},
								undefined,
								undefined,
								env,
							),
					}),
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
		const activeHost = host;
		// The external root's inbox; the controller is completed below, once memory and Loki are known.
		let externalSink: ((events: RuntimeEvent[]) => void) | undefined;
		events = new AsyncEventDispatcher({
			harness,
			host: activeHost,
			...(external === undefined ? {} : { rootSink: (batch: RuntimeEvent[]) => externalSink?.(batch) }),
			enabled: asyncEventsEnabled(process.env.ULTRON_ASYNC_EVENTS),
			maxRuns: maxEventRunsFromEnv(process.env.ULTRON_ASYNC_EVENTS_MAX_RUNS),
			holdActivity: () => holdActivity?.() ?? (() => {}),
			// A completion re-invokes the root only within the budget of the request that started the work.
			refuse: async (rootId) => {
				const exhausted = await usage.turnBudgetExhausted(rootId);
				if (exhausted) return exhausted;
				const status = await usage.status(rootId);
				if (status.remainingWallMs === 0) return `wall budget of ${rootId} exhausted`;
				const cap = status.cost.maxCostUsd;
				if (cap !== null && (status.cost.spentUsd >= cap || status.cost.unknownPricedCalls > 0))
					return `cost cap of ${rootId} reached or unenforceable`;
				return undefined;
			},
			// Esc on a root turn stops the shell jobs and extension tool calls it started.
			onRootAborted: (rootId) =>
				Promise.all([shellJobs.cancelRoot(rootId), toolCalls.cancelRoot(rootId)]).then(() => {}),
		});
		const removeAsyncEvents = events.install();
		// Top-level work admitted after the turn ends (a schedule firing) gets a root of its own.
		const removeRootTurnListener = harness.events.on("run_end", (event) => {
			if (event.lane === "main") host?.endRootTurn(event.runId);
		});
		// Automatic per-turn memory for the root lane (ULTRON_AUTO_MEMORY=off|recall|on, default on). It needs
		// both Hindsight and Jev: without Jev nothing could pass the gate, so it stays out of the way.
		const autoMemoryMode = autoMemoryModeFromEnv(process.env.ULTRON_AUTO_MEMORY);
		const autoMemory =
			autoMemoryMode !== "off" && nativeServices.memory && jev
				? new AutoMemory({
						mode: autoMemoryMode,
						scope: autoMemoryScopeFromEnv(process.env.ULTRON_AUTO_MEMORY_SCOPE),
						memory: nativeServices.memory,
						sessionId: options.metadata.id,
						holdActivity: () => holdActivity?.() ?? (() => {}),
						...legacyRecallOption(
							hindsightUrl(process.env.ULTRON_HINDSIGHT_URL, settingsManager.getHindsightUrl()),
						),
					})
				: undefined;
		// An external root's memory runs from its hooks (ExternalRootController), not from runs of the root lane.
		const removeAutoMemory = external === undefined ? (autoMemory?.install(harness) ?? (() => {})) : () => {};
		// Per-root turn, token and cost limits: every model response on any lane (the root's own, sub-agents, frames,
		// typed agents, background jobs) is charged, tokens and cost, to the root that admitted its lane. Once a
		// root's tree is spent its tool calls are refused with the limit error, its runs stop, and no lane of the tree
		// gets another model request (no-ops without a limit; the tally still feeds `agents.status`).
		// A response is recorded in the after_response hook, which the drive awaits before it prepares the next
		// request, so a spent tree can never slip another request through. (A message_end event listener raced the
		// next request's check under load.) The request check still waits for any record in flight.
		const pendingTurnRecords = pendingSet();
		const removeBudgetTurnListener = harness.hooks.on("after_response", async (event) => {
			const reported = (event.message as { usage?: { totalTokens?: number; cost?: { total?: number } } }).usage;
			const record = usage
				.recordTurn(host?.usageRootForLane(event.lane, event.runId), {
					totalTokens: reported?.totalTokens ?? null,
					cost: reported?.cost?.total ?? null,
				})
				.catch(() => {});
			pendingTurnRecords.track(record);
			await record;
			return undefined;
		});
		const removeBudgetToolHook = harness.hooks.on("before_tool", async (event) => {
			const reason = await usage.turnBudgetExhausted(host?.usageRootForLane(event.lane, event.runId));
			return reason === undefined ? undefined : { block: { reason, terminate: true } };
		});
		// Tool calls the harness rejects before `before_tool` (an unknown tool, invalid arguments) would otherwise
		// loop past the limit, so a spent root's next model request is refused as well.
		const removeBudgetRequestHook = harness.hooks.on("before_request", async (event) => {
			if (event.step !== "assistant") return undefined;
			await pendingTurnRecords.settled();
			const reason = await usage.turnBudgetExhausted(host?.usageRootForLane(event.lane, event.runId));
			return reason === undefined ? undefined : { block: { reason } };
		});
		// Brake for open-ended research loops on the root agent (ULTRON_TOOL_ROUNDS_NUDGE, 0 disables). While
		// subagents or tasks the root started are running it says to wait for them instead of checking on them.
		const nudger = new ToolRoundNudger(
			toolRoundsNudgeFromEnv(process.env.ULTRON_TOOL_ROUNDS_NUDGE),
			(message) => lane.steer(message, undefined, BACKGROUND_CONTEXT),
			{ asyncEvents: asyncEventsEnabled(process.env.ULTRON_ASYNC_EVENTS) },
		);
		// After a long streak of successful tool rounds, suggest saving the procedure as a code skill
		// (ULTRON_SKILL_NUDGE, 0 disables).
		const skillNudger = new SkillExtractionNudger(skillNudgeFromEnv(process.env.ULTRON_SKILL_NUDGE), (message) =>
			lane.steer(message, undefined, BACKGROUND_CONTEXT),
		);
		const removeNudgeTurnListener = harness.events.on("turn_end", (event) => {
			if (event.lane !== "main") return;
			const toolCalls = event.message.content.filter((part) => part.type === "toolCall").length;
			// Root-started subagents and tasks still running (those of a root the user aborted are being cancelled).
			const running = host?.pendingRootNotifications((rootId) => events?.rootAborted(rootId) ?? false) ?? 0;
			nudger.turnEnded(event.runId, toolCalls, running);
			skillNudger.turnEnded(
				event.runId,
				toolCalls,
				event.toolResults.filter((result) => result.isError).length,
				running,
			);
		});
		const removeNudgeRunListener = harness.events.on("run_end", (event) => {
			// Every lane's stuck-loop detection starts over with its next run.
			cellHints.runEnded(event.lane);
			if (event.lane !== "main") return;
			nudger.runEnded(event.runId);
			skillNudger.runEnded(event.runId);
		});
		const externalRoot =
			external === undefined
				? undefined
				: new ExternalRootController({
						execute: (code, invocation, context) =>
							rlmTool.execute(
								invocation.invocationId,
								{ code },
								() => {},
								{ env: executionEnv },
								invocation,
								context,
							),
						host: {
							beginRootTurn: (runId) => activeHost.beginRootTurn(runId),
							endRootTurn: (runId) => activeHost.endRootTurn(runId),
							rootIdOfRun: (runId) => activeHost.rootIdOfRun(runId),
							pendingRootNotifications: () => activeHost.pendingRootNotifications(),
						},
						usage: { turnBudgetExhausted: (rootId) => usage.turnBudgetExhausted(rootId) },
						hints: cellHints,
						fileHooks,
						...(autoMemory === undefined ? {} : { autoMemory }),
						lokiNotice: loki.notice,
						...(loki.context === undefined ? {} : { lokiContext: loki.context }),
						toolRoundsNudge: toolRoundsNudgeFromEnv(process.env.ULTRON_TOOL_ROUNDS_NUDGE),
						skillNudge: skillNudgeFromEnv(process.env.ULTRON_SKILL_NUDGE),
						asyncEvents: asyncEventsEnabled(process.env.ULTRON_ASYNC_EVENTS),
					});
		externalSink = externalRoot?.sink;
		const currentActiveToolNames = await lane.getActiveTools(TODO_CONTEXT);
		if (
			currentActiveToolNames.length !== extensionActiveToolNames.length ||
			currentActiveToolNames.some((name, index) => name !== extensionActiveToolNames[index])
		) {
			await lane.setActiveTools(extensionActiveToolNames, TODO_CONTEXT);
		}
		return {
			harness,
			model: `${resolved.model.provider}/${resolved.model.id}`,
			hostRequest: (type, payload, context, caller) => activeHost.handle(type, payload, context, caller),
			...(externalRoot === undefined ? {} : { externalRoot }),
			closeRlm: async () => {
				await claudeLanes?.close();
				removeLokiListeners();
				removeRootTurnListener();
				removeToolCallListeners();
				removeAsyncEvents();
				await events?.close();
				removeContextControl();
				removeInferenceHooks();
				removeAutoMemory();
				await autoMemory?.settle();
				removeNudgeTurnListener();
				removeBudgetTurnListener();
				removeBudgetToolHook();
				removeBudgetRequestHook();
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
				// Root-owned work whose completion may still re-invoke the root (print clients wait for it).
				if (request === "async.pending") {
					if (!events?.enabled) return { pending: false, jobs: 0, tasks: 0, events: 0 };
					const aborted = (rootId: string | undefined) => events?.rootAborted(rootId) ?? false;
					const counts = {
						jobs: shellJobs.running("main", aborted),
						tools: toolCalls.running("main", aborted),
						tasks: host?.pendingRootNotifications(aborted) ?? 0,
						events: events.pendingFor("main"),
						// A run an event just started, before its start reaches the client.
						running: (await lane.inspectExecution(context)).current === null ? 0 : 1,
					};
					return {
						pending: counts.jobs + counts.tools + counts.tasks + counts.events + counts.running > 0,
						...counts,
					};
				}
				if (request === "jev.decisions") {
					return {
						available: {
							jev: nativeJev !== undefined,
							hindsight:
								hindsightUrl(process.env.ULTRON_HINDSIGHT_URL, settingsManager.getHindsightUrl()) !== undefined,
						},
						capacity: JEV_DECISION_CAPACITY,
						// The gates' cut-offs, so a view can show a score against the line it had to clear.
						thresholds: { recall: JEV_RECALL_THRESHOLD, keep: AUTOMATIC_KEEP_THRESHOLD },
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
			extensionUI,
			extensionSessionEvents: {
				beforeFork: async (entryId, position) =>
					(await legacyExtensions?.beforeFork(entryId, position)) ?? { cancelled: false },
				forked: (targetSessionFile) => legacyExtensions?.forked(targetSessionFile),
			},
			resourceSourceInfo: () =>
				new Map([
					...resourceLoader.getPrompts().prompts.map((template) => [template.name, template.sourceInfo] as const),
					...resourceLoader.getSkills().skills.map((skill) => [`skill:${skill.name}`, skill.sourceInfo] as const),
				]),
			facetLoader: createSessionPluginFacetLoader(options.pluginManifestPaths),
		};
	} catch (error) {
		try {
			await claudeLanes?.close();
			await legacyExtensions?.close();
			await harness.close(TODO_CONTEXT);
		} catch (cleanupError) {
			throw new AggregateError([error, cleanupError], "Session worker model selection and cleanup failed");
		}
		throw error;
	}
}

/**
 * Per-turn guard time starts over with each root turn, and Loki's one-time setup note (created and committed, or why
 * not) opens the first root turn that follows it.
 */
function installLokiNotice(
	harness: AgentHarnessInstance,
	fileHooks: FileHooks,
	notice: Promise<string | undefined>,
): () => void {
	let text: string | undefined;
	// True once the note was shown, or once it is known there is none.
	let shown = false;
	const known = notice.then(
		(value) => {
			text = value;
			if (value === undefined) shown = true;
		},
		() => {
			shown = true;
		},
	);
	const removers = [
		harness.events.on("run_start", (event) => {
			if (event.lane === "main") fileHooks.beginTurn();
		}),
		harness.hooks.on("before_run", async (event) => {
			if (event.lane !== "main" || shown) return undefined;
			// A commit still running (a slow hook) is waited for briefly; otherwise the note opens a later turn.
			await Promise.race([known, new Promise((done) => setTimeout(done, 5_000).unref?.())]);
			if (text === undefined) return undefined;
			shown = true;
			return {
				messages: [
					{ role: "custom", customType: "loki", content: text, display: true, details: {}, timestamp: Date.now() },
				],
			};
		}),
	];
	return () => {
		for (const remove of removers) remove();
	};
}

/** Read-only recall from the Pi extension's Hindsight bank, when Hindsight and that bank are configured. */
function legacyRecallOption(url: string | undefined): { legacyRecall?: ReturnType<typeof createLegacyRecall> } {
	const bank = legacyBankFromEnv(process.env.ULTRON_HINDSIGHT_LEGACY_BANK);
	return url && bank ? { legacyRecall: createLegacyRecall(url, bank) } : {};
}

/** The completion event of a shell job the model is not already waiting on; undefined when none is due. */
function jobEvent(end: ShellJobEnd): RuntimeEvent | undefined {
	if (end.awaited || end.rootAborted) return undefined;
	return {
		kind: "job_done",
		id: end.job.id,
		status: end.job.status,
		summary: jobSummary(end.job),
		fetch: `await rlm.job("${end.job.id}")`,
		lane: end.job.lane,
		...(end.job.rootId === null ? {} : { rootId: end.job.rootId }),
	};
}

/** The completion event of a detached REPL tool call the model is not already waiting on. */
function toolEvent(end: ToolCallEnd): RuntimeEvent | undefined {
	if (end.awaited || end.rootAborted || end.call.source !== "repl") return undefined;
	return {
		kind: "tool_done",
		id: end.call.id,
		status: end.call.status,
		summary: toolCallSummary(end.call),
		fetch: `await tools.result("${end.call.id}")`,
		lane: end.call.lane,
		...(end.call.rootId === null ? {} : { rootId: end.call.rootId }),
	};
}

/** Record every tool the model calls directly, except the REPL itself, for the graph. */
function recordNativeToolCalls(harness: AgentHarnessInstance, calls: ExtensionToolCalls): () => void {
	const removers = [
		harness.events.on("tool_start", (event) => {
			if (event.toolName !== "rlm") calls.nativeStarted(event);
		}),
		harness.events.on("tool_update", (event) => {
			if (event.toolName !== "rlm") calls.nativeUpdated(event);
		}),
		harness.events.on("tool_end", (event) => {
			if (event.toolName !== "rlm") calls.nativeEnded(event);
		}),
		harness.events.on("run_end", (event) => calls.nativeInterrupted(event.lane)),
	];
	return () => {
		for (const remove of removers) remove();
	};
}

/** Extension tools as the extensions registered them while loading (before the worker's adapter exists). */
function loadedExtensionTools(resourceLoader: ResourceLoader): ExtensionToolInfo[] {
	const byName = new Map<string, ExtensionToolInfo>();
	for (const extension of resourceLoader.getExtensions().extensions)
		for (const { definition } of extension.tools.values())
			if (!byName.has(definition.name) && definition.name !== "rlm" && definition.name !== "ipython")
				byName.set(definition.name, {
					name: definition.name,
					...(definition.label === undefined ? {} : { label: definition.label }),
					description: definition.description,
					parameters: definition.parameters,
				});
	return [...byName.values()];
}

/** MCP server names from the pi-mcp-adapter's gateway description ("Servers: a, b"). */
function mcpServerNames(tools: readonly ExtensionToolInfo[]): string[] {
	const gateway = tools.find((tool) => tool.name === "mcp");
	const line = gateway?.description.match(/^Servers: (.+)$/m)?.[1];
	return line === undefined
		? []
		: line
				.split(",")
				.map((name) => name.trim())
				.filter(Boolean);
}

/** The completion event of a detached task (`rlm.spawn`, `agents.spawn`, `background.start`). */
function taskEvent(end: DetachedTaskEnd): RuntimeEvent {
	const result = end.task.result;
	const value = result?.value;
	const detail =
		result?.status === "succeeded"
			? typeof value === "string"
				? value
				: JSON.stringify(value ?? null)
			: (result?.error ?? "");
	// A subagent's verdict and its check lead the summary: `[passed, verified] ...`, `[unverified] ...`.
	const tag = result === undefined ? undefined : verdictTag(result);
	return {
		kind: end.kind,
		id: end.task.id,
		status: result?.status ?? end.task.state,
		summary: boundedSummary(`${end.task.definition}: ${tag === undefined ? "" : `${tag} `}${detail}`),
		fetch: end.fetch,
		lane: end.ownerLane,
		...(end.rootId === undefined ? {} : { rootId: end.rootId }),
	};
}

/** Promises still running, awaitable as a group (for records a later check must see). */
function pendingSet(): { track(promise: Promise<unknown>): void; settled(): Promise<void> } {
	const pending = new Set<Promise<unknown>>();
	return {
		track(promise) {
			pending.add(promise);
			void promise.finally(() => pending.delete(promise));
		},
		async settled() {
			await Promise.all([...pending]);
		},
	};
}

/**
 * Hindsight memory is on by default against a local server, as in the Pi Jev extension.
 * ULTRON_HINDSIGHT_URL overrides the address, then the `hindsightUrl` setting (written by `ultron setup`);
 * "off" (or "none"/"0"/"false") disables memory.
 */
export function hindsightUrl(configured: string | undefined, setting?: string): string | undefined {
	const value = configured?.trim() ? configured.trim() : setting?.trim();
	if (!value) return DEFAULT_HINDSIGHT_URL;
	return ["off", "none", "0", "false"].includes(value.toLowerCase()) ? undefined : value;
}

export function runSessionWorkerProcess(args: readonly string[]): Promise<void> {
	return runSessionWorkerWithHarness(args, createUltronRuntime);
}

if (isDirectInternalProcessEntry(import.meta.url)) {
	const role = consumeInternalProcessRole();
	if (role !== "session-worker") {
		throw new Error("Session worker entrypoint requires an internal session-worker invocation");
	}
	void runSessionWorkerProcess(process.argv.slice(2)).catch(() => process.exit(1));
}
