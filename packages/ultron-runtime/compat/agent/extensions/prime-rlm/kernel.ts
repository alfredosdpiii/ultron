import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { randomUUID } from "node:crypto";

export type KernelHostHandler = (type: string, payload: Record<string, unknown>) => Promise<unknown> | unknown;

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
type Frame = Record<string, unknown> & { event?: string; request?: string; id?: string };
type ExecutionWaiter = Deferred<KernelExecutionResult> & { state: KernelExecutionResult };
type Generation = {
  child: ChildProcessWithoutNullStreams;
  ownsProcessGroup: boolean;
  started: Deferred<void>;
  exited: Deferred<void>;
  pending: Map<string, ExecutionWaiter>;
  readyReceived: boolean;
  initialized: boolean;
  failure?: Error;
  startupTimer?: ReturnType<typeof setTimeout>;
  restorePath?: string;
  buffer?: Buffer;
  bufferedBytes: number;
};

const MAX_FRAME_BYTES = 1024 * 1024;
const STARTUP_TIMEOUT_MS = 10_000;
const SHUTDOWN_TIMEOUT_MS = 1_000;

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
  return typeof value === "string" ? value : value == null ? "" : JSON.stringify(value);
}

export class RlmKernel {
  private generation?: Generation;
  private readonly operations = new Set<Deferred<KernelExecutionResult>>();
  private executionQueue: Promise<void> = Promise.resolve();
  private shutdownPromise?: Promise<void>;
  private closed = false;
  private autoRestore = true;
  private stderr = "";

  constructor(
    private readonly options: {
      cwd: string;
      runtimePath: string;
      python?: string;
      snapshotPath?: string;
      /** Bounds initialization, including restore, not user execution. */
      startupTimeoutMs?: number;
    },
    private readonly hostHandler: KernelHostHandler,
  ) {
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
      child = spawn(this.options.python ?? this.defaultPython(), [this.options.runtimePath], {
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
      readyReceived: false,
      initialized: false,
      restorePath: restoreSnapshot && this.autoRestore ? this.options.snapshotPath : undefined,
      buffer: Buffer.allocUnsafe(MAX_FRAME_BYTES),
      bufferedBytes: 0,
    };
    this.generation = generation;
    this.stderr = "";
    const timeout = this.options.startupTimeoutMs ?? STARTUP_TIMEOUT_MS;
    generation.startupTimer = setTimeout(() => {
      this.fail(generation, new Error(`RLM kernel startup timed out after ${timeout}ms${this.diagnostics()}`));
    }, timeout);
    child.stdout.on("data", (chunk: Buffer) => this.handleData(generation, chunk));
    child.stderr.on("data", (chunk: Buffer) => {
      if (this.isCurrent(generation)) this.stderr = (this.stderr + chunk.toString("utf8")).slice(-8000);
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

  private defaultPython(): string {
    const managed = join(homedir(), ".prime", "agent", "kernel-venv", "bin", "python");
    return existsSync(managed) ? managed : "python3";
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

  private handleData(generation: Generation, chunk: Buffer): void {
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
      const line = buffer.toString("utf8", 0, generation.bufferedBytes);
      generation.bufferedBytes = 0;
      this.handleLine(generation, line);
      offset = newline + 1;
    }
  }

  private handleLine(generation: Generation, line: string): void {
    let frame: Frame;
    try {
      const parsed: unknown = JSON.parse(line);
      if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("Expected an object");
      frame = parsed as Frame;
    } catch {
      this.stderr = `${this.stderr}\n[protocol] invalid JSON: ${line.slice(-8000)}`.slice(-8000);
      return;
    }
    if (frame.event === "ready") {
      if (!generation.readyReceived) {
        generation.readyReceived = true;
        void this.completeStartup(generation);
      }
      return;
    }
    if (frame.event === "host_request") {
      void this.handleHostRequest(generation, frame);
      return;
    }
    if (typeof frame.id === "string") this.routeExecutionFrame(generation, frame.id, frame);
  }

  private async handleHostRequest(generation: Generation, frame: Frame): Promise<void> {
    const id = typeof frame.id === "string" ? frame.id : "";
    try {
      const type = typeof frame.type === "string" ? frame.type : "";
      const payload = frame.payload && typeof frame.payload === "object" ? frame.payload as Record<string, unknown> : {};
      const result = await this.hostHandler(type, payload);
      if (this.isCurrent(generation)) this.write(generation, { request: "host_reply", id, payload: result });
    } catch (error) {
      if (!this.isCurrent(generation)) return;
      try {
        this.write(generation, { request: "host_reply", id, error: asError(error).message });
      } catch (writeError) {
        this.fail(generation, asError(writeError));
      }
    }
  }

  private routeExecutionFrame(generation: Generation, requestId: string, frame: Frame): void {
    const waiter = generation.pending.get(requestId);
    if (!waiter) return;
    const state = waiter.state;
    if (frame.event === "stdout") state.stdout += frameString(frame.text);
    if (frame.event === "stderr") state.stderr += frameString(frame.text);
    if (frame.event === "result") state.result = frameString(frame.result);
    if (frame.event === "error") {
      state.status = "error";
      state.error = {
        ename: frameString(frame.ename),
        evalue: frameString(frame.evalue),
        traceback: Array.isArray(frame.traceback) ? frame.traceback.filter((item): item is string => typeof item === "string") : [],
      };
    }
    if (frame.event === "done") {
      state.status = frame.status === "error" ? "error" : state.status;
      // Snapshot/restore failures are reported only on the done frame by runtime.py.
      if (state.status === "error" && !state.error && frame.error != null) {
        state.error = { ename: "RuntimeError", evalue: frameString(frame.error), traceback: [] };
      }
      generation.pending.delete(requestId);
      waiter.resolve(state);
    }
  }

  private write(generation: Generation, frame: Record<string, unknown>): void {
    if (!this.isCurrent(generation) || !generation.child.stdin.writable || this.closed) {
      throw generation.failure ?? new Error("RLM kernel is not running");
    }
    generation.child.stdin.write(`${JSON.stringify(frame)}\n`, (error) => {
      if (error) this.fail(generation, error);
    });
  }

  private request(generation: Generation, frame: Record<string, unknown>): Promise<KernelExecutionResult> {
    const id = randomUUID();
    const waiter: ExecutionWaiter = Object.assign(deferred<KernelExecutionResult>(), {
      state: { status: "ok" as const, stdout: "", stderr: "" },
    });
    if (!this.isCurrent(generation)) {
      waiter.reject(generation.failure ?? new Error("RLM kernel is not running"));
    } else {
      generation.pending.set(id, waiter);
      try {
        this.write(generation, { ...frame, id });
      } catch (error) {
        this.fail(generation, asError(error));
      }
    }
    return waiter.promise;
  }

  private rejectOperations(error: Error): void {
    for (const waiter of this.operations) waiter.reject(error);
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
      try { generation.child.kill("SIGKILL"); } catch { /* Already gone. */ }
    }
    generation.child.stdin.destroy();
    generation.child.stdout.destroy();
    generation.child.stderr.destroy();
  }

  private enqueue(frame: Record<string, unknown>, signal?: AbortSignal): Promise<KernelExecutionResult> {
    if (this.closed) return Promise.reject(new Error("RLM kernel is shut down"));
    const waiter = deferred<KernelExecutionResult>();
    this.operations.add(waiter);
    const onAbort = () => {
      if (waiter.settled) return;
      const error = abortError(signal!);
      this.autoRestore = false;
      if (this.generation) this.fail(this.generation, error);
      else this.rejectOperations(error);
    };
    const cleanup = () => {
      this.operations.delete(waiter);
      signal?.removeEventListener("abort", onAbort);
    };
    void waiter.promise.then(cleanup, cleanup);
    signal?.addEventListener("abort", onAbort, { once: true });
    if (signal?.aborted) onAbort();
    const run = async () => {
      if (waiter.settled) return;
      try {
        // Explicit restore can also recover from a broken configured checkpoint.
        await this.ensureStarted(frame.request !== "restore");
        if (waiter.settled) return;
        waiter.resolve(await this.request(this.generation!, frame));
      } catch (error) {
        waiter.reject(asError(error));
      }
    };
    this.executionQueue = this.executionQueue.then(run, run);
    return waiter.promise;
  }

  execute(code: string, signal?: AbortSignal): Promise<KernelExecutionResult> {
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
    this.shutdownPromise = generation ? new Promise<void>((resolve) => {
      const timer = setTimeout(resolve, SHUTDOWN_TIMEOUT_MS);
      void generation.exited.promise.then(() => {
        clearTimeout(timer);
        resolve();
      });
    }) : Promise.resolve();
    return this.shutdownPromise;
  }
}
