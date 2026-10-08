/**
 * "What did this session do?": one report of a session's turns, cells, depth (frames, subagents, workflows, jobs),
 * tokens and cost per lane kind and model, guardrails and memory decisions. `/usage` shows it for the running
 * session and `ultron usage` for any session file, with no model call and no server.
 *
 * Everything comes from what the session already keeps (see session-log.ts): the task journal (`ultron.tasks`),
 * frame traces (`ultron.rlm.frames`), the usage ledger (`ultron.usage`), lane models (`pi.lane.config`), memory
 * operations, hint counts, the run records of each lane, the entries' provider-reported usage, and the runtime
 * counters of session-stats.ts. A number the session did not keep is `null` and named in `unrecorded`; it is never
 * reported as zero.
 */

import { type LogEntry, type LogUsage, logUsage, logValue, logValues, type SessionLog } from "./session-log.ts";
import {
	CELL_APIS,
	emptySessionStats,
	parseSessionStats,
	SESSION_STATS_MODULE,
	type SessionStatsCells,
	type SessionStatsDocument,
	type SessionStatsGuard,
} from "./session-stats.ts";

export const SESSION_REPORT_SCHEMA = "ultron.session-report/1";
/** The worker's read-only inspection request that returns the running session's report. */
export const SESSION_REPORT_REQUEST = "usage.report";

/** How the session's root agent ran: an Ultron lane, an Ultron lane on Claude Code, or Claude Code over MCP. */
export type SessionMode = "ultron" | "ultron --claude" | "ultron claude";

export type LaneKind = "root" | "frames" | "subagents" | "other";

export type ReportCost = {
	/** Provider-reported cost of responses billed per token; null when no response reported one. */
	reportedUsd: number | null;
	/** Notional cost of responses made on a subscription (Claude Code, an OAuth plan); null when there were none. */
	subscriptionUsd: number | null;
	/** Responses that used tokens and reported no price: their cost is unknown, not zero. */
	unpricedResponses: number;
};

export type UsageBucket = {
	responses: number;
	input: number;
	output: number;
	cacheRead: number;
	cacheWrite: number;
	totalTokens: number;
	cost: ReportCost;
	/** Work whose tokens were never reported (a subagent process that returned no usage). */
	unmeasured: number;
};

export type ModelUsage = UsageBucket & { model: string };

export type ReportCells = { count: number; failed: number; apis: Record<string, number> };

export type ModelCount = {
	model: string;
	count: number;
	/** Tokens of the ones that reported usage. */
	tokens: number;
	/** How many ended without any usage reported: their tokens are unknown, not zero. */
	unmeasured: number;
};

export type ReportWorktree = {
	task: string;
	branch: string;
	/** Files the child changed on its branch. */
	changedFiles: number;
	/** How `rlm.merge` ended (`merged`, `conflict`, `empty`, ...); null when not merged or not recorded. */
	merge: string | null;
};

export type SessionReport = {
	schema: typeof SESSION_REPORT_SCHEMA;
	session: {
		id: string;
		path: string;
		cwd: string;
		name: string | null;
		createdAt: number;
		/** The file's modification time. */
		modifiedAt: number;
		bytes: number;
		parentSessionId: string | null;
	};
	mode: SessionMode;
	/** Root turns (user requests the root ran); null when the session kept none (see `unrecorded.turns`). */
	turns: {
		count: number;
		completed: number;
		aborted: number;
		failed: number;
		running: number;
		wallMs: number;
	} | null;
	root: {
		/** Models that answered on the root lane; empty when Claude Code owns the root conversation. */
		models: Array<{ model: string; responses: number }>;
	};
	/** RLM cells; null when the session kept neither a transcript nor counters. */
	cells: {
		/** `transcript`: the rlm tool calls in the session's entries; `counters`: the runtime's own count. */
		source: "transcript" | "counters";
		total: ReportCells;
		root: ReportCells;
		subagents: ReportCells;
		other: ReportCells;
		/** Model tools other than `rlm` called on any lane (an MCP tool, Pi's native tools). */
		otherTools: Record<string, { calls: number; failed: number }>;
	} | null;
	depth: {
		/** One line: `root only`, `depth 1: 99 frames, 0 sub-agents`, `depth 2: 6 sub-agents (2 nested)`. */
		verdict: string;
		/** 0 for root only; else the deepest level of delegated work (a root's frame or subagent is level 1). */
		level: number;
		frames: {
			count: number;
			complete: number;
			incomplete: number;
			failed: number;
			cancelled: number;
			running: number;
			/** Frames started by a subagent or by another frame. */
			nested: number;
			incompleteReasons: Record<string, number>;
			byModel: ModelCount[];
			/** Tokens the frames' budgets were charged (cache reads weighted), as the frame traces record them. */
			budgetTokens: number;
			/** `rlm.infer` and `rlm.map` calls; null when the runtime counters do not cover the session. */
			calls: { infer: number; map: number } | null;
		};
		subagents: {
			count: number;
			maxDepth: number;
			/** Subagents spawned by a subagent. */
			nested: number;
			completed: number;
			failed: number;
			cancelled: number;
			interrupted: number;
			running: number;
			byModel: ModelCount[];
			/** The host's check of each ended subagent's `rlm.finish` verdict. */
			verdicts: {
				verified: number;
				contradicted: number;
				/** `invalid` + `unchecked` + `none`. */
				unverified: number;
				/** Every `rlm.finish` call was rejected, so there is no verdict. */
				invalid: number;
				/** A verdict whose declared files could not be compared. */
				unchecked: number;
				/** Ended without calling `rlm.finish`. */
				none: number;
			};
			/** What the verdicts claimed. */
			claims: { passed: number; failed: number; blocked: number };
			worktrees: ReportWorktree[];
		};
		/** `workflows.run` calls; null when the runtime counters do not cover the session. */
		workflows: { runs: number } | null;
		typedAgents: { count: number; byDefinition: Record<string, number> };
		backgroundJobs: { count: number; completed: number; failed: number; cancelled: number; running: number };
	};
	usage: {
		lanes: Record<LaneKind, UsageBucket>;
		models: ModelUsage[];
		total: UsageBucket;
	};
	guardrails: {
		/** File-write guards by name (`Loki`); null when the session kept no guard outcomes. */
		guards: Record<string, SessionStatsGuard> | null;
		secretsMasked: number | null;
		/** Hints fired, by tag. */
		hints: Record<string, number>;
		nudges: { toolRounds: number; wait: number; skill: number } | null;
		usageLimitBlocks: number | null;
		/** When the runtime counters began, if later than the session: earlier outcomes are not in them. */
		countersSince: number | null;
	};
	memory: {
		/** Memory operations by kind and state (`prepare.skipped`, `retain.completed`). */
		operations: Record<string, number> | null;
	};
	/** Why a field above is `null`, by field path. */
	unrecorded: Record<string, string>;
};

/** What a running worker knows beyond the file. */
export type LiveReportExtras = {
	/** The runtime counters as they are in memory (not yet saved). */
	stats?: SessionStatsDocument;
	/** Providers billed through a subscription login, whose cost is notional. */
	subscriptionProviders?: readonly string[];
};

/** Claude Code runs on the user's Claude login in every mode; its reported cost is always notional. */
const SUBSCRIPTION_PROVIDERS = ["claude-code"];
/** Counters that began within this long of the session's first activity cover it (clock granularity). */
const COUNTER_SLACK_MS = 1000;

function record(value: unknown): Record<string, unknown> | undefined {
	return value !== null && typeof value === "object" && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

function list(value: unknown): unknown[] {
	return Array.isArray(value) ? value : [];
}

function emptyBucket(): UsageBucket {
	return {
		responses: 0,
		input: 0,
		output: 0,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens: 0,
		cost: { reportedUsd: null, subscriptionUsd: null, unpricedResponses: 0 },
		unmeasured: 0,
	};
}

function emptyCells(): ReportCells {
	return { count: 0, failed: 0, apis: {} };
}

function addCells(target: ReportCells, source: ReportCells | SessionStatsCells): void {
	target.count += source.count;
	target.failed += source.failed;
	for (const [api, count] of Object.entries(source.apis)) target.apis[api] = (target.apis[api] ?? 0) + count;
}

/** `apis` in the report's fixed order, unknown names last. */
function orderedApis(apis: Record<string, number>): Record<string, number> {
	const known = CELL_APIS as readonly string[];
	const ordered: Record<string, number> = {};
	for (const api of known) if (apis[api] !== undefined) ordered[api] = apis[api];
	for (const api of Object.keys(apis).sort()) if (!known.includes(api)) ordered[api] = apis[api]!;
	return ordered;
}

function laneKind(lane: string | undefined): LaneKind {
	if (lane === "main") return "root";
	if (lane?.startsWith("ultron.rlm-frame.")) return "frames";
	if (lane?.startsWith("ultron.rlm-child.")) return "subagents";
	return "other";
}

function definitionName(definition: unknown): string {
	return String(definition ?? "unknown").replace(/@\d+$/, "");
}

type Task = {
	id: string;
	definition: string;
	state: string;
	parentId?: string;
	result?: Record<string, unknown>;
};

function tasksOf(log: SessionLog): Task[] {
	const tasks: Task[] = [];
	for (const item of list(record(logValue(log, "ultron.tasks", "root"))?.tasks)) {
		const task = record(item);
		if (task === undefined || typeof task.id !== "string") continue;
		tasks.push({
			id: task.id,
			definition: definitionName(task.definition),
			state: typeof task.state === "string" ? task.state : "unknown",
			...(typeof task.parentId === "string" ? { parentId: task.parentId } : {}),
			...(record(task.result) === undefined ? {} : { result: record(task.result)! }),
		});
	}
	return tasks;
}

function modelName(config: unknown): string | undefined {
	const model = record(record(config)?.model);
	return typeof model?.provider === "string" && typeof model.modelId === "string"
		? `${model.provider}/${model.modelId}`
		: undefined;
}

function providerOf(model: string): string {
	const slash = model.indexOf("/");
	return slash === -1 ? model : model.slice(0, slash);
}

class Buckets {
	readonly lanes: Record<LaneKind, UsageBucket> = {
		root: emptyBucket(),
		frames: emptyBucket(),
		subagents: emptyBucket(),
		other: emptyBucket(),
	};
	readonly models = new Map<string, UsageBucket>();
	readonly byLane = new Map<string, UsageBucket>();
	readonly total = emptyBucket();
	readonly #subscription: Set<string>;

	constructor(subscriptionProviders: readonly string[]) {
		this.#subscription = new Set([...SUBSCRIPTION_PROVIDERS, ...subscriptionProviders]);
	}

	#targets(kind: LaneKind, model: string, lane: string | undefined): UsageBucket[] {
		let byModel = this.models.get(model);
		if (byModel === undefined) {
			byModel = emptyBucket();
			this.models.set(model, byModel);
		}
		const targets = [this.lanes[kind], byModel, this.total];
		if (lane !== undefined) {
			let byLane = this.byLane.get(lane);
			if (byLane === undefined) {
				byLane = emptyBucket();
				this.byLane.set(lane, byLane);
			}
			targets.push(byLane);
		}
		return targets;
	}

	/** One model response (or one settled call) with the usage it reported. */
	add(kind: LaneKind, model: string, usage: LogUsage, lane?: string): void {
		const subscription = this.#subscription.has(providerOf(model));
		for (const bucket of this.#targets(kind, model, lane)) {
			bucket.responses += 1;
			bucket.input += usage.input;
			bucket.output += usage.output;
			bucket.cacheRead += usage.cacheRead;
			bucket.cacheWrite += usage.cacheWrite;
			bucket.totalTokens += usage.totalTokens;
			if (subscription) bucket.cost.subscriptionUsd = (bucket.cost.subscriptionUsd ?? 0) + (usage.cost ?? 0);
			else if (usage.cost !== null && usage.cost > 0)
				bucket.cost.reportedUsd = (bucket.cost.reportedUsd ?? 0) + usage.cost;
			else if (usage.totalTokens > 0) bucket.cost.unpricedResponses += 1;
		}
	}

	/** Work that reported no usage at all. */
	unmeasured(kind: LaneKind, model: string): void {
		for (const bucket of this.#targets(kind, model, undefined)) bucket.unmeasured += 1;
	}
}

function plural(count: number, one: string, many = `${one}s`): string {
	return `${count} ${count === 1 ? one : many}`;
}

/** The one-line answer to "did this session use depth or only the root?". */
export function depthVerdict(depth: {
	level: number;
	frames: number;
	subagents: number;
	nestedSubagents: number;
	typedAgents: number;
	backgroundJobs: number;
}): string {
	if (depth.level === 0) return "root only";
	const parts = [plural(depth.frames, "frame")];
	parts.push(
		`${plural(depth.subagents, "sub-agent")}${depth.nestedSubagents > 0 ? ` (${depth.nestedSubagents} nested)` : ""}`,
	);
	if (depth.typedAgents > 0) parts.push(plural(depth.typedAgents, "typed-agent task"));
	if (depth.backgroundJobs > 0) parts.push(plural(depth.backgroundJobs, "background job"));
	return `depth ${depth.level}: ${parts.join(", ")}`;
}

/** Whether `value` is a report of this schema (a worker's answer to {@link SESSION_REPORT_REQUEST}). */
export function isSessionReport(value: unknown): value is SessionReport {
	return record(value)?.schema === SESSION_REPORT_SCHEMA;
}

/** Whether nothing ran in the session: no entry, no task, and no runtime value beyond the skill catalog. */
export function sessionIsEmpty(log: SessionLog): boolean {
	if (log.entries.length > 0) return false;
	for (const address of log.values.keys())
		if (address.startsWith("ultron.") && address !== "ultron.module/skills") return false;
	return true;
}

/** Build the report from a session file's digest, and from a running worker's memory when given. */
export function buildSessionReport(log: SessionLog, live: LiveReportExtras = {}): SessionReport {
	const unrecorded: Record<string, string> = {};
	const tasks = tasksOf(log);
	const taskById = new Map(tasks.map((task) => [task.id, task]));

	// Entries belong to the lane whose branch they are on. Lanes never share a tree: each starts from no entry.
	const entryById = new Map(log.entries.map((entry) => [entry.id, entry]));
	const rootIds = new Map<string, string>();
	const treeRoot = (id: string): string => {
		const path = new Set<string>();
		let current: string | undefined = id;
		let root = id;
		while (current !== undefined) {
			const known = rootIds.get(current);
			if (known !== undefined) {
				root = known;
				break;
			}
			path.add(current);
			root = current;
			const parent: string | null | undefined = entryById.get(current)?.parentId;
			current = parent === null || parent === undefined || path.has(parent) ? undefined : parent;
		}
		for (const visited of path) rootIds.set(visited, root);
		return root;
	};
	const treeLane = new Map<string, string>();
	const claim = (entryId: unknown, lane: string): void => {
		if (typeof entryId !== "string" || !entryById.has(entryId)) return;
		const root = treeRoot(entryId);
		if (!treeLane.has(root) || lane === "main") treeLane.set(root, lane);
	};
	for (const [lane, tip] of logValues(log, "pi.branch.tip")) claim(tip, lane);
	for (const result of log.results) {
		const lane = log.operations.get(result.operationId)?.lane;
		if (lane === undefined) continue;
		claim(result.tipId, lane);
		claim(result.fromTipId, lane);
	}
	const laneOf = (entry: LogEntry): string | undefined => treeLane.get(treeRoot(entry.id));
	const laneModels = new Map<string, string>();
	for (const [lane, config] of logValues(log, "pi.lane.config")) {
		const model = modelName(config);
		if (model !== undefined) laneModels.set(lane, model);
	}

	// Mode.
	const mainEntries = log.entries.filter((entry) => laneOf(entry) === "main");
	const mainRuns = log.results.filter(
		(result) => result.kind === "run" && log.operations.get(result.operationId)?.lane === "main",
	);
	const name =
		typeof logValue(log, "pi.session.name", "") === "string"
			? (logValue(log, "pi.session.name", "") as string)
			: null;
	const usageRoots = record(record(logValue(log, "ultron.usage", "root"))?.roots) ?? {};
	const externalMarks =
		name === "claude code" ||
		name?.startsWith("claude subagent (") === true ||
		Object.keys(usageRoots).some((id) => id.startsWith("turn:cc-"));
	// When the first thing in the session happened: an entry, a run, or work admitted to the ledger.
	let firstActivity = Number.POSITIVE_INFINITY;
	const happened = (at: unknown): void => {
		if (typeof at === "number" && at > 0 && at < firstActivity) firstActivity = at;
	};
	for (const entry of log.entries) happened(entry.timestamp);
	for (const operation of log.operations.values()) happened(operation.startedAt);
	for (const root of Object.values(usageRoots)) happened(record(root)?.startedAt);
	// A session in which nothing ran has nothing to count: its counters are zero, not missing.
	const nothingRan = firstActivity === Number.POSITIVE_INFINITY && tasks.length === 0 && !externalMarks;
	const stats =
		live.stats ??
		parseSessionStats(logValue(log, "ultron.module", SESSION_STATS_MODULE)) ??
		(nothingRan ? emptySessionStats("lane", log.header.createdAt) : undefined);
	const mode: SessionMode =
		stats?.root === "external" || (stats === undefined && mainEntries.length === 0 && externalMarks)
			? "ultron claude"
			: logValues(log, "ultron.claude-code.lanes").size > 0 ||
					mainEntries.some((entry) => entry.kind === "assistant" && entry.provider === "claude-code")
				? "ultron --claude"
				: "ultron";
	const external = mode === "ultron claude";
	// The counters cover the session when they began before anything in it happened.
	const statsCoverSession = stats !== undefined && stats.since <= firstActivity + COUNTER_SLACK_MS;

	// Turns.
	let turns: SessionReport["turns"];
	if (external) {
		if (stats === undefined) {
			turns = null;
			unrecorded.turns = "Claude Code owns the root conversation; this session predates Ultron's turn count";
		} else {
			turns = {
				count: stats.externalTurns.count,
				completed: stats.externalTurns.count,
				aborted: 0,
				failed: 0,
				running: 0,
				wallMs: stats.externalTurns.wallMs,
			};
		}
	} else {
		const ended = new Set(log.results.map((result) => result.operationId));
		const running = [...log.operations.values()].filter(
			(operation) => operation.lane === "main" && operation.kind === "run" && !ended.has(operation.id),
		).length;
		turns = {
			count: mainRuns.length + running,
			completed: mainRuns.filter((run) => run.status === "completed").length,
			aborted: mainRuns.filter((run) => run.status === "aborted").length,
			failed: mainRuns.filter((run) => run.status === "failed" || run.status === "declined").length,
			running,
			wallMs: mainRuns.reduce((total, run) => total + Math.max(0, run.endedAt - run.startedAt), 0),
		};
	}

	// Usage per lane kind and model, from each response's own report.
	const buckets = new Buckets(live.subscriptionProviders ?? []);
	const rootModels = new Map<string, number>();
	const attributed = new Set<string>();
	for (const entry of log.entries) {
		if (entry.usage === undefined) continue;
		const lane = laneOf(entry);
		const kind = laneKind(lane);
		if (entry.kind === "assistant") {
			const model = `${entry.provider ?? "unknown"}/${entry.model ?? "unknown"}`;
			buckets.add(kind, model, entry.usage, lane);
			if (kind === "root") rootModels.set(model, (rootModels.get(model) ?? 0) + 1);
		} else {
			// A compaction or branch summary: a model call the lane made about itself.
			buckets.add(
				kind,
				(lane === undefined ? undefined : laneModels.get(lane)) ?? "unknown (summaries)",
				entry.usage,
				lane,
			);
		}
		attributed.add(entry.id);
	}
	for (const row of log.usageRows) {
		// A response's row repeats the usage its entry carries.
		if (row.entryId !== null && attributed.has(row.entryId)) continue;
		const entry = row.entryId === null ? undefined : entryById.get(row.entryId);
		if (entry === undefined) buckets.add("other", row.adjustment ? "imported history" : "unattributed", row.usage);
		else {
			const lane = laneOf(entry);
			buckets.add(
				laneKind(lane),
				(lane === undefined ? undefined : laneModels.get(lane)) ?? "unknown",
				row.usage,
				lane,
			);
		}
	}
	// Under `ultron claude` a subagent runs as a process of its own (Claude Code) unless it was given a lane: it has
	// no entries here, and the ledger holds what it reported.
	const childModels = new Map<string, string>();
	const ledgerCalls: Array<Record<string, unknown>> = [];
	for (const root of Object.values(usageRoots))
		ledgerCalls.push(...(list(record(root)?.calls) as Record<string, unknown>[]));
	const processChildren = new Set<string>();
	for (const task of tasks) {
		if (!external || task.definition !== "rlm-child" || laneModels.has(`ultron.rlm-child.${task.id}`)) continue;
		processChildren.add(task.id);
		const model = stats?.childModels[task.id] ?? "claude-code/(model not recorded)";
		childModels.set(task.id, model);
		const call = ledgerCalls.find((item) => item.kind === "model" && item.taskId === task.id);
		const measured = record(call?.usage);
		if (measured === undefined || typeof measured.totalTokens !== "number") {
			if (task.state !== "admitted" && task.state !== "running") buckets.unmeasured("subagents", model);
			continue;
		}
		const usage = logUsage({
			input: measured.inputTokens,
			output: measured.outputTokens,
			totalTokens: measured.totalTokens,
			...(typeof measured.cost === "number" ? { cost: { total: measured.cost } } : {}),
		});
		if (usage !== undefined) buckets.add("subagents", model, usage, `ultron.rlm-child.${task.id}`);
	}

	// Cells.
	const transcript = { root: emptyCells(), subagents: emptyCells(), other: emptyCells() };
	const otherTools: Record<string, { calls: number; failed: number }> = {};
	const otherTool = (name: string): { calls: number; failed: number } => {
		otherTools[name] ??= { calls: 0, failed: 0 };
		return otherTools[name];
	};
	let redactions = 0;
	const nudges = { toolRounds: 0, wait: 0, skill: 0 };
	const cellKind = (lane: string | undefined): "root" | "subagents" | "other" => {
		const kind = laneKind(lane);
		return kind === "frames" ? "other" : kind;
	};
	for (const entry of log.entries) {
		const kind = cellKind(laneOf(entry));
		if (entry.kind === "assistant") {
			for (const call of entry.toolCalls ?? []) {
				if (call.name === "rlm") {
					transcript[kind].count += 1;
					for (const api of call.apis ?? []) transcript[kind].apis[api] = (transcript[kind].apis[api] ?? 0) + 1;
				} else otherTool(call.name).calls += 1;
			}
		} else if (entry.kind === "toolResult") {
			redactions += entry.redactions ?? 0;
			if (entry.isError !== true) continue;
			if (entry.toolName === "rlm") transcript[kind].failed += 1;
			else if (entry.toolName !== undefined) otherTool(entry.toolName).failed += 1;
		} else if (entry.kind === "user" && entry.nudge !== undefined) nudges[entry.nudge] += 1;
	}
	let cells: SessionReport["cells"];
	const cellsFrom = (
		source: "transcript" | "counters",
		by: Record<"root" | "subagents" | "other", ReportCells | SessionStatsCells>,
	): NonNullable<SessionReport["cells"]> => {
		const total = emptyCells();
		const part = (cellsOfKind: ReportCells | SessionStatsCells): ReportCells => {
			addCells(total, cellsOfKind);
			return { count: cellsOfKind.count, failed: cellsOfKind.failed, apis: orderedApis(cellsOfKind.apis) };
		};
		const root = part(by.root);
		const subagents = part(by.subagents);
		const other = part(by.other);
		return { source, total: { ...total, apis: orderedApis(total.apis) }, root, subagents, other, otherTools };
	};
	if (!external) cells = cellsFrom("transcript", transcript);
	else if (stats !== undefined) cells = cellsFrom("counters", stats.cells);
	else {
		cells = null;
		unrecorded.cells = "Claude Code owns the transcript; this session predates Ultron's cell count";
	}

	// Depth: subagents.
	const children = tasks.filter((task) => task.definition === "rlm-child");
	const childDepth = (task: Task): number => {
		let depth = 0;
		const seen = new Set<string>();
		for (
			let current: Task | undefined = task;
			current && !seen.has(current.id);
			current = current.parentId === undefined ? undefined : taskById.get(current.parentId)
		) {
			seen.add(current.id);
			if (current.definition === "rlm-child") depth += 1;
		}
		return depth;
	};
	/** Levels of delegation above a task's own work: its subagent ancestors, itself included. */
	const levelOfTask = (id: string | undefined): number => {
		const task = id === undefined ? undefined : taskById.get(id);
		return task === undefined ? 0 : childDepth(task);
	};
	const state = (items: Task[], ...states: string[]): number =>
		items.filter((task) => states.includes(task.state)).length;
	const verdicts = { verified: 0, contradicted: 0, unverified: 0, invalid: 0, unchecked: 0, none: 0 };
	const claims = { passed: 0, failed: 0, blocked: 0 };
	const worktrees: ReportWorktree[] = [];
	const childByModel = new Map<string, { count: number; tokens: number; unmeasured: number }>();
	for (const child of children) {
		const lane = `ultron.rlm-child.${child.id}`;
		const model = laneModels.get(lane) ?? childModels.get(child.id) ?? stats?.childModels[child.id] ?? "not recorded";
		const tokens = buckets.byLane.get(lane)?.totalTokens;
		const byModel = childByModel.get(model) ?? { count: 0, tokens: 0, unmeasured: 0 };
		byModel.count += 1;
		byModel.tokens += tokens ?? 0;
		// A process child that ended with no usage in the ledger: its tokens are unknown, not zero.
		if (tokens === undefined && processChildren.has(child.id) && child.result !== undefined) byModel.unmeasured += 1;
		childByModel.set(model, byModel);
		const result = child.result;
		if (result === undefined) continue;
		const verdict = record(result.verdict);
		const outcome = record(result.check)?.outcome;
		// Without a verdict the check says whether the child's `rlm.finish` calls were all rejected (`invalid`).
		if (verdict === undefined) verdicts[outcome === "invalid" ? "invalid" : "none"] += 1;
		else if (outcome === "verified") verdicts.verified += 1;
		else if (outcome === "contradicted") verdicts.contradicted += 1;
		else verdicts.unchecked += 1;
		if (verdict?.status === "passed" || verdict?.status === "failed" || verdict?.status === "blocked")
			claims[verdict.status] += 1;
		const worktree = record(result.worktree);
		if (typeof worktree?.branch === "string")
			worktrees.push({
				task: child.id,
				branch: worktree.branch,
				changedFiles: list(worktree.changed_files).length,
				merge: stats?.merges[child.id] ?? (worktree.commit === null ? "empty" : null),
			});
	}
	verdicts.unverified = verdicts.invalid + verdicts.unchecked + verdicts.none;
	const depths = children.map(childDepth);
	const maxChildDepth = depths.reduce((max, depth) => Math.max(max, depth), 0);

	// Depth: frames, from their traces; a frame task without a trace (cancelled before it began) counts by its state.
	const traces = [...logValues(log, "ultron.rlm.frames")]
		.filter(([key]) => key !== "index")
		.map(([, value]) => record(value));
	const frameTasks = tasks.filter((task) => task.definition === "rlm-frame");
	const traced = new Set<string>();
	const frames = {
		count: 0,
		complete: 0,
		incomplete: 0,
		failed: 0,
		cancelled: 0,
		running: 0,
		nested: 0,
		incompleteReasons: {} as Record<string, number>,
		budgetTokens: 0,
	};
	const frameByModel = new Map<string, { count: number; tokens: number; unmeasured: number }>();
	const frameLevels = new Map<string, number>();
	const traceById = new Map<string, Record<string, unknown>>();
	for (const trace of traces) if (typeof trace?.id === "string") traceById.set(trace.id, trace);
	const frameLevel = (trace: Record<string, unknown>, seen = new Set<string>()): number => {
		const id = String(trace.id);
		const known = frameLevels.get(id);
		if (known !== undefined) return known;
		seen.add(id);
		const parent =
			typeof trace.parentFrame === "string" && !seen.has(trace.parentFrame)
				? traceById.get(trace.parentFrame)
				: undefined;
		const level =
			parent !== undefined
				? frameLevel(parent, seen) + 1
				: levelOfTask(typeof trace.callerTaskId === "string" ? trace.callerTaskId : undefined) + 1;
		frameLevels.set(id, level);
		return level;
	};
	let maxFrameLevel = 0;
	const countFrame = (model: string, lane: string | undefined): void => {
		const tokens = lane === undefined ? undefined : buckets.byLane.get(lane)?.totalTokens;
		const byModel = frameByModel.get(model) ?? { count: 0, tokens: 0, unmeasured: 0 };
		byModel.count += 1;
		byModel.tokens += tokens ?? 0;
		frameByModel.set(model, byModel);
	};
	for (const trace of traceById.values()) {
		frames.count += 1;
		if (typeof trace.taskId === "string") traced.add(trace.taskId);
		const status = trace.status;
		if (status === "complete") frames.complete += 1;
		else if (status === "incomplete") {
			frames.incomplete += 1;
			const reason = typeof trace.reason === "string" ? trace.reason : "unknown";
			frames.incompleteReasons[reason] = (frames.incompleteReasons[reason] ?? 0) + 1;
		} else if (status === "error") frames.failed += 1;
		else frames.running += 1;
		const spent = record(trace.spent)?.tokens;
		if (typeof spent === "number" && Number.isFinite(spent)) frames.budgetTokens += spent;
		const level = frameLevel(trace);
		if (level > 1) frames.nested += 1;
		maxFrameLevel = Math.max(maxFrameLevel, level);
		const lane =
			typeof trace.lane === "string"
				? trace.lane
				: typeof trace.taskId === "string"
					? `ultron.rlm-frame.${trace.taskId}`
					: undefined;
		const model =
			(typeof trace.model === "string" ? trace.model : undefined) ??
			(lane === undefined ? undefined : laneModels.get(lane)) ??
			"not recorded";
		countFrame(model, lane);
	}
	for (const task of frameTasks) {
		if (traced.has(task.id)) continue;
		frames.count += 1;
		if (task.state === "completed") frames.complete += 1;
		else if (task.state === "cancelled" || task.state === "interrupted") frames.cancelled += 1;
		else if (task.state === "failed") frames.failed += 1;
		else frames.running += 1;
		const level = levelOfTask(task.parentId) + 1;
		if (level > 1) frames.nested += 1;
		maxFrameLevel = Math.max(maxFrameLevel, level);
		const lane = `ultron.rlm-frame.${task.id}`;
		countFrame(laneModels.get(lane) ?? "not recorded", lane);
	}

	const typed = tasks.filter((task) => !["rlm-child", "rlm-frame", "background-job"].includes(task.definition));
	const byDefinition: Record<string, number> = {};
	for (const task of typed) byDefinition[task.definition] = (byDefinition[task.definition] ?? 0) + 1;
	const background = tasks.filter((task) => task.definition === "background-job");
	const otherLevel = [...typed, ...background].reduce((max, task) => Math.max(max, levelOfTask(task.parentId) + 1), 0);
	const level = Math.max(maxChildDepth, maxFrameLevel, otherLevel);
	const nestedChildren = depths.filter((depth) => depth > 1).length;
	if (!statsCoverSession) {
		const reason =
			stats === undefined
				? "this session predates Ultron's runtime counters"
				: "the runtime counters began after this session did";
		unrecorded["depth.workflows"] = reason;
		unrecorded["depth.frames.calls"] = reason;
	}
	const byModelList = (models: Map<string, { count: number; tokens: number; unmeasured: number }>): ModelCount[] =>
		[...models]
			.map(([model, value]) => ({ model, ...value }))
			.sort((left, right) => right.count - left.count || left.model.localeCompare(right.model));

	// Guardrails.
	const hints: Record<string, number> = {};
	for (const lane of Object.values(record(record(logValue(log, "ultron.module", "hints"))?.lanes) ?? {})) {
		for (const [tag, fired] of Object.entries(record(record(lane)?.fired) ?? {}))
			if (typeof fired === "number" && fired > 0) hints[tag] = (hints[tag] ?? 0) + fired;
	}
	if (stats === undefined) {
		unrecorded["guardrails.guards"] = "kept only in the worker's memory when this session ran";
		unrecorded["guardrails.usageLimitBlocks"] = "this session predates the count";
	}
	let secretsMasked: number | null;
	let reportedNudges: SessionReport["guardrails"]["nudges"];
	if (!external) {
		secretsMasked = redactions;
		reportedNudges = nudges;
	} else if (stats !== undefined) {
		secretsMasked = stats.secretsMasked;
		reportedNudges = { ...stats.nudges };
	} else {
		secretsMasked = null;
		reportedNudges = null;
		unrecorded["guardrails.secretsMasked"] = "cell output went to Claude Code; this session predates the count";
		unrecorded["guardrails.nudges"] =
			"steers went to Claude Code with a cell result; this session predates the count";
	}

	// Memory.
	const memoryOperations = list(record(logValue(log, "ultron.memory.state", "root"))?.operations).map(record);
	let operations: Record<string, number> | null = null;
	if (memoryOperations.length > 0) {
		operations = {};
		for (const operation of memoryOperations) {
			const key = `${String(operation?.kind ?? "unknown")}.${String(operation?.state ?? "unknown")}`;
			operations[key] = (operations[key] ?? 0) + 1;
		}
	} else unrecorded["memory.operations"] = "no memory operation recorded: Hindsight not configured, or memory unused";

	if (external)
		unrecorded["root.models"] =
			"Claude Code owns the root conversation: its model, tokens and cost are Claude Code's";
	const depthSummary = {
		level,
		frames: frames.count,
		subagents: children.length,
		nestedSubagents: nestedChildren,
		typedAgents: typed.length,
		backgroundJobs: background.length,
	};
	return {
		schema: SESSION_REPORT_SCHEMA,
		session: {
			id: log.header.id,
			path: log.path,
			cwd: log.header.cwd,
			name,
			createdAt: log.header.createdAt,
			modifiedAt: log.modifiedAt,
			bytes: log.bytes,
			parentSessionId: log.header.parentSessionId ?? null,
		},
		mode,
		turns,
		root: {
			models: [...rootModels]
				.map(([model, responses]) => ({ model, responses }))
				.sort((left, right) => right.responses - left.responses || left.model.localeCompare(right.model)),
		},
		cells,
		depth: {
			verdict: depthVerdict(depthSummary),
			level,
			frames: {
				...frames,
				byModel: byModelList(frameByModel),
				calls: statsCoverSession
					? { infer: stats?.hostCalls["rlm.infer"] ?? 0, map: stats?.hostCalls["rlm.map"] ?? 0 }
					: null,
			},
			subagents: {
				count: children.length,
				maxDepth: maxChildDepth,
				nested: nestedChildren,
				completed: state(children, "completed"),
				failed: state(children, "failed"),
				cancelled: state(children, "cancelled"),
				interrupted: state(children, "interrupted"),
				running: state(children, "admitted", "running"),
				byModel: byModelList(childByModel),
				verdicts,
				claims,
				worktrees,
			},
			workflows: statsCoverSession ? { runs: stats?.hostCalls["workflows.run"] ?? 0 } : null,
			typedAgents: { count: typed.length, byDefinition },
			backgroundJobs: {
				count: background.length,
				completed: state(background, "completed"),
				failed: state(background, "failed"),
				cancelled: state(background, "cancelled", "interrupted"),
				running: state(background, "admitted", "running"),
			},
		},
		usage: {
			lanes: buckets.lanes,
			models: [...buckets.models]
				.map(([model, bucket]) => ({ model, ...bucket }))
				.sort((left, right) => right.totalTokens - left.totalTokens || left.model.localeCompare(right.model)),
			total: buckets.total,
		},
		guardrails: {
			guards: stats === undefined ? null : stats.guards,
			secretsMasked,
			hints,
			nudges: reportedNudges,
			usageLimitBlocks: stats === undefined ? null : stats.usageLimitBlocks,
			countersSince: stats !== undefined && !statsCoverSession ? stats.since : null,
		},
		memory: { operations },
		unrecorded,
	};
}
