import { existsSync, mkdirSync, readFileSync, writeFileSync, appendFileSync, readdirSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { randomUUID, createHash } from "node:crypto";

export type ChildStatus = "running" | "completed" | "error" | "cancelled";

export type ChildLifecycleEvent = "spawned" | "completed" | "error" | "cancelled" | "timeout";

// Cumulative message usage across invocations. A null field means at least one
// record did not report it. Cost is never inferred from a model's price table.
export type ChildUsage = {
  input: number | null;
  output: number | null;
  cacheRead: number | null;
  cacheWrite: number | null;
  totalTokens: number | null;
  cost: number | null;
};

export type ChildEntry = {
  rlm_child_id: string;
  active_session_id: string | null;
  session_id: string | null;
  session_name: string;
  session_dir: string;
  parent_session_key: string;
  parent_branch_anchor: string;
  status: ChildStatus;
  model: string;
  prompt: string;
  timeout_ms: number;
  retain: boolean;
  cwd?: string;
  thinking?: string;
  continuation_mode?: "live" | "session";
  usage: ChildUsage | null;
  usage_records: number;
  usage_unknown_records: number;
  answer_preview?: string;
  result?: string;
  result_status?: "partial" | "complete";
  terminal_reason?: ChildLifecycleEvent;
  messages?: string[];
  error?: string;
  started_at: string;
  ended_at?: string;
};

type AssistantOutcome = {
  text: string;
  hasText: boolean;
  stopReason?: string;
  error?: string;
};

type ChildProcessState = {
  entry: ChildEntry;
  process: ChildProcessWithoutNullStreams;
  buffer: string;
  stderr: string;
  initialPromptId: string;
  initialPromptPending: boolean;
  lastAssistant?: AssistantOutcome;
  inputClosed?: boolean;
  exited?: boolean;
  termination?: Promise<void>;
  timeout?: NodeJS.Timeout;
  removeAbortListener?: () => void;
};

type MessageRecord = { timestamp: string; message: string; sender: string };
const DEFAULT_CHILD_TIMEOUT_MS = 1_800_000;
const MAX_CHILD_TIMEOUT_MS = 3_600_000;
const TERMINATION_GRACE_MS = 500;
const USAGE_FIELDS = ["input", "output", "cacheRead", "cacheWrite", "totalTokens", "cost"] as const;

function reportedNumber(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : null;
}

function messageUsage(value: unknown): ChildUsage | null {
  if (!value || typeof value !== "object") return null;
  const usage = value as Record<string, unknown>;
  const result: ChildUsage = {
    input: reportedNumber(usage.input),
    output: reportedNumber(usage.output),
    cacheRead: reportedNumber(usage.cacheRead),
    cacheWrite: reportedNumber(usage.cacheWrite),
    totalTokens: reportedNumber(usage.totalTokens),
    cost: reportedNumber(usage.cost && typeof usage.cost === "object" ? (usage.cost as Record<string, unknown>).total : usage.cost),
  };
  if (result.totalTokens === null && [result.input, result.output, result.cacheRead, result.cacheWrite].every((n) => n !== null)) {
    result.totalTokens = result.input! + result.output! + result.cacheRead! + result.cacheWrite!;
  }
  return USAGE_FIELDS.some((field) => result[field] !== null) ? result : null;
}

function stableJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(stableJson).join(",")}]`;
  if (value && typeof value === "object") {
    return `{${Object.entries(value).sort(([a], [b]) => a.localeCompare(b)).map(([k, v]) => `${JSON.stringify(k)}:${stableJson(v)}`).join(",")}}`;
  }
  return JSON.stringify(value) ?? "null";
}

function usageMessageKey(message: Record<string, unknown>): string {
  const id = message.id ?? message.messageId;
  if (typeof id === "string" && id) return `${message.role}:${id}`;
  // Pi's base AssistantMessage has no id. The timestamp and finalized content
  // identify the same snapshot in message_end and agent_end without object identity.
  const { usage: _usage, ...identity } = message;
  return createHash("sha256").update(stableJson(identity)).digest("hex");
}

function boundedTimeout(value: unknown, fallback: number, maximum: number): number {
  return typeof value === "number" && Number.isFinite(value) && value >= 1
    ? Math.min(Math.floor(value), maximum)
    : fallback;
}

function asText(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .map((part) => (part && typeof part === "object" && "text" in part ? String((part as { text?: unknown }).text ?? "") : ""))
    .join("");
}

const MAX_ANSWER_PREVIEW_LENGTH = 4000;
const MAX_STDERR_DIAGNOSTIC_LENGTH = 4000;

function errorText(value: unknown): string | undefined {
  if (typeof value === "string" && value.trim()) return value;
  if (!value || typeof value !== "object") return undefined;
  const message = (value as { message?: unknown }).message;
  return typeof message === "string" && message.trim() ? message : undefined;
}

function assistantOutcome(message: Record<string, unknown>): AssistantOutcome {
  const text = asText(message.content);
  return {
    text,
    hasText: Boolean(text.trim()),
    stopReason: typeof message.stopReason === "string" ? message.stopReason : undefined,
    error: errorText(message.errorMessage) ?? errorText(message.error),
  };
}

export class RlmChildRegistry {
  private readonly children = new Map<string, ChildProcessState>();
  private readonly sessionDir: string;
  private readonly metadataPath: string;
  private readonly maxDepth: number;
  private readonly depth: number;
  private readonly piExecutable: string;
  private readonly extensionPath?: string;
  private readonly approvalBypass?: () => boolean;
  private readonly onChange?: (event: ChildLifecycleEvent, entry: ChildEntry) => void;
  private parentSessionKey: string;
  private parentBranchAnchor: string;
  private readonly cwd: string;
  private persisted: ChildEntry[] = [];
  private readonly usageRecords = new Map<string, Map<string, ChildUsage | null>>();
  private shuttingDown = false;

  constructor(options: {
    sessionId: string;
    cwd: string;
    sessionDir?: string;
    maxDepth?: number;
    depth?: number;
    piExecutable?: string;
    extensionPath?: string;
    approvalBypass?: () => boolean;
    onChange?: (event: ChildLifecycleEvent, entry: ChildEntry) => void;
    parentBranchAnchor?: string;
    childTimeoutMs?: number;
  }) {
    this.sessionDir = options.sessionDir ?? join(homedir(), ".pi", "rlm-sessions", options.sessionId);
    this.metadataPath = join(this.sessionDir, "registry.json");
    this.maxDepth = options.maxDepth ?? Number(process.env.PI_RLM_MAX_DEPTH ?? 3);
    this.depth = options.depth ?? Number(process.env.PI_RLM_DEPTH ?? 0);
    this.piExecutable = options.piExecutable ?? process.env.PI_RLM_PI_EXECUTABLE ?? "pi";
    this.extensionPath = options.extensionPath;
    this.approvalBypass = options.approvalBypass;
    this.onChange = options.onChange;
    this.parentSessionKey = options.sessionId;
    this.parentBranchAnchor = options.parentBranchAnchor ?? "root";
    this.childTimeoutMs = boundedTimeout(
      options.childTimeoutMs ?? Number(process.env.PI_RLM_CHILD_TIMEOUT_MS),
      DEFAULT_CHILD_TIMEOUT_MS,
      MAX_CHILD_TIMEOUT_MS,
    );
    this.parentDir = process.env.PI_RLM_PARENT_DIR;
    this.parentId = process.env.PI_RLM_PARENT_ID;
    this.cwd = options.cwd;
    mkdirSync(this.sessionDir, { recursive: true, mode: 0o700 });
    this.load();
  }

  private readonly parentDir?: string;
  private readonly childTimeoutMs: number;
  private readonly parentId?: string;
  setParentContext(sessionKey: string, branchAnchor: string): void {
    this.parentSessionKey = sessionKey;
    this.parentBranchAnchor = branchAnchor;
  }

  private notify(event: ChildLifecycleEvent, entry: ChildEntry): void {
    try {
      this.onChange?.(event, { ...entry });
    } catch {
      // Lifecycle reporting must never break child execution.
    }
  }


  private load(): void {
    if (!existsSync(this.metadataPath)) return;
    try {
      const parsed = JSON.parse(readFileSync(this.metadataPath, "utf8")) as {
        children?: unknown;
        usage_records?: Record<string, Record<string, ChildUsage | null>>;
      };
      const children = Array.isArray(parsed.children) ? parsed.children : [];
      this.persisted = children
        .filter((entry): entry is Record<string, unknown> => Boolean(entry) && typeof entry === "object")
        .map((entry) => ({
          ...(entry as unknown as ChildEntry),
          parent_session_key: typeof entry.parent_session_key === "string" ? entry.parent_session_key : "legacy",
          parent_branch_anchor: typeof entry.parent_branch_anchor === "string" ? entry.parent_branch_anchor : "legacy",
          timeout_ms: boundedTimeout(entry.timeout_ms, this.childTimeoutMs, MAX_CHILD_TIMEOUT_MS),
          retain: entry.retain === true,
          usage: messageUsage(entry.usage),
          usage_records: reportedNumber(entry.usage_records) ?? 0,
          usage_unknown_records: reportedNumber(entry.usage_unknown_records) ?? 0,
        }));
      for (const entry of this.persisted) {
        const stored = parsed.usage_records?.[entry.rlm_child_id];
        if (stored && typeof stored === "object") {
          this.usageRecords.set(entry.rlm_child_id, new Map(Object.entries(stored).map(([key, usage]) => [key, messageUsage(usage)])));
          this.updateUsage(entry);
        }
        if (entry.status === "running") {
          entry.status = "error";
          entry.terminal_reason = "error";
          entry.error = "child interrupted: registry reloaded without its process";
          entry.ended_at = new Date().toISOString();
          if (entry.result) entry.result_status = "partial";
        }
      }
    } catch {
      this.persisted = [];
      this.usageRecords.clear();
    }
    this.save();
  }

  private save(): void {
    const live = [...this.children.values()].map(({ entry }) => entry);
    const byId = new Map([...this.persisted, ...live].map((entry) => [entry.rlm_child_id, entry]));
    this.persisted = [...byId.values()];
    const usage_records = Object.fromEntries([...this.usageRecords].map(([id, records]) => [id, Object.fromEntries(records)]));
    writeFileSync(this.metadataPath, JSON.stringify({ version: 2, children: this.persisted, usage_records }, null, 2) + "\n", { mode: 0o600 });
  }

  async spawn(prompt: string, kwargs: Record<string, unknown>, signal?: AbortSignal): Promise<ChildEntry> {
    if (this.shuttingDown) throw new Error("RLM registry is shut down");
    if (this.depth >= this.maxDepth) throw new Error(`RLM maximum depth ${this.maxDepth} reached`);
    if (typeof prompt !== "string" || !prompt.trim()) throw new Error("rlm.spawn prompt must be a non-empty string");
    const name = typeof kwargs.name === "string" ? kwargs.name.trim() : "";
    if (!name) throw new Error("rlm.spawn name must be a non-empty string");
    if (name.length > 64) throw new Error("rlm.spawn name must be at most 64 characters");
    const unknown = Object.keys(kwargs).filter((key) => !["name", "model", "thinking", "timeout_ms", "retain"].includes(key));
    if (unknown.length) throw new Error(`rlm.spawn unknown options: ${unknown.join(", ")}`);
    if ("retain" in kwargs && typeof kwargs.retain !== "boolean") throw new Error("rlm.spawn retain must be a boolean");
    signal?.throwIfAborted();

    const id = `sub-${randomUUID().slice(0, 8)}`;
    const childDir = join(this.sessionDir, id);
    mkdirSync(childDir, { recursive: true, mode: 0o700 });
    const model = typeof kwargs.model === "string" && kwargs.model.trim() ? kwargs.model.trim() : "inherited";
    const timeoutMs = boundedTimeout(kwargs.timeout_ms, this.childTimeoutMs, MAX_CHILD_TIMEOUT_MS);
    const entry: ChildEntry = {
      rlm_child_id: id,
      active_session_id: null,
      session_id: null,
      session_name: name,
      session_dir: childDir,
      parent_session_key: this.parentSessionKey,
      parent_branch_anchor: this.parentBranchAnchor,
      status: "running",
      model,
      prompt,
      timeout_ms: timeoutMs,
      retain: kwargs.retain === true,
      cwd: this.cwd,
      thinking: typeof kwargs.thinking === "string" ? kwargs.thinking.trim() || undefined : undefined,
      usage: null,
      usage_records: 0,
      usage_unknown_records: 0,
      started_at: new Date().toISOString(),
    };
    this.startProcess(entry, signal);
    return entry;
  }

  // Returns admission, not completion. collect/list report the new invocation.
  // A session resume restores conversation history, not a Python checkpoint.
  async continue(selector: string, prompt: string, signal?: AbortSignal): Promise<ChildEntry> {
    if (this.shuttingDown) throw new Error("RLM registry is shut down");
    if (typeof prompt !== "string" || !prompt.trim()) throw new Error("rlm.continue prompt must be a non-empty string");
    signal?.throwIfAborted();
    const selected = this.resolve(selector);
    if (!selected.retain) throw new Error("rlm.continue requires a retained child");
    if (selected.status !== "completed") throw new Error(`rlm.continue requires a completed child, got ${selected.status}`);
    const state = this.children.get(selected.rlm_child_id);
    const live = state && !state.exited && !state.termination && !state.inputClosed
      && state.process.exitCode === null && state.process.signalCode === null
      && state.process.stdin.writable && !state.process.stdin.destroyed;
    // Discover before mutating the record so a missing session leaves it intact.
    const session = live ? undefined : this.findSession(selected);
    const entry = live ? state.entry : selected;
    entry.status = "running";
    entry.prompt = prompt;
    entry.started_at = new Date().toISOString();
    entry.continuation_mode = live ? "live" : "session";
    delete entry.ended_at;
    delete entry.terminal_reason;
    delete entry.error;
    delete entry.result;
    delete entry.result_status;
    delete entry.answer_preview;
    if (live) {
      this.beginInvocation(state, signal);
    } else {
      entry.cwd = entry.cwd ?? session!.cwd ?? this.cwd;
      entry.session_id = session!.id;
      this.startProcess(entry, signal, session!.path);
    }
    return entry;
  }

  private findSession(entry: ChildEntry): { path: string; id: string; cwd?: string } {
    const candidates: { path: string; id: string; cwd?: string }[] = [];
    if (existsSync(entry.session_dir)) {
      for (const file of readdirSync(entry.session_dir, { withFileTypes: true })) {
        if (!file.isFile() || !file.name.endsWith(".jsonl")) continue;
        const path = join(entry.session_dir, file.name);
        try {
          const header = JSON.parse(readFileSync(path, "utf8").split("\n", 1)[0]);
          if (header.type === "session" && typeof header.id === "string") {
            candidates.push({ path, id: header.id, cwd: typeof header.cwd === "string" ? header.cwd : undefined });
          }
        } catch { /* Ignore non-session artifacts. */ }
      }
    }
    const matches = entry.session_id ? candidates.filter((candidate) => candidate.id === entry.session_id) : candidates;
    if (matches.length !== 1) throw new Error(matches.length
      ? `Multiple session files found for retained child ${entry.rlm_child_id}`
      : `No session file found for retained child ${entry.rlm_child_id}`);
    return matches[0];
  }

  private startProcess(entry: ChildEntry, signal?: AbortSignal, sessionFile?: string): void {
    const args = ["--mode", "rpc", "--offline", "--session-dir", entry.session_dir];
    if (sessionFile) args.push("--session", sessionFile);
    // A resumed session already stores the model and thinking level.
    if (!sessionFile && entry.model !== "inherited") args.push("--model", entry.model);
    if (!sessionFile && entry.thinking) args.push("--thinking", entry.thinking);
    if (this.extensionPath) args.push("--extension", this.extensionPath);
    const child = spawn(this.piExecutable, args, {
      cwd: entry.cwd ?? this.cwd,
      detached: process.platform !== "win32",
      env: {
        ...process.env,
        PI_RLM_DEPTH: String(this.depth + 1),
        PI_RLM_PARENT_ID: entry.rlm_child_id,
        PI_RLM_PARENT_DIR: this.sessionDir,
        PI_RLM_PARENT_SESSION_KEY: entry.parent_session_key,
        PI_RLM_PARENT_BRANCH_ANCHOR: entry.parent_branch_anchor,
        PI_RLM_EXPLICIT_MODEL: sessionFile || entry.model !== "inherited" ? "1" : "0",
        PI_JEV_AUTO_RUN: this.approvalBypass?.() ? "1" : "0",
      },
      stdio: ["pipe", "pipe", "pipe"],
    });
    const initialPromptId = `prompt-${randomUUID()}`;
    const state: ChildProcessState = {
      entry,
      process: child,
      buffer: "",
      stderr: "",
      initialPromptId,
      initialPromptPending: true,
    };
    this.children.set(entry.rlm_child_id, state);
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => {
      state.buffer += chunk;
      const lines = state.buffer.split("\n");
      state.buffer = lines.pop() ?? "";
      for (const line of lines) this.handleChildEvent(state, line);
    });
    child.stdout.once("end", () => this.flushChildOutput(state));
    child.stderr.on("data", (chunk: Buffer) => this.appendStderr(state, chunk.toString("utf8")));
    child.stdin.on("error", (error) => {
      if (state.entry.status === "running") this.finish(state, "error", "error", `child stdin error: ${error.message}`);
    });
    child.once("error", (error) => {
      this.finish(state, "error", "error", error.message);
    });
    child.once("exit", () => {
      state.exited = true;
      // A tool descendant can outlive Pi and keep its output pipes open.
      void this.terminateProcess(state);
    });
    child.once("close", (code, signalName) => {
      this.flushChildOutput(state);
      if (state.entry.status === "running") {
        this.finishUnexpectedProcessExit(state, code, signalName);
      } else {
        this.clearRuntimeGuards(state);
        this.save();
      }
    });
    this.beginInvocation(state, signal);
  }

  private beginInvocation(state: ChildProcessState, signal?: AbortSignal): void {
    this.clearRuntimeGuards(state);
    state.lastAssistant = undefined;
    state.stderr = "";
    state.initialPromptId = `prompt-${randomUUID()}`;
    state.initialPromptPending = true;
    const timeoutMs = state.entry.timeout_ms;
    state.timeout = setTimeout(() => {
      this.finish(state, "cancelled", "timeout", `child exceeded ${timeoutMs}ms timeout`);
    }, timeoutMs);
    state.timeout.unref?.();
    const abort = () => this.finish(state, "cancelled", "cancelled", "child cancelled by parent signal");
    if (signal) {
      signal.addEventListener("abort", abort, { once: true });
      state.removeAbortListener = () => signal.removeEventListener("abort", abort);
    }
    this.save();
    this.notify("spawned", state.entry);
    if (signal?.aborted) abort();
    if (state.entry.status !== "running") return;
    try {
      this.sendPrompt(state, state.entry.prompt, state.initialPromptId);
    } catch (error) {
      this.finish(state, "error", "error", `failed to send child prompt: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private terminateProcess(state: ChildProcessState): Promise<void> {
    if (state.termination) return state.termination;
    this.closeInput(state);
    const pid = state.process.pid;
    if (!pid) return Promise.resolve();
    const target = process.platform === "win32" ? pid : -pid;
    const send = (signal: NodeJS.Signals | 0): boolean => {
      // Never use child.killed here: it means a signal was sent, not that it exited.
      if (process.platform === "win32" && state.exited) return false;
      try { process.kill(target, signal); return true; }
      catch { return false; }
    };
    send("SIGTERM");
    state.termination = new Promise((resolve) => {
      const deadline = Date.now() + TERMINATION_GRACE_MS;
      const poll = () => {
        if (!send(0)) { resolve(); return; }
        if (Date.now() >= deadline) {
          // Signal the whole group even if the direct child has already exited.
          send("SIGKILL");
          setTimeout(resolve, 25);
          return;
        }
        setTimeout(poll, 25);
      };
      poll();
    });
    return state.termination;
  }

  private clearRuntimeGuards(state: ChildProcessState): void {
    if (state.timeout) clearTimeout(state.timeout);
    state.timeout = undefined;
    state.removeAbortListener?.();
    state.removeAbortListener = undefined;
  }

  private closeInput(state: ChildProcessState): void {
    if (state.inputClosed) return;
    state.inputClosed = true;
    try {
      if (!state.process.stdin.destroyed && state.process.stdin.writable) state.process.stdin.end();
    } catch {
      // The child is already exiting; its terminal state remains authoritative.
    }
  }

  private appendStderr(state: ChildProcessState, text: string): void {
    state.stderr = `${state.stderr}${text}`.slice(-MAX_STDERR_DIAGNOSTIC_LENGTH);
  }

  private flushChildOutput(state: ChildProcessState): void {
    if (!state.buffer) return;
    const line = state.buffer;
    state.buffer = "";
    this.handleChildEvent(state, line);
  }

  private finish(state: ChildProcessState, status: ChildStatus, event: ChildLifecycleEvent, error?: string): void {
    if (state.entry.status !== "running") return;
    state.entry.status = status;
    state.entry.terminal_reason = event;
    state.entry.ended_at = new Date().toISOString();
    if (status === "completed") delete state.entry.error;
    else if (error) state.entry.error = error;
    this.clearRuntimeGuards(state);
    if (status !== "completed") {
      if (state.entry.result) state.entry.result_status = "partial";
      void this.terminateProcess(state);
    } else if (!state.entry.retain) {
      this.closeInput(state);
    }
    this.save();
    this.notify(event, state.entry);
  }

  private finishUnexpectedProcessExit(
    state: ChildProcessState,
    code: number | null,
    signalName: NodeJS.Signals | null,
  ): void {
    const exit = signalName
      ? `child terminated by ${signalName}`
      : `child exited with code ${code ?? "unknown"} before completing an agent turn`;
    const diagnostic = state.stderr.trim();
    this.finish(state, "error", "error", diagnostic ? `${exit}: ${diagnostic}` : exit);
  }

  private sendPrompt(state: ChildProcessState, message: string, id?: string): void {
    if (!state.process.stdin.writable || state.inputClosed) throw new Error("RLM child stdin is not writable");
    const command: { type: "prompt"; message: string; id?: string } = { type: "prompt", message };
    if (id) command.id = id;
    state.process.stdin.write(`${JSON.stringify(command)}\n`);
  }

  private updateUsage(entry: ChildEntry): void {
    const records = [...(this.usageRecords.get(entry.rlm_child_id)?.values() ?? [])];
    entry.usage_records = records.length;
    entry.usage_unknown_records = records.filter((usage) => !usage || USAGE_FIELDS.some((field) => usage[field] === null)).length;
    if (!records.some((usage) => usage !== null)) { entry.usage = null; return; }
    const total = {} as ChildUsage;
    for (const field of USAGE_FIELDS) {
      total[field] = records.some((usage) => usage?.[field] == null)
        ? null : records.reduce((sum, usage) => sum + usage![field]!, 0);
    }
    entry.usage = total;
  }

  private recordUsage(entry: ChildEntry, message: Record<string, unknown>): void {
    // Tool usage is optional; ordinary tools are not unknown model usage records.
    if (message.role !== "assistant" && !(message.role === "toolResult" && message.usage != null)) return;
    let records = this.usageRecords.get(entry.rlm_child_id);
    if (!records) {
      records = new Map();
      if (entry.usage) records.set("legacy-total", entry.usage);
      this.usageRecords.set(entry.rlm_child_id, records);
    }
    const key = usageMessageKey(message);
    const usage = messageUsage(message.usage);
    const previous = records.get(key);
    if (previous && usage) {
      for (const field of USAGE_FIELDS) usage[field] ??= previous[field];
    }
    records.set(key, usage ?? previous ?? null);
    this.updateUsage(entry);
  }

  private recordAssistantMessage(state: ChildProcessState, message: Record<string, unknown>): AssistantOutcome {
    const outcome = assistantOutcome(message);
    state.lastAssistant = outcome;
    if (outcome.hasText) {
      const finalAnswer = outcome.stopReason === "stop" && !outcome.error;
      const preserveExisting = Boolean(state.entry.result) && (outcome.stopReason === "error" || outcome.stopReason === "aborted" || outcome.error);
      if (!preserveExisting) {
        state.entry.result = outcome.text;
        state.entry.answer_preview = outcome.text.slice(0, MAX_ANSWER_PREVIEW_LENGTH);
      }
      if (!finalAnswer && state.entry.result_status !== "complete") state.entry.result_status = "partial";
    }
    return outcome;
  }

  private recordAgentEnd(state: ChildProcessState, event: Record<string, unknown>): void {
    const messages = Array.isArray(event.messages) ? event.messages : [];
    for (const message of messages) {
      if (!message || typeof message !== "object") continue;
      const candidate = message as Record<string, unknown>;
      this.recordUsage(state.entry, candidate);
      if (state.entry.status === "running" && candidate.role === "assistant") this.recordAssistantMessage(state, candidate);
    }
  }

  private finishAgentSettled(state: ChildProcessState): void {
    if (state.entry.status !== "running") return;
    const assistant = state.lastAssistant;
    if (!assistant) {
      this.finish(state, "error", "error", "child settled without an assistant response");
      return;
    }
    const detail = assistant.error ? `: ${assistant.error}` : "";
    if (assistant.stopReason === "error" || assistant.error) {
      this.finish(state, "error", "error", `child assistant failed${detail}`);
      return;
    }
    if (assistant.stopReason === "aborted") {
      this.finish(state, "cancelled", "cancelled", `child assistant was aborted${detail}`);
      return;
    }
    if (!assistant.stopReason) {
      this.finish(state, "error", "error", "child settled without a final stop reason");
      return;
    }
    if (assistant.stopReason !== "stop") {
      this.finish(state, "error", "error", assistant.stopReason === "length"
        ? "child response truncated: stopReason length"
        : `child settled without a final response: stopReason ${assistant.stopReason}`);
      return;
    }
    if (assistant.hasText) state.entry.result_status = "complete";
    this.finish(state, "completed", "completed");
  }

  private handlePromptResponse(state: ChildProcessState, event: Record<string, unknown>): void {
    if (event.command !== "prompt" || !state.initialPromptPending) return;
    if (event.id !== state.initialPromptId) return;
    if (event.success === true) {
      state.initialPromptPending = false;
      return;
    }
    if (event.success === false) {
      state.initialPromptPending = false;
      const reason = typeof event.error === "string" && event.error.trim() ? event.error : "child rejected prompt";
      this.finish(state, "error", "error", `child prompt rejected: ${reason}`);
    }
  }

  private handleChildEvent(state: ChildProcessState, line: string): void {
    if (!line.trim() || this.children.get(state.entry.rlm_child_id) !== state) return;
    try {
      const event = JSON.parse(line) as Record<string, unknown>;
      if (event.type === "session") state.entry.session_id = typeof event.id === "string" ? event.id : null;
      if (event.type === "message_end" && event.message && typeof event.message === "object") {
        const message = event.message as Record<string, unknown>;
        this.recordUsage(state.entry, message);
        if (state.entry.status === "running" && message.role === "assistant") this.recordAssistantMessage(state, message);
      }
      if (event.type === "response") this.handlePromptResponse(state, event);
      if (event.type === "agent_end") this.recordAgentEnd(state, event);
      if (event.type === "agent_settled") this.finishAgentSettled(state);
      this.save();
    } catch {
      // The child protocol is diagnostic; a malformed line must not crash the parent.
    }
  }

  private readInbox(entry: ChildEntry): string[] {
    const path = join(this.sessionDir, `${entry.rlm_child_id}.messages.jsonl`);
    if (!existsSync(path)) return [];
    try {
      return readFileSync(path, "utf8")
        .split("\n")
        .filter(Boolean)
        .flatMap((line) => {
          try {
            const value = JSON.parse(line) as MessageRecord;
            return typeof value.message === "string" ? [value.message] : [];
          } catch {
            return [];
          }
        });
    } catch {
      return [];
    }
  }

  list(): ChildEntry[] {
    const live = [...this.children.values()].map(({ entry }) => ({ ...entry, messages: this.readInbox(entry) }));
    const byId = new Map([...this.persisted, ...live].map((entry) => [entry.rlm_child_id, entry]));
    return [...byId.values()].map((entry) => ({ ...entry, messages: this.readInbox(entry) }));
  }

  async delete(selector: string): Promise<ChildEntry> {
    const entry = this.resolve(selector);
    const live = this.children.get(entry.rlm_child_id);
    if (live) {
      this.finish(live, "cancelled", "cancelled", "child deleted");
      await this.terminateProcess(live);
    } else if (entry.status === "running") {
      entry.status = "cancelled";
      entry.terminal_reason = "cancelled";
      entry.ended_at = new Date().toISOString();
      entry.error = "child deleted";
      this.notify("cancelled", entry);
    }
    this.persisted = this.persisted.filter((candidate) => candidate.rlm_child_id !== entry.rlm_child_id);
    this.children.delete(entry.rlm_child_id);
    this.usageRecords.delete(entry.rlm_child_id);
    this.save();
    return entry;
  }

  async collect(selectors: string[], timeoutMs: number): Promise<ChildEntry[]> {
    const deadline = Date.now() + Math.max(0, Math.min(timeoutMs || 0, 120000));
    const matches = () => {
      const entries = this.list();
      return selectors.length
        ? entries.filter((entry) => selectors.includes(entry.rlm_child_id) || selectors.includes(entry.session_id ?? "") || selectors.includes(entry.session_name))
        : entries;
    };
    let entries = matches();
    while (timeoutMs > 0 && entries.some((entry) => entry.status === "running") && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 50));
      entries = matches();
    }
    return entries;
  }

  async message(selector: string, message: string): Promise<{ delivered: boolean; child: ChildEntry }> {
    if (!message.trim()) throw new Error("agent message must be non-empty");
    const entry = this.resolve(selector);
    const live = this.children.get(entry.rlm_child_id);
    if (!live || !live.process.stdin.writable || entry.status !== "running") return { delivered: false, child: entry };
    this.sendPrompt(live, message);
    return { delivered: true, child: entry };
  }

  sendToParent(message: string): { delivered: boolean; receiver: string } {
    if (!this.parentDir || !this.parentId) throw new Error("This RLM session has no parent receiver");
    const path = join(this.parentDir, `${this.parentId}.messages.jsonl`);
    appendFileSync(path, JSON.stringify({ timestamp: new Date().toISOString(), message, sender: this.sessionDir }) + "\n", { mode: 0o600 });
    return { delivered: true, receiver: "parent" };
  }

  private resolve(selector: string): ChildEntry {
    const matches = this.list().filter((entry) => [entry.rlm_child_id, entry.session_id, entry.session_name].includes(selector));
    if (matches.length !== 1) throw new Error(matches.length ? `Child selector ${selector} is ambiguous` : `Unknown child ${selector}`);
    return matches[0];
  }

  async shutdown(): Promise<void> {
    this.shuttingDown = true;
    const terminations: Promise<void>[] = [];
    for (const state of this.children.values()) {
      if (state.entry.status === "running") {
        this.finish(state, "cancelled", "cancelled", "RLM registry shutdown");
      }
      terminations.push(this.terminateProcess(state));
    }
    await Promise.all(terminations);
    this.save();
    this.children.clear();
  }
}
