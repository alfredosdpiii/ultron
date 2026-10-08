import { randomUUID } from "node:crypto";
import { lstat } from "node:fs/promises";
import { isAbsolute, join, relative, sep } from "node:path";
import type { AgentHarness, AgentLane, Context, Entry } from "@ultron/agent-core";
import { isJsonValue, type JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT, withAbortSignal, withoutAbortSignal } from "@ultron/chord/context";
import { FRAME_THINKING_LEVELS, type FrameThinkingLevel } from "../../core/settings-manager.ts";
import type {
	NativeUsageCallStatus,
	NativeUsageLedgerLike,
	NativeUsageMeasurement,
	NativeUsageReservation,
} from "../usage.ts";
import { diffSnapshots, snapshotWorkspace, type WorkspaceSnapshot } from "../workspace-snapshot.ts";
import {
	type NativeDefinition,
	type NativeDefinitionAdapter,
	NativeDefinitionRegistry,
	type NativeDefinitionStore,
} from "./definition-registry.ts";
import { type HostCaller, type NativeHostApi, type NativeHostModule, ROOT_CALLER } from "./host-module.ts";
import {
	type NativeHostStore,
	type NativeTask,
	NativeTaskJournal,
	type NativeResult as StoredTaskResult,
	taskFingerprint,
} from "./task-store.ts";
import { checkFiles, MAX_VERDICT_REJECTIONS, type Verdict, type VerdictCheck, validateVerdict } from "./verdict.ts";
import {
	type ConflictFile,
	commitChildWorktree,
	commitMessage,
	createChildWorktree,
	keepWorktrees,
	listDirty,
	listRecords,
	mergeChildWorktree,
	pruneWorktrees,
	removeChildWorktree,
	runSetupCommand,
	saveRecord,
	WorktreeError,
	type WorktreeInfo,
	type WorktreeRecord,
	type WorktreeSetup,
	worktreeBriefNote,
	worktreeEnv,
} from "./worktrees.ts";

type Payload = Record<string, unknown>;
export type NativeResult = StoredTaskResult;

type TaskRecord = NativeTask & {
	promise?: Promise<NativeResult>;
	resolve?: (result: NativeResult) => void;
	reject?: (error: unknown) => void;
	finishing?: Promise<NativeResult>;
	lane?: AgentLane;
	laneName?: string;
	controller?: AbortController;
	cleanup?: () => void;
	usageReservation?: NativeUsageReservation;
	usageSettled?: boolean;
	/** Provider-reported cost of the task's own model run, when known. */
	cost?: number;
	/** Detached work whose end is announced as a completion event (`rlm.spawn`, `agents.spawn`, `background.start`). */
	notify?: DetachedEndKind;
	/** Requests currently waiting on this task's result (a cell that is waiting already gets the result). */
	waiters?: number;
	/** In-memory timing and provenance for the read-only graph view (`agents.status {graph: true}`). */
	startedAt?: number;
	endedAt?: number;
	tokens?: number;
	inputPreview?: string;
	workflow?: WorkflowMembership;
	/** Live lane stats for the RLM pane: current model, assistant turns, tool calls, latest assistant text. */
	model?: string;
	turns?: number;
	toolCallCount?: number;
	lastText?: string;
	/** For an `rlm.spawn` subagent: how many more nesting levels it may create (`depth=` at its spawn; in memory). */
	spawnDepth?: number;
	/** A subagent's accepted `rlm.finish` verdict (the latest valid call wins). */
	verdict?: Verdict;
	/** Rejected `rlm.finish` calls, and the problems of the latest one. */
	verdictRejections?: number;
	verdictProblems?: string[];
	/** The rejections ran out with no valid verdict: the subagent's reply is returned unverified. */
	verdictInvalid?: boolean;
	/** A subagent started with `worktree=True`: its private Git worktree (see worktrees.ts). */
	worktree?: WorktreeRecord;
	/** The worktree's files when the child started (after setup): what it changed is committed on its branch. */
	worktreeBaseline?: WorkspaceSnapshot;
	/** The parent's tree when a worktree child started, to see writes it made outside its worktree. */
	outsideBefore?: WorkspaceSnapshot;
	/** How `rlm.merge` last ended for this child (in memory, for the graph view). */
	mergeStatus?: string;
};

/** A task's place in a `workflows.run` graph: the run, its node id, and the dependencies it joined. */
type WorkflowMembership = { run: string; node: string; dependsOn: string[]; join: "all" | "any" };

/** Tasks listed by `agents.status {graph: true}` (the newest are kept). */
const GRAPH_TASK_LIMIT = 300;
const GRAPH_PREVIEW_CHARS = 240;
/** The latest assistant text of a task's lane, as shown in the RLM pane's node cards. */
const GRAPH_TEXT_CHARS = 160;
const GRAPH_MODEL_CHARS = 80;
/** Workflow runs listed by `agents.status {graph: true}` (the newest), and planned nodes per run. */
const GRAPH_WORKFLOW_LIMIT = 20;
const GRAPH_WORKFLOW_NODE_LIMIT = 64;

/** A `workflows.run` call's plan, kept in memory for the graph view: nodes not yet admitted show as pending. */
type WorkflowRunRecord = {
	run: string;
	parentId?: string;
	startedAt: number;
	endedAt?: number;
	nodes: { id: string; definition: string; dependsOn: string[]; join: "all" | "any" }[];
	/** How each finished node ended (a skipped node never gets a task), with a bounded reason. */
	ended: Record<string, { status: string; reason?: string }>;
};

/** The graph view of a workflow run: its planned nodes (bounded) and how the finished ones ended. */
function graphWorkflow(record: WorkflowRunRecord): Record<string, unknown> {
	const nodes = record.nodes.slice(0, GRAPH_WORKFLOW_NODE_LIMIT);
	return {
		run: record.run,
		...(record.parentId === undefined ? {} : { parentId: record.parentId }),
		startedAt: record.startedAt,
		...(record.endedAt === undefined ? {} : { endedAt: record.endedAt }),
		nodes: nodes.map((node) => ({ ...node, dependsOn: [...node.dependsOn], ...(record.ended[node.id] ?? {}) })),
		...(record.nodes.length > nodes.length ? { truncatedNodes: record.nodes.length - nodes.length } : {}),
	};
}

export type DetachedEndKind = "child_done" | "task_done";

/** A detached task reached its durable terminal result. */
export type DetachedTaskEnd = {
	task: NativeTask;
	kind: DetachedEndKind;
	/** Lane that started the task: "main" for root-owned work, else the parent task's lane. */
	ownerLane: string;
	/** Usage root the task was admitted under (a root turn, or a chain of turns it continues). */
	rootId: string | undefined;
	/** A request was waiting on the result when it ended. */
	awaited: boolean;
	/** How the owner fetches the full result. */
	fetch: string;
	cost: number | null;
};

type TaskRequest = {
	definition: string;
	input: JsonValue;
	model?: string;
	key?: string;
	timeoutMs: number;
	/** Run on this existing lane (a retained instance) instead of a fresh task lane. */
	lane?: string;
	/** Set for a workflow node's task (read-only provenance for the graph view). */
	workflow?: WorkflowMembership;
	/** A subagent's private worktree, created before admission (`worktree=True`). */
	worktree?: WorktreeRecord;
	/** `rlm.spawn(thinking=...)`: the child lane's thinking level. */
	thinking?: FrameThinkingLevel;
	/** `rlm.spawn(fork=True)`: the parent's entry the child's lane starts at (its conversation so far). */
	forkAt?: string;
	/** No wall-clock limit (a session goal's job): `timeoutMs` is ignored. */
	untimed?: boolean;
	/** Usage root to admit the task under instead of the caller's (a session goal's own root). */
	usageRoot?: string;
};

/** Leads a forked child's brief: the conversation above is its parent's, and its kernel does not have its state. */
const FORK_BRIEF_NOTE =
	"You are a subagent forked from the conversation above: it is your parent's context. Your REPL kernel is new: variables, imports and functions from earlier cells do not exist (files do). Do only the brief below.";

/**
 * Where a fork of `path` (a branch, oldest first) starts: the newest entry after which no tool call waits for its
 * result, so the cell running `rlm.spawn` and its siblings stay out; undefined for an empty branch.
 */
export function forkPoint(path: readonly Entry[]): string | undefined {
	const pending = new Set<string>();
	let at: string | undefined;
	for (const entry of path) {
		if (entry.type === "message") {
			const message = entry.message;
			if (message.role === "assistant")
				for (const part of message.content) if (part.type === "toolCall") pending.add(part.id);
			if (message.role === "toolResult") pending.delete(message.toolCallId);
		}
		if (pending.size === 0) at = entry.id;
	}
	return at;
}

type RlmChildHandle = {
	rlm_child_id: string;
	name: string;
	session_dir: string;
	model: string;
	timeout_ms: number;
	parent_branch_anchor: string;
	/** The child's worktree (`worktree=True`), else null. */
	worktree: { branch: string; path: string; cwd: string } | null;
};

type WorkflowRoute = {
	/** Dependency whose succeeded result decides whether this node runs. */
	node: string;
	/** Top-level field of that result to compare; the whole value when absent. */
	field?: string;
	equals: JsonValue;
};

/** Hard upper bound on the rounds of any revision loop. */
const WORKFLOW_MAX_ROUNDS = 10;

type WorkflowRevision = {
	/** Reviewer node: depends on the revised node and is re-run after every round of it. */
	from: string;
	/** Condition on the reviewer's result that ends the loop as converged. */
	until: { field?: string; equals: JsonValue };
	maxRounds: number;
};

type WorkflowNode = Omit<TaskRequest, "input"> & {
	id: string;
	input?: JsonValue;
	dependsOn: string[];
	/**
	 * One dependency (its value is the input) or several (fan-in: an object keyed by dependency ID; under an any-of
	 * join it holds only the dependencies that succeeded).
	 */
	inputFrom?: string | string[];
	when?: WorkflowRoute;
	/** `all`: runs only when every dependency succeeded. `any`: runs when at least one did, once all are terminal. */
	join: "all" | "any";
	revise?: WorkflowRevision;
	/** An `rlm-child@1` node that runs in its own worktree (`worktree: true`), branched when the node starts. */
	worktreeMode?: WorktreeMode;
};

type WorkflowRound = { round: number; work: WorkflowOutcome; review?: WorkflowOutcome };

type WorkflowOutcome = (
	| NativeResult
	| { status: "skipped"; reason: string }
	| { status: "exhausted"; value?: JsonValue; verification: "unverified" }
) & {
	/** Present on a revised node: how its loop ended and every round's work and review results. */
	revision?: { outcome: "converged" | "exhausted" | "failed"; rounds: number; max_rounds: number };
	rounds?: WorkflowRound[];
};

/** The compared value of a route or loop condition, or undefined when the field is absent. */
function conditionMet(value: JsonValue | undefined, condition: { field?: string; equals: JsonValue }): boolean {
	const actual =
		condition.field === undefined
			? value
			: value !== null && typeof value === "object" && !Array.isArray(value)
				? value[condition.field]
				: undefined;
	return actual !== undefined && canonicalJson(actual) === canonicalJson(condition.equals);
}

/**
 * The value a later workflow node binds from a finished one (`inputFrom`, `when`, `revise.until`): a subagent's
 * checked verdict when it gave one, else the node's value.
 */
function boundValue(outcome: WorkflowOutcome | undefined): JsonValue | undefined {
	const result = outcome as (NativeResult & { value?: JsonValue }) | undefined;
	const verdict = result?.verdict as Verdict | null | undefined;
	if (verdict === null || verdict === undefined || typeof verdict !== "object") return result?.value;
	return {
		status: verdict.status,
		summary: verdict.summary,
		outputs: verdict.outputs,
		evidence: verdict.evidence,
		changed_files: verdict.changed_files,
		check: (result?.check as VerdictCheck | undefined)?.outcome ?? "unchecked",
		reply: typeof result?.value === "string" ? result.value : null,
	};
}

/**
 * A workflow node backed by a subagent succeeds only when its verdict passed and was not contradicted; a subagent
 * without a verdict keeps its plain success (it is marked unverified).
 */
function verdictGate(result: NativeResult): NativeResult {
	const verdict = result.verdict as Verdict | null | undefined;
	if (result.status !== "succeeded" || verdict === null || verdict === undefined) return result;
	const check = result.check as VerdictCheck | undefined;
	if (verdict.status !== "passed")
		return { ...result, status: "failed", error: `Subagent verdict ${verdict.status}: ${verdict.summary}` };
	if (check?.outcome === "contradicted")
		return {
			...result,
			status: "failed",
			error: `Subagent verdict contradicted: declared as changed but unchanged: ${check.unobserved.join(", ")}`,
		};
	return result;
}

function canonicalJson(value: JsonValue): string {
	if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
	if (value !== null && typeof value === "object")
		return `{${Object.keys(value)
			.sort()
			.map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
			.join(",")}}`;
	return JSON.stringify(value);
}

function objectInput(value: unknown): Record<string, unknown> {
	if (value === null || typeof value !== "object" || Array.isArray(value))
		throw new Error("agent input must be a JSON object");
	return value as Record<string, unknown>;
}

function textOf(entry: Entry): string {
	if (entry.type !== "message" || entry.message.role !== "assistant") return "";
	return entry.message.content
		.filter((part) => part.type === "text")
		.map((part) => part.text)
		.join("\n");
}

/** The text parts of an assistant message, flattened (thinking and tool calls left out); "" for other messages. */
function assistantText(message: unknown): string {
	const body = message as { role?: unknown; content?: unknown } | undefined;
	if (body?.role !== "assistant" || !Array.isArray(body.content)) return "";
	return body.content
		.filter((part): part is { type: "text"; text: string } => part?.type === "text" && typeof part.text === "string")
		.map((part) => part.text)
		.join(" ")
		.replace(/\s+/g, " ")
		.trim();
}

/** Assistant turns, tool calls and the latest assistant text of one run's entries (the graph view's fallback). */
function runStats(entries: readonly Entry[]): { turns: number; toolCalls: number; text?: string } {
	let turns = 0;
	let toolCalls = 0;
	let text: string | undefined;
	// Entries arrive newest first; the first text seen is the latest.
	for (const entry of entries) {
		if (entry.type !== "message" || entry.message.role !== "assistant") continue;
		turns += 1;
		const content = entry.message.content as readonly { type?: string }[];
		toolCalls += content.filter((part) => part?.type === "toolCall").length;
		const line = assistantText(entry.message);
		if (text === undefined && line.length > 0) text = line;
	}
	return { turns, toolCalls, ...(text === undefined ? {} : { text }) };
}

/** Provider-reported usage of one run's assistant messages; unknown unless every message reports it. */
function runUsage(entries: readonly Entry[]): NativeUsageMeasurement | undefined {
	const total = { inputTokens: 0, outputTokens: 0, totalTokens: 0, cost: 0 };
	let messages = 0;
	for (const entry of entries) {
		if (entry.type !== "message" || entry.message.role !== "assistant") continue;
		const usage = (entry.message as { usage?: unknown }).usage as
			| { input?: unknown; output?: unknown; totalTokens?: unknown; cost?: { total?: unknown } }
			| undefined;
		const values = [usage?.input, usage?.output, usage?.totalTokens, usage?.cost?.total];
		if (!values.every((item) => typeof item === "number" && Number.isFinite(item) && item >= 0)) return undefined;
		total.inputTokens += usage!.input as number;
		total.outputTokens += usage!.output as number;
		total.totalTokens += usage!.totalTokens as number;
		total.cost += usage!.cost!.total as number;
		messages += 1;
	}
	return messages === 0 ? undefined : total;
}

function jsonFrom(text: string): unknown {
	const value = text.trim().replace(/^```(?:json)?\s*\n([\s\S]*?)\n```$/, "$1");
	return JSON.parse(value);
}

function definitionKey(value: unknown): string {
	if (typeof value !== "string" || !/^[a-z][a-z0-9-]*@[0-9]+$/.test(value))
		throw new Error("definition must be id@version");
	return value;
}

/** The kernel call that returns a detached task's full result. */
function fetchHint(task: TaskRecord): string {
	if (task.definition === "rlm-child@1") return `await rlm.collect(["${task.id}"])`;
	if (task.definition === "background-job@1") return `await background.result("${task.id}")`;
	return `await agents.result("${task.id}")`;
}

function publicRecord(task: TaskRecord): NativeTask {
	return {
		id: task.id,
		key: task.key,
		fingerprint: task.fingerprint,
		definition: task.definition,
		state: task.state,
		...(task.result === undefined ? {} : { result: structuredClone(task.result) }),
		...(task.parentId === undefined ? {} : { parentId: task.parentId }),
	};
}

/**
 * Ceiling on `rlm.spawn` nesting (`ULTRON_SPAWN_DEPTH`, default 3; 0 means no ceiling). Below the ceiling a subagent may
 * delegate only as deep as its parent allowed with `rlm.spawn(..., depth=N)`; the default `depth=0` keeps it doing its
 * brief itself. Each level re-sends the guide and its own transcript every turn, so nesting is opt-in per spawn.
 */
export const DEFAULT_SPAWN_DEPTH = 3;

/** The nesting levels a spawned subagent may still create below itself (`depth=` at its spawn; 0 by default). */
function spawnAllowance(task: TaskRecord): number {
	return task.spawnDepth ?? 0;
}

export function spawnDepthLimit(env: NodeJS.ProcessEnv = process.env): number {
	const raw = env.ULTRON_SPAWN_DEPTH?.trim();
	const value = raw ? Number(raw) : Number.NaN;
	return Number.isSafeInteger(value) && value >= 0 ? value : DEFAULT_SPAWN_DEPTH;
}

function publicTask(task: TaskRecord): Record<string, unknown> {
	return {
		id: task.id,
		definition: task.definition,
		state: task.state,
		...(task.parentId === undefined ? {} : { parentId: task.parentId }),
		...(task.result === undefined ? {} : { result: task.result }),
	};
}

function preview(value: unknown, limit = GRAPH_PREVIEW_CHARS): string {
	let text: string;
	try {
		text = typeof value === "string" ? value : (JSON.stringify(value) ?? String(value));
	} catch {
		text = String(value);
	}
	const flat = text.replace(/\s+/g, " ").trim();
	return flat.length > limit ? `${flat.slice(0, limit - 1)}…` : flat;
}

/**
 * Bounded, read-only graph view of a task for the TUI: lane, timing, spend, input and result previews, workflow
 * membership, and how to fetch the full result from Python. Timing and spend exist only for tasks this process ran.
 */
function graphTask(task: TaskRecord): Record<string, unknown> {
	const result = task.result;
	return {
		id: task.id,
		definition: task.definition,
		state: task.state,
		...(task.parentId === undefined ? {} : { parentId: task.parentId }),
		...(task.laneName === undefined ? {} : { lane: task.laneName }),
		...(task.startedAt === undefined ? {} : { startedAt: task.startedAt }),
		...(task.endedAt === undefined ? {} : { endedAt: task.endedAt }),
		...(task.cost === undefined ? {} : { cost: task.cost }),
		...(task.tokens === undefined ? {} : { tokens: task.tokens }),
		...(task.inputPreview === undefined ? {} : { input: task.inputPreview }),
		...(task.workflow === undefined ? {} : { workflow: structuredClone(task.workflow) }),
		...(task.model === undefined ? {} : { model: task.model }),
		...(task.turns === undefined ? {} : { turns: task.turns }),
		...(task.toolCallCount === undefined ? {} : { toolCallCount: task.toolCallCount }),
		...(task.lastText === undefined ? {} : { lastText: task.lastText }),
		...(worktreeLine(task) === undefined ? {} : { worktree: worktreeLine(task) }),
		...(result === undefined
			? {}
			: {
					result: {
						status: result.status,
						...(result.error === undefined ? {} : { error: preview(result.error) }),
						...(result.value === undefined ? {} : { preview: preview(result.value) }),
					},
				}),
		fetch: fetchHint(task),
	};
}

/** A worktree child's branch for the graph view, with how its merge went (`ultron/ab12/fix-parser · merged`). */
function worktreeLine(task: TaskRecord): string | undefined {
	const branch = task.worktree?.branch ?? ((task.result?.worktree ?? null) as WorktreeInfo | null)?.branch;
	if (branch === undefined) return undefined;
	return task.mergeStatus === undefined ? branch : `${branch} · ${task.mergeStatus}`;
}

function fields(payload: Payload, allowed: string[]): void {
	for (const key of Object.keys(payload)) {
		if (!allowed.includes(key)) throw new Error(`Unknown payload field: ${key}`);
	}
}

function nonemptyString(value: unknown, name: string): string {
	if (typeof value !== "string" || !value.trim()) throw new Error(`${name} must be a nonempty string`);
	return value;
}

function taskOptions(payload: Payload): Pick<TaskRequest, "model" | "key" | "timeoutMs"> {
	let model: string | undefined;
	if (payload.model !== undefined && payload.model !== null) {
		if (typeof payload.model !== "string" || !/^[^/\s]+\/[^/\s]+(?:\/[^/\s]+)*$/.test(payload.model))
			throw new Error("model must be provider/model");
		model = payload.model;
	}
	const key = payload.key == null ? undefined : nonemptyString(payload.key, "key");
	const timeoutMs = payload.timeout_ms === undefined ? 30 * 60 * 1000 : payload.timeout_ms;
	if (typeof timeoutMs !== "number" || !Number.isSafeInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 60 * 60 * 1000)
		throw new Error("timeout_ms must be an integer between 1 and 3600000");
	return { model, key, timeoutMs };
}

/** One `rlm-frame@1` task run on its own lane (see inference.ts); the executor owns prompts, repair and budget. */
export type NativeFrameRun = {
	taskId: string;
	input: JsonValue;
	lane: AgentLane;
	laneName: string;
	signal: AbortSignal;
	context: Context;
	usageRootId?: string;
	deadlineAt: number | null;
	timeoutMs: number;
};
export type NativeFrameExecutor = (run: NativeFrameRun) => Promise<NativeResult>;

/**
 * One `rlm.spawn` subagent run outside the harness: an agent process of its own (Claude Code as a child agent, with
 * its own REPL) instead of a lane of this harness. It answers with its final reply; its `rlm.finish` verdict reaches
 * this host through `handle("rlm.finish", ..., { lane: laneName })`, so the verdict check is the native one.
 */
export type NativeExternalChildRun = {
	taskId: string;
	/** The lane name the child acts as: its relayed host requests (rlm.finish) name it as their caller. */
	laneName: string;
	prompt: string;
	/** Nesting levels above the child (1 for a child of the root) and how many more it may create below itself. */
	level: number;
	allowance: number;
	/** The spawn's `model=`, when given. */
	model?: string;
	/** A worktree child's working directory and extra environment (`worktree=True`); else the host's own. */
	cwd?: string;
	env?: Record<string, string>;
	signal: AbortSignal;
	context: Context;
	deadlineAt: number | null;
	timeoutMs: number;
	/** Live stats for the graph view. */
	progress(stats: { turns?: number; toolCalls?: number; text?: string; model?: string }): void;
};
export type NativeExternalChildResult = {
	text: string;
	usage?: NativeUsageMeasurement;
	turns: number;
	toolCalls: number;
	model?: string;
};
export type NativeExternalChildRunner = (run: NativeExternalChildRun) => Promise<NativeExternalChildResult>;

export type NativeHostService = {
	handle(type: string, payload: Record<string, unknown>, context: Context): Promise<unknown>;
};
export type NativeHostOptions = {
	store: NativeHostStore;
	definitionStore?: NativeDefinitionStore;
	services?: NativeHostService;
	usage?: NativeUsageLedgerLike;
	deterministic?: NativeDefinitionAdapter;
	predict?: NativeDefinitionAdapter;
	/** Runs `rlm-frame@1` tasks (bounded inference frames). */
	frames?: NativeFrameExecutor;
	modules?: readonly NativeHostModule[];
	/** Active instruction refinements targeting a definition id, applied to its model prompt. */
	refinements?: (
		definitionId: string,
		context: Context,
	) => Promise<Array<{ id: string; version: number | null; text: string }>>;
	/** Called before a task runs on a reused lane, so per-invocation scratch can be cleared. */
	beforeLaneReuse?: (lane: string, context: Context) => Promise<void>;
	/** Marks a live task as active work until it reaches a terminal state; returns the release. */
	holdActivity?: () => () => void;
	/**
	 * Charge usage to one root per root turn (see {@link NativeRlmHost.beginRootTurn}). Without it every
	 * reservation uses the ledger's single default root. With it, top-level work admitted while no turn is
	 * running (a schedule firing) gets a root of its own.
	 */
	rootTurns?: boolean;
	/** Keep a lane's kernel alive against eviction; returns false when the pool has no pin capacity left. */
	pinLane?: (lane: string, holder: string) => boolean;
	unpinLane?: (lane: string, holder: string) => void;
	/** Called once a task's terminal result is durable (context control collapses returned results on it). */
	onTaskEnd?: (task: NativeTask, info: { cost: number | null; model?: string }) => void;
	/** Called when `rlm.merge` settles how a worktree child's branch ended (`merged`, `conflict`, `empty`). */
	onMerge?: (taskId: string, status: string) => void;
	/** Called once a detached task's terminal result is durable (completion events). */
	onDetachedEnd?: (end: DetachedTaskEnd) => void;
	/** Extra fields for `agents.status` (for example shell jobs), per calling lane. */
	statusExtras?: (caller: HostCaller) => Record<string, JsonValue>;
	/**
	 * The working directory subagents share. When set, the host snapshots it when an `rlm.spawn` child starts and
	 * ends, and checks the child's declared `changed_files` against what changed (see verdict.ts).
	 */
	workspace?: string;
	/**
	 * Model of `rlm.spawn` subagents on harness lanes when the spawn names none (default: the lane's model). A
	 * function is read at each spawn, so a changed setting (`rlm.childModel`) applies to the next subagent.
	 */
	childModel?: string | (() => string | undefined);
	/** Runs `rlm.spawn` subagents as processes of their own instead of harness lanes (see NativeExternalChildRun). */
	externalChild?: NativeExternalChildRunner;
	/**
	 * The root is itself a subagent (an `ultron mcp --child` server): how many levels sit above it and how many more
	 * it may create. Default: a top-level root, which may always spawn.
	 */
	rootSpawn?: { level: number; allowance: number };
	/** A root that is itself a subagent relays its `rlm.finish` verdict to its parent host. */
	rootFinish?: (payload: Payload, context: Context) => Promise<unknown>;
	/** Names worktree branches (`ultron/<session>/<child>`); default "session". */
	sessionId?: string;
	/** Default worktree setup (the `worktrees` setting), read per spawn; a spawn's `worktree_setup=` overrides it. */
	worktreeSetup?: () => WorktreeSetup | undefined;
	now?: () => number;
};

/** A workflow node's `worktree` option, or a spawn's. */
type WorktreeMode = boolean | "auto";

function worktreeMode(value: unknown, name: string): WorktreeMode {
	if (value === undefined || value === null || value === false) return false;
	if (value === true || value === "auto") return value;
	throw new Error(`${name} must be True, False or "auto" (each worktree child gets its own private worktree)`);
}

/** A spawn's `worktree_setup=`: which gitignored paths to link or copy, and a command to run first. */
function worktreeSetupOption(value: unknown): WorktreeSetup | undefined {
	if (value === undefined || value === null) return undefined;
	const setup = objectInput(value);
	fields(setup, ["link", "copy", "command", "timeout_ms"]);
	const list = (item: unknown, name: string): string[] | undefined => {
		if (item === undefined || item === null) return undefined;
		if (!Array.isArray(item) || item.some((path) => typeof path !== "string" || !path.trim()))
			throw new Error(`worktree_setup.${name} must be a list of relative paths`);
		return item as string[];
	};
	const link = list(setup.link, "link");
	const copy = list(setup.copy, "copy");
	if (setup.command !== undefined && setup.command !== null && typeof setup.command !== "string")
		throw new Error("worktree_setup.command must be a string");
	const timeout = setup.timeout_ms;
	if (timeout !== undefined && (typeof timeout !== "number" || !Number.isSafeInteger(timeout) || timeout < 1))
		throw new Error("worktree_setup.timeout_ms must be a positive integer");
	return {
		...(link === undefined ? {} : { link }),
		...(copy === undefined ? {} : { copy }),
		...(typeof setup.command === "string" && setup.command.trim() ? { command: setup.command } : {}),
		...(typeof timeout === "number" ? { timeoutMs: timeout } : {}),
	};
}

/** An absolute path into the parent's checkout, as the same path in a child's worktree; any other path as it is. */
function inWorktree(path: string, worktree: WorktreeRecord): string {
	if (!isAbsolute(path)) return path;
	const inside = relative(worktree.path, path);
	if (!inside.startsWith("..") && !isAbsolute(inside)) return path;
	const rel = relative(worktree.repo, path);
	return rel.startsWith("..") || isAbsolute(rel) ? path : join(worktree.path, rel);
}

/** A worktree as a finished child's result reports it. */
function worktreeSummary(record: WorktreeRecord): { branch: string; path: string; cwd: string } {
	return { branch: record.branch, path: record.path, cwd: record.cwd };
}

export class NativeRlmHost {
	private readonly tasks = new Map<string, TaskRecord>();
	private loading?: Promise<void>;
	private admissions: Promise<void> = Promise.resolve();
	private closing?: Promise<void>;
	private closed = false;
	/** Workflow plans for the graph view, newest last (bounded). */
	private readonly workflowRuns = new Map<string, WorkflowRunRecord>();
	/** Removes the harness event listeners that keep the graph view's lane stats live. */
	private stopObserving?: () => void;
	private readonly harness: AgentHarness;
	private readonly services: NativeHostService | undefined;
	private readonly usage: NativeUsageLedgerLike | undefined;
	private readonly journal: NativeTaskJournal;
	private readonly registry: NativeDefinitionRegistry;
	private readonly modules: readonly NativeHostModule[];
	/** Lane name -> owning task, so host requests from a child kernel carry the child's identity. */
	private readonly laneTasks = new Map<string, string>();
	private readonly now: () => number;
	private readonly beforeLaneReuse: NativeHostOptions["beforeLaneReuse"];
	private readonly holdActivity: NativeHostOptions["holdActivity"];
	private readonly refinements: NativeHostOptions["refinements"];
	private readonly frames: NativeHostOptions["frames"];
	private modulesStarted?: Promise<void>;
	private readonly rootTurns: boolean;
	/** Usage root of the current or most recent root turn. */
	private rootTurn: string | undefined;
	private rootTurnActive = false;
	private readonly pinLane: NativeHostOptions["pinLane"];
	private readonly unpinLane: NativeHostOptions["unpinLane"];
	private readonly onTaskEnd: NativeHostOptions["onTaskEnd"];
	private readonly onDetachedEnd: NativeHostOptions["onDetachedEnd"];
	private readonly onMerge: NativeHostOptions["onMerge"];
	private readonly statusExtras: NativeHostOptions["statusExtras"];
	private readonly workspace: string | undefined;
	private readonly externalChild: NativeHostOptions["externalChild"];
	private readonly childModel: NativeHostOptions["childModel"];
	private readonly rootSpawn: NativeHostOptions["rootSpawn"];
	private readonly rootFinish: NativeHostOptions["rootFinish"];
	/** Root-lane runs that continue an earlier root (a completion event re-invoking the model): run id -> root. */
	private readonly rootAliases = new Map<string, string>();
	private readonly sessionId: string;
	private readonly worktreeSetup: NativeHostOptions["worktreeSetup"];
	/** `rlm.merge` calls run one at a time: each writes into a working tree. */
	private merging: Promise<void> = Promise.resolve();
	private pruned = false;

	constructor(harness: AgentHarness, _rootLane: AgentLane, options: NativeHostOptions) {
		if (!options?.store) throw new Error("NativeRlmHost requires options.store");
		this.harness = harness;
		this.services = options.services;
		this.usage = options.usage;
		this.journal = new NativeTaskJournal(options.store);
		this.modules = options.modules ?? [];
		this.beforeLaneReuse = options.beforeLaneReuse;
		this.holdActivity = options.holdActivity;
		this.refinements = options.refinements;
		this.frames = options.frames;
		this.now = options.now ?? Date.now;
		this.rootTurns = options.rootTurns ?? false;
		this.pinLane = options.pinLane;
		this.unpinLane = options.unpinLane;
		this.onTaskEnd = options.onTaskEnd;
		this.onDetachedEnd = options.onDetachedEnd;
		this.onMerge = options.onMerge;
		this.statusExtras = options.statusExtras;
		this.workspace = options.workspace;
		this.externalChild = options.externalChild;
		this.childModel = options.childModel;
		this.rootSpawn = options.rootSpawn;
		this.rootFinish = options.rootFinish;
		this.sessionId = options.sessionId ?? "session";
		this.worktreeSetup = options.worktreeSetup;
		this.registry = new NativeDefinitionRegistry(options.definitionStore, {
			deterministic: options.deterministic,
			predict: options.predict,
		});
	}

	/**
	 * A root-lane run started: its work gets a fresh usage root (wall budget, admission and cost windows).
	 * Work admitted earlier keeps the root and deadline it was admitted under.
	 */
	beginRootTurn(runId: string): void {
		if (!runId) throw new Error("Root turn id must be nonempty");
		this.rootTurn = this.rootIdOfRun(runId);
		this.rootTurnActive = true;
	}

	/** The root-lane run ended; work it started keeps running under its own root. */
	endRootTurn(runId: string): void {
		if (this.rootTurn === this.rootIdOfRun(runId)) this.rootTurnActive = false;
	}

	/**
	 * Charge root-lane run `runId` to an earlier root instead of a fresh one: a run started by a completion event
	 * continues the request that started the work, so its turn, token, wall and cost limits keep counting.
	 */
	continueRootTurn(runId: string, rootId: string): void {
		if (!runId || !rootId) throw new Error("Root turn continuation needs a run id and a root id");
		this.rootAliases.set(runId, rootId);
		// Kept after the run ends (abort handling reads it); bounded to the most recent continuations.
		while (this.rootAliases.size > 256) this.rootAliases.delete(this.rootAliases.keys().next().value!);
	}

	/** Detached root-owned tasks still running whose end will be announced (roots in `excluded` do not count). */
	pendingRootNotifications(excluded: (rootId: string | undefined) => boolean = () => false): number {
		return [...this.tasks.values()].filter(
			(task) =>
				task.notify !== undefined &&
				task.parentId === undefined &&
				task.result === undefined &&
				!excluded(task.usageReservation?.rootId),
		).length;
	}

	/**
	 * Start a session goal's job (goal.ts): a background agent with its own REPL and no wall-clock limit, admitted
	 * under the goal's own usage root, whose end is announced to the root as a `task_done` event. Returns its task id.
	 */
	async startGoalJob(prompt: string, options: { key: string; usageRoot: string }, context: Context): Promise<string> {
		// The journal loads before the first task, so loading it later cannot replace the live record.
		await this.loadTasks();
		const task = await this.spawnTask(
			{
				definition: "background-job@1",
				input: { prompt },
				key: `background:${options.key}`,
				timeoutMs: 30 * 60 * 1000,
				untimed: true,
				usageRoot: options.usageRoot,
			},
			context,
			undefined,
			true,
			"task_done",
		);
		return task.id;
	}

	/** Stop a running task (and its subtree); no-op when it already ended. */
	async stopTask(id: string, reason: string): Promise<void> {
		const task = this.tasks.get(id);
		if (task !== undefined && task.result === undefined) await this.cancel(task, reason);
	}

	/** The lane a task runs on, once it started. */
	laneOfTask(id: string): string | undefined {
		return this.tasks.get(id)?.laneName;
	}

	/** Usage root of a root-lane run. */
	rootIdOfRun(runId: string): string {
		return this.rootAliases.get(runId) ?? `turn:${runId}`;
	}

	/** Usage root that new work of `parentId` is charged to; undefined means the ledger's default root. */
	private admissionRoot(parentId: string | null | undefined): string | undefined {
		const inherited = parentId == null ? undefined : this.tasks.get(parentId)?.usageReservation?.rootId;
		if (inherited !== undefined) return inherited;
		if (!this.rootTurns) return undefined;
		if (this.rootTurnActive && this.rootTurn !== undefined) return this.rootTurn;
		// Top-level work outside any root turn is its own job with its own budget.
		return `job:${randomUUID()}`;
	}

	/**
	 * Usage root that a model turn on `lane` during run `runId` is charged to for the turn and token limits: a
	 * main-lane run is its own root turn, a task lane belongs to its task's root. Undefined means the ledger's
	 * default root.
	 */
	usageRootForLane(lane: string, runId: string): string | undefined {
		if (lane === "main") return this.rootTurns ? this.rootIdOfRun(runId) : undefined;
		const taskId = this.laneTasks.get(lane);
		return taskId === undefined ? undefined : this.tasks.get(taskId)?.usageReservation?.rootId;
	}

	/** Usage root shown to a caller: its task's root, else the current or last root turn. */
	private statusRoot(parentId: string | null | undefined): string | undefined {
		const inherited = parentId == null ? undefined : this.tasks.get(parentId)?.usageReservation?.rootId;
		return inherited ?? (this.rootTurns ? this.rootTurn : undefined);
	}

	private definition(key: string): NativeDefinition {
		return this.registry.get(key);
	}

	/** The nearest worktree of a task and its ancestors: a child without its own works in its parent's. */
	private worktreeOf(taskId: string | undefined): WorktreeRecord | undefined {
		for (let id = taskId; id !== undefined; id = this.tasks.get(id)?.parentId) {
			const worktree = this.tasks.get(id)?.worktree;
			if (worktree !== undefined) return worktree;
		}
		return undefined;
	}

	/** The directory a task works in: its (or an ancestor's) worktree, else the shared workspace. */
	private workspaceOf(taskId: string | undefined): string | undefined {
		return this.worktreeOf(taskId)?.cwd ?? this.workspace;
	}

	/** Whether any subagent of this host has a worktree (so a lane may work elsewhere than the shared workspace). */
	hasWorktrees(): boolean {
		for (const task of this.tasks.values()) if (task.worktree !== undefined) return true;
		return false;
	}

	/**
	 * Where a lane works when it is not the shared workspace: a worktree child's lane (or a lane of its descendants)
	 * gets the worktree's directory and environment for its kernel, shell commands, file tools and file hooks.
	 */
	laneWorkspace(
		lane: string,
	): { cwd: string; root: string; repo: string; branch: string; env: Record<string, string> } | undefined {
		const worktree = this.worktreeOf(this.laneTasks.get(lane));
		if (worktree === undefined) return undefined;
		return {
			cwd: worktree.cwd,
			root: worktree.path,
			repo: worktree.repo,
			branch: worktree.branch,
			env: worktreeEnv(worktree),
		};
	}

	private async loadTasks(): Promise<void> {
		this.loading ??= (async () => {
			await this.registry.ready();
			for (const stored of await this.journal.list()) this.tasks.set(stored.id, stored);
			await this.usage?.ready?.();
			// No task of this owner is live yet, so every open reservation belongs to an ended owner.
			// Settle them as unknown so the ledger agrees with the interrupted journal (A21/A24).
			await this.usage?.reconcile?.([]);
			// Worktrees of sessions that ended without cleaning up (a crash): their work is kept on its branch.
			if (!this.pruned && this.workspace !== undefined && !keepWorktrees()) {
				this.pruned = true;
				void pruneWorktrees(this.workspace).catch(() => {});
			}
		})();
		await this.loading;
		await this.journal.ready();
		this.modulesStarted ??= (async () => {
			for (const module of this.modules) await module.start?.(this.api);
		})();
		await this.modulesStarted;
	}

	/** Operations exposed to host modules. */
	readonly api: NativeHostApi = {
		callerTaskId: (caller) => this.laneTasks.get(caller.lane) ?? null,
		taskLane: (taskId) => this.tasks.get(taskId)?.laneName ?? null,
		strategy: (definition) => this.definition(definitionKey(definition)).strategy,
		tasks: () => this.journal.list(),
		spawn: async (request, parentTaskId, context) => {
			const definition = definitionKey(request.definition);
			const item = this.definition(definition);
			if (!isJsonValue(request.input) || !this.registry.isValidInput(item, request.input))
				throw this.registry.validationError(item, request.input, "input");
			const task = await this.spawnTask(
				{
					definition,
					input: request.input,
					model: request.model,
					key: request.key,
					timeoutMs: request.timeoutMs ?? 30 * 60 * 1000,
					...(request.lane === undefined ? {} : { lane: request.lane }),
				},
				context,
				parentTaskId ?? undefined,
				// Module work (instance invocations, skill runs, schedule firings, verifiers) is collected later or
				// awaited explicitly; it must not die with the cell that requested it. Parent cancellation still cascades.
				true,
			);
			return publicRecord(task);
		},
		result: async (taskId) => {
			const task = this.tasks.get(taskId);
			if (!task) throw new Error("Unknown Ultron task");
			return structuredClone(await this.awaitTask(task));
		},
		cancel: async (taskId, reason) => {
			const task = this.tasks.get(taskId);
			if (!task) throw new Error("Unknown Ultron task");
			return this.cancel(task, reason);
		},
		steer: async (taskId, message, context) => {
			const task = this.tasks.get(taskId);
			if (!task?.lane || task.result) return false;
			return (await task.lane.steer(message, undefined, context)).ok;
		},
		usage: async () =>
			this.usage ? ((await this.usage.status(this.statusRoot(null))) as unknown as JsonValue) : null,
		pinLane: (lane, holder) => this.pinLane?.(lane, holder) ?? false,
		rootOf: (caller) => {
			if (!this.rootTurns) return undefined;
			if (caller.lane === "main") return this.rootTurnActive ? this.rootTurn : undefined;
			const taskId = this.laneTasks.get(caller.lane);
			return taskId === undefined ? undefined : this.tasks.get(taskId)?.usageReservation?.rootId;
		},
		unpinLane: (lane, holder) => this.unpinLane?.(lane, holder),
		now: () => this.now(),
	};

	/**
	 * Keep each task lane's read-only stats live for the graph view: the model that answered, assistant turns, tool
	 * calls, and the latest assistant text (bounded). One listener pair for all lanes; a lane maps to its newest task.
	 */
	private observeLanes(): void {
		if (this.stopObserving !== undefined) return;
		const events = (this.harness as Partial<AgentHarness>).events;
		if (typeof events?.on !== "function") {
			this.stopObserving = () => {};
			return;
		}
		const taskOf = (lane: string | undefined): TaskRecord | undefined => {
			const id = lane === undefined ? undefined : this.laneTasks.get(lane);
			const task = id === undefined ? undefined : this.tasks.get(id);
			return task?.result === undefined ? task : undefined;
		};
		const removers = [
			events.on("message_end", (event) => {
				const task = taskOf(event.lane);
				const message = event.message as { role?: string; provider?: unknown; model?: unknown };
				if (task === undefined || message.role !== "assistant") return;
				task.turns = (task.turns ?? 0) + 1;
				const text = assistantText(message);
				if (text.length > 0) task.lastText = preview(text, GRAPH_TEXT_CHARS);
				if (typeof message.provider === "string" && typeof message.model === "string")
					task.model = preview(`${message.provider}/${message.model}`, GRAPH_MODEL_CHARS);
			}),
			events.on("tool_start", (event) => {
				const task = taskOf(event.lane);
				if (task !== undefined) task.toolCallCount = (task.toolCallCount ?? 0) + 1;
			}),
		];
		this.stopObserving = () => {
			for (const remove of removers) remove();
		};
	}

	list(): ReturnType<NativeDefinitionRegistry["list"]> {
		return this.registry.list();
	}

	private request(payload: Payload): TaskRequest {
		fields(payload, ["definition", "input", "model", "key", "timeout_ms"]);
		const definition = definitionKey(payload.definition);
		if (!isJsonValue(payload.input)) throw new Error("Agent input is not JSON");
		const item = this.definition(definition);
		if (!this.registry.isValidInput(item, payload.input))
			throw this.registry.validationError(item, payload.input, "input");
		return { definition, input: payload.input, ...taskOptions(payload) };
	}

	private async execute(task: TaskRecord, request: TaskRequest, context: Context): Promise<NativeResult> {
		const definition = this.definition(task.definition);
		const signal = task.controller!.signal;
		const taskContext = withAbortSignal(signal, context);
		let modelReservation: NativeUsageReservation | undefined;
		let modelStatus: NativeUsageCallStatus = "unknown";
		let modelUsage: NativeUsageMeasurement | undefined;
		try {
			signal.throwIfAborted();
			if (definition.strategy === "deterministic") {
				const value = await this.registry.deterministicValue(definition, request.input, taskContext, signal);
				if (!isJsonValue(value)) throw new Error("Deterministic adapter returned a non-JSON result");
				if (!this.registry.isValidOutput(definition, value))
					throw this.registry.validationError(definition, value, "output");
				return { status: "succeeded", value, verification: "unverified" };
			}
			if (definition.strategy === "predict") {
				let value: unknown;
				let repair: Parameters<NativeDefinitionAdapter>[0]["repair"];
				for (let attempt = 0; attempt <= definition.maxRepairs; attempt += 1) {
					signal.throwIfAborted();
					// Every model attempt, including each repair, is charged to this task.
					const reservation = await this.usage?.reserve({
						kind: "model",
						...(task.usageReservation === undefined ? {} : { rootId: task.usageReservation.rootId }),
						parentTaskId: task.id,
						taskId: task.id,
						requestKey: attempt === 0 ? `${task.id}:model` : `${task.id}:model:repair-${attempt}`,
						timeoutMs: request.timeoutMs,
						signal,
					});
					let attemptStatus: NativeUsageCallStatus = "failed";
					try {
						value = await this.registry.predictValue(definition, request.input, taskContext, signal, repair);
						attemptStatus = "succeeded";
					} catch (error) {
						attemptStatus = signal.aborted ? "cancelled" : "failed";
						throw error;
					} finally {
						if (reservation) await this.usage?.settle(reservation, { status: attemptStatus });
						// A predict attempt is one model turn of its root (tokens are not reported by the adapter).
						await this.usage?.recordTurn?.(task.usageReservation?.rootId);
					}
					if (isJsonValue(value) && this.registry.isValidOutput(definition, value))
						return { status: "succeeded", value, verification: "unverified" };
					repair = {
						attempt: attempt + 1,
						previous: value,
						error: this.registry.validationError(definition, value, "output").message,
					};
				}
				const error = repair?.error ?? "Predict adapter returned an invalid output";
				throw new Error(
					definition.maxRepairs === 0
						? error
						: `${error} after ${definition.maxRepairs} repair attempt${definition.maxRepairs === 1 ? "" : "s"}`,
				);
			}
			const laneName = request.lane ?? `ultron.${definition.id}.${task.id}`;
			if (definition.id === "rlm-child" && this.externalChild !== undefined)
				return await this.executeExternalChild(task, request, laneName, taskContext, signal);
			if (request.lane !== undefined) await this.beforeLaneReuse?.(laneName, taskContext);
			// A forked child's lane starts on the parent's branch (an existing lane keeps its own tip).
			const lane =
				request.forkAt === undefined
					? await this.harness.lane(laneName, taskContext)
					: await this.harness.lane(laneName, { createAt: request.forkAt }, taskContext);
			task.lane = lane;
			task.laneName = laneName;
			// A reused lane now acts for its newest invocation.
			this.laneTasks.set(laneName, task.id);
			this.observeLanes();
			if (signal.aborted) this.abortLane(task);
			signal.throwIfAborted();
			await lane.getActiveTools(taskContext);
			signal.throwIfAborted();
			// Each lane resolves to its own Python kernel. Keep ipython enabled so
			// recursive RLM work can continue in the child lane without sharing state.
			const model = request.model ?? definition.model;
			if (model) {
				const split = model.indexOf("/");
				await lane.setModel({ provider: model.slice(0, split), modelId: model.slice(split + 1) }, taskContext);
				signal.throwIfAborted();
				task.model = preview(model, GRAPH_MODEL_CHARS);
			} else if (typeof lane.getModel === "function") {
				// The lane's default model, for the graph view; an unreadable model is simply not shown.
				const current = await lane.getModel(taskContext).catch(() => undefined);
				if (current) task.model = preview(`${current.provider}/${current.id}`, GRAPH_MODEL_CHARS);
			}
			if (request.thinking !== undefined && typeof lane.setThinkingLevel === "function") {
				await lane.setThinkingLevel(request.thinking, taskContext);
				signal.throwIfAborted();
			}
			if (definition.id === "rlm-frame" && this.frames)
				return await this.frames({
					taskId: task.id,
					input: request.input,
					lane,
					laneName,
					signal,
					context: taskContext,
					...(task.usageReservation === undefined ? {} : { usageRootId: task.usageReservation.rootId }),
					deadlineAt: task.usageReservation?.deadlineAt ?? null,
					timeoutMs: request.timeoutMs,
				});
			const basePrompt =
				definition.id === "rlm-child"
					? this.childBrief(task, request)
					: `${definition.instructions}\n\nInput data:\n${JSON.stringify(request.input)}\n\nOutput contract:\n${definition.outputDescription}`;
			// Active refinements for this definition apply to every later run; a rollback removes them.
			// Refinements are optional; an unavailable refinement service must not fail the task (A34).
			const refinements = (await this.refinements?.(definition.id, taskContext).catch(() => [])) ?? [];
			signal.throwIfAborted();
			const prompt =
				refinements.length === 0
					? basePrompt
					: `${basePrompt}\n\n${refinements
							.map(
								(refinement) =>
									`Active refinement ${refinement.id} (version ${refinement.version ?? "unversioned"}):\n${refinement.text}`,
							)
							.join("\n\n")}`;
			// A subagent's files are pictured before its first model turn, to check its verdict against at the end.
			const before = definition.id === "rlm-child" ? await this.childStart(task, signal) : undefined;
			signal.throwIfAborted();
			modelReservation = await this.usage?.reserve({
				kind: "model",
				...(task.usageReservation === undefined ? {} : { rootId: task.usageReservation.rootId }),
				parentTaskId: task.id,
				taskId: task.id,
				requestKey: `${task.id}:model`,
				// The model call lives within its task's admitted deadline; a fresh full timeout would overrun it.
				...(task.usageReservation?.deadlineAt == null
					? request.untimed === true
						? {}
						: { timeoutMs: request.timeoutMs }
					: { deadlineAt: task.usageReservation.deadlineAt }),
				signal,
			});
			const response = await lane.prompt(prompt, undefined, taskContext);
			signal.throwIfAborted();
			if (!response.ok) {
				modelStatus = "failed";
				throw new Error(JSON.stringify(response.error));
			}
			modelStatus = "succeeded";
			if (response.value.status !== "completed")
				throw new Error(`Agent run did not complete: ${response.value.status}`);
			const tipId = response.value.tipId;
			if (!tipId) throw new Error("Agent produced no assistant result");
			const fromTipId = response.value.fromTipId ?? null;
			const entries = await lane.findEntries(
				{ start: tipId, order: "newestFirst", ...(fromTipId === null ? {} : { stopAtId: fromTipId }) },
				taskContext,
			);
			signal.throwIfAborted();
			const runEntries = entries.filter((candidate) => candidate.id !== fromTipId);
			modelUsage = runUsage(runEntries);
			// The run's own entries are authoritative for the graph view's counts (events can be missed).
			const stats = runStats(runEntries);
			task.turns = Math.max(task.turns ?? 0, stats.turns);
			task.toolCallCount = Math.max(task.toolCallCount ?? 0, stats.toolCalls);
			if (stats.text !== undefined) task.lastText = preview(stats.text, GRAPH_TEXT_CHARS);
			const entry = entries.find((candidate) => candidate.id === tipId);
			const text = entry ? textOf(entry) : "";
			if (definition.id === "rlm-child") return await this.childResult(task, text, before, signal);
			if (!text.trim()) throw new Error("Agent produced no assistant result at the completed tip");
			const value = definition.id === "rlm-child" || definition.id === "background-job" ? text : jsonFrom(text);
			if (!isJsonValue(value)) throw new Error("Agent produced a non-JSON result");
			if (!this.registry.isValidOutput(definition, value))
				throw this.registry.validationError(definition, value, "output");
			return { status: "succeeded", value, verification: "unverified" };
		} catch (error) {
			modelStatus = signal.aborted ? "cancelled" : modelStatus === "unknown" ? "failed" : modelStatus;
			return {
				status: signal.aborted ? "cancelled" : "failed",
				error: String(error instanceof Error ? error.message : error),
				verification: "unverified",
			};
		} finally {
			if (typeof modelUsage?.cost === "number") task.cost = modelUsage.cost;
			if (typeof modelUsage?.totalTokens === "number") task.tokens = modelUsage.totalTokens;
			if (modelReservation)
				await this.usage?.settle(modelReservation, {
					status: modelStatus,
					...(modelUsage === undefined ? {} : { usage: modelUsage }),
				});
		}
	}

	/** The first terminal request wins, but nothing is published until its write commits. */
	private finish(task: TaskRecord, result: NativeResult): Promise<NativeResult> {
		if (task.finishing) return task.finishing;
		task.finishing = (async () => {
			try {
				// A worktree child's work is committed on its branch before its result is published.
				result = await this.finalizeWorktree(task, result);
				const state = result.status === "succeeded" ? "completed" : result.status;
				const committed = await this.journal.transition(task.id, state, result);
				Object.assign(task, committed);
				task.endedAt ??= this.now();
				if (!committed.result) throw new Error("Terminal task has no durable result");
				if (task.usageReservation && !task.usageSettled) {
					await this.usage?.settle(task.usageReservation, {
						status:
							result.status === "succeeded"
								? "succeeded"
								: result.status === "cancelled"
									? "cancelled"
									: result.status === "interrupted"
										? "unknown"
										: "failed",
					});
					task.usageSettled = true;
				}
				const awaited = (task.waiters ?? 0) > 0;
				task.resolve?.(committed.result);
				try {
					this.onTaskEnd?.(publicRecord(task), {
						cost: task.cost ?? null,
						...(task.model === undefined ? {} : { model: task.model }),
					});
				} catch {
					// Observers never affect a task's durable result.
				}
				if (task.notify && !this.closed) {
					try {
						this.onDetachedEnd?.({
							task: publicRecord(task),
							kind: task.notify,
							ownerLane:
								task.parentId === undefined ? "main" : (this.tasks.get(task.parentId)?.laneName ?? "main"),
							rootId: task.usageReservation?.rootId,
							awaited,
							fetch: fetchHint(task),
							cost: task.cost ?? null,
						});
					} catch {
						// Observers never affect a task's durable result.
					}
				}
				return committed.result;
			} catch (error) {
				task.reject?.(error);
				task.controller?.abort(error);
				this.abortLane(task);
				throw error;
			} finally {
				task.cleanup?.();
			}
		})();
		return task.finishing;
	}

	/**
	 * Commit what a worktree child changed on its branch (message: its verdict's summary) and add the worktree to its
	 * result. A child that changed nothing has its worktree and branch removed at once. Never fails the task: a Git
	 * problem is reported in `worktree.error` and the worktree is kept for inspection.
	 */
	private async finalizeWorktree(task: TaskRecord, result: NativeResult): Promise<NativeResult> {
		const record = task.worktree;
		if (record === undefined || record.state !== "active") return result;
		const verdict = task.verdict;
		const status = verdict?.status ?? result.status;
		let info: WorktreeInfo;
		try {
			const summary =
				verdict?.summary ??
				(typeof result.value === "string" ? result.value : result.error !== undefined ? result.error : "");
			const commit = await commitChildWorktree(record, {
				message: commitMessage(summary, record.name, task.id, status),
				...(await this.worktreeChanges(task, record)),
			});
			record.state = commit.commit !== null && status !== "passed" && status !== "succeeded" ? "failed" : "ended";
			record.commit = commit.commit;
			const empty = commit.commit === null;
			if (empty && !keepWorktrees()) await removeChildWorktree(record, { deleteBranch: true });
			else await saveRecord(record);
			info = {
				task: task.id,
				branch: record.branch,
				path: record.path,
				repo: record.repo,
				base: record.base,
				commit: commit.commit,
				changed_files: commit.changedFiles,
				diffstat: commit.diffstat,
				...(empty && !keepWorktrees() ? { removed: true } : {}),
			};
			if (empty) this.merged(task, "empty");
		} catch (error) {
			record.state = "failed";
			await saveRecord(record).catch(() => {});
			info = {
				task: task.id,
				branch: record.branch,
				path: record.path,
				repo: record.repo,
				base: record.base,
				commit: null,
				changed_files: [],
				diffstat: "",
				error: `committing the worktree failed: ${error instanceof Error ? error.message : String(error)}`,
			};
		}
		return { ...result, worktree: structuredClone(info) as unknown as JsonValue };
	}

	/**
	 * What a worktree child changed: Git's changes in its worktree, minus files unchanged since it started (its setup
	 * command's output). Undefined lets the commit take every change (no picture at the start).
	 */
	private async worktreeChanges(task: TaskRecord, record: WorktreeRecord): Promise<{ paths?: string[] }> {
		const baseline = task.worktreeBaseline;
		if (baseline === undefined || !baseline.complete) return {};
		const now = await snapshotWorkspace(baseline.root).catch(() => undefined);
		if (now === undefined || !now.complete) return {};
		const status = await listDirty(record.path);
		if (status === undefined) return {};
		const prefix = relative(record.path, baseline.root).split(sep).join("/");
		return {
			paths: status.filter((path) => {
				const inside =
					prefix === "" ? path : path.startsWith(`${prefix}/`) ? path.slice(prefix.length + 1) : undefined;
				// Outside the pictured directory nothing is known: keep it.
				if (inside === undefined) return true;
				return baseline.entries.get(inside) !== now.entries.get(inside);
			}),
		};
	}

	/** How a worktree child's merge ended: kept for the graph view and told to the session's counters. */
	private merged(task: TaskRecord, status: string): void {
		task.mergeStatus = status;
		try {
			this.onMerge?.(task.id, status);
		} catch {
			// Observers never affect a merge.
		}
	}

	private abortLane(task: TaskRecord): void {
		// Lane cleanup is best effort. An uncooperative lane must not hold a durable result hostage.
		void Promise.resolve()
			.then(() => task.lane?.abort(BACKGROUND_CONTEXT))
			.catch(() => {});
	}

	/** Cancel a task and every unfinished descendant; siblings and other subtrees keep running. */
	private async cancel(task: TaskRecord, reason: string): Promise<NativeResult> {
		// Only the first party to end a task aborts its work; a parent abort and a cascade can race here.
		const alreadyEnding = task.result !== undefined || task.finishing !== undefined;
		const own = task.result
			? Promise.resolve(task.result)
			: this.finish(task, { status: "cancelled", error: reason, verification: "unverified" });
		if (!alreadyEnding) {
			task.controller?.abort(new Error(reason));
			this.abortLane(task);
		}
		const children = [...this.tasks.values()].filter((child) => child.parentId === task.id && !child.result);
		await Promise.allSettled(children.map((child) => this.cancel(child, `Ancestor ${task.id} cancelled: ${reason}`)));
		return own;
	}

	private async run(task: TaskRecord, request: TaskRequest, context: Context): Promise<void> {
		try {
			const committed = await this.journal.transition(task.id, "running");
			Object.assign(task, committed);
			if (task.finishing) return;
			const result = await this.execute(task, request, context);
			await this.finish(task, await this.withLimitReason(task, result));
		} catch (error) {
			task.reject?.(error);
			task.controller?.abort(error);
			if (task.usageReservation && !task.usageSettled) {
				await this.usage?.settle(task.usageReservation, { status: "unknown" }).catch(() => {});
				task.usageSettled = true;
			}
			task.cleanup?.();
			this.abortLane(task);
		}
	}

	/**
	 * A task that failed once its root's tree ran out of turns, tokens or cost (its next model request was refused)
	 * fails with the limit message rather than whatever the stopped run left behind.
	 */
	private async withLimitReason(task: TaskRecord, result: NativeResult): Promise<NativeResult> {
		if (result.status !== "failed") return result;
		const reason = await this.usage?.turnBudgetExhausted?.(task.usageReservation?.rootId).catch(() => undefined);
		return reason === undefined || result.error === reason ? result : { ...result, error: reason };
	}

	/** A subagent's brief; a worktree child is told where it works. */
	private childBrief(task: TaskRecord, request: TaskRequest): string {
		const prompt = String(objectInput(request.input).prompt);
		const brief = request.forkAt === undefined ? prompt : `${FORK_BRIEF_NOTE}\n\n${prompt}`;
		return task.worktree === undefined ? brief : `${brief}\n\n${worktreeBriefNote(task.worktree)}`;
	}

	/**
	 * Before a subagent's first turn: its worktree's setup command runs, and its files are pictured (its worktree's,
	 * or the shared workspace), plus the parent's tree for a worktree child, to see writes it makes outside it.
	 */
	private async childStart(task: TaskRecord, signal: AbortSignal): Promise<WorkspaceSnapshot | Error | undefined> {
		if (task.worktree !== undefined) await runSetupCommand(task.worktree, signal);
		const before = await this.snapshot(signal, this.workspaceOf(task.id));
		if (task.worktree !== undefined && before !== undefined && !(before instanceof Error)) {
			task.worktreeBaseline = before;
			const outside = await this.snapshot(signal, this.workspaceOf(task.parentId));
			if (outside !== undefined && !(outside instanceof Error)) task.outsideBefore = outside;
		}
		return before;
	}

	/** A picture of a workspace (the shared one by default), an Error when it could not be taken, or undefined. */
	private async snapshot(
		signal: AbortSignal,
		workspace: string | undefined = this.workspace,
	): Promise<WorkspaceSnapshot | Error | undefined> {
		if (workspace === undefined) return undefined;
		try {
			return await snapshotWorkspace(workspace, { signal });
		} catch (error) {
			return error instanceof Error ? error : new Error(String(error));
		}
	}

	/**
	 * Other lane-backed tasks whose runs overlapped `task`'s, outside its own line (its ancestors and descendants):
	 * work that may have written files while it ran.
	 */
	private concurrentWith(task: TaskRecord): TaskRecord[] {
		const start = task.startedAt ?? 0;
		const end = this.now();
		const ancestors = new Set<string>();
		for (let id = task.parentId; id !== undefined; id = this.tasks.get(id)?.parentId) ancestors.add(id);
		const descends = (other: TaskRecord): boolean => {
			for (let id = other.parentId; id !== undefined; id = this.tasks.get(id)?.parentId)
				if (id === task.id) return true;
			return false;
		};
		// Work in another worktree writes elsewhere.
		const workspace = this.workspaceOf(task.id);
		return [...this.tasks.values()].filter(
			(other) =>
				other !== task &&
				other.laneName !== undefined &&
				this.workspaceOf(other.id) === workspace &&
				!other.definition.startsWith("rlm-frame@") &&
				other.startedAt !== undefined &&
				other.startedAt <= end &&
				(other.endedAt ?? Number.POSITIVE_INFINITY) >= start &&
				!ancestors.has(other.id) &&
				!descends(other),
		);
	}

	/**
	 * `rlm.merge`: each child's branch, in order, into the working tree at `target` as uncommitted changes. The first
	 * conflict stops the rest (`on_conflict="skip"` goes on with the next child); nothing of a conflicting child is
	 * written unless `on_conflict="markers"`. A merged or empty child's worktree and branch are removed; a conflicting
	 * or failed one is kept for inspection.
	 */
	private async mergeChildren(
		children: readonly TaskRecord[],
		target: string,
		onConflict: "stop" | "skip" | "markers",
		includeFailed: boolean,
	): Promise<Record<string, unknown>> {
		const results: Record<string, unknown>[] = [];
		let stoppedAt: string | null = null;
		for (const task of children) {
			const result = task.result;
			const info = (result?.worktree ?? null) as WorktreeInfo | null;
			const name = task.worktree?.name ?? info?.branch.split("/").pop() ?? task.id;
			const base = { id: task.id, name, ...(info === null ? {} : { branch: info.branch }) };
			if (stoppedAt !== null) {
				results.push({ ...base, status: "not_attempted", reason: `stopped at the conflict of ${stoppedAt}` });
				continue;
			}
			if (info === null) {
				results.push({
					...base,
					status: "no_worktree",
					reason: "it ran in the shared tree: its changes are already there",
				});
				continue;
			}
			if (task.mergeStatus === "merged") {
				results.push({ ...base, status: "merged", files: info.changed_files, note: "already merged" });
				continue;
			}
			const verdict = result?.verdict as Verdict | null | undefined;
			const passed =
				result?.status === "succeeded" &&
				(verdict === null || verdict === undefined || verdict.status === "passed");
			if (!passed && !includeFailed) {
				results.push({
					...base,
					status: "skipped",
					reason: `the child did not succeed (${result?.status ?? "unknown"}${verdict ? `, verdict ${verdict.status}` : ""}); include_failed=True merges it anyway`,
					...(info.commit === null ? {} : { commit: info.commit }),
				});
				continue;
			}
			if (info.error !== undefined) {
				results.push({ ...base, status: "failed", reason: info.error });
				stoppedAt = onConflict === "skip" ? null : task.id;
				continue;
			}
			let outcome: Awaited<ReturnType<typeof mergeChildWorktree>>;
			try {
				outcome = await mergeChildWorktree(
					target,
					{ name, branch: info.branch, base: info.base, commit: info.commit },
					{ onConflict: onConflict === "markers" ? "markers" : "stop" },
				);
			} catch (error) {
				results.push({ ...base, status: "failed", reason: error instanceof Error ? error.message : String(error) });
				if (onConflict !== "skip") stoppedAt = task.id;
				continue;
			}
			this.merged(task, outcome.status);
			const record = task.worktree ?? (await listRecords(target)).find((item) => item.taskId === task.id);
			if (outcome.status === "conflict") {
				if (record !== undefined) {
					record.state = "conflict";
					await saveRecord(record).catch(() => {});
				}
				results.push({
					...base,
					status: "conflict",
					files: outcome.files,
					conflicts: outcome.conflicts.map((file: ConflictFile) => ({ ...file })),
					markers: outcome.markers,
					message: outcome.markers
						? `conflict markers written in ${outcome.files.join(", ")}: resolve them, or restore those files`
						: `nothing of ${name} was written: ${outcome.message || "the merge conflicts"}. Its branch ${info.branch} keeps the work (worktree ${info.path})`,
					commit: info.commit,
				});
				if (onConflict !== "skip") stoppedAt = task.id;
				continue;
			}
			// Merged or empty: its worktree and branch have done their job.
			if (!keepWorktrees()) {
				if (record !== undefined) await removeChildWorktree(record, { deleteBranch: true }).catch(() => {});
			} else if (record !== undefined) {
				record.state = "merged";
				await saveRecord(record).catch(() => {});
			}
			results.push(
				outcome.status === "merged"
					? { ...base, status: "merged", files: outcome.files, diffstat: outcome.diffstat }
					: { ...base, status: "empty" },
			);
		}
		const ok = results.every((item) => ["merged", "empty", "no_worktree"].includes(String(item.status)));
		return {
			ok,
			merged: results.filter((item) => item.status === "merged").map((item) => item.id),
			...(stoppedAt === null ? {} : { stopped_at: stoppedAt }),
			results,
			note: "Merged changes are uncommitted modifications in your working tree (new files untracked); review and commit them yourself.",
		};
	}

	/**
	 * A new private worktree for a child of `parentId` (`worktree=True`), branched from the spawner's tree: its own
	 * worktree when it has one, else the shared workspace. `"auto"` falls back to the shared tree without Git.
	 */
	private async newWorktree(
		mode: WorktreeMode,
		name: string,
		setup: WorktreeSetup | undefined,
		parentId: string | undefined,
		context: Context,
	): Promise<WorktreeRecord | undefined> {
		if (mode === false) return undefined;
		const from = this.workspaceOf(parentId);
		if (from === undefined) {
			if (mode === "auto") return undefined;
			throw new Error("worktree=True needs a working directory, and this session has none");
		}
		try {
			return await createChildWorktree({
				cwd: from,
				sessionId: this.sessionId,
				name,
				setup: { ...(this.worktreeSetup?.() ?? {}), ...(setup ?? {}) },
				...(context.abortSignal === undefined ? {} : { signal: context.abortSignal }),
			});
		} catch (error) {
			if (mode === "auto" && error instanceof WorktreeError && error.fallback) return undefined;
			throw error instanceof WorktreeError ? new Error(`worktree=True: ${error.message}`) : error;
		}
	}

	/** Nesting levels above a subagent task: its `rlm-child` ancestors, itself, and the levels above this host's root. */
	private childLevel(task: TaskRecord): number {
		let level = this.rootSpawn?.level ?? 0;
		for (let id: string | undefined = task.id; id !== undefined; id = this.tasks.get(id)?.parentId)
			if (this.tasks.get(id)?.definition === "rlm-child@1") level += 1;
		return level;
	}

	/** An `rlm.spawn` subagent run as a process of its own; the same prompt, budget, verdict check and graph stats. */
	private async executeExternalChild(
		task: TaskRecord,
		request: TaskRequest,
		laneName: string,
		context: Context,
		signal: AbortSignal,
	): Promise<NativeResult> {
		task.laneName = laneName;
		this.laneTasks.set(laneName, task.id);
		const basePrompt = this.childBrief(task, request);
		const refinements = (await this.refinements?.("rlm-child", context).catch(() => [])) ?? [];
		const prompt =
			refinements.length === 0
				? basePrompt
				: `${basePrompt}\n\n${refinements
						.map(
							(refinement) =>
								`Active refinement ${refinement.id} (version ${refinement.version ?? "unversioned"}):\n${refinement.text}`,
						)
						.join("\n\n")}`;
		const before = await this.childStart(task, signal);
		signal.throwIfAborted();
		const reservation = await this.usage?.reserve({
			kind: "model",
			...(task.usageReservation === undefined ? {} : { rootId: task.usageReservation.rootId }),
			parentTaskId: task.id,
			taskId: task.id,
			requestKey: `${task.id}:model`,
			...(task.usageReservation?.deadlineAt == null
				? { timeoutMs: request.timeoutMs }
				: { deadlineAt: task.usageReservation.deadlineAt }),
			signal,
		});
		let status: NativeUsageCallStatus = "failed";
		let usage: NativeUsageMeasurement | undefined;
		let turns = 0;
		try {
			const run = await this.externalChild!({
				taskId: task.id,
				laneName,
				prompt,
				level: this.childLevel(task),
				allowance: spawnAllowance(task),
				...(request.model === undefined ? {} : { model: request.model }),
				// A worktree child (or a descendant of one) runs in that worktree.
				...(this.worktreeOf(task.id) === undefined
					? {}
					: { cwd: this.worktreeOf(task.id)!.cwd, env: worktreeEnv(this.worktreeOf(task.id)!) }),
				signal,
				context,
				deadlineAt: task.usageReservation?.deadlineAt ?? null,
				timeoutMs: request.timeoutMs,
				progress: (stats) => {
					if (stats.turns !== undefined) task.turns = stats.turns;
					if (stats.toolCalls !== undefined) task.toolCallCount = stats.toolCalls;
					if (stats.text) task.lastText = preview(stats.text, GRAPH_TEXT_CHARS);
					if (stats.model) task.model = preview(stats.model, GRAPH_MODEL_CHARS);
				},
			});
			signal.throwIfAborted();
			status = "succeeded";
			usage = run.usage;
			turns = run.turns;
			task.turns = Math.max(task.turns ?? 0, run.turns);
			task.toolCallCount = Math.max(task.toolCallCount ?? 0, run.toolCalls);
			if (run.model) task.model = preview(run.model, GRAPH_MODEL_CHARS);
			if (run.text.trim()) task.lastText = preview(run.text, GRAPH_TEXT_CHARS);
			return await this.childResult(task, run.text, before, signal);
		} catch (error) {
			status = signal.aborted ? "cancelled" : "failed";
			throw error;
		} finally {
			if (typeof usage?.cost === "number") task.cost = usage.cost;
			if (typeof usage?.totalTokens === "number") task.tokens = usage.totalTokens;
			if (reservation) await this.usage?.settle(reservation, { status, ...(usage === undefined ? {} : { usage }) });
			// The child's model turns count toward its root's turn and token limits, its tokens and cost on the first.
			const rootId = task.usageReservation?.rootId;
			for (let index = 0; index < Math.max(1, turns); index += 1)
				await this.usage
					?.recordTurn?.(
						rootId,
						index === 0
							? { totalTokens: usage?.totalTokens ?? null, cost: usage?.cost ?? null }
							: { totalTokens: 0, cost: 0 },
					)
					.catch(() => {});
		}
	}

	/**
	 * A finished subagent's result: its reply, its verdict (null without a valid `rlm.finish`), and the host's check
	 * of the verdict's declared files against the files that changed while it ran (see verdict.ts).
	 */
	private async childResult(
		task: TaskRecord,
		text: string,
		before: WorkspaceSnapshot | Error | undefined,
		signal: AbortSignal,
	): Promise<NativeResult> {
		const verdict = task.verdict;
		if (!text.trim() && verdict === undefined)
			throw new Error("Agent produced no assistant result at the completed tip");
		const value = text.trim() ? text : verdict!.summary;
		const after =
			before instanceof Error || before === undefined
				? undefined
				: await this.snapshot(signal, this.workspaceOf(task.id));
		signal.throwIfAborted();
		const concurrent = this.concurrentWith(task);
		const explained = new Set<string>();
		for (const other of concurrent) {
			const declared = (other.result?.verdict as Verdict | null | undefined)?.changed_files;
			for (const path of Array.isArray(declared) ? declared : []) explained.add(path);
		}
		const empty = { unobserved: [], unreported: [], unlisted: [], concurrent: concurrent.map((other) => other.id) };
		let check: VerdictCheck;
		if (before === undefined || after === undefined || before instanceof Error || after instanceof Error) {
			const failed = before instanceof Error ? before : after instanceof Error ? after : undefined;
			check = {
				outcome: task.verdictInvalid ? "invalid" : "unchecked",
				...empty,
				reason: failed ? `workspace snapshot failed: ${failed.message}` : "no workspace to compare",
			};
		} else {
			const workspace = before.root;
			// Without a verdict nothing is declared, so every change seen during the run is reported.
			check = await checkFiles({
				verdict: verdict ?? { status: "failed", summary: "", outputs: {}, evidence: [], changed_files: [] },
				workspace,
				before,
				after,
				diff: diffSnapshots(before, after),
				explained,
				concurrent: empty.concurrent,
				exists: (path) =>
					lstat(join(workspace, path)).then(
						() => true,
						() => false,
					),
			});
			if (verdict === undefined) {
				check.outcome = task.verdictInvalid ? "invalid" : "unchecked";
				check.reason = "the subagent ended without a verdict (rlm.finish)";
			}
		}
		if (task.verdictInvalid) check.problems = task.verdictProblems ?? [];
		const outside = await this.outsideWrites(task, signal);
		if (outside.length > 0) check.outside = outside;
		// A child with no verdict and no workspace to observe has nothing to report beyond being unverified.
		const nothingChecked = verdict === undefined && !task.verdictInvalid && before === undefined;
		return {
			status: "succeeded",
			value,
			verification: "unverified",
			verdict: verdict === undefined ? null : (structuredClone(verdict) as unknown as JsonValue),
			...(nothingChecked ? {} : { check: check as unknown as JsonValue }),
			...(verdict === undefined ? { unverified: true as const } : {}),
		};
	}

	/**
	 * Files in the parent's tree that changed while a worktree child ran, minus what concurrent work there declared:
	 * a worktree child should write only in its worktree (its shell commands can still reach the parent's tree).
	 */
	private async outsideWrites(task: TaskRecord, signal: AbortSignal): Promise<string[]> {
		const before = task.outsideBefore;
		if (task.worktree === undefined || before === undefined) return [];
		const after = await this.snapshot(signal, before.root);
		if (after === undefined || after instanceof Error) return [];
		const explained = new Set<string>();
		for (const other of this.tasks.values()) {
			if (other === task || this.workspaceOf(other.id) !== this.workspaceOf(task.parentId)) continue;
			const declared =
				(other.result?.verdict as Verdict | null | undefined)?.changed_files ?? other.verdict?.changed_files;
			for (const path of Array.isArray(declared) ? declared : []) explained.add(path);
		}
		return diffSnapshots(before, after)
			.changed.filter((path) => !explained.has(path))
			.slice(0, 100);
	}

	/**
	 * A detached task is admitted under the caller's cancellation but then runs independently of it: a background
	 * job must outlive the RLM cell (and the client) that started it, stopping only on its own stop, timeout, or
	 * host close.
	 */
	/** Wait for a task's result, marking it awaited so its end is not also announced as an event. */
	private async awaitTask(task: TaskRecord): Promise<NativeResult> {
		if (!task.promise) return task.result!;
		task.waiters = (task.waiters ?? 0) + 1;
		try {
			return await task.promise;
		} finally {
			task.waiters -= 1;
		}
	}

	private spawnTask(
		request: TaskRequest,
		context: Context,
		parentId?: string,
		detached = false,
		notify?: DetachedEndKind,
	): Promise<TaskRecord> {
		// Serialize through installation of the live promise, not through execution.
		const pending = this.admissions.then(async () => {
			if (this.closed) throw new Error("Ultron task host is closed");
			context.abortSignal?.throwIfAborted();
			const fingerprint = taskFingerprint({
				definition: request.definition,
				input: request.input,
				model: request.model ?? null,
				timeout_ms: request.timeoutMs,
			});
			const key = request.key ?? randomUUID();
			// An idempotent retry resolves to the existing task before any budget is touched: it must
			// neither consume a new admission slot nor settle the slot the live original still holds.
			if (request.key !== undefined) {
				const prior = (await this.journal.list()).find((task) => task.key === key);
				if (prior) {
					if (prior.fingerprint !== fingerprint) throw new Error("Idempotency key reused for a different task");
					const existing = this.tasks.get(prior.id);
					if (existing) return existing;
					this.tasks.set(prior.id, prior);
					return prior;
				}
			}
			const rootId = request.usageRoot ?? this.admissionRoot(parentId);
			const usageReservation = await this.usage?.reserve({
				kind: "task",
				...(rootId === undefined ? {} : { rootId }),
				modelBacked: this.definition(request.definition).strategy !== "deterministic",
				requestKey: key,
				...(request.untimed === true ? {} : { timeoutMs: request.timeoutMs }),
				signal: context.abortSignal,
			});
			let admitted: Awaited<ReturnType<NativeTaskJournal["admit"]>>;
			try {
				admitted = await this.journal.admit(request.definition, fingerprint, key, context.abortSignal, parentId);
			} catch (error) {
				if (usageReservation) await this.usage?.settle(usageReservation, { status: "unknown" }).catch(() => {});
				throw error;
			}
			if (!admitted.created) {
				if (usageReservation) await this.usage?.settle(usageReservation, { status: "succeeded" });
				const existing = this.tasks.get(admitted.task.id);
				if (existing) return existing;
				this.tasks.set(admitted.task.id, admitted.task);
				return admitted.task;
			}
			const task: TaskRecord = {
				...admitted.task,
				controller: new AbortController(),
				usageReservation,
				...(notify === undefined ? {} : { notify }),
				startedAt: this.now(),
				inputPreview: preview(request.input),
				...(request.workflow === undefined ? {} : { workflow: request.workflow }),
				...(request.worktree === undefined ? {} : { worktree: request.worktree }),
			};
			task.promise = new Promise<NativeResult>((resolve, reject) => {
				task.resolve = resolve;
				task.reject = reject;
			});
			// Spawn callers need not observe the result immediately, including store errors.
			void task.promise.catch(() => {});
			this.tasks.set(task.id, task);
			const runContext = detached ? withoutAbortSignal(context) : context;
			const onAbort = () => {
				void this.cancel(task, "Parent task aborted").catch(() => {});
			};
			const timeoutDelay =
				usageReservation?.deadlineAt === null || usageReservation?.deadlineAt === undefined
					? request.untimed === true
						? undefined
						: request.timeoutMs
					: Math.max(
							1,
							Math.min(
								request.untimed === true ? Number.POSITIVE_INFINITY : request.timeoutMs,
								usageReservation.deadlineAt - this.now(),
							),
						);
			// Only a deadline the ledger capped below the task's own timeout is the root wall deadline.
			const deadlineTimeout =
				usageReservation?.deadlineAt != null &&
				(request.untimed === true || usageReservation.deadlineAt < usageReservation.admittedAt + request.timeoutMs);
			const timer =
				timeoutDelay === undefined
					? undefined
					: setTimeout(() => {
							void this.cancel(
								task,
								deadlineTimeout
									? "Ultron root wall deadline exceeded"
									: `Ultron task exceeded ${request.timeoutMs}ms timeout`,
							).catch(() => {});
						}, timeoutDelay);
			timer?.unref();
			runContext.abortSignal?.addEventListener("abort", onAbort, { once: true });
			const releaseActivity = this.holdActivity?.();
			task.cleanup = () => {
				if (timer !== undefined) clearTimeout(timer);
				runContext.abortSignal?.removeEventListener("abort", onAbort);
				releaseActivity?.();
			};
			if (this.closed) void this.cancel(task, "Ultron task host closed").catch(() => {});
			else if (runContext.abortSignal?.aborted) onAbort();
			else void this.run(task, request, runContext);
			return task;
		});
		this.admissions = pending.then(
			() => {},
			() => {},
		);
		return pending;
	}

	close(): Promise<void> {
		this.closed = true;
		this.stopObserving?.();
		this.closing ??= (async () => {
			await this.admissions;
			for (const module of this.modules) await module.close?.();
			const results = await Promise.allSettled(
				[...this.tasks.values()]
					.filter((task) => !task.result)
					.map((task) => this.cancel(task, "Ultron task host closed")),
			);
			// This session's finished, unmerged children: the directory goes, the branch keeps the work. Conflicted and
			// failed ones stay for inspection until a later session prunes them.
			if (this.workspace !== undefined && !keepWorktrees())
				await pruneWorktrees(this.workspace, {
					force: true,
					select: (record) =>
						record.sessionId === this.sessionId && record.owner.pid === process.pid && record.state === "ended",
				}).catch(() => {});
			const failed = results.find((result) => result.status === "rejected");
			if (failed?.status === "rejected") throw failed.reason;
		})();
		return this.closing;
	}

	private workflow(payload: Payload): WorkflowNode[] {
		fields(payload, ["nodes", "key"]);
		if (!Array.isArray(payload.nodes)) throw new Error("nodes must be an array");
		const workflowKey = payload.key === undefined ? undefined : nonemptyString(payload.key, "Workflow key");
		const condition = (value: unknown, name: string): { field?: string; equals: JsonValue } => {
			const route = objectInput(value);
			if (!Object.hasOwn(route, "equals") || !isJsonValue(route.equals))
				throw new Error(`${name}.equals must be a JSON value`);
			return {
				...(route.field === undefined ? {} : { field: nonemptyString(route.field, `${name}.field`) }),
				equals: route.equals,
			};
		};
		const nodes: WorkflowNode[] = payload.nodes.map((value) => {
			const node = objectInput(value);
			fields(node, [
				"id",
				"definition",
				"input",
				"model",
				"key",
				"timeout_ms",
				"dependsOn",
				"inputFrom",
				"when",
				"join",
				"revise",
				"worktree",
			]);
			const id = nonemptyString(node.id, "Workflow node ID");
			const definition = definitionKey(node.definition);
			const mode = worktreeMode(node.worktree, `Workflow node ${id} worktree`);
			if (mode !== false && definition !== "rlm-child@1")
				throw new Error(`Workflow node ${id}: worktree applies only to rlm-child@1 nodes (subagents)`);
			if (mode !== false && node.revise !== undefined)
				throw new Error(
					`Workflow node ${id}: a revised node cannot use a worktree (each round would start over from your tree)`,
				);
			const item = this.definition(definition);
			// A node that could never execute is a validation error, not a runtime failure after effects.
			this.registry.canExecute(item);
			const dependsOn = node.dependsOn === undefined ? [] : node.dependsOn;
			if (
				!Array.isArray(dependsOn) ||
				dependsOn.some((dependency) => typeof dependency !== "string" || !dependency.trim()) ||
				new Set(dependsOn).size !== dependsOn.length
			)
				throw new Error("dependsOn must be an array of unique nonempty strings");
			let inputFrom: string | string[] | undefined;
			if (Array.isArray(node.inputFrom)) {
				inputFrom = node.inputFrom.map((source) => nonemptyString(source, "inputFrom entry"));
				if (inputFrom.length === 0 || new Set(inputFrom).size !== inputFrom.length)
					throw new Error("inputFrom must be a nonempty array of unique dependencies");
			} else if (node.inputFrom !== undefined) inputFrom = nonemptyString(node.inputFrom, "inputFrom");
			const join = node.join === undefined ? "all" : node.join;
			if (join !== "all" && join !== "any") throw new Error('join must be "all" or "any"');
			if (join === "any") {
				// Any-of over a single dependency is an all-join; a single bound input may be absent.
				if (dependsOn.length < 2) throw new Error('join "any" requires at least two dependencies');
				if (typeof inputFrom === "string")
					throw new Error('join "any" requires inputFrom to be an array (fan-in of succeeded dependencies)');
			}
			let when: WorkflowRoute | undefined;
			if (node.when !== undefined) {
				fields(objectInput(node.when), ["node", "field", "equals"]);
				const routeNode = nonemptyString((node.when as Payload).node, "when.node");
				if (!dependsOn.includes(routeNode)) throw new Error("when.node must name a dependency");
				when = { node: routeNode, ...condition(node.when, "when") };
			}
			let revise: WorkflowRevision | undefined;
			if (node.revise !== undefined) {
				const loop = objectInput(node.revise);
				fields(loop, ["from", "until", "max_rounds"]);
				const maxRounds = loop.max_rounds;
				if (
					typeof maxRounds !== "number" ||
					!Number.isSafeInteger(maxRounds) ||
					maxRounds < 1 ||
					maxRounds > WORKFLOW_MAX_ROUNDS
				)
					throw new Error(`revise.max_rounds must be an integer between 1 and ${WORKFLOW_MAX_ROUNDS}`);
				if (loop.until === undefined) throw new Error("revise.until is required");
				fields(objectInput(loop.until), ["field", "equals"]);
				revise = {
					from: nonemptyString(loop.from, "revise.from"),
					until: condition(loop.until, "revise.until"),
					maxRounds,
				};
			}
			if (inputFrom !== undefined) {
				for (const source of Array.isArray(inputFrom) ? inputFrom : [inputFrom])
					if (!dependsOn.includes(source)) throw new Error("inputFrom must name a dependency");
				if (Object.hasOwn(node, "input")) throw new Error("Specify input or inputFrom, not both");
			} else {
				if (!isJsonValue(node.input)) throw new Error("Agent input is not JSON");
				if (!this.registry.isValidInput(item, node.input))
					throw this.registry.validationError(item, node.input, "input");
			}
			const options = taskOptions(node);
			return {
				id,
				definition,
				input: node.input as JsonValue | undefined,
				dependsOn,
				inputFrom,
				join,
				...(when === undefined ? {} : { when }),
				...(revise === undefined ? {} : { revise }),
				...(mode === false ? {} : { worktreeMode: mode }),
				...options,
				// A keyed workflow gives every node a stable key; an explicit node key wins.
				key: options.key ?? (workflowKey === undefined ? undefined : `${workflowKey}:${id}`),
			};
		});
		const ids = new Set(nodes.map((node) => node.id));
		if (ids.size !== nodes.length) throw new Error("Duplicate workflow node ID");
		for (const node of nodes) {
			if (node.dependsOn.some((dependency) => !ids.has(dependency))) throw new Error("Unknown workflow dependency");
		}
		// Revision loops are declared constructs, not edges: the topology check below still sees a DAG.
		const byId = new Map(nodes.map((node) => [node.id, node]));
		const ancestors = (id: string, seen = new Set<string>()): Set<string> => {
			for (const dependency of byId.get(id)?.dependsOn ?? [])
				if (!seen.has(dependency)) {
					seen.add(dependency);
					ancestors(dependency, seen);
				}
			return seen;
		};
		const reviewers = new Set<string>();
		const pending = new Set(ids);
		while (pending.size) {
			const ready = nodes.filter(
				(node) => pending.has(node.id) && node.dependsOn.every((dependency) => !pending.has(dependency)),
			);
			if (!ready.length) throw new Error("Workflow contains a cycle");
			for (const node of ready) pending.delete(node.id);
		}
		for (const node of nodes) {
			if (!node.revise) continue;
			const reviewer = byId.get(node.revise.from);
			if (!reviewer) throw new Error("revise.from must name a workflow node");
			if (!reviewer.dependsOn.includes(node.id)) throw new Error("revise.from must depend on the revised node");
			if (reviewer.revise) throw new Error("A revision reviewer cannot itself be revised");
			if (reviewer.when) throw new Error("A revision reviewer cannot have a when route");
			if (reviewers.has(reviewer.id)) throw new Error("A node can review only one revision loop");
			reviewers.add(reviewer.id);
			// The reviewer re-runs every round, so its other inputs must be settled before the loop starts.
			const before = ancestors(node.id);
			if (reviewer.dependsOn.some((dependency) => dependency !== node.id && !before.has(dependency)))
				throw new Error("A revision reviewer may depend only on the revised node and its ancestors");
		}
		return nodes;
	}

	/**
	 * Decides whether a ready node runs and with what input, from its dependencies' terminal outcomes.
	 * Returns the input to run with, or the explicit outcome that replaces running it.
	 */
	private workflowInput(
		node: WorkflowNode,
		output: ReadonlyMap<string, WorkflowOutcome>,
	): { input: JsonValue } | { outcome: WorkflowOutcome } {
		const statusOf = (dependency: string) => output.get(dependency)!.status;
		if (node.join === "all") {
			const unmet = node.dependsOn.find((dependency) => statusOf(dependency) !== "succeeded");
			if (unmet !== undefined)
				return {
					outcome: { status: "skipped", reason: `Dependency ${unmet} did not succeed (${statusOf(unmet)})` },
				};
		} else if (!node.dependsOn.some((dependency) => statusOf(dependency) === "succeeded"))
			return {
				outcome: {
					status: "skipped",
					reason: `No dependency succeeded (${node.dependsOn.map((dependency) => `${dependency}: ${statusOf(dependency)}`).join(", ")})`,
				},
			};
		const succeeded = (dependency: string) => statusOf(dependency) === "succeeded";
		const resultOf = (dependency: string) => boundValue(output.get(dependency));
		if (node.when) {
			const label = `${node.when.node}${node.when.field === undefined ? "" : `.${node.when.field}`}`;
			if (!succeeded(node.when.node))
				return {
					outcome: {
						status: "skipped",
						reason: `Route dependency ${node.when.node} did not succeed (${statusOf(node.when.node)})`,
					},
				};
			if (!conditionMet(resultOf(node.when.node), node.when))
				return { outcome: { status: "skipped", reason: `Route condition on ${label} not met` } };
		}
		const input =
			node.inputFrom === undefined
				? node.input
				: Array.isArray(node.inputFrom)
					? Object.fromEntries(
							node.inputFrom.filter(succeeded).map((source) => [source, resultOf(source) ?? null]),
						)
					: resultOf(node.inputFrom);
		return { input: input as JsonValue };
	}

	/** Validates a bound input, admits the node's task, and waits for its durable terminal result. */
	private async workflowTask(
		node: WorkflowNode,
		input: JsonValue | undefined,
		key: string | undefined,
		context: Context,
		parentId: string | undefined,
		run?: string,
		/** Fail a subagent node whose verdict did not pass (a revision reviewer's verdict is data for `until`). */
		gate = true,
	): Promise<WorkflowOutcome> {
		const definition = this.definition(node.definition);
		// A bound input that does not fit fails this node explicitly; nothing is spawned for it.
		if (!isJsonValue(input) || !this.registry.isValidInput(definition, input))
			return {
				status: "failed",
				error: `Bound input rejected: ${this.registry.validationError(definition, input, "input").message}`,
				verification: "unverified",
			};
		let task: TaskRecord;
		let worktree: WorktreeRecord | undefined;
		try {
			const workflow =
				run === undefined ? undefined : { run, node: node.id, dependsOn: [...node.dependsOn], join: node.join };
			const { worktreeMode: mode, ...request } = node;
			// A worktree node branches from the caller's tree as it is when the node starts.
			worktree = await this.newWorktree(mode ?? false, node.id, undefined, parentId, context);
			task = await this.spawnTask(
				{ ...request, input, key, workflow, ...(worktree === undefined ? {} : { worktree }) },
				context,
				parentId,
			);
			if (worktree !== undefined && task.worktree !== worktree) {
				// An idempotent retry found the node's earlier task: the new worktree is not needed.
				await removeChildWorktree(worktree, { deleteBranch: true }).catch(() => {});
			} else if (worktree !== undefined) {
				worktree.taskId = task.id;
				await saveRecord(worktree).catch(() => {});
			}
		} catch (error) {
			if (worktree !== undefined && this.tasks.get(worktree.taskId ?? "")?.worktree !== worktree)
				await removeChildWorktree(worktree, { deleteBranch: true }).catch(() => {});
			// Refused admission (capacity, deadline, closed host) is an explicit node failure.
			return {
				status: "failed",
				error: `Admission refused: ${error instanceof Error ? error.message : String(error)}`,
				verification: "unverified",
			};
		}
		const result = await (task.promise ?? task.result!);
		return gate ? verdictGate(result) : result;
	}

	/**
	 * Runs a bounded revision loop: the revised node, then its reviewer, repeated with the review as input until the
	 * reviewer's result meets `until` or `max_rounds` is reached. Every round is its own task (keyed
	 * `<key>:round-<n>`) and its own admission. Returns the revised node's and the reviewer's outcomes.
	 */
	private async workflowLoop(
		work: WorkflowNode,
		reviewer: WorkflowNode,
		firstInput: JsonValue,
		output: ReadonlyMap<string, WorkflowOutcome>,
		context: Context,
		parentId: string | undefined,
		run?: string,
	): Promise<[WorkflowOutcome, WorkflowOutcome]> {
		const revise = work.revise!;
		const rounds: WorkflowRound[] = [];
		const roundKey = (node: WorkflowNode, round: number) =>
			node.key === undefined ? undefined : `${node.key}:round-${round}`;
		const finish = (
			outcome: "converged" | "exhausted" | "failed",
			workResult: WorkflowOutcome,
			reviewResult: WorkflowOutcome,
		): [WorkflowOutcome, WorkflowOutcome] => [
			{ ...workResult, revision: { outcome, rounds: rounds.length, max_rounds: revise.maxRounds }, rounds },
			reviewResult,
		];
		let input = firstInput;
		for (let round = 1; ; round++) {
			const workResult = await this.workflowTask(work, input, roundKey(work, round), context, parentId, run);
			if (workResult.status !== "succeeded") {
				const skipped: WorkflowOutcome = {
					status: "skipped",
					reason: `Dependency ${work.id} did not succeed (${workResult.status})`,
				};
				rounds.push({ round, work: workResult });
				return finish("failed", workResult, skipped);
			}
			const view = new Map(output).set(work.id, workResult);
			const decided = this.workflowInput(reviewer, view);
			const reviewResult =
				"outcome" in decided
					? decided.outcome
					: await this.workflowTask(
							reviewer,
							decided.input,
							roundKey(reviewer, round),
							context,
							parentId,
							run,
							false,
						);
			rounds.push({ round, work: workResult, review: reviewResult });
			if (reviewResult.status !== "succeeded")
				return finish(
					"failed",
					{
						status: "failed",
						value: workResult.value,
						error: `Reviewer ${reviewer.id} did not succeed in round ${round} (${reviewResult.status})`,
						verification: "unverified",
					},
					reviewResult,
				);
			if (conditionMet(boundValue(reviewResult), revise.until)) return finish("converged", workResult, reviewResult);
			if (round === revise.maxRounds)
				return finish(
					"exhausted",
					{ status: "exhausted", value: workResult.value, verification: "unverified" },
					reviewResult,
				);
			input = {
				input: firstInput,
				previous: boundValue(workResult) ?? null,
				review: boundValue(reviewResult) ?? null,
				round: round + 1,
			};
		}
	}

	private async runWorkflow(
		nodes: WorkflowNode[],
		context: Context,
		parentId: string | undefined,
	): Promise<Record<string, WorkflowOutcome>> {
		const output = new Map<string, WorkflowOutcome>();
		const byId = new Map(nodes.map((node) => [node.id, node]));
		// Groups this run's tasks in the read-only graph view.
		const run = `wf-${randomUUID().slice(0, 8)}`;
		const record: WorkflowRunRecord = {
			run,
			...(parentId === undefined ? {} : { parentId }),
			startedAt: this.now(),
			nodes: nodes.map((node) => ({
				id: node.id,
				definition: node.definition,
				dependsOn: [...node.dependsOn],
				join: node.join,
			})),
			ended: {},
		};
		this.workflowRuns.set(run, record);
		for (const old of [...this.workflowRuns.keys()].slice(0, -GRAPH_WORKFLOW_LIMIT)) this.workflowRuns.delete(old);
		// A reviewer runs inside its loop; its outcome is published when the loop ends.
		const reviewers = new Set(nodes.flatMap((node) => (node.revise ? [node.revise.from] : [])));
		// A dependency is satisfied only by its durable terminal result, never by admission.
		while (output.size < nodes.length) {
			const ready = nodes.filter(
				(node) =>
					!output.has(node.id) &&
					!reviewers.has(node.id) &&
					node.dependsOn.every((dependency) => output.has(dependency)),
			);
			const results = await Promise.all(
				ready.map(async (node): Promise<Array<readonly [string, WorkflowOutcome]>> => {
					const reviewer = node.revise ? byId.get(node.revise.from)! : undefined;
					const decided = this.workflowInput(node, output);
					if ("outcome" in decided) {
						if (!reviewer) return [[node.id, decided.outcome]];
						return [
							[node.id, decided.outcome],
							[
								reviewer.id,
								{
									status: "skipped",
									reason: `Dependency ${node.id} did not succeed (${decided.outcome.status})`,
								},
							],
						];
					}
					if (reviewer) {
						const [work, review] = await this.workflowLoop(
							node,
							reviewer,
							decided.input,
							output,
							context,
							parentId,
							run,
						);
						return [
							[node.id, work],
							[reviewer.id, review],
						];
					}
					return [[node.id, await this.workflowTask(node, decided.input, node.key, context, parentId, run)]];
				}),
			);
			for (const [id, result] of results.flat()) {
				output.set(id, result);
				const reason =
					"reason" in result && typeof result.reason === "string"
						? result.reason
						: "error" in result && typeof result.error === "string"
							? result.error
							: undefined;
				record.ended[id] = {
					status: result.status,
					...(reason === undefined ? {} : { reason: preview(reason, GRAPH_TEXT_CHARS) }),
				};
			}
		}
		record.endedAt = this.now();
		return Object.fromEntries(output);
	}

	async handle(type: string, payload: Payload, context: Context, caller: HostCaller = ROOT_CALLER): Promise<unknown> {
		if (this.closed) throw new Error("Ultron task host is closed");
		if (!isJsonValue(payload) || payload === null || Array.isArray(payload) || typeof payload !== "object")
			throw new Error("Host payload must be a JSON object");
		payload = structuredClone(payload);
		await this.loadTasks();
		if (this.closed) throw new Error("Ultron task host is closed");
		const parentId = this.laneTasks.get(caller.lane);
		// A child lane sees only its own task and descendants; siblings and ancestors' other work stay out of its
		// context (A38). The root lane sees the whole session.
		const journal = await this.journal.list();
		const visible = (id: string): boolean => {
			if (parentId === undefined) return true;
			const byId = new Map(journal.map((task) => [task.id, task]));
			for (let current = byId.get(id); current; current = current.parentId ? byId.get(current.parentId) : undefined)
				if (current.id === parentId) return true;
			return false;
		};
		const module = this.modules.find((candidate) => candidate.prefixes.some((prefix) => type.startsWith(prefix)));
		if (module) return module.handle({ type, payload, caller, context }, this.api);
		if (["ping", "agents.list", "agents.tasks"].includes(type)) fields(payload, []);
		if (type === "agents.status") fields(payload, ["graph"]);
		if (type === "ping") return { ok: true };
		if (type === "agents.list") return this.list();
		if (type === "agents.status" || type === "agents.tasks") {
			const usage = this.usage ? await this.usage.status(this.statusRoot(parentId)) : null;
			const shown = journal.filter((task) => visible(task.id));
			// The graph view is bounded: the newest tasks with previews instead of whole results.
			const graph = payload.graph === true;
			const listed = graph ? shown.slice(-GRAPH_TASK_LIMIT) : shown;
			return {
				definitions: this.list(),
				tasks: graph ? listed.map((task) => graphTask(this.tasks.get(task.id) ?? task)) : listed.map(publicTask),
				...(graph && shown.length > listed.length ? { truncatedTasks: shown.length - listed.length } : {}),
				// Workflow plans (pending nodes have no task yet); a child lane sees only runs its subtree started.
				...(graph
					? {
							workflows: [...this.workflowRuns.values()]
								.filter((record) =>
									record.parentId === undefined ? parentId === undefined : visible(record.parentId),
								)
								.map(graphWorkflow),
						}
					: {}),
				usage,
				limits: usage?.limits ?? null,
				// The spend of the caller's root tree: every model response of the root and each lane it admitted.
				spend:
					usage === null
						? null
						: {
								root: usage.rootId,
								turns: usage.turns.turns,
								tokens: usage.turns.tokens,
								costUsd: usage.cost.spentUsd,
								maxTotalTurns: usage.limits.maxTotalTurns,
								maxTotalTokens: usage.limits.maxTotalTokens,
								maxCostUsd: usage.limits.maxCostUsd,
							},
				...(this.statusExtras?.(caller) ?? {}),
				controls: Object.fromEntries(
					[
						"permissionPrompts",
						"riskBlocking",
						"capabilityEnforcement",
						"budgetEnforcement",
						"completionGates",
						"refinementApproval",
						"sandboxRequired",
					].map((key) => [key, false]),
				),
			};
		}
		if (type === "agents.register") {
			fields(payload, ["definition"]);
			return this.registry.register(payload.definition);
		}
		if (type === "background.start") {
			fields(payload, ["prompt", "model", "key", "timeout_ms"]);
			const prompt = nonemptyString(payload.prompt, "prompt");
			if (prompt.length > 65_536) throw new Error("Background prompt exceeds 65536 characters");
			const request: TaskRequest = {
				definition: "background-job@1",
				input: { prompt },
				model: typeof payload.model === "string" ? payload.model : undefined,
				key: typeof payload.key === "string" ? `background:${payload.key}` : undefined,
				timeoutMs: typeof payload.timeout_ms === "number" ? payload.timeout_ms : 30 * 60 * 1000,
			};
			if (request.key !== undefined && request.key.length > 264) throw new Error("Background key is too long");
			const task = await this.spawnTask(request, context, parentId, true, "task_done");
			return publicTask(task);
		}
		if (type === "background.list") {
			fields(payload, []);
			return journal.filter((task) => task.definition === "background-job@1" && visible(task.id)).map(publicTask);
		}
		if (type === "background.inspect" || type === "background.result" || type === "background.stop") {
			fields(payload, ["id"]);
			const id = nonemptyString(payload.id, "id");
			const stored = journal.find((task) => task.id === id && task.definition === "background-job@1");
			if (!stored || !visible(id)) throw new Error("Unknown background job");
			const task = this.tasks.get(id) ?? stored;
			if (type === "background.inspect") return publicTask(task);
			if (type === "background.stop")
				return { cancelled: (await this.cancel(task, "Background job stopped")).status === "cancelled" };
			const liveTask = this.tasks.get(id);
			return structuredClone(liveTask ? await this.awaitTask(liveTask) : task.result);
		}
		if (type === "rlm.spawn") {
			fields(payload, ["prompt", "kwargs"]);
			// Recursive fan-out (children re-splitting their slice into grandchildren) multiplies the whole prompt and
			// transcript per level; a subagent nested this deep does its part itself.
			const limit = spawnDepthLimit();
			const record = (id: string) => this.tasks.get(id) ?? journal.find((task) => task.id === id);
			let depth = this.rootSpawn?.level ?? 0;
			let spawner: TaskRecord | undefined;
			for (let id = parentId; id !== undefined; id = record(id)?.parentId) {
				const ancestor = record(id);
				if (ancestor?.definition !== "rlm-child@1") continue;
				depth += 1;
				spawner ??= ancestor;
			}
			if (limit > 0 && depth >= limit)
				throw new Error(
					`rlm.spawn refused: subagents nest at most ${limit} level(s) deep (ULTRON_SPAWN_DEPTH). Do this part yourself: narrow with code, read the candidates, and use rlm.map for passages you cannot settle.`,
				);
			// How many more levels the spawning subagent's parent let it create (rlm.spawn(..., depth=N)); the root may
			// always spawn.
			const allowance =
				spawner === undefined ? (this.rootSpawn?.allowance ?? Number.POSITIVE_INFINITY) : spawnAllowance(spawner);
			if (allowance < 1)
				throw new Error(
					"rlm.spawn refused: this subagent was started with depth=0, so it does its brief itself. A parent that wants nested delegation passes rlm.spawn(brief, name=..., depth=N).",
				);
			const prompt = nonemptyString(payload.prompt, "prompt");
			const kwargs = payload.kwargs === undefined ? {} : objectInput(payload.kwargs);
			fields(kwargs, ["name", "model", "thinking", "timeout_ms", "depth", "worktree", "worktree_setup", "fork"]);
			if (kwargs.fork !== undefined && typeof kwargs.fork !== "boolean")
				throw new Error("fork must be True or False");
			if (kwargs.fork === true && this.externalChild !== undefined)
				throw new Error("rlm.spawn fork=True is not available when subagents run as external processes");
			const name = nonemptyString(kwargs.name, "name");
			const mode = worktreeMode(kwargs.worktree, "worktree");
			const setup = worktreeSetupOption(kwargs.worktree_setup);
			if (setup !== undefined && mode === false) throw new Error("worktree_setup needs worktree=True");
			const childDepth = kwargs.depth === undefined ? 0 : kwargs.depth;
			const maxChildDepth = Math.min(allowance - 1, limit > 0 ? limit - depth - 1 : Number.POSITIVE_INFINITY);
			if (typeof childDepth !== "number" || !Number.isSafeInteger(childDepth) || childDepth < 0)
				throw new Error("depth must be a non-negative integer");
			if (childDepth > maxChildDepth)
				throw new Error(
					`rlm.spawn depth=${childDepth} is too deep here: at most ${maxChildDepth} (ULTRON_SPAWN_DEPTH=${limit} levels in all).`,
				);
			const request: TaskRequest = {
				definition: "rlm-child@1",
				input: { prompt },
				model:
					typeof kwargs.model === "string"
						? kwargs.model
						: this.externalChild
							? undefined
							: typeof this.childModel === "function"
								? this.childModel()
								: this.childModel,
				timeoutMs: typeof kwargs.timeout_ms === "number" ? kwargs.timeout_ms : 30 * 60 * 1000,
			};
			if (!Number.isSafeInteger(request.timeoutMs) || request.timeoutMs < 1 || request.timeoutMs > 60 * 60 * 1000)
				throw new Error("timeout_ms must be an integer between 1 and 3600000");
			if (kwargs.fork === true) {
				const forkAt = forkPoint(
					await (await this.harness.lane(caller.lane, context)).findEntries({ order: "oldestFirst" }, context),
				);
				if (forkAt === undefined)
					throw new Error("rlm.spawn fork=True: your lane has no finished conversation to fork yet");
				request.forkAt = forkAt;
			}
			// The worktree branches from the spawner's tree as it is now, before the child is admitted.
			const worktree = await this.newWorktree(mode, name, setup, parentId, context);
			if (worktree !== undefined) request.worktree = worktree;
			if (kwargs.thinking !== undefined) {
				const thinking = FRAME_THINKING_LEVELS.find((level) => level === kwargs.thinking);
				if (thinking === undefined) throw new Error(`thinking must be one of ${FRAME_THINKING_LEVELS.join(", ")}`);
				request.thinking = thinking;
			}
			let task: TaskRecord;
			try {
				// A spawned child outlives the cell that started it; its subtree still stops with its parent task.
				task = await this.spawnTask(request, context, parentId, true, "child_done");
			} catch (error) {
				if (worktree !== undefined) await removeChildWorktree(worktree, { deleteBranch: true }).catch(() => {});
				throw error;
			}
			if (childDepth > 0) task.spawnDepth = childDepth;
			if (worktree !== undefined) {
				worktree.taskId = task.id;
				await saveRecord(worktree).catch(() => {});
			}
			return {
				rlm_child_id: task.id,
				name,
				session_dir: "",
				model: request.model ?? "",
				timeout_ms: request.timeoutMs,
				parent_branch_anchor: request.forkAt ?? "",
				worktree: worktree === undefined ? null : worktreeSummary(worktree),
			} satisfies RlmChildHandle;
		}
		if (type === "rlm.finish") {
			// A root that is itself a subagent (an `ultron mcp --child` server) gives its verdict to its parent's host.
			if (parentId === undefined && this.rootFinish !== undefined) return this.rootFinish(payload, context);
			const task = parentId === undefined ? undefined : this.tasks.get(parentId);
			if (task?.definition !== "rlm-child@1")
				throw new Error("rlm.finish is for subagents started with rlm.spawn; here, answer in your reply");
			if (task.result !== undefined || task.finishing !== undefined)
				throw new Error("rlm.finish: this subagent has already ended");
			if (task.verdictInvalid)
				throw new Error("rlm.finish: no attempts left; end your turn, your reply is returned unverified");
			const worktree = this.worktreeOf(task.id);
			// A worktree child may name files by their path in its parent's checkout: they mean its own copies.
			if (worktree !== undefined && Array.isArray(payload.changed_files))
				payload.changed_files = payload.changed_files.map((path) =>
					typeof path === "string" ? inWorktree(path, worktree) : path,
				);
			const checked = validateVerdict(payload, this.workspaceOf(task.id));
			if ("verdict" in checked) {
				task.verdict = checked.verdict;
				return {
					recorded: true,
					status: checked.verdict.status,
					next: "End your turn now with a short reply; the host checks changed_files against the files that changed while you ran.",
				};
			}
			task.verdictRejections = (task.verdictRejections ?? 0) + 1;
			task.verdictProblems = checked.problems;
			const left = MAX_VERDICT_REJECTIONS + 1 - task.verdictRejections;
			const problems = checked.problems.map((problem) => `- ${problem}`).join("\n");
			if (left > 0 || task.verdict !== undefined)
				throw new Error(
					`rlm.finish rejected:\n${problems}\nFix these and call rlm.finish again${task.verdict === undefined ? ` (${left} attempt${left === 1 ? "" : "s"} left)` : "; your earlier verdict stands until then"}.`,
				);
			task.verdictInvalid = true;
			throw new Error(
				`rlm.finish rejected:\n${problems}\nNo attempts left: end your turn; your reply is returned unverified, with these problems.`,
			);
		}
		if (type === "rlm.list_subagents") {
			fields(payload, []);
			return {
				subagents: [...this.tasks.values()]
					.filter((task) => task.definition === "rlm-child@1" && task.id !== parentId && visible(task.id))
					.map(publicTask),
			};
		}
		if (type === "rlm.collect") {
			fields(payload, ["selectors", "timeout_ms"]);
			const selectors = Array.isArray(payload.selectors)
				? payload.selectors.map((selector) => nonemptyString(selector, "selector"))
				: [];
			const tasks = [...this.tasks.values()].filter(
				(task) =>
					task.definition === "rlm-child@1" &&
					task.id !== parentId &&
					visible(task.id) &&
					(selectors.length === 0 || selectors.includes(task.id)),
			);
			const results = await Promise.all(
				tasks.map(async (task) => ({ id: task.id, result: await this.awaitTask(task) })),
			);
			return { results };
		}
		if (type === "rlm.delete_subagent") {
			fields(payload, ["selector"]);
			const selector = nonemptyString(payload.selector, "selector");
			const task = this.tasks.get(selector);
			if (!task || task.definition !== "rlm-child@1" || !visible(task.id)) throw new Error("Unknown RLM child");
			return { deleted: (await this.cancel(task, "RLM child deleted")).status === "cancelled" };
		}
		if (type === "rlm.merge") {
			fields(payload, ["selectors", "on_conflict", "include_failed"]);
			const onConflict = payload.on_conflict === undefined ? "stop" : payload.on_conflict;
			if (onConflict !== "stop" && onConflict !== "markers" && onConflict !== "skip")
				throw new Error('on_conflict must be "stop", "skip" or "markers"');
			const selectors = Array.isArray(payload.selectors)
				? payload.selectors.map((selector) => nonemptyString(selector, "selector"))
				: [];
			const children =
				selectors.length > 0
					? selectors.map((id) => {
							const task = this.tasks.get(id);
							if (!task || task.definition !== "rlm-child@1" || task.id === parentId || !visible(id))
								throw new Error(`Unknown RLM child: ${id}`);
							return task;
						})
					: // Default: the caller's own worktree children, in the order they were spawned.
						[...this.tasks.values()].filter(
							(task) =>
								task.definition === "rlm-child@1" &&
								task.parentId === parentId &&
								(task.worktree !== undefined || (task.result?.worktree ?? null) !== null),
						);
			if (new Set(children).size !== children.length) throw new Error("rlm.merge: a child is listed twice");
			// Every child ends first; the merges then run in the given order, one call at a time.
			await Promise.all(children.map((task) => this.awaitTask(task).catch(() => undefined)));
			const target = this.workspaceOf(parentId);
			if (target === undefined) throw new Error("rlm.merge needs a working directory, and this session has none");
			const run = this.merging.then(() =>
				this.mergeChildren(children, target, onConflict, payload.include_failed === true),
			);
			this.merging = run.then(
				() => {},
				() => {},
			);
			return run;
		}
		if (type === "rlm.worktrees.list") {
			fields(payload, ["all"]);
			const records = await listRecords(this.workspaceOf(parentId) ?? this.workspace ?? process.cwd());
			return {
				worktrees: records
					.filter((record) => payload.all === true || record.sessionId === this.sessionId)
					.map((record) => ({
						id: record.id,
						task: record.taskId ?? null,
						name: record.name,
						branch: record.branch,
						path: record.path,
						state: record.state,
						commit: record.commit ?? null,
						running: record.taskId !== undefined && this.tasks.get(record.taskId)?.result === undefined,
						session: record.sessionId,
					})),
			};
		}
		if (type === "rlm.worktrees.cleanup") {
			fields(payload, ["branches"]);
			const workspace = this.workspaceOf(parentId) ?? this.workspace;
			if (workspace === undefined) return { removed: [], kept_branches: [], running: [] };
			const running = (record: WorktreeRecord) =>
				record.taskId !== undefined &&
				this.tasks.get(record.taskId) !== undefined &&
				this.tasks.get(record.taskId)!.result === undefined;
			const records = (await listRecords(workspace)).filter((record) => record.sessionId === this.sessionId);
			// This session's ended children, and anything whose owner is gone.
			const own = await pruneWorktrees(workspace, {
				force: true,
				deleteBranches: payload.branches === true,
				select: (record) => record.sessionId === this.sessionId && !running(record),
			});
			const orphans = await pruneWorktrees(workspace, { deleteBranches: payload.branches === true });
			return {
				removed: [...own.removed, ...orphans.removed],
				kept_branches: [...own.keptBranches, ...orphans.keptBranches],
				running: records.filter(running).map((record) => record.branch),
			};
		}
		if (type === "agents.spawn" || type === "agents.invoke") {
			// invoke waits inside the cell, so the cell's cancellation applies; spawn hands the task back to be
			// collected later, so it must not die when the cell ends. Parent-task cancellation cascades either way.
			const spawn = type === "agents.spawn";
			const task = await this.spawnTask(
				this.request(payload),
				context,
				parentId,
				spawn,
				spawn ? "task_done" : undefined,
			);
			if (type === "agents.spawn") return { id: task.id, state: task.state };
			return structuredClone(await (task.promise ?? task.result));
		}
		if (["agents.inspect", "agents.result", "agents.cancel"].includes(type)) {
			fields(payload, ["id"]);
			const id = nonemptyString(payload.id, "id");
			const stored = journal.find((task) => task.id === id);
			if (!stored || !visible(id)) throw new Error("Unknown Ultron task");
			if (type === "agents.inspect") return publicTask(stored);
			const task = this.tasks.get(id)!;
			if (type === "agents.cancel") {
				// Stopping a task stops its subtree, even when the task itself already finished.
				const result = await this.cancel(task, "Ultron task cancelled");
				return { cancelled: result.status === "cancelled" };
			}
			return structuredClone(task.promise ? await this.awaitTask(task) : stored.result);
		}
		if (
			type.startsWith("memory.") ||
			type.startsWith("refinements.") ||
			type.startsWith("artifacts.") ||
			type.startsWith("experiments.")
		) {
			if (!this.services) throw new Error("Ultron local services are not connected to this session worker");
			return this.services.handle(type, payload, context);
		}
		if (type === "workflows.run") {
			const nodes = this.workflow(payload);
			return structuredClone(await this.runWorkflow(nodes, context, parentId));
		}
		throw new Error(`Ultron RLM host request is not wired: ${type}`);
	}
}
