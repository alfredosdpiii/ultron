import { type ChildProcessWithoutNullStreams, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync } from "node:fs";

/** The signal covers the active cell, including failure, completion and shutdown. */
export type KernelHostHandler = (
	type: string,
	payload: Record<string, unknown>,
	signal?: AbortSignal,
) => Promise<unknown> | unknown;

export type KernelExecutionResult = {
	status: "ok" | "error";
	stdout: string;
	stderr: string;
	result?: string;
	error?: { ename: string; evalue: string; traceback: string[] };
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

export class RlmKernel {
	private generation?: Generation;
	private readonly operations = new Set<Operation>();
	private executionQueue: Promise<void> = Promise.resolve();
	private shutdownPromise?: Promise<void>;
	private closed = false;
	private autoRestore = true;
	private stderr = "";

	private readonly options: {
		cwd: string;
		runtimePath: string;
		python?: string;
		snapshotPath?: string;
		/** Bounds initialization, including restore, not user execution. */
		startupTimeoutMs?: number;
	};
	private readonly hostHandler: KernelHostHandler;
	constructor(
		options: {
			cwd: string;
			runtimePath: string;
			python?: string;
			snapshotPath?: string;
			startupTimeoutMs?: number;
		},
		hostHandler: KernelHostHandler,
	) {
		this.options = options;
		this.hostHandler = hostHandler;
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
		try {
			const python =
				this.options.python ??
				process.env.ULTRON_PYTHON ??
				(process.platform === "linux" && existsSync("/usr/bin/python3") ? "/usr/bin/python3" : "python3");
			child = spawn(python, [this.options.runtimePath], {
				cwd: this.options.cwd,
				env: { ...process.env, NO_COLOR: "1", PYTHONDONTWRITEBYTECODE: "1" },
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
			if (this.isCurrent(generation)) {
				this.fail(generation, new Error(`RLM kernel exited (${signal ?? code ?? "unknown"})${this.diagnostics()}`));
			}
		});
		child.stdout.once("end", () => {
			if (this.isCurrent(generation)) {
				this.fail(generation, new Error(`RLM kernel stdout closed${this.diagnostics()}`));
			}
		});
		// Streams may still report errors after termination. Keep handlers scoped
		// to their process so late EPIPE/exit/data events cannot affect a restart.
		for (const stream of [child.stdin, child.stdout, child.stderr]) {
			stream.on("error", (error) => this.fail(generation, error));
		}
		return generation.started.promise;
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
				const result = await this.request(generation, { request: "restore", path: generation.restorePath });
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
			state.status = frame.status === "error" ? "error" : state.status;
			// Snapshot/restore failures are reported only on the done frame by runtime.py.
			if (state.status === "error" && !state.error && frame.error != null) {
				state.error = { ename: "RuntimeError", evalue: frameString(frame.error), traceback: [] };
			}
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

		// The detached child owns this group. SIGKILL also stops infinite loops and
		// descendants that ignore SIGTERM, even if the group leader already exited.
		try {
			if (generation.ownsProcessGroup && generation.child.pid) {
				process.kill(-generation.child.pid, "SIGKILL");
			} else {
				generation.child.kill("SIGKILL");
			}
		} catch {
			// ESRCH is normal when the child exited before its exit event was handled.
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
				operation.resolve(await this.request(this.generation!, frame, operation));
			} catch (error) {
				operation.reject(asError(error));
			} finally {
				operation.running = false;
			}
		};
		this.executionQueue = this.executionQueue.then(run, run);
		return operation.promise;
	}

	execute(code: string, signal?: AbortSignal): Promise<KernelExecutionResult> {
		if (typeof code !== "string") return Promise.reject(new Error("RLM execute code must be a string"));
		return this.enqueue({ request: "execute", code }, signal);
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
