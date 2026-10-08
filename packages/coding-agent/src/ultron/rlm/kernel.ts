import { type ChildProcess, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import type { Readable, Writable } from "node:stream";
import { rlmOutputBudget } from "./output-truncation.ts";
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
 *   Not on macOS, which counts the whole virtual map against RLIMIT_DATA and refuses a GiB-scale limit (#1).
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
/** Raw fd 1 or fd 2 output of one cell: head and tail within the per-cell limit (see {@link captureRaw}). */
type RawCapture = { head: string; tail: string[]; tailLength: number; dropped: number; markerSeen: boolean };
type RawStreamName = "stdout" | "stderr";
/** One of the kernel's plain output pipes (fd 1, fd 2) on the private-fd protocol. */
type RawStream = {
	decoder: InstanceType<typeof TextDecoder>;
	/** Bytes that may be the start of the running cell's flush marker, held until the next chunk. */
	held: Buffer;
	/** Output that arrived while no cell owned the stream; reported at the start of the next cell. */
	orphan: string;
	orphanDropped: boolean;
	/** Whether the runtime's startup marker has arrived; output before it is startup diagnostics. */
	started: boolean;
};
type ExecutionWaiter = Deferred<KernelExecutionResult> & {
	state: KernelExecutionResult;
	requestId: string;
	/** Raw output of an execute cell on the private-fd protocol, and the output from outside a cell before it. */
	raw?: {
		marker: Buffer;
		stdout: RawCapture;
		stderr: RawCapture;
		orphan: Record<RawStreamName, string>;
		/** Set by the done frame: the flush markers the runtime wrote and the host still waits for. */
		awaiting?: Record<RawStreamName, boolean>;
		timer?: ReturnType<typeof setTimeout>;
	};
	controller: AbortController;
	hostRequests: Set<string>;
	/** Serves this cell's host requests instead of the kernel's handler (see {@link RlmKernel.execute}). */
	hostHandler?: KernelHostHandler;
};
type Operation = Deferred<KernelExecutionResult> & {
	running: boolean;
	cell?: ExecutionWaiter;
	hostHandler?: KernelHostHandler;
};
type Generation = {
	child: ChildProcess;
	/** Frames from the kernel: fd 3 on the private-fd protocol, else the kernel's stdout. */
	protocolOut: Readable;
	/** Messages to the kernel: fd 4 on the private-fd protocol, else the kernel's stdin. */
	protocolIn: Writable;
	privateProtocol: boolean;
	raw: Record<RawStreamName, RawStream>;
	/** The execute cell that owns raw output until its flush markers arrive (or time out). */
	outputCell?: ExecutionWaiter;
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
};

// Protocol output is bounded per frame and per request, never over the kernel's lifetime: a long session sends
// any amount in total. The frame limit applies to wire bytes, including JSON overhead; runtime.py shrinks its
// own frames to fit. Frames travel on the private fd 3 (fd 4 carries host messages), so raw writes to fd 1 or
// fd 2 (os.write, subprocesses, C code) are the running cell's output, not protocol. On Windows (protocolChannel
// "stdio") frames share the kernel's stdout, and a raw write there can still exceed the frame limit.
//
// Ordering: fd 1/fd 2 are separate pipes from fd 3. After a cell, runtime.py flushes Python and C stdio and writes
// a flush marker naming the cell's request id to fd 1 and fd 2, then sends `done` listing the markers it wrote.
// The host resolves the cell once it has read those markers, so all raw output the cell wrote is in its result:
// Python's captured print output first, then the raw output in write order. Raw output arriving while no cell owns
// the streams (a background process) is kept, bounded, and reported at the start of the next cell's output.
const MAX_FRAME_BYTES = 1024 * 1024;
/**
 * stdout or stderr kept for one request (cell). runtime.py sends each stream once, within a frame; this bounds
 * a flood of repeated frames for one request. A result replaces, rather than grows, and fits a frame.
 */
const MAX_REQUEST_STREAM_CHARS = 4 * MAX_FRAME_BYTES;
const REQUEST_STREAM_MARKER = `\n[... further output dropped: over the RLM kernel's 4 MiB per-cell output limit ...]\n`;
/** Raw stderr of the kernel process is kept as a tail for diagnostics only. */
const STDERR_TAIL_CHARS = 8000;
/** Raw output from outside any cell (a background thread or process), kept for the next cell. */
const MAX_ORPHAN_CHARS = 64 * 1024;
/**
 * How long a finished cell waits for the flush markers the runtime wrote to fd 1 and fd 2. They normally arrive
 * with (or before) the done frame; this only bounds a pathological case, such as a pipe the host is slow to drain.
 */
const RAW_FLUSH_TIMEOUT_MS = 5_000;
const EMPTY_BUFFER = Buffer.alloc(0);
const MAX_HOST_REQUESTS = 16;
const STARTUP_TIMEOUT_MS = 10_000;
const FRAME_DECODER = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
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

/** `current` + `text` within the per-cell stream limit; the marker is added once, the first time it cuts. */
function appendBounded(current: string, text: string): string {
	const room = MAX_REQUEST_STREAM_CHARS - current.length;
	if (text.length <= room) return current + text;
	if (room < 0) return current;
	return current + text.slice(0, room) + REQUEST_STREAM_MARKER;
}

/** The marker runtime.py writes to fd 1 and fd 2 after a cell's output; the id is a host-made random UUID. */
function flushMarker(requestId: string): Buffer {
	return Buffer.from(`\x1eultron-rlm-flush:${requestId}\x1e`, "utf8");
}

/** Length of the longest tail of `data` that is a proper prefix of `marker` (it may complete in the next chunk). */
function markerPrefixLength(data: Buffer, marker: Buffer): number {
	const from = Math.max(0, data.length - marker.length + 1);
	for (let start = data.indexOf(marker[0]!, from); start !== -1; start = data.indexOf(marker[0]!, start + 1)) {
		if (data.subarray(start).equals(marker.subarray(0, data.length - start))) return data.length - start;
	}
	return 0;
}

function rawCapture(): RawCapture {
	return { head: "", tail: [], tailLength: 0, dropped: 0, markerSeen: false };
}

/** Keep the head and tail of a cell's raw output, each half the per-cell stream limit, and count the middle cut. */
function captureRaw(capture: RawCapture, text: string): void {
	const half = MAX_REQUEST_STREAM_CHARS / 2;
	if (capture.head.length < half) {
		const room = half - capture.head.length;
		capture.head += text.slice(0, room);
		text = text.slice(room);
	}
	if (!text) return;
	capture.tail.push(text);
	capture.tailLength += text.length;
	while (capture.tail.length > 1 && capture.tailLength - capture.tail[0]!.length >= half) {
		const first = capture.tail.shift()!;
		capture.tailLength -= first.length;
		capture.dropped += first.length;
	}
}

function rawCaptureText(capture: RawCapture, fd: number): string {
	const half = MAX_REQUEST_STREAM_CHARS / 2;
	let tail = capture.tail.join("");
	let dropped = capture.dropped;
	if (tail.length > half) {
		dropped += tail.length - half;
		tail = tail.slice(tail.length - half);
	}
	if (!dropped) return capture.head + tail;
	return `${capture.head}\n[... ${dropped} characters of raw fd ${fd} output cut from the middle: over the RLM kernel's 4 MiB per-cell output limit ...]\n${tail}`;
}

function orphanText(text: string, dropped: boolean): string {
	if (!text) return "";
	const cut = dropped ? `\n[... further output dropped: over the ${MAX_ORPHAN_CHARS / 1024} KiB limit ...]` : "";
	const end = cut || !text.endsWith("\n") ? "\n" : "";
	return `[kernel output written outside a cell, before this one (a background thread or process):]\n${text}${cut}${end}[end of output from outside a cell]\n`;
}

/** Written by runtime.py to fd 1 and fd 2 just before its ready frame. */
const STARTUP_MARKER = flushMarker("ready");

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
	/**
	 * Where protocol frames travel. "private-fd" (the default except on Windows): frames on fd 3, host messages on
	 * fd 4, stdin is /dev/null, and fd 1/fd 2 are plain output attributed to the running cell. "stdio" (the Windows
	 * default, where Python cannot rely on inheriting extra descriptors): frames on stdout and messages on stdin, so
	 * raw writes to fd 1 corrupt the stream and a frame over 1 MiB ends the kernel.
	 */
	protocolChannel?: "private-fd" | "stdio";
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

	/**
	 * Proportional memory of the kernel's process tree (Linux, read from /proc), and the tree cap; undefined while
	 * no kernel process runs. Read-only; used by the `rlm.pool` inspection.
	 */
	memoryUsage(): { bytes: number; capBytes: number | null } | undefined {
		const pid = this.generation?.child.pid;
		if (!pid || this.closed || process.platform !== "linux") return undefined;
		try {
			return {
				bytes: treeMemoryUsage(pid).bytes,
				capBytes: this.treeMemoryMb > 0 ? this.treeMemoryMb * 1024 * 1024 : null,
			};
		} catch {
			return undefined;
		}
	}

	start(): Promise<void> {
		return this.ensureStarted();
	}

	private ensureStarted(restoreSnapshot = true): Promise<void> {
		if (this.closed) return Promise.reject(new Error("RLM kernel is shut down"));
		if (this.generation) return this.generation.started.promise;
		let child: ChildProcess;
		const ownsProcessGroup = process.platform !== "win32";
		const privateProtocol =
			(this.options.protocolChannel ?? (process.platform === "win32" ? "stdio" : "private-fd")) === "private-fd";
		const treeBackend = treeMemoryBackend(this.treeMemoryMb, this.options.treeMemoryBackend);
		try {
			const python =
				this.options.python ??
				process.env.ULTRON_PYTHON ??
				(process.platform === "linux" && existsSync("/usr/bin/python3") ? "/usr/bin/python3" : "python3");
			const runtime = privateProtocol ? [this.options.runtimePath, "--protocol-fds"] : [this.options.runtimePath];
			const command =
				treeBackend === "cgroup"
					? [...cgroupScopeCommand(this.treeMemoryMb), python, ...runtime]
					: [python, ...runtime];
			child = spawn(command[0]!, command.slice(1), {
				cwd: this.options.cwd,
				env: {
					...kernelEnvironment(process.env),
					NO_COLOR: "1",
					PYTHONDONTWRITEBYTECODE: "1",
					ULTRON_RLM_MAX_MEMORY_MB: String(this.limits.maxMemoryMb),
					ULTRON_RLM_MAX_CPU_SECONDS: String(this.limits.maxCpuSeconds),
					// Each captured stdout/stderr keeps its head and tail within the tool-output budget.
					ULTRON_RLM_OUTPUT_BYTES: String(rlmOutputBudget()),
					...this.options.env,
				},
				stdio: privateProtocol ? ["ignore", "pipe", "pipe", "pipe", "pipe"] : ["pipe", "pipe", "pipe"],
				detached: ownsProcessGroup,
			});
		} catch (error) {
			return Promise.reject(asError(error));
		}
		const protocolOut = (privateProtocol ? child.stdio[3] : child.stdout) as Readable;
		const protocolIn = (privateProtocol ? child.stdio[4] : child.stdin) as Writable;
		const rawStream = (): RawStream => ({
			decoder: new TextDecoder("utf-8"),
			held: EMPTY_BUFFER,
			orphan: "",
			orphanDropped: false,
			started: false,
		});
		const generation: Generation = {
			child,
			protocolOut,
			protocolIn,
			privateProtocol,
			raw: { stdout: rawStream(), stderr: rawStream() },
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
			treeBackend,
			knownDescendants: new Map(),
		};
		this.generation = generation;
		this.stderr = "";
		const timeout = this.options.startupTimeoutMs ?? STARTUP_TIMEOUT_MS;
		generation.startupTimer = setTimeout(() => {
			this.fail(generation, new Error(`RLM kernel startup timed out after ${timeout}ms${this.diagnostics()}`));
		}, timeout);
		protocolOut.on("data", (chunk: Buffer) => this.handleData(generation, chunk));
		if (privateProtocol) {
			child.stdout!.on("data", (chunk: Buffer) => this.handleRaw(generation, "stdout", chunk));
			child.stderr!.on("data", (chunk: Buffer) => this.handleRaw(generation, "stderr", chunk));
		} else {
			child.stderr!.on("data", (chunk: Buffer) => {
				if (!this.isCurrent(generation)) return;
				this.keepStderrTail(chunk.subarray(Math.max(0, chunk.length - 4 * STDERR_TAIL_CHARS)).toString("utf8"));
			});
		}
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
		protocolOut.once("end", () => {
			if (!this.isCurrent(generation)) return;
			// Output usually ends just before the exit event; wait briefly so the exit code
			// (for example a resource limit) is what gets reported.
			const timer = setTimeout(() => {
				if (this.isCurrent(generation)) {
					const channel = privateProtocol ? "protocol channel (fd 3)" : "stdout";
					this.fail(generation, new Error(`RLM kernel ${channel} closed${this.diagnostics()}`));
				}
			}, 200);
			void generation.exited.promise.then(() => clearTimeout(timer));
		});
		// Streams may still report errors after termination. Keep handlers scoped
		// to their process so late EPIPE/exit/data events cannot affect a restart.
		for (const stream of new Set([child.stdout, child.stderr, protocolOut, protocolIn])) {
			stream?.on("error", (error) => this.fail(generation, error));
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

	private handleData(generation: Generation, chunk: Buffer): void {
		if (!this.isCurrent(generation)) return;
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
			// One frame-sized buffer per generation is reused for every frame.
			const buffer = generation.buffer!;
			chunk.copy(buffer, generation.bufferedBytes, offset, end);
			generation.bufferedBytes += length;
			if (newline === -1) return;
			try {
				const line = FRAME_DECODER.decode(buffer.subarray(0, generation.bufferedBytes));
				generation.bufferedBytes = 0;
				this.handleLine(generation, line);
			} catch (error) {
				this.fail(generation, new Error(`RLM kernel invalid protocol frame: ${asError(error).message}`));
			}
			offset = newline + 1;
		}
	}

	private keepStderrTail(text: string): void {
		// Only a bounded tail is kept, so a chatty process never grows host memory or ends the kernel.
		if (text) this.stderr = (this.stderr + text.slice(-STDERR_TAIL_CHARS)).slice(-STDERR_TAIL_CHARS);
	}

	/**
	 * Raw bytes from the kernel's fd 1 or fd 2 (private-fd protocol). They belong to the cell that owns the output
	 * until that cell's flush marker arrives on the stream; anything else is output from outside a cell.
	 */
	private handleRaw(generation: Generation, name: RawStreamName, chunk: Buffer): void {
		if (!this.isCurrent(generation)) return;
		const stream = generation.raw[name];
		let data = stream.held.length ? Buffer.concat([stream.held, chunk]) : chunk;
		stream.held = EMPTY_BUFFER;
		if (!stream.started) {
			// Before the startup marker: stderr only feeds the diagnostic tail, stdout is dropped.
			const index = data.indexOf(STARTUP_MARKER);
			const end = index === -1 ? data.length - markerPrefixLength(data, STARTUP_MARKER) : index;
			if (name === "stderr") this.keepStderrTail(data.subarray(0, end).toString("utf8"));
			if (index === -1) {
				stream.held = Buffer.from(data.subarray(end));
				return;
			}
			stream.started = true;
			data = data.subarray(index + STARTUP_MARKER.length);
		}
		const cell = generation.outputCell;
		const raw = cell?.raw;
		if (cell && raw && !raw[name].markerSeen) {
			const index = data.indexOf(raw.marker);
			if (index !== -1) {
				this.routeRaw(generation, name, data.subarray(0, index), raw[name]);
				raw[name].markerSeen = true;
				this.routeRaw(generation, name, data.subarray(index + raw.marker.length));
				this.finishCellIfFlushed(generation, cell);
				return;
			}
			const held = markerPrefixLength(data, raw.marker);
			if (held) {
				stream.held = Buffer.from(data.subarray(data.length - held));
				data = data.subarray(0, data.length - held);
			}
			this.routeRaw(generation, name, data, raw[name]);
			return;
		}
		this.routeRaw(generation, name, data);
	}

	private routeRaw(generation: Generation, name: RawStreamName, bytes: Buffer, capture?: RawCapture): void {
		if (!bytes.length) return;
		const stream = generation.raw[name];
		const text = stream.decoder.decode(bytes, { stream: true });
		if (name === "stderr") this.keepStderrTail(text);
		if (capture) {
			captureRaw(capture, text);
			return;
		}
		const room = MAX_ORPHAN_CHARS - stream.orphan.length;
		if (text.length > room) stream.orphanDropped = true;
		if (room > 0) stream.orphan += text.slice(0, room);
	}

	/** Resolve a finished cell once every flush marker its done frame announced has arrived. */
	private finishCellIfFlushed(generation: Generation, waiter: ExecutionWaiter): void {
		const raw = waiter.raw;
		if (!raw?.awaiting || generation.outputCell !== waiter) return;
		if ((raw.awaiting.stdout && !raw.stdout.markerSeen) || (raw.awaiting.stderr && !raw.stderr.markerSeen)) return;
		this.finishCell(generation, waiter);
	}

	/** Combine the output from outside a cell, the cell's captured frames and its raw output, and resolve it. */
	private finishCell(generation: Generation, waiter: ExecutionWaiter): void {
		const raw = waiter.raw;
		if (raw) {
			clearTimeout(raw.timer);
			if (generation.outputCell === waiter) {
				generation.outputCell = undefined;
				// A held partial marker was output after all.
				for (const name of ["stdout", "stderr"] as const) {
					const stream = generation.raw[name];
					if (!stream.started) continue;
					const held = stream.held;
					stream.held = EMPTY_BUFFER;
					this.routeRaw(generation, name, held);
				}
			}
			const state = waiter.state;
			state.stdout = raw.orphan.stdout + state.stdout + rawCaptureText(raw.stdout, 1);
			state.stderr = raw.orphan.stderr + state.stderr + rawCaptureText(raw.stderr, 2);
		}
		waiter.resolve(waiter.state);
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
			const result = await (cell.hostHandler ?? this.hostHandler)(type, payload, cell.controller.signal);
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
		if (frame.event === "stdout") state.stdout = appendBounded(state.stdout, frameString(frame.text));
		if (frame.event === "stderr") state.stderr = appendBounded(state.stderr, frameString(frame.text));
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
			const raw = waiter.raw;
			if (!raw || generation.outputCell !== waiter) {
				waiter.resolve(state);
				return;
			}
			// fd 1/fd 2 are separate pipes from fd 3: the cell's raw output may still be in flight. runtime.py wrote a
			// flush marker after it on each stream it could; wait for those (bounded), so the result is complete.
			const flush = frame.flush && typeof frame.flush === "object" ? (frame.flush as Record<string, unknown>) : {};
			raw.awaiting = { stdout: flush.stdout === true, stderr: flush.stderr === true };
			raw.timer = setTimeout(() => {
				if (this.isCurrent(generation)) this.finishCell(generation, waiter);
			}, RAW_FLUSH_TIMEOUT_MS);
			this.finishCellIfFlushed(generation, waiter);
		}
	}

	private write(generation: Generation, frame: Frame, encoded?: string): void {
		if (!this.isCurrent(generation) || !generation.protocolIn.writable || this.closed) {
			throw generation.failure ?? new Error("RLM kernel is not running");
		}
		const line = encoded ?? encodeFrame(frame);
		generation.protocolIn.write(line, (error) => {
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
			requestId: id,
			controller: new AbortController(),
			hostRequests: new Set<string>(),
			...(operation?.hostHandler === undefined ? {} : { hostHandler: operation.hostHandler }),
		}) as ExecutionWaiter;
		if (!this.isCurrent(generation)) {
			waiter.reject(generation.failure ?? new Error("RLM kernel is not running"));
		} else {
			generation.pending.set(id, waiter);
			if (frame.request === "execute") {
				generation.cellCpuStart = generation.child.pid ? processCpuSeconds(generation.child.pid) : undefined;
				generation.activeCell = waiter;
				if (operation) operation.cell = waiter;
				if (generation.privateProtocol) this.takeRawOutput(generation, waiter);
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

	/** The cell owns fd 1/fd 2 output from now on, and reports the output from outside a cell that came before it. */
	private takeRawOutput(generation: Generation, waiter: ExecutionWaiter): void {
		const orphan = { stdout: "", stderr: "" };
		for (const name of ["stdout", "stderr"] as const) {
			const stream = generation.raw[name];
			orphan[name] = orphanText(stream.orphan, stream.orphanDropped);
			stream.orphan = "";
			stream.orphanDropped = false;
		}
		waiter.raw = { marker: flushMarker(waiter.requestId), stdout: rawCapture(), stderr: rawCapture(), orphan };
		generation.outputCell = waiter;
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
		// A finished cell still waiting for its flush markers is no longer in `pending`.
		if (generation.outputCell) {
			clearTimeout(generation.outputCell.raw?.timer);
			generation.outputCell.reject(error);
			generation.outputCell = undefined;
		}
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
		for (const stream of [generation.child.stdin, generation.child.stdout, generation.child.stderr])
			stream?.destroy();
		generation.protocolIn.destroy();
		generation.protocolOut.destroy();
	}

	private enqueue(
		frame: Frame,
		signal?: AbortSignal,
		hostHandler?: KernelHostHandler,
	): Promise<KernelExecutionResult> {
		if (this.closed) return Promise.reject(new Error("RLM kernel is shut down"));
		if (signal?.aborted) return Promise.reject(abortError(signal));
		try {
			this.validateRequestFrame(frame);
		} catch (error) {
			return Promise.reject(asError(error));
		}
		const operation = Object.assign(deferred<KernelExecutionResult>(), {
			running: false,
			...(hostHandler === undefined ? {} : { hostHandler }),
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

	/**
	 * Run a cell. `hostHandler`, when given, serves this cell's host requests in place of the kernel's handler, so
	 * per-cell state (the images `view_image` attaches) stays with the cell even when cells are queued.
	 */
	execute(code: string, signal?: AbortSignal, hostHandler?: KernelHostHandler): Promise<KernelExecutionResult> {
		if (typeof code !== "string") return Promise.reject(new Error("RLM execute code must be a string"));
		return this.enqueue({ request: "execute", code }, signal, hostHandler);
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
