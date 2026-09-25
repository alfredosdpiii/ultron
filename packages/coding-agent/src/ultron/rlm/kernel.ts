import { type ChildProcessWithoutNullStreams, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { processSnapshotKey, sha256Hex, signSnapshot, verifySnapshot } from "./snapshot-auth.ts";
import {
	cgroupOomKills,
	cgroupScopeCommand,
	kernelTreeMemoryLimit,
	killProcessTree,
	processCgroupDir,
	processStat,
	type TreeMemoryBackend,
	treeMemoryBackend,
	treeMemoryUsage,
	watchdogPollMs,
} from "./tree-memory.ts";

const CREDENTIAL_NAME =
	/(API_?KEY|ACCESS_?KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|PRIVATE_?KEY|AUTH|COOKIE|SESSION_?KEY|WEBHOOK)/i;

/**
 * Environment for model-written Python. Credentials and the worker's control channel are
 * removed so the kernel receives no ambient authority; host capabilities go through host
 * requests instead. ULTRON_RLM_ENV_ALLOW lists names (comma-separated) to pass through anyway.
 */
export function kernelEnvironment(source: NodeJS.ProcessEnv): Record<string, string> {
	const allowed = new Set(
		(source.ULTRON_RLM_ENV_ALLOW ?? "")
			.split(",")
			.map((name) => name.trim())
			.filter(Boolean),
	);
	const environment: Record<string, string> = {};
	for (const [name, value] of Object.entries(source)) {
		if (value === undefined) continue;
		const control = name.startsWith("PI_SESSION_WORKER_") || name.startsWith("ULTRON_SESSION_WORKER_");
		if (!allowed.has(name) && (control || CREDENTIAL_NAME.test(name))) continue;
		environment[name] = value;
	}
	return environment;
}

/**
 * Per-kernel resource limits; 0 disables one. These are resource limits, not isolation.
 * - maxMemoryMb: RLIMIT_DATA (heap and private writable mappings) for the kernel and, inherited, for
 *   every process a cell spawns. Each process gets the limit; it is not an aggregate over the tree.
 *   RLIMIT_DATA rather than RLIMIT_AS, so runtimes that reserve large address ranges (V8, Go) still start.
 * - maxCpuSeconds: CPU time per cell, not per process: kernels are long-lived and would otherwise
 *   eventually die of a lifetime budget, and cells legitimately wait on subagents for hours without
 *   using CPU. Python raises RlmCpuLimitExceeded in the cell; a cell stuck in C code or swallowing the
 *   error is killed by the host after a grace period. Spawned processes inherit the soft RLIMIT_CPU.
 */
export type KernelResourceLimits = { maxMemoryMb: number; maxCpuSeconds: number };

export const DEFAULT_KERNEL_LIMITS: Readonly<KernelResourceLimits> = { maxMemoryMb: 4096, maxCpuSeconds: 1800 };

function limitValue(value: string | undefined, fallback: number): number {
	if (value === undefined || value.trim() === "") return fallback;
	return /^\s*[0-9]+\s*$/.test(value) ? Number(value) : fallback;
}

/** Limits from ULTRON_RLM_MAX_MEMORY_MB and ULTRON_RLM_MAX_CPU_SECONDS, then explicit overrides. */
export function kernelResourceLimits(
	source: NodeJS.ProcessEnv = process.env,
	overrides: Partial<KernelResourceLimits> = {},
): KernelResourceLimits {
	const limits = {
		maxMemoryMb:
			overrides.maxMemoryMb ?? limitValue(source.ULTRON_RLM_MAX_MEMORY_MB, DEFAULT_KERNEL_LIMITS.maxMemoryMb),
		maxCpuSeconds:
			overrides.maxCpuSeconds ?? limitValue(source.ULTRON_RLM_MAX_CPU_SECONDS, DEFAULT_KERNEL_LIMITS.maxCpuSeconds),
	};
	for (const [name, value] of Object.entries(limits)) {
		if (!Number.isSafeInteger(value) || value < 0) throw new Error(`RLM ${name} must be a non-negative integer`);
	}
	return limits;
}

/** Runtime exit codes (runtime.py) for limits it could not report in-band. */
const EXIT_MEMORY = 86;
const EXIT_CPU = 87;

function memoryLimitMessage(limits: KernelResourceLimits): string {
	return `RLM kernel exceeded its memory limit (${limits.maxMemoryMb} MiB per process, ULTRON_RLM_MAX_MEMORY_MB) and was stopped; the next cell starts a fresh kernel without earlier Python state`;
}

function treeMemoryLimitMessage(capMb: number, backend: TreeMemoryBackend): string {
	const how = backend === "cgroup" ? "cgroup MemoryMax" : "host watchdog";
	return `RLM kernel process tree exceeded its memory limit (${capMb} MiB total across the kernel and every process its cells started, ULTRON_RLM_MAX_TREE_MEMORY_MB, enforced by ${how}) and was stopped; the next cell starts a fresh kernel without earlier Python state`;
}

function cpuLimitMessage(limits: KernelResourceLimits): string {
	return `RLM kernel exceeded its CPU limit (${limits.maxCpuSeconds} CPU-seconds per cell, ULTRON_RLM_MAX_CPU_SECONDS) and was stopped; the next cell starts a fresh kernel without earlier Python state`;
}

/** CPU seconds (user + system) of one Linux process, or undefined when unavailable. */
function processCpuSeconds(pid: number): number | undefined {
	try {
		const stat = readFileSync(`/proc/${pid}/stat`, "utf8");
		// After the command name: state(3) ppid(4) ... utime(14) stime(15), clock ticks (USER_HZ = 100 on Linux).
		const fields = stat.slice(stat.lastIndexOf(")") + 2).split(" ");
		return (Number(fields[11]) + Number(fields[12])) / 100;
	} catch {
		return undefined;
	}
}

/** The signal covers the active cell, including failure, completion and shutdown. */
export type KernelHostHandler = (
	type: string,
	payload: Record<string, unknown>,
	signal?: AbortSignal,
) => Promise<unknown> | unknown;

/** Names are saved only as constrained data; every other user name is listed with a reason. */
export type KernelSnapshotReport = {
	saved: string[];
	skipped: string[];
	reasons: Record<string, string>;
	sha256: string;
	/** sha256 of the unsigned file content the host signed. */
	contentSha256?: string;
};

/** `skipped` repeats the names the snapshot could not restore; they are absent, not stale. */
export type KernelRestoreReport = {
	restored: string[];
	missing: boolean;
	skipped: string[];
	reasons: Record<string, string>;
};

export type KernelExecutionResult = {
	status: "ok" | "error";
	stdout: string;
	stderr: string;
	result?: string;
	error?: { ename: string; evalue: string; traceback: string[] };
	snapshot?: KernelSnapshotReport;
	restore?: KernelRestoreReport;
};

type Deferred<T> = {
	promise: Promise<T>;
	settled: boolean;
	resolve: (value: T) => void;
	reject: (error: Error) => void;
};
type Frame = Record<string, unknown>;
type ExecutionWaiter = Deferred<KernelExecutionResult> & {
	state: KernelExecutionResult;
	controller: AbortController;
	hostRequests: Set<string>;
};
type Operation = Deferred<KernelExecutionResult> & {
	running: boolean;
	cell?: ExecutionWaiter;
};
type Generation = {
	child: ChildProcessWithoutNullStreams;
	ownsProcessGroup: boolean;
	started: Deferred<void>;
	exited: Deferred<void>;
	pending: Map<string, ExecutionWaiter>;
	activeCell?: ExecutionWaiter;
	hostRequests: Map<string, ExecutionWaiter>;
	readyReceived: boolean;
	initialized: boolean;
	failure?: Error;
	startupTimer?: ReturnType<typeof setTimeout>;
	restorePath?: string;
	cpuTimer?: ReturnType<typeof setInterval>;
	cellCpuStart?: number;
	treeBackend: TreeMemoryBackend;
	/** cgroup v2 directory of the kernel's scope, resolved once the runtime is up. */
	cgroupDir?: string;
	oomKillsAtStart?: number;
	memoryTimer?: ReturnType<typeof setTimeout>;
	/** Descendants seen by watchdog scans (pid to start time), killed with the tree even if they left it. */
	knownDescendants: Map<number, number>;
	buffer?: Buffer;
	bufferedBytes: number;
	protocolBytes: number;
};

// Limits apply to wire bytes, including JSON overhead, not JavaScript characters.
const MAX_FRAME_BYTES = 1024 * 1024;
const MAX_PROTOCOL_BYTES = 4 * MAX_FRAME_BYTES;
const MAX_HOST_REQUESTS = 16;
const STARTUP_TIMEOUT_MS = 10_000;
const SHUTDOWN_TIMEOUT_MS = 1_000;
const PLACEHOLDER_ID = "00000000-0000-0000-0000-000000000000";

function deferred<T>(): Deferred<T> {
	let resolve!: (value: T) => void;
	let reject!: (error: Error) => void;
	const promise = new Promise<T>((resolvePromise, rejectPromise) => {
		resolve = resolvePromise;
		reject = rejectPromise;
	});
	const value: Deferred<T> = {
		promise,
		settled: false,
		resolve(result) {
			if (value.settled) return;
			value.settled = true;
			resolve(result);
		},
		reject(error) {
			if (value.settled) return;
			value.settled = true;
			reject(error);
		},
	};
	return value;
}

function asError(error: unknown): Error {
	return error instanceof Error ? error : new Error(String(error));
}

function abortError(signal: AbortSignal): Error {
	if (signal.reason instanceof Error) return signal.reason;
	const error = new Error(signal.reason == null ? "RLM execution aborted" : String(signal.reason));
	error.name = "AbortError";
	return error;
}

function frameString(value: unknown): string {
	if (typeof value === "string") return value;
	if (value == null) return "";
	try {
		const encoded = JSON.stringify(value);
		return encoded === undefined ? String(value) : encoded;
	} catch {
		return String(value);
	}
}

function stringList(value: unknown): string[] {
	return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
}

function stringRecord(value: unknown): Record<string, string> {
	const result: Record<string, string> = {};
	if (!value || typeof value !== "object" || Array.isArray(value)) return result;
	for (const [key, item] of Object.entries(value)) if (typeof item === "string") result[key] = item;
	return result;
}

function snapshotReport(value: unknown): KernelSnapshotReport | undefined {
	if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
	const report = value as Record<string, unknown>;
	return {
		saved: stringList(report.saved),
		skipped: stringList(report.skipped),
		reasons: stringRecord(report.reasons),
		sha256: typeof report.sha256 === "string" ? report.sha256 : "",
		...(typeof report.content_sha256 === "string" ? { contentSha256: report.content_sha256 } : {}),
	};
}

function removeQuietly(path: string): void {
	try {
		rmSync(path, { force: true });
	} catch {
		/* The directory may not exist (the kernel failed to write), or the file is already gone. */
	}
}

function snapshotError(message: string): KernelExecutionResult {
	return {
		status: "error",
		stdout: "",
		stderr: "",
		error: { ename: "SnapshotIntegrityError", evalue: message, traceback: [] },
	};
}

function restoreReport(value: unknown): KernelRestoreReport | undefined {
	if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
	const report = value as Record<string, unknown>;
	return {
		restored: stringList(report.restored),
		missing: report.missing === true,
		skipped: stringList(report.skipped),
		reasons: stringRecord(report.reasons),
	};
}

function encodeFrame(frame: Frame): string {
	let line: string | undefined;
	try {
		line = JSON.stringify(frame);
	} catch (error) {
		throw new Error(`RLM kernel cannot encode outbound frame: ${asError(error).message}`);
	}
	if (line === undefined) throw new Error("RLM kernel cannot encode outbound frame");
	if (Buffer.byteLength(line, "utf8") > MAX_FRAME_BYTES) {
		throw new Error(`RLM kernel outbound frame exceeds 1 MiB (${MAX_FRAME_BYTES} bytes)`);
	}
	return `${line}\n`;
}

export type RlmKernelOptions = {
	cwd: string;
	runtimePath: string;
	python?: string;
	snapshotPath?: string;
	/** Bounds initialization, including restore, not user execution. */
	startupTimeoutMs?: number;
	/** Overrides ULTRON_RLM_MAX_MEMORY_MB / ULTRON_RLM_MAX_CPU_SECONDS; 0 disables a limit. */
	limits?: Partial<KernelResourceLimits>;
	/**
	 * Memory cap in MiB over the kernel and all its descendants; overrides ULTRON_RLM_MAX_TREE_MEMORY_MB, whose
	 * default is twice the per-process limit. 0 disables it.
	 */
	maxTreeMemoryMb?: number;
	/** How the tree cap is enforced; default ULTRON_RLM_TREE_MEMORY_BACKEND or "auto" (cgroup scope, else watchdog). */
	treeMemoryBackend?: "auto" | "cgroup" | "watchdog" | "off";
	/**
	 * HMAC key that signs and verifies snapshots in this host process; the kernel never receives it.
	 * Defaults to a per-process random key, so snapshots are then restorable only within this process.
	 */
	snapshotKey?: Uint8Array;
	/** Extra environment for the kernel process, applied after the credential filter (for example ULTRON_CODE_SKILLS_DIR). */
	env?: Readonly<Record<string, string>>;
};

export class RlmKernel {
	private generation?: Generation;
	private readonly operations = new Set<Operation>();
	private executionQueue: Promise<void> = Promise.resolve();
	private shutdownPromise?: Promise<void>;
	private closed = false;
	private autoRestore = true;
	private stderr = "";

	private readonly options: RlmKernelOptions;
	private readonly hostHandler: KernelHostHandler;
	private readonly limits: KernelResourceLimits;
	private readonly treeMemoryMb: number;
	private readonly snapshotKey: Uint8Array;
	constructor(options: RlmKernelOptions, hostHandler: KernelHostHandler) {
		this.options = options;
		this.hostHandler = hostHandler;
		this.limits = kernelResourceLimits(process.env, options.limits);
		this.treeMemoryMb = kernelTreeMemoryLimit(process.env, this.limits.maxMemoryMb, options.maxTreeMemoryMb);
		this.snapshotKey = options.snapshotKey ?? processSnapshotKey();
		const timeout = options.startupTimeoutMs ?? STARTUP_TIMEOUT_MS;
		if (!Number.isFinite(timeout) || timeout <= 0 || timeout > 2_147_483_647) {
			throw new Error("RLM startupTimeoutMs must be a positive, finite timer duration");
		}
	}

	get isRunning(): boolean {
		return Boolean(this.generation?.initialized && !this.closed);
	}

	get stderrTail(): string {
		return this.stderr;
	}

	start(): Promise<void> {
		return this.ensureStarted();
	}

	private ensureStarted(restoreSnapshot = true): Promise<void> {
		if (this.closed) return Promise.reject(new Error("RLM kernel is shut down"));
		if (this.generation) return this.generation.started.promise;
		let child: ChildProcessWithoutNullStreams;
		const ownsProcessGroup = process.platform !== "win32";
		const treeBackend = treeMemoryBackend(this.treeMemoryMb, this.options.treeMemoryBackend);
		try {
			const python =
				this.options.python ??
				process.env.ULTRON_PYTHON ??
				(process.platform === "linux" && existsSync("/usr/bin/python3") ? "/usr/bin/python3" : "python3");
			const command =
				treeBackend === "cgroup"
					? [...cgroupScopeCommand(this.treeMemoryMb), python, this.options.runtimePath]
					: [python, this.options.runtimePath];
			child = spawn(command[0]!, command.slice(1), {
				cwd: this.options.cwd,
				env: {
					...kernelEnvironment(process.env),
					NO_COLOR: "1",
					PYTHONDONTWRITEBYTECODE: "1",
					ULTRON_RLM_MAX_MEMORY_MB: String(this.limits.maxMemoryMb),
					ULTRON_RLM_MAX_CPU_SECONDS: String(this.limits.maxCpuSeconds),
					...this.options.env,
				},
				stdio: ["pipe", "pipe", "pipe"],
				detached: ownsProcessGroup,
			});
		} catch (error) {
			return Promise.reject(asError(error));
		}
		const generation: Generation = {
			child,
			ownsProcessGroup,
			started: deferred<void>(),
			exited: deferred<void>(),
			pending: new Map(),
			hostRequests: new Map(),
			readyReceived: false,
			initialized: false,
			restorePath: restoreSnapshot && this.autoRestore ? this.options.snapshotPath : undefined,
			buffer: Buffer.allocUnsafe(MAX_FRAME_BYTES),
			bufferedBytes: 0,
			protocolBytes: 0,
			treeBackend,
			knownDescendants: new Map(),
		};
		this.generation = generation;
		this.stderr = "";
		const timeout = this.options.startupTimeoutMs ?? STARTUP_TIMEOUT_MS;
		generation.startupTimer = setTimeout(() => {
			this.fail(generation, new Error(`RLM kernel startup timed out after ${timeout}ms${this.diagnostics()}`));
		}, timeout);
		child.stdout.on("data", (chunk: Buffer) => this.handleData(generation, chunk));
		child.stderr.on("data", (chunk: Buffer) => {
			if (this.accountProtocol(generation, chunk.length)) {
				this.stderr = (this.stderr + chunk.toString("utf8")).slice(-8000);
			}
		});
		child.once("error", (error) => {
			generation.exited.resolve(undefined);
			this.fail(generation, error);
		});
		child.once("exit", (code, signal) => {
			generation.exited.resolve(undefined);
			if (!this.isCurrent(generation)) return;
			// A cgroup OOM kill of the kernel itself arrives as SIGKILL; the scope still exists while its other
			// processes live, so its memory.events says whether the tree cap did it.
			const treeKill = (signal === "SIGKILL" || signal === "SIGTERM") && this.treeMemoryExceeded(generation);
			this.fail(
				generation,
				treeKill ? new Error(treeMemoryLimitMessage(this.treeMemoryMb, treeBackend)) : this.exitError(code, signal),
			);
		});
		this.startCpuWatchdog(generation);
		this.startTreeMemoryWatchdog(generation);
		child.stdout.once("end", () => {
			if (!this.isCurrent(generation)) return;
			// Output usually ends just before the exit event; wait briefly so the exit code
			// (for example a resource limit) is what gets reported.
			const timer = setTimeout(() => {
				if (this.isCurrent(generation)) {
					this.fail(generation, new Error(`RLM kernel stdout closed${this.diagnostics()}`));
				}
			}, 200);
			void generation.exited.promise.then(() => clearTimeout(timer));
		});
		// Streams may still report errors after termination. Keep handlers scoped
		// to their process so late EPIPE/exit/data events cannot affect a restart.
		for (const stream of [child.stdin, child.stdout, child.stderr]) {
			stream.on("error", (error) => this.fail(generation, error));
		}
		return generation.started.promise;
	}

	private exitError(code: number | null, signal: NodeJS.Signals | null): Error {
		if (code === EXIT_MEMORY) return new Error(memoryLimitMessage(this.limits));
		if (code === EXIT_CPU || signal === "SIGXCPU") return new Error(cpuLimitMessage(this.limits));
		// The host's own kills happen after fail(), so a SIGKILL seen here came from elsewhere.
		const hint = signal === "SIGKILL" ? "; the system out-of-memory killer may have stopped it" : "";
		return new Error(`RLM kernel exited (${signal ?? code ?? "unknown"})${hint}${this.diagnostics()}`);
	}

	/**
	 * Backstop for the per-cell CPU limit: Python cannot interrupt a cell stuck in C code, and cell code
	 * can catch the limit error or raise its own soft RLIMIT_CPU. Kill the kernel after a grace period.
	 */
	private startCpuWatchdog(generation: Generation): void {
		const limit = this.limits.maxCpuSeconds;
		const pid = generation.child.pid;
		if (limit <= 0 || process.platform !== "linux" || !pid) return;
		const grace = Math.max(2, Math.min(10, limit * 0.1));
		generation.cpuTimer = setInterval(() => {
			if (!generation.activeCell || generation.cellCpuStart === undefined) return;
			const used = processCpuSeconds(pid);
			if (used !== undefined && used - generation.cellCpuStart > limit + grace) {
				this.fail(generation, new Error(cpuLimitMessage(this.limits)));
			}
		}, 500);
		generation.cpuTimer.unref?.();
	}

	/**
	 * Whether the tree cap has been hit: for a cgroup scope, an OOM kill in its memory.events; for the watchdog,
	 * the summed resident memory of the kernel's tree is over the cap.
	 */
	private treeMemoryExceeded(generation: Generation): boolean {
		const pid = generation.child.pid;
		if (!pid) return false;
		if (generation.treeBackend === "watchdog")
			return this.watchdogTreeBytes(generation) > this.treeMemoryMb * 1024 * 1024;
		if (generation.treeBackend !== "cgroup") return false;
		if (!generation.cgroupDir) {
			// Before systemd-run execs the runtime the pid may still sit in the caller's cgroup.
			const dir = processCgroupDir(pid);
			if (!dir?.endsWith(".scope") || !generation.readyReceived) return false;
			generation.cgroupDir = dir;
			generation.oomKillsAtStart = cgroupOomKills(dir) ?? 0;
		}
		const kills = cgroupOomKills(generation.cgroupDir);
		return kills !== undefined && kills > (generation.oomKillsAtStart ?? 0);
	}

	/** Memory of the kernel's tree for the watchdog; remembers the descendants it saw (see knownDescendants). */
	private watchdogTreeBytes(generation: Generation): number {
		const pid = generation.child.pid;
		if (!pid) return 0;
		const { bytes, descendants } = treeMemoryUsage(pid);
		const known = generation.knownDescendants;
		for (const [seen, startTime] of known) {
			if (processStat(seen)?.startTime !== startTime) known.delete(seen);
		}
		for (const descendant of descendants) {
			const stat = processStat(descendant);
			if (stat) known.set(descendant, stat.startTime);
		}
		return bytes;
	}

	/**
	 * Polls the tree cap and SIGKILLs the whole tree (through fail) when it is exceeded. The scope's memory.events
	 * is read every 500 ms (the kernel enforces MemoryMax itself); the watchdog polls faster as the tree nears the cap.
	 */
	private startTreeMemoryWatchdog(generation: Generation): void {
		if (generation.treeBackend === "off") return;
		const capBytes = this.treeMemoryMb * 1024 * 1024;
		const poll = () => {
			if (!this.isCurrent(generation)) return;
			let delay = 500;
			if (generation.treeBackend === "watchdog") {
				const bytes = this.watchdogTreeBytes(generation);
				if (bytes > capBytes) {
					this.fail(generation, new Error(treeMemoryLimitMessage(this.treeMemoryMb, generation.treeBackend)));
					return;
				}
				delay = watchdogPollMs(bytes, capBytes);
			} else if (this.treeMemoryExceeded(generation)) {
				this.fail(generation, new Error(treeMemoryLimitMessage(this.treeMemoryMb, generation.treeBackend)));
				return;
			}
			generation.memoryTimer = setTimeout(poll, delay);
			generation.memoryTimer.unref?.();
		};
		generation.memoryTimer = setTimeout(poll, 500);
		generation.memoryTimer.unref?.();
	}

	private diagnostics(): string {
		return this.stderrTail ? `: ${this.stderrTail}` : "";
	}

	private isCurrent(generation: Generation): boolean {
		return this.generation === generation && !generation.failure;
	}

	private async completeStartup(generation: Generation): Promise<void> {
		try {
			if (generation.restorePath) {
				const result = await this.verifiedRestore(generation, generation.restorePath);
				if (result.status === "error") {
					throw new Error(`RLM kernel startup restore failed: ${result.error?.evalue ?? "unknown restore error"}`);
				}
			}
			if (!this.isCurrent(generation)) return;
			clearTimeout(generation.startupTimer);
			generation.initialized = true;
			generation.started.resolve(undefined);
		} catch (error) {
			this.fail(generation, asError(error));
		}
	}

	private accountProtocol(generation: Generation, bytes: number): boolean {
		if (!this.isCurrent(generation)) return false;
		generation.protocolBytes += bytes;
		if (generation.protocolBytes > MAX_PROTOCOL_BYTES) {
			this.fail(generation, new Error(`RLM kernel protocol output exceeds 4 MiB (${MAX_PROTOCOL_BYTES} bytes)`));
			return false;
		}
		return true;
	}

	private handleData(generation: Generation, chunk: Buffer): void {
		// Count all child output, including repeated valid frames and stderr.
		if (!this.accountProtocol(generation, chunk.length)) return;
		let offset = 0;
		while (offset < chunk.length && this.isCurrent(generation)) {
			const newline = chunk.indexOf(10, offset);
			const end = newline === -1 ? chunk.length : newline;
			const length = end - offset;
			// Bound raw bytes before copying, decoding or JSON.parse, including
			// unterminated frames and frames split across arbitrarily many chunks.
			if (generation.bufferedBytes + length > MAX_FRAME_BYTES) {
				this.fail(generation, new Error(`RLM kernel protocol frame exceeds 1 MiB (${MAX_FRAME_BYTES} bytes)`));
				return;
			}
			const buffer = generation.buffer!;
			chunk.copy(buffer, generation.bufferedBytes, offset, end);
			generation.bufferedBytes += length;
			if (newline === -1) return;
			try {
				const line = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(
					buffer.subarray(0, generation.bufferedBytes),
				);
				generation.bufferedBytes = 0;
				this.handleLine(generation, line);
			} catch (error) {
				this.fail(generation, new Error(`RLM kernel invalid protocol frame: ${asError(error).message}`));
			}
			offset = newline + 1;
		}
	}

	private handleLine(generation: Generation, line: string): void {
		let parsed: unknown;
		try {
			parsed = JSON.parse(line);
		} catch (error) {
			throw new Error(`invalid JSON: ${asError(error).message}`);
		}
		if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
			throw new Error("protocol frame must be a JSON object");
		}
		const frame = parsed as Frame;
		if (typeof frame.event !== "string") throw new Error("protocol frame is missing event");
		switch (frame.event) {
			case "ready":
				if (generation.readyReceived) throw new Error("duplicate ready event");
				if (frame.id !== undefined) throw new Error("ready event cannot have an id");
				generation.readyReceived = true;
				// Resolve the scope and its OOM baseline now that systemd-run has exec'd the runtime.
				if (generation.treeBackend === "cgroup") this.treeMemoryExceeded(generation);
				void this.completeStartup(generation);
				return;
			case "host_request":
				this.validateHostRequest(frame);
				void this.handleHostRequest(generation, frame);
				return;
			case "stdout":
			case "stderr":
			case "result":
			case "error":
			case "done":
				if (typeof frame.id !== "string" || !frame.id) {
					throw new Error(`${frame.event} event requires an id`);
				}
				if (
					frame.event === "done" &&
					frame.status !== undefined &&
					frame.status !== "ok" &&
					frame.status !== "error"
				) {
					throw new Error("done event has invalid status");
				}
				this.routeExecutionFrame(generation, frame.id, frame);
				return;
			default:
				throw new Error(`unknown protocol event: ${frame.event}`);
		}
	}

	private validateHostRequest(frame: Frame): void {
		if (typeof frame.id !== "string" || !frame.id) throw new Error("host_request requires an id");
		if (typeof frame.type !== "string" || !frame.type) throw new Error("host_request requires a type");
		if (
			frame.payload !== undefined &&
			(!frame.payload || typeof frame.payload !== "object" || Array.isArray(frame.payload))
		) {
			throw new Error("host_request payload must be an object");
		}
	}

	private async handleHostRequest(generation: Generation, frame: Frame): Promise<void> {
		const id = frame.id as string;
		const cell = generation.activeCell;
		if (generation.hostRequests.has(id)) {
			this.fail(generation, new Error(`RLM kernel protocol duplicate host request id: ${id}`));
			return;
		}
		if (!cell || this.findPendingId(generation, cell) === undefined) {
			this.sendHostError(generation, id, "RLM host request has no active cell");
			return;
		}
		if (generation.hostRequests.size >= MAX_HOST_REQUESTS) {
			this.sendHostError(generation, id, `RLM host request concurrency exceeds ${MAX_HOST_REQUESTS}`);
			return;
		}
		const type = frame.type as string;
		const payload = (frame.payload ?? {}) as Record<string, unknown>;
		generation.hostRequests.set(id, cell);
		cell.hostRequests.add(id);
		try {
			const result = await this.hostHandler(type, payload, cell.controller.signal);
			if (this.isCurrent(generation) && !cell.controller.signal.aborted) {
				this.sendHostReply(generation, id, { request: "host_reply", id, payload: result });
			}
		} catch (error) {
			if (this.isCurrent(generation) && !cell.controller.signal.aborted) {
				this.sendHostError(generation, id, asError(error).message);
			}
		} finally {
			generation.hostRequests.delete(id);
			cell.hostRequests.delete(id);
		}
	}

	private findPendingId(generation: Generation, waiter: ExecutionWaiter): string | undefined {
		for (const [id, candidate] of generation.pending) {
			if (candidate === waiter) return id;
		}
		return undefined;
	}

	private sendHostReply(generation: Generation, id: string, frame: Frame): void {
		try {
			this.write(generation, frame);
		} catch (error) {
			if (!this.isCurrent(generation)) return;
			try {
				this.write(generation, {
					request: "host_reply",
					id,
					error: asError(error).message,
				});
			} catch (writeError) {
				this.fail(generation, asError(writeError));
			}
		}
	}

	private sendHostError(generation: Generation, id: string, message: string): void {
		if (!this.isCurrent(generation)) return;
		try {
			this.write(generation, { request: "host_reply", id, error: message });
		} catch (error) {
			this.fail(generation, asError(error));
		}
	}

	private routeExecutionFrame(generation: Generation, requestId: string, frame: Frame): void {
		const waiter = generation.pending.get(requestId);
		if (!waiter) throw new Error(`protocol frame references unknown request id: ${requestId}`);
		const state = waiter.state;
		if (frame.event === "stdout") state.stdout += frameString(frame.text);
		if (frame.event === "stderr") state.stderr += frameString(frame.text);
		if (frame.event === "result") state.result = frameString(frame.result);
		if (frame.event === "error") {
			state.status = "error";
			state.error = {
				ename: frameString(frame.ename),
				evalue: frameString(frame.evalue),
				traceback: Array.isArray(frame.traceback)
					? frame.traceback.filter((item): item is string => typeof item === "string")
					: [],
			};
		}
		if (frame.event === "done") {
			// A subprocess killed by the scope's OOM killer may let the cell finish; report the cap, not a stray error.
			if (generation.treeBackend === "cgroup" && this.treeMemoryExceeded(generation)) {
				this.fail(generation, new Error(treeMemoryLimitMessage(this.treeMemoryMb, generation.treeBackend)));
				return;
			}
			state.status = frame.status === "error" ? "error" : state.status;
			// Snapshot/restore failures are reported only on the done frame by runtime.py.
			if (state.status === "error" && !state.error && frame.error != null) {
				state.error = { ename: "RuntimeError", evalue: frameString(frame.error), traceback: [] };
			}
			const snapshot = snapshotReport(frame.snapshot);
			if (snapshot) state.snapshot = snapshot;
			const restore = restoreReport(frame.restore);
			if (restore) state.restore = restore;
			generation.pending.delete(requestId);
			if (generation.activeCell === waiter) generation.activeCell = undefined;
			waiter.controller.abort(new Error("RLM cell completed"));
			waiter.resolve(state);
		}
	}

	private write(generation: Generation, frame: Frame, encoded?: string): void {
		if (!this.isCurrent(generation) || !generation.child.stdin.writable || this.closed) {
			throw generation.failure ?? new Error("RLM kernel is not running");
		}
		const line = encoded ?? encodeFrame(frame);
		generation.child.stdin.write(line, (error) => {
			if (error) this.fail(generation, error);
		});
	}

	private request(generation: Generation, frame: Frame, operation?: Operation): Promise<KernelExecutionResult> {
		const id = randomUUID();
		let encoded: string;
		try {
			encoded = encodeFrame({ ...frame, id });
		} catch (error) {
			return Promise.reject(asError(error));
		}
		const waiter = Object.assign(deferred<KernelExecutionResult>(), {
			state: { status: "ok" as const, stdout: "", stderr: "" },
			controller: new AbortController(),
			hostRequests: new Set<string>(),
		}) as ExecutionWaiter;
		if (!this.isCurrent(generation)) {
			waiter.reject(generation.failure ?? new Error("RLM kernel is not running"));
		} else {
			generation.pending.set(id, waiter);
			if (frame.request === "execute") {
				generation.cellCpuStart = generation.child.pid ? processCpuSeconds(generation.child.pid) : undefined;
				generation.activeCell = waiter;
				if (operation) operation.cell = waiter;
			}
			try {
				this.write(generation, { ...frame, id }, encoded);
			} catch (error) {
				generation.pending.delete(id);
				if (generation.activeCell === waiter) generation.activeCell = undefined;
				waiter.reject(asError(error));
				this.fail(generation, asError(error));
			}
		}
		return waiter.promise;
	}

	private validateRequestFrame(frame: Frame): void {
		encodeFrame({ ...frame, id: PLACEHOLDER_ID });
	}

	private rejectOperations(error: Error): void {
		for (const operation of this.operations) operation.reject(error);
		this.operations.clear();
		this.executionQueue = Promise.resolve();
	}

	private fail(generation: Generation, error: Error): void {
		if (!this.isCurrent(generation)) return;
		generation.failure = error;
		this.generation = undefined;
		// A failed startup must keep reporting a bad configured snapshot on retry.
		// Once user code could have run, never silently resurrect an older checkpoint.
		if (generation.initialized) this.autoRestore = false;
		clearTimeout(generation.startupTimer);
		clearInterval(generation.cpuTimer);
		clearTimeout(generation.memoryTimer);
		generation.buffer = undefined;
		generation.bufferedBytes = 0;
		generation.activeCell?.controller.abort(error);
		for (const waiter of generation.pending.values()) waiter.controller.abort(error);
		generation.hostRequests.clear();
		generation.activeCell = undefined;
		generation.started.reject(error);
		for (const waiter of generation.pending.values()) waiter.reject(error);
		generation.pending.clear();
		this.rejectOperations(error);

		// The detached child owns this group. SIGKILL also stops infinite loops and descendants that ignore
		// SIGTERM, even if the group leader already exited. The kernel is stopped while its descendants are
		// killed, so orphans it adopts as a subreaper are caught too.
		const pid = generation.child.pid;
		if (pid) {
			killProcessTree(pid, {
				ownsProcessGroup: generation.ownsProcessGroup,
				rootAlive: generation.child.exitCode === null && generation.child.signalCode === null,
				known: generation.knownDescendants,
				cgroupDir: generation.cgroupDir,
			});
		}
		if (!generation.ownsProcessGroup || !pid) {
			try {
				generation.child.kill("SIGKILL");
			} catch {
				/* Already gone. */
			}
		}
		generation.child.stdin.destroy();
		generation.child.stdout.destroy();
		generation.child.stderr.destroy();
	}

	private enqueue(frame: Frame, signal?: AbortSignal): Promise<KernelExecutionResult> {
		if (this.closed) return Promise.reject(new Error("RLM kernel is shut down"));
		if (signal?.aborted) return Promise.reject(abortError(signal));
		try {
			this.validateRequestFrame(frame);
		} catch (error) {
			return Promise.reject(asError(error));
		}
		const operation = Object.assign(deferred<KernelExecutionResult>(), {
			running: false,
		}) as Operation;
		this.operations.add(operation);
		const onAbort = () => {
			if (operation.settled) return;
			const error = abortError(signal!);
			if (!operation.running) {
				// This cell is still queued. Do not touch the process serving the
				// preceding cell.
				operation.reject(error);
				return;
			}
			this.autoRestore = false;
			operation.cell?.controller.abort(error);
			if (this.generation) this.fail(this.generation, error);
			else operation.reject(error);
		};
		const cleanup = () => {
			this.operations.delete(operation);
			signal?.removeEventListener("abort", onAbort);
		};
		void operation.promise.then(cleanup, cleanup);
		signal?.addEventListener("abort", onAbort, { once: true });
		if (signal?.aborted) onAbort();
		const run = async () => {
			if (operation.settled) return;
			operation.running = true;
			try {
				// Explicit restore can also recover from a broken configured checkpoint.
				await this.ensureStarted(frame.request !== "restore");
				if (operation.settled) return;
				const generation = this.generation!;
				const path = typeof frame.path === "string" ? frame.path : "";
				operation.resolve(
					await (frame.request === "snapshot"
						? this.signedSnapshot(generation, path, operation)
						: frame.request === "restore"
							? this.verifiedRestore(generation, path, operation)
							: this.request(generation, frame, operation)),
				);
			} catch (error) {
				operation.reject(asError(error));
			} finally {
				operation.running = false;
			}
		};
		this.executionQueue = this.executionQueue.then(run, run);
		return operation.promise;
	}

	/**
	 * The kernel writes an unsigned temporary file; the host checks it is exactly what the kernel
	 * reported, signs it, and atomically replaces `path`. A failure leaves the previous snapshot intact.
	 */
	private async signedSnapshot(
		generation: Generation,
		path: string,
		operation?: Operation,
	): Promise<KernelExecutionResult> {
		const unsigned = join(dirname(path), `.${basename(path)}.${randomUUID()}.unsigned`);
		try {
			const result = await this.request(generation, { request: "snapshot", path: unsigned }, operation);
			if (result.status !== "ok") return result;
			const content = readFileSync(unsigned);
			if (!result.snapshot?.contentSha256 || sha256Hex(content) !== result.snapshot.contentSha256) {
				return snapshotError("snapshot changed after the kernel wrote it; refusing to sign it");
			}
			const temporary = join(dirname(path), `.${basename(path)}.${randomUUID()}.tmp`);
			try {
				writeFileSync(temporary, signSnapshot(this.snapshotKey, content), { mode: 0o600 });
				renameSync(temporary, path);
			} finally {
				removeQuietly(temporary);
			}
			return result;
		} catch (error) {
			return snapshotError(`snapshot signing failed: ${asError(error).message}`);
		} finally {
			removeQuietly(unsigned);
		}
	}

	/** Refuses unsigned or tampered snapshots before the kernel reads them; the namespace stays untouched. */
	private async verifiedRestore(
		generation: Generation,
		path: string,
		operation?: Operation,
	): Promise<KernelExecutionResult> {
		let raw: Buffer;
		try {
			raw = readFileSync(path);
		} catch (error) {
			// A missing snapshot is reported by the kernel as `missing`; nothing is loaded.
			const code = (error as NodeJS.ErrnoException).code;
			if (code === "ENOENT" || code === "ENOTDIR") {
				return this.request(generation, { request: "restore", path }, operation);
			}
			return snapshotError(`snapshot cannot be read: ${asError(error).message}`);
		}
		let contentSha256: string;
		try {
			({ contentSha256 } = verifySnapshot(this.snapshotKey, raw));
		} catch (error) {
			return snapshotError(asError(error).message);
		}
		// The kernel re-hashes what it reads, so a file swapped after this check is refused too.
		return this.request(generation, { request: "restore", path, content_sha256: contentSha256 }, operation);
	}

	execute(code: string, signal?: AbortSignal): Promise<KernelExecutionResult> {
		if (typeof code !== "string") return Promise.reject(new Error("RLM execute code must be a string"));
		return this.enqueue({ request: "execute", code }, signal);
	}

	/** Clear invocation scratch while keeping the API bindings and the declared `state` dict. */
	resetScratch(): Promise<KernelExecutionResult> {
		return this.enqueue({ request: "reset_scratch" });
	}

	snapshot(path = this.options.snapshotPath): Promise<KernelExecutionResult> {
		if (!path) return Promise.reject(new Error("RLM snapshot path is not configured"));
		return this.enqueue({ request: "snapshot", path });
	}

	restore(path = this.options.snapshotPath): Promise<KernelExecutionResult> {
		if (!path) return Promise.reject(new Error("RLM snapshot path is not configured"));
		return this.enqueue({ request: "restore", path });
	}

	/** Permanently close this instance; interrupting execute alone permits restart. */
	shutdown(): Promise<void> {
		if (this.shutdownPromise) return this.shutdownPromise;
		this.closed = true;
		const generation = this.generation;
		const error = new Error("RLM kernel is shut down");
		if (generation) this.fail(generation, error);
		else this.rejectOperations(error);
		this.shutdownPromise = generation
			? new Promise<void>((resolve) => {
					const timer = setTimeout(resolve, SHUTDOWN_TIMEOUT_MS);
					void generation.exited.promise.then(() => {
						clearTimeout(timer);
						resolve();
					});
				})
			: Promise.resolve();
		return this.shutdownPromise;
	}
}
