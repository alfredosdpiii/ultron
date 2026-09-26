/**
 * The RLM run as a live graph: the root turn, its `rlm` cells, and everything the cells started (typed agent tasks,
 * `rlm.spawn` children, `rlm.infer` frames, `rlm.map` fan-outs, workflows with joins, shell jobs), with status glyphs,
 * elapsed time, spend, budget gauges and a kernel-pool strip.
 *
 * Pure and width-bounded: `buildRlmGraph` turns an inspection snapshot into nodes, `layoutGraph` flattens them with
 * box-drawing connectors (finished subtrees collapse automatically), and the renderers produce the three levels the
 * TUI shows: a one-line footer summary, a docked panel, and the full-screen focus view (see rlm-focus.ts).
 */
import { truncateToWidth, visibleWidth } from "@ultron/tui";
import {
	formatDuration,
	PLAIN_STYLE,
	type RlmFrame,
	type RlmJob,
	type RlmRootCell,
	type RlmSnapshot,
	type RlmStyle,
	type RlmTask,
	renderContextLines,
	shortId,
} from "./rlm-visualizer.ts";

export type GraphStatus = "running" | "pending" | "done" | "failed" | "cancelled" | "incomplete";
export type GraphKind =
	| "turn"
	| "cell"
	| "task"
	| "child"
	| "background"
	| "infer"
	| "fanout"
	| "job"
	| "workflow"
	| "earlier";

export interface GraphProgress {
	readonly total: number;
	readonly done: number;
	readonly running: number;
	readonly pending: number;
	readonly incomplete: number;
	readonly failed: number;
}

export interface GraphNode {
	/** Stable across refreshes (selection and collapse state are keyed by it). */
	readonly key: string;
	readonly kind: GraphKind;
	readonly label: string;
	readonly status: GraphStatus;
	readonly startedAt?: number;
	readonly endedAt?: number;
	readonly tokens?: number;
	readonly cost?: number;
	/** Fan-out progress (`rlm.map`) or workflow node progress. */
	readonly progress?: GraphProgress;
	/** Workflow join marker, e.g. "⇐ plan+code (all)". */
	readonly join?: string;
	/** One-line result or error summary. */
	readonly note?: string;
	readonly children: GraphNode[];
	/** Label/value lines shown when the node's details are expanded. */
	readonly details: readonly (readonly [string, string])[];
	/** Frame trace id to fetch lazily for details (`rlm.frames {id}`). */
	readonly traceId?: string;
}

const TERMINAL: ReadonlySet<GraphStatus> = new Set(["done", "failed", "cancelled", "incomplete"]);

export function isFinished(status: GraphStatus): boolean {
	return TERMINAL.has(status);
}

export function taskStatus(state: string): GraphStatus {
	switch (state) {
		case "running":
			return "running";
		case "admitted":
			return "pending";
		case "completed":
			return "done";
		case "cancelled":
			return "cancelled";
		case "interrupted":
			return "incomplete";
		default:
			return "failed";
	}
}

function frameStatus(frame: RlmFrame): GraphStatus {
	switch (frame.status) {
		case "running":
			return frame.taskId === null ? "pending" : "running";
		case "complete":
			return "done";
		case "incomplete":
			return "incomplete";
		default:
			return "failed";
	}
}

function jobStatus(job: RlmJob): GraphStatus {
	if (job.status === "running") return "running";
	if (job.status === "completed") return job.exitCode === 0 ? "done" : "failed";
	if (job.status === "cancelled") return "cancelled";
	if (job.status === "interrupted" || job.status === "timed_out") return "incomplete";
	return "failed";
}

function flat(text: string, limit = 200): string {
	const line = text.replace(/\s+/g, " ").trim();
	return line.length > limit ? `${line.slice(0, limit - 1)}…` : line;
}

function safeJson(value: unknown): string {
	try {
		return JSON.stringify(value) ?? String(value);
	} catch {
		return String(value);
	}
}

function firstCodeLine(code: string): string {
	const line = code
		.split("\n")
		.map((item) => item.trim())
		.find((item) => item.length > 0 && !item.startsWith("#"));
	return line ?? "";
}

/** Combine children statuses into a parent status (running wins, then pending, failures, incompletes). */
function rollup(counts: GraphProgress): GraphStatus {
	if (counts.running > 0) return "running";
	if (counts.pending > 0) return counts.done + counts.failed + counts.incomplete > 0 ? "running" : "pending";
	if (counts.failed > 0 && counts.done === 0 && counts.incomplete === 0) return "failed";
	if (counts.failed + counts.incomplete > 0) return "incomplete";
	return "done";
}

function count(statuses: readonly GraphStatus[], total = statuses.length): GraphProgress {
	const progress = { total, done: 0, running: 0, pending: 0, incomplete: 0, failed: 0 };
	for (const status of statuses) {
		if (status === "done") progress.done++;
		else if (status === "running") progress.running++;
		else if (status === "pending") progress.pending++;
		else if (status === "incomplete") progress.incomplete++;
		else progress.failed++;
	}
	// Frames of a map beyond the listing window are still queued or unseen.
	progress.pending += Math.max(0, total - statuses.length);
	return progress;
}

function minDefined(values: readonly (number | undefined)[]): number | undefined {
	const defined = values.filter((value): value is number => value !== undefined);
	return defined.length === 0 ? undefined : Math.min(...defined);
}

function maxDefined(values: readonly (number | undefined)[]): number | undefined {
	const defined = values.filter((value): value is number => value !== undefined);
	return defined.length === 0 ? undefined : Math.max(...defined);
}

function sum(values: readonly (number | undefined)[]): number | undefined {
	const defined = values.filter((value): value is number => value !== undefined);
	return defined.length === 0 ? undefined : defined.reduce((total, value) => total + value, 0);
}

interface Mutable {
	node: GraphNode;
	/** Where it attaches: a task id, a fan-out key, or a root-lane time for cell matching. */
	parentTask?: string;
	at?: number;
}

function taskKind(task: RlmTask): GraphKind {
	if (task.definition === "rlm-child@1") return "child";
	if (task.definition === "background-job@1") return "background";
	return "task";
}

function taskLabel(task: RlmTask): string {
	const kind = taskKind(task);
	const id = shortId(task.id);
	if (kind === "child") return `rlm.spawn child ${id}`;
	if (kind === "background") return `background ${id}`;
	return `${task.workflow ? task.workflow.node : "agent"} ${task.definition} ${id}`;
}

function taskNote(task: RlmTask): string | undefined {
	const result = task.result;
	if (result === undefined) return undefined;
	if (typeof result.error === "string" && result.error.length > 0) return flat(result.error, 120);
	if (typeof result.preview === "string" && result.preview.length > 0) return `→ ${flat(result.preview, 120)}`;
	if (result.value === undefined || result.value === null) return undefined;
	const text = typeof result.value === "string" ? result.value : safeJson(result.value);
	return `→ ${flat(text, 120)}`;
}

function fetchHint(task: RlmTask): string {
	if (task.fetch) return task.fetch;
	if (task.definition === "rlm-child@1") return `await rlm.collect(["${task.id}"])`;
	if (task.definition === "background-job@1") return `await background.result("${task.id}")`;
	return `await agents.result("${task.id}")`;
}

function taskNode(task: RlmTask, snapshot: RlmSnapshot): GraphNode {
	const timing = snapshot.timing?.get(task.id);
	const startedAt = task.startedAt ?? timing?.startedAt;
	const endedAt = task.endedAt ?? timing?.endedAt;
	const details: [string, string][] = [
		["id", task.id],
		["definition", task.definition],
		["state", task.state],
	];
	if (task.lane) details.push(["lane", task.lane]);
	if (task.parentId) details.push(["parent", task.parentId]);
	if (task.workflow)
		details.push([
			"workflow",
			`${task.workflow.run} node ${task.workflow.node}${task.workflow.dependsOn.length > 0 ? ` after ${task.workflow.dependsOn.join(", ")} (${task.workflow.join})` : ""}`,
		]);
	if (task.input) details.push(["input", task.input]);
	const result = task.result;
	if (result?.error) details.push(["error", flat(result.error, 400)]);
	else if (result?.preview) details.push(["result", result.preview]);
	else if (result?.value !== undefined && result.value !== null)
		details.push(["result", flat(typeof result.value === "string" ? result.value : safeJson(result.value), 400)]);
	if (snapshot.retained?.has(task.id)) details.push(["retained", "open instance keeps this lane's kernel"]);
	const progress = snapshot.progress?.get(task.id);
	if (progress) details.push(["progress", `${progress.classification} · ${progress.receipts} receipts`]);
	details.push(["fetch", fetchHint(task)]);
	const note = taskNote(task);
	return {
		key: `task:${task.id}`,
		kind: taskKind(task),
		label: taskLabel(task),
		status: taskStatus(task.state),
		...(startedAt === undefined ? {} : { startedAt }),
		...(endedAt === undefined ? {} : { endedAt }),
		...(task.tokens === undefined ? {} : { tokens: task.tokens }),
		...(task.cost === undefined ? {} : { cost: task.cost }),
		...(note === undefined ? {} : { note }),
		...(task.workflow && task.workflow.dependsOn.length > 0
			? {
					join: `⇐ ${task.workflow.dependsOn.join("+")}${task.workflow.dependsOn.length > 1 ? ` (${task.workflow.join})` : ""}`,
				}
			: {}),
		children: [],
		details,
	};
}

function frameGroupNode(key: string, frames: readonly RlmFrame[]): GraphNode {
	const first = frames[0]!;
	const batch = Math.max(first.batch ?? frames.length, frames.length);
	const isMap = first.kind === "map" || batch > 1;
	const statuses = frames.map(frameStatus);
	const progress = count(statuses, batch);
	const status = rollup(progress);
	const calls = sum(frames.map((frame) => frame.spent.calls)) ?? 0;
	const tokens = sum(frames.map((frame) => frame.spent.tokens)) ?? 0;
	const startedAt = minDefined(frames.map((frame) => frame.startedAt));
	const endedAt = isFinished(status) ? maxDefined(frames.map((frame) => frame.endedAt)) : undefined;
	const budget = first.budget ?? null;
	const details: [string, string][] = [];
	if (isMap)
		details.push([
			"frames",
			`${batch} (${progress.done} done · ${progress.running} running · ${progress.pending} queued · ${progress.incomplete} incomplete · ${progress.failed} failed)`,
		]);
	details.push(["task", flat(first.task, 300)]);
	if (budget)
		details.push([
			"budget",
			`${budget.id.replace(/^budget-/, "").slice(0, 8)} · calls ${calls}/${budget.calls ?? "∞"} · tokens ${tokens}/${budget.tokens ?? "∞"} · depth ${budget.depth}`,
		]);
	else details.push(["spent", `${calls} calls · ${tokens} tokens`]);
	const troubled = frames.filter((frame) => frame.status === "incomplete" || frame.status === "error").slice(0, 3);
	for (const frame of troubled) details.push([frame.status, `${frame.id}${frame.reason ? ` (${frame.reason})` : ""}`]);
	if (isMap) {
		details.push(["traces", "await rlm.frames(limit=200)"]);
		if (troubled[0]) details.push(["fetch", `await rlm.frames("${troubled[0].id}")`]);
	} else {
		details.push(["trace", first.id]);
		if (first.reason) details.push(["reason", first.reason]);
		details.push(["fetch", `await rlm.frames("${first.id}")`]);
	}
	const reasons = [...new Set(frames.map((frame) => frame.reason).filter((reason) => reason !== undefined))];
	return {
		key: `frames:${key}`,
		kind: isMap ? "fanout" : "infer",
		label: isMap
			? `rlm.map ${batch} frame${batch === 1 ? "" : "s"}`
			: `rlm.infer ${first.id.replace(/^frame-/, "").slice(0, 8)}`,
		status,
		...(startedAt === undefined ? {} : { startedAt }),
		...(endedAt === undefined ? {} : { endedAt }),
		tokens,
		...(isMap ? { progress } : {}),
		...(isMap
			? { note: flat(first.task, 80) }
			: { note: reasons.length > 0 ? reasons.join(", ") : flat(first.task, 80) }),
		children: [],
		details,
		...(isMap ? {} : { traceId: first.id }),
	};
}

function jobNode(job: RlmJob, now: number): GraphNode {
	const status = jobStatus(job);
	const details: [string, string][] = [
		["id", job.id],
		["command", flat(job.command, 400)],
		["status", job.status === "completed" ? `exit ${job.exitCode}` : job.status],
	];
	if (job.lane) details.push(["lane", job.lane]);
	if (job.tail) details.push(["output", flat(job.tail, 300)]);
	details.push(["fetch", `await (await rlm.job("${job.id}")).read()`]);
	const endedAt = job.endedAt ?? undefined;
	return {
		key: `job:${job.id}`,
		kind: "job",
		label: `bash ${flat(job.command, 60)}`,
		status,
		...(job.startedAt === undefined ? {} : { startedAt: job.startedAt }),
		...(endedAt === undefined ? (status === "running" ? {} : { endedAt: now }) : { endedAt }),
		...(job.status === "completed" && job.exitCode !== 0 ? { note: `exit ${job.exitCode}` } : {}),
		...(job.status !== "completed" && job.status !== "running" ? { note: job.status } : {}),
		children: [],
		details,
	};
}

function cellNode(cell: RlmRootCell, index: number): GraphNode {
	const status: GraphStatus = cell.status === "running" ? "running" : cell.status === "ok" ? "done" : "failed";
	const code = cell.code
		.split("\n")
		.map((line) => line.replace(/\t/g, "  ").trimEnd())
		.filter((line) => line.trim().length > 0);
	const details: [string, string][] = [["call", cell.toolCallId]];
	for (const line of code.slice(0, 6)) details.push(["code", line]);
	if (code.length > 6) details.push(["code", `… ${code.length - 6} more lines`]);
	if (cell.output) details.push([cell.status === "error" ? "error" : "output", flat(cell.output, 400)]);
	return {
		key: `cell:${cell.toolCallId}`,
		kind: "cell",
		label: `cell ${index + 1}  ${flat(firstCodeLine(cell.code), 100)}`,
		status,
		...(cell.startedAt === undefined ? {} : { startedAt: cell.startedAt }),
		...(cell.endedAt === undefined ? {} : { endedAt: cell.endedAt }),
		...(cell.status === "error" && cell.output ? { note: flat(cell.output.split("\n").at(-1) ?? "", 100) } : {}),
		children: [],
		details,
	};
}

function sortChronological(nodes: GraphNode[]): void {
	nodes.sort(
		(left, right) => (left.startedAt ?? Number.MAX_SAFE_INTEGER) - (right.startedAt ?? Number.MAX_SAFE_INTEGER),
	);
	for (const node of nodes) sortChronological(node.children);
}

/** Rebuild a node with a new child list and derived status (for workflows and the turn). */
function withChildren(node: GraphNode, children: GraphNode[]): GraphNode {
	return { ...node, children };
}

/**
 * Build the run graph. Children attach by parent task id (tasks), caller task (frames), owning lane (jobs), or, for
 * root-lane work, to the cell running when it started. Finished work from before this turn goes under one
 * "earlier" node; `rlm-frame@1` tasks are shown through their frames, not as tasks.
 */
export function buildRlmGraph(snapshot: RlmSnapshot): GraphNode {
	const now = snapshot.now;
	const turnStart = snapshot.turn?.startedAt;
	const frameTaskIds = new Set<string>();
	for (const frame of snapshot.frames ?? []) if (typeof frame.taskId === "string") frameTaskIds.add(frame.taskId);

	// Frame groups: one node per `rlm.map`/`rlm.infer` call (shared budget node).
	const groups = new Map<string, RlmFrame[]>();
	for (const frame of snapshot.frames ?? []) {
		const key = frame.budget?.id ?? frame.id;
		const list = groups.get(key) ?? [];
		list.push(frame);
		groups.set(key, list);
	}
	const items: Mutable[] = [];
	/** Frame task id -> its group's node key (nested frames attach to the group of the frame that called them). */
	const frameGroupOf = new Map<string, string>();
	for (const [key, frames] of groups) {
		const node = frameGroupNode(key, frames);
		for (const frame of frames) if (typeof frame.taskId === "string") frameGroupOf.set(frame.taskId, node.key);
		const caller = frames[0]!.callerTaskId;
		items.push({
			node,
			...(caller ? { parentTask: caller } : {}),
			...(node.startedAt === undefined ? {} : { at: node.startedAt }),
		});
	}

	// Frame tasks the frame listing does not cover (older worker, trimmed index) fan out per parent.
	const orphanFrames = new Map<string, RlmTask[]>();
	const tasks: RlmTask[] = [];
	for (const task of snapshot.tasks) {
		if (task.definition === "rlm-frame@1") {
			if (!frameTaskIds.has(task.id)) {
				const parent = task.parentId ?? "";
				orphanFrames.set(parent, [...(orphanFrames.get(parent) ?? []), task]);
			}
			continue;
		}
		tasks.push(task);
	}
	for (const [parent, list] of orphanFrames) {
		const statuses = list.map((task) => taskStatus(task.state));
		const progress = count(statuses);
		const startedAt = minDefined(list.map((task) => task.startedAt ?? snapshot.timing?.get(task.id)?.startedAt));
		const node: GraphNode = {
			key: `frametasks:${parent || "root"}`,
			kind: "fanout",
			label: `frames ${list.length}`,
			status: rollup(progress),
			...(startedAt === undefined ? {} : { startedAt }),
			progress,
			children: [],
			details: [
				["frames", `${list.length} rlm-frame tasks`],
				["traces", "await rlm.frames()"],
			],
		};
		items.push({
			node,
			...(parent ? { parentTask: parent } : {}),
			...(startedAt === undefined ? {} : { at: startedAt }),
		});
	}

	// Tasks, with workflow members grouped under one workflow node per run.
	const taskNodes = new Map<string, GraphNode>();
	const workflows = new Map<string, { tasks: RlmTask[]; nodes: GraphNode[] }>();
	for (const task of tasks) {
		const node = taskNode(task, snapshot);
		taskNodes.set(task.id, node);
		if (task.workflow) {
			const group = workflows.get(task.workflow.run) ?? { tasks: [], nodes: [] };
			group.tasks.push(task);
			group.nodes.push(node);
			workflows.set(task.workflow.run, group);
			continue;
		}
		items.push({
			node,
			...(task.parentId ? { parentTask: task.parentId } : {}),
			...(node.startedAt === undefined ? {} : { at: node.startedAt }),
		});
	}
	for (const [run, group] of workflows) {
		const statuses = group.nodes.map((node) => node.status);
		const progress = count(statuses);
		const startedAt = minDefined(group.nodes.map((node) => node.startedAt));
		const status = rollup(progress);
		const endedAt = isFinished(status) ? maxDefined(group.nodes.map((node) => node.endedAt)) : undefined;
		const joins = group.tasks.filter((task) => (task.workflow?.dependsOn.length ?? 0) > 1).length;
		const node: GraphNode = {
			key: `workflow:${run}`,
			kind: "workflow",
			label: `workflow ${run.replace(/^wf-/, "")} ${group.nodes.length} nodes`,
			status,
			...(startedAt === undefined ? {} : { startedAt }),
			...(endedAt === undefined ? {} : { endedAt }),
			tokens: sum(group.nodes.map((item) => item.tokens)),
			cost: sum(group.nodes.map((item) => item.cost)),
			progress,
			...(joins > 0 ? { note: `${joins} join${joins === 1 ? "" : "s"}` } : {}),
			children: group.nodes,
			details: [
				["run", run],
				["nodes", group.tasks.map((task) => task.workflow!.node).join(", ")],
				[
					"joins",
					group.tasks
						.filter((task) => (task.workflow?.dependsOn.length ?? 0) > 1)
						.map(
							(task) =>
								`${task.workflow!.node} ⇐ ${task.workflow!.dependsOn.join("+")} (${task.workflow!.join})`,
						)
						.join("; ") || "none",
				],
				["fetch", "the workflows.run(...) call returns every node's outcome"],
			],
		};
		const parent = group.tasks[0]!.parentId;
		items.push({
			node,
			...(parent ? { parentTask: parent } : {}),
			...(startedAt === undefined ? {} : { at: startedAt }),
		});
	}

	// Jobs belong to the lane that started them: the root ("main") or a child task's lane.
	const laneTask = new Map<string, string>();
	for (const task of tasks) if (task.lane) laneTask.set(task.lane, task.id);
	for (const job of snapshot.jobs ?? []) {
		const node = jobNode(job, now);
		const owner = job.lane && job.lane !== "main" ? laneTask.get(job.lane) : undefined;
		items.push({
			node,
			...(owner ? { parentTask: owner } : {}),
			...(job.startedAt === undefined ? {} : { at: job.startedAt }),
		});
	}

	// A parent chain that loops (a corrupt journal) is cut: its tasks attach as if their parent were unknown.
	const parentOf = new Map(tasks.map((task) => [task.id, task.parentId]));
	const inCycle = (id: string): boolean => {
		const seen = new Set<string>();
		for (let current: string | undefined = id; current !== undefined; current = parentOf.get(current)) {
			if (seen.has(current)) return true;
			seen.add(current);
		}
		return false;
	};
	for (const item of items)
		if (item.node.key.startsWith("task:") && item.parentTask !== undefined && inCycle(item.node.key.slice(5)))
			delete item.parentTask;

	// Attach: by parent task (or the frame group of a nested frame's task), else to a root cell by time.
	const cells = (
		snapshot.cells && snapshot.cells.length > 0 ? snapshot.cells : snapshot.rootCell ? [snapshot.rootCell] : []
	).map((cell, index) => ({ cell, node: cellNode(cell, index) }));
	const byKey = new Map<string, GraphNode>();
	for (const item of items) byKey.set(item.node.key, item.node);
	const turnChildren: GraphNode[] = [];
	const earlier: GraphNode[] = [];
	for (const item of items) {
		const parentTask = item.parentTask;
		if (parentTask !== undefined) {
			const parent = taskNodes.get(parentTask) ?? byKey.get(frameGroupOf.get(parentTask) ?? "");
			if (parent !== undefined && parent !== item.node) {
				parent.children.push(item.node);
				continue;
			}
		}
		const at = item.at;
		const cell =
			at === undefined
				? undefined
				: cells.find(
						({ cell }) =>
							cell.startedAt !== undefined && at >= cell.startedAt - 250 && at <= (cell.endedAt ?? now) + 250,
					);
		if (cell) {
			cell.node.children.push(item.node);
			continue;
		}
		const current = turnStart === undefined || (at !== undefined && at >= turnStart - 1000);
		if (current || !isFinished(item.node.status)) turnChildren.push(item.node);
		else earlier.push(item.node);
	}

	const children: GraphNode[] = [];
	if (earlier.length > 0) {
		sortChronological(earlier);
		children.push({
			key: "earlier",
			kind: "earlier",
			label: `earlier turns · ${earlier.length} finished`,
			status: rollup(count(earlier.map((node) => node.status))),
			children: earlier,
			details: [["items", `${earlier.length} nodes started before this turn`]],
		});
	}
	for (const { node } of cells) children.push(node);
	sortChronological(turnChildren);
	children.push(...turnChildren);
	for (const { node } of cells) sortChronological(node.children);

	const statuses = [...cells.map(({ node }) => node.status), ...turnChildren.map((node) => node.status)];
	const active = statuses.some((status) => status === "running" || status === "pending");
	const usage = snapshot.usage;
	const turnStartedAt = turnStart ?? usage?.startedAt ?? undefined;
	const lastEnd = maxDefined([...cells.map(({ node }) => node.endedAt), ...turnChildren.map((node) => node.endedAt)]);
	const tokens = usage?.turns?.tokens || usage?.usage?.totalTokens || undefined;
	const cost = usage?.cost?.spentUsd ?? usage?.usage?.cost ?? undefined;
	const failed = statuses.some((status) => status === "failed");
	const details: [string, string][] = [];
	if (snapshot.turn?.prompt) details.push(["prompt", flat(snapshot.turn.prompt, 300)]);
	if (usage?.rootId) details.push(["root", usage.rootId]);
	details.push(["cells", String(cells.length)]);
	if (snapshot.truncatedTasks) details.push(["listing", `${snapshot.truncatedTasks} older tasks not listed`]);
	return withChildren(
		{
			key: "turn",
			kind: "turn",
			label: snapshot.turn?.prompt ? `turn  “${flat(snapshot.turn.prompt, 60)}”` : "turn",
			status: active ? "running" : failed ? "failed" : statuses.length === 0 ? "pending" : "done",
			...(turnStartedAt == null ? {} : { startedAt: turnStartedAt }),
			...(active || lastEnd === undefined ? {} : { endedAt: lastEnd }),
			...(typeof tokens === "number" ? { tokens } : {}),
			...(typeof cost === "number" && cost > 0 ? { cost } : {}),
			children: [],
			details,
		},
		children,
	);
}

// ---------------------------------------------------------------------------------------------
// Layout: flatten with connectors, collapsing finished subtrees unless the viewer opened them.

export interface GraphRow {
	readonly node: GraphNode;
	readonly depth: number;
	/** For each ancestor level below the root: whether that ancestor was the last child (no rail). */
	readonly rails: readonly boolean[];
	readonly last: boolean;
	readonly collapsed: boolean;
	/** Descendants hidden by collapse. */
	readonly hidden: number;
	readonly parentKey?: string;
}

/** Collapse overrides by node key: true collapsed, false expanded; absent means automatic. */
export type CollapseState = ReadonlyMap<string, boolean>;

/** Finished subtrees fold, unless something under them still runs (a job outlives the cell that started it). */
export function defaultCollapsed(node: GraphNode): boolean {
	if (node.children.length === 0 || node.kind === "turn") return false;
	if (node.kind === "earlier") return true;
	return isFinished(node.status) && !hasLiveDescendant(node);
}

function hasLiveDescendant(node: GraphNode): boolean {
	return node.children.some((child) => !isFinished(child.status) || hasLiveDescendant(child));
}

function descendants(node: GraphNode): number {
	return node.children.reduce((total, child) => total + 1 + descendants(child), 0);
}

export function layoutGraph(root: GraphNode, collapse: CollapseState = new Map()): GraphRow[] {
	const rows: GraphRow[] = [];
	const visit = (node: GraphNode, depth: number, rails: boolean[], last: boolean, parentKey?: string): void => {
		const collapsed = node.children.length > 0 && (collapse.get(node.key) ?? defaultCollapsed(node));
		rows.push({
			node,
			depth,
			rails,
			last,
			collapsed,
			hidden: collapsed ? descendants(node) : 0,
			...(parentKey === undefined ? {} : { parentKey }),
		});
		if (collapsed) return;
		node.children.forEach((child, index) => {
			const childLast = index === node.children.length - 1;
			visit(child, depth + 1, depth === 0 ? [] : [...rails, last], childLast, node.key);
		});
	};
	visit(root, 0, [], true);
	return rows;
}

// ---------------------------------------------------------------------------------------------
// Rendering helpers.

const SPINNER = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

export function spinnerGlyph(frame: number): string {
	return SPINNER[Math.abs(Math.floor(frame)) % SPINNER.length]!;
}

type Color = Parameters<RlmStyle["fg"]>[0];

export function statusGlyph(status: GraphStatus, spinner: string): string {
	switch (status) {
		case "running":
			return spinner;
		case "pending":
			return "○";
		case "done":
			return "✓";
		case "failed":
			return "✗";
		case "cancelled":
			return "⊘";
		default:
			return "◐";
	}
}

export function statusColor(status: GraphStatus): Color {
	switch (status) {
		case "running":
			return "accent";
		case "pending":
			return "muted";
		case "done":
			return "success";
		case "failed":
			return "error";
		case "cancelled":
			return "dim";
		default:
			return "warning";
	}
}

export function formatTokens(tokens: number): string {
	if (tokens < 1000) return String(Math.round(tokens));
	if (tokens < 10_000) return `${(tokens / 1000).toFixed(1)}k`;
	if (tokens < 1_000_000) return `${Math.round(tokens / 1000)}k`;
	return `${(tokens / 1_000_000).toFixed(1)}M`;
}

export function formatCost(cost: number): string {
	return cost >= 1 ? `$${cost.toFixed(2)}` : cost >= 0.01 ? `$${cost.toFixed(2)}` : `$${cost.toFixed(3)}`;
}

export function formatBytes(bytes: number): string {
	const mib = bytes / (1024 * 1024);
	return mib >= 1024 ? `${(mib / 1024).toFixed(1)} GiB` : `${Math.round(mib)} MiB`;
}

/** `▰▰▰▱▱` filled in proportion; at least one cell shows any nonzero progress. */
export function bar(fraction: number, cells: number): string {
	const clamped = Math.max(0, Math.min(1, Number.isFinite(fraction) ? fraction : 0));
	let filled = Math.round(clamped * cells);
	if (filled === 0 && clamped > 0) filled = 1;
	if (filled === cells && clamped < 1) filled = cells - 1;
	return "▰".repeat(filled) + "▱".repeat(Math.max(0, cells - filled));
}

/** Longer than this is a clock or data problem (an epoch-zero timestamp), not a real elapsed time. */
const MAX_PLAUSIBLE_MS = 7 * 24 * 60 * 60_000;

function elapsed(node: GraphNode, now: number): string | undefined {
	if (node.startedAt === undefined) return undefined;
	const end = node.endedAt ?? (isFinished(node.status) ? undefined : now);
	if (end === undefined) return undefined;
	const duration = Math.max(0, end - node.startedAt);
	return duration > MAX_PLAUSIBLE_MS ? undefined : formatDuration(duration);
}

function meta(node: GraphNode, now: number, compact: boolean): string {
	const parts: string[] = [];
	const time = elapsed(node, now);
	if (time) parts.push(time);
	if (!compact && typeof node.tokens === "number" && node.tokens > 0) parts.push(`${formatTokens(node.tokens)} tok`);
	if (!compact && typeof node.cost === "number" && node.cost > 0) parts.push(formatCost(node.cost));
	return parts.join(" ");
}

function progressText(progress: GraphProgress, cells: number, style: RlmStyle): string {
	const finished = progress.done + progress.incomplete + progress.failed;
	const parts = [
		`${style.fg(progress.failed + progress.incomplete > 0 ? "warning" : "success", bar(finished / Math.max(1, progress.total), cells))} ${finished}/${progress.total}`,
	];
	if (progress.running > 0) parts.push(`${progress.running} running`);
	if (progress.incomplete > 0) parts.push(style.fg("warning", `${progress.incomplete} incomplete`));
	if (progress.failed > 0) parts.push(style.fg("error", `${progress.failed} failed`));
	return parts.join(" · ");
}

export interface RowRenderOptions {
	readonly style?: RlmStyle;
	readonly now: number;
	readonly spinner: string;
	/** Key of the selected node (focus mode): drawn with a marker. */
	readonly selectedKey?: string;
	/** Reserve a two-column selection gutter (focus mode). */
	readonly gutter?: boolean;
}

/** The connector prefix for a row; deep trees compress rails to one column per level on narrow widths. */
function connector(row: GraphRow, width: number): string {
	if (row.depth === 0) return "";
	const compress = row.depth * 3 > Math.max(12, Math.floor(width / 3));
	const rail = compress ? "│" : "│  ";
	const gap = compress ? " " : "   ";
	const tee = compress ? (row.last ? "└" : "├") : row.last ? "└─ " : "├─ ";
	return row.rails.map((last) => (last ? gap : rail)).join("") + tee;
}

/** One graph row, fitted to `width`: connectors, glyph, label, progress, note, and right-aligned time and spend. */
export function renderGraphRow(row: GraphRow, width: number, options: RowRenderOptions): string {
	const style = options.style ?? PLAIN_STYLE;
	const node = row.node;
	const narrow = width < 60;
	const selected = options.selectedKey === node.key;
	const gutter = options.gutter ? (selected ? style.fg("accent", "▶ ") : "  ") : "";
	const color = statusColor(node.status);
	const glyph =
		node.kind === "turn"
			? style.fg(color, node.status === "running" ? options.spinner : "◆")
			: style.fg(color, statusGlyph(node.status, options.spinner));
	const labelColor: Color =
		node.kind === "earlier" ? "muted" : node.kind === "turn" || node.kind === "cell" ? "toolTitle" : "text";
	const label = selected ? style.bold(style.fg(labelColor, node.label)) : style.fg(labelColor, node.label);
	// The fold marker leads, so a long label (a cell's code) cannot push it off the line.
	const fold = row.collapsed ? `${style.fg("dim", `[+${row.hidden}]`)} ` : "";
	const parts: string[] = [`${glyph} ${fold}${label}`];
	if (node.progress && (node.kind === "fanout" || node.kind === "workflow"))
		parts.push(progressText(node.progress, narrow ? 5 : 10, style));
	if (node.join) parts.push(style.fg("accent", node.join));
	if (node.note && !narrow)
		parts.push(
			style.fg(node.status === "failed" ? "error" : node.status === "incomplete" ? "warning" : "muted", node.note),
		);
	const left = `${gutter}${style.fg("dim", connector(row, width))}${parts.join(" ")}`;
	const right = meta(node, options.now, narrow);
	if (right.length === 0) return truncateToWidth(left, width, "…");
	const rightWidth = visibleWidth(right);
	const room = width - rightWidth - 1;
	if (room < 12) return truncateToWidth(`${left} ${style.fg("dim", right)}`, width, "…");
	const fitted = truncateToWidth(left, room, "…");
	const pad = Math.max(1, width - visibleWidth(fitted) - rightWidth);
	return `${fitted}${" ".repeat(pad)}${style.fg("dim", right)}`;
}

/** Detail lines for an expanded node, indented under it with a dotted rail. */
export function renderDetails(
	row: GraphRow,
	extra: readonly (readonly [string, string])[] | undefined,
	width: number,
	options: RowRenderOptions,
	maxLines = 10,
): string[] {
	const style = options.style ?? PLAIN_STYLE;
	const indent = `${options.gutter ? "  " : ""}${" ".repeat(Math.min(row.depth * 3 + 2, Math.floor(width / 3)))}`;
	const entries = [...row.node.details, ...(extra ?? [])];
	const lines: string[] = [];
	for (const [label, value] of entries.slice(0, maxLines)) {
		const color: Color =
			label === "error" || label === "failed"
				? "error"
				: label === "fetch" || label === "traces"
					? "accent"
					: "muted";
		lines.push(
			truncateToWidth(
				`${indent}${style.fg("dim", "┆ ")}${style.fg("dim", `${label.padEnd(9)}`)} ${style.fg(color, value)}`,
				width,
				"…",
			),
		);
	}
	if (entries.length > maxLines)
		lines.push(truncateToWidth(`${indent}${style.fg("dim", `┆ … ${entries.length - maxLines} more`)}`, width, "…"));
	return lines;
}

/**
 * The rows to show when there are more than `max`: a window that keeps `focus` in view, preferring to show the
 * rows before it (the turn and its earlier cells) and marking what was cut above and below.
 */
export function windowRows(total: number, max: number, focus: number): { start: number; end: number } {
	if (total <= max) return { start: 0, end: total };
	const size = Math.max(1, max);
	const clamped = Math.max(0, Math.min(total - 1, focus));
	let start = Math.max(0, clamped - size + 2);
	if (clamped < size - 1) start = 0;
	start = Math.min(start, total - size);
	return { start, end: start + size };
}

// ---------------------------------------------------------------------------------------------
// Budget gauges and the kernel-pool strip.

export interface Gauge {
	readonly label: string;
	readonly used: number;
	readonly limit: number | null;
	readonly text: string;
}

export function budgetGauges(snapshot: RlmSnapshot, graph: GraphNode): Gauge[] {
	const gauges: Gauge[] = [];
	const usage = snapshot.usage;
	const limits = snapshot.limits;
	const admitted = usage?.admittedTasks;
	if (typeof admitted === "number") {
		const max = limits?.maxAdmittedTasks ?? null;
		gauges.push({
			label: "tasks",
			used: admitted,
			limit: max,
			text: max === null ? String(admitted) : `${admitted}/${max}`,
		});
	}
	const maxWall = limits?.maxWallMs ?? null;
	const remaining = usage?.remainingWallMs;
	if (typeof maxWall === "number" && typeof remaining === "number") {
		const used = Math.max(0, maxWall - remaining);
		gauges.push({ label: "wall", used, limit: maxWall, text: `${formatDuration(used)}/${formatDuration(maxWall)}` });
	}
	const turns = usage?.turns;
	const tokenLimit = limits?.maxTotalTokens ?? turns?.maxTotalTokens ?? null;
	const tokens = turns?.tokens || usage?.usage?.totalTokens || 0;
	if (tokens > 0 || tokenLimit !== null)
		gauges.push({
			label: "tokens",
			used: tokens,
			limit: tokenLimit,
			text: tokenLimit === null ? formatTokens(tokens) : `${formatTokens(tokens)}/${formatTokens(tokenLimit)}`,
		});
	const turnLimit = limits?.maxTotalTurns ?? turns?.maxTotalTurns ?? null;
	if (turnLimit !== null && typeof turns?.turns === "number")
		gauges.push({ label: "turns", used: turns.turns, limit: turnLimit, text: `${turns.turns}/${turnLimit}` });
	const spent = usage?.cost?.spentUsd ?? usage?.usage?.cost ?? 0;
	const costLimit = usage?.cost?.maxCostUsd ?? limits?.maxCostUsd ?? null;
	if ((typeof spent === "number" && spent > 0) || costLimit !== null)
		gauges.push({
			label: "cost",
			used: spent ?? 0,
			limit: costLimit,
			text: costLimit === null ? formatCost(spent ?? 0) : `${formatCost(spent ?? 0)}/${formatCost(costLimit)}`,
		});
	// The frame budget of the newest active (else newest) inference call: calls, tokens and depth.
	const frames = snapshot.frames ?? [];
	const byBudget = new Map<string, RlmFrame[]>();
	for (const frame of frames) {
		if (!frame.budget) continue;
		byBudget.set(frame.budget.id, [...(byBudget.get(frame.budget.id) ?? []), frame]);
	}
	const candidates = [...byBudget.values()].sort(
		(left, right) =>
			Number(right.some((frame) => frame.status === "running")) -
				Number(left.some((frame) => frame.status === "running")) ||
			(maxDefined(right.map((frame) => frame.startedAt)) ?? 0) -
				(maxDefined(left.map((frame) => frame.startedAt)) ?? 0),
	);
	const chosen = candidates[0];
	if (chosen && graph.children.length >= 0) {
		const budget = chosen[0]!.budget!;
		const calls = sum(chosen.map((frame) => frame.spent.calls)) ?? 0;
		const frameTokens = sum(chosen.map((frame) => frame.spent.tokens)) ?? 0;
		gauges.push({
			label: "calls",
			used: calls,
			limit: budget.calls,
			text: budget.calls === null ? String(calls) : `${calls}/${budget.calls}`,
		});
		if (budget.tokens !== null)
			gauges.push({
				label: "frame tok",
				used: frameTokens,
				limit: budget.tokens,
				text: `${formatTokens(frameTokens)}/${formatTokens(budget.tokens)}`,
			});
		gauges.push({ label: "depth", used: budget.depth, limit: null, text: String(budget.depth) });
	}
	return gauges;
}

function gaugeColor(fraction: number): Color {
	if (fraction >= 0.85) return "error";
	if (fraction >= 0.6) return "warning";
	return "success";
}

/** Gauges packed into at most `maxLines` lines of `width` (lowest-priority gauges drop first). */
export function renderGauges(gauges: readonly Gauge[], width: number, style: RlmStyle, maxLines = 2): string[] {
	const cells = width < 50 ? 4 : 6;
	const rendered = gauges.map((gauge) => {
		if (gauge.limit === null || gauge.limit <= 0)
			return `${style.fg("dim", gauge.label)} ${style.fg("text", gauge.text)}`;
		const fraction = gauge.used / gauge.limit;
		return `${style.fg("dim", gauge.label)} ${style.fg(gaugeColor(fraction), bar(fraction, cells))} ${style.fg("text", gauge.text)}`;
	});
	const lines: string[] = [];
	let current = "";
	for (const item of rendered) {
		const candidate = current.length === 0 ? item : `${current}  ${item}`;
		if (visibleWidth(candidate) <= width || current.length === 0) current = candidate;
		else {
			lines.push(current);
			if (lines.length >= maxLines) {
				current = "";
				break;
			}
			current = item;
		}
	}
	if (current.length > 0 && lines.length < maxLines) lines.push(current);
	return lines.map((line) => truncateToWidth(line, width, "…"));
}

/** `kernels ■■◆□······ 3/16 · 1 busy · 1 pinned · 2 evicted · 212 MiB`. */
export function renderKernelStrip(snapshot: RlmSnapshot, width: number, style: RlmStyle): string | undefined {
	const pool = snapshot.pool;
	if (!pool) return undefined;
	const busy = pool.lanes.filter((lane) => lane.running > 0).length;
	const pinned = pool.lanes.filter((lane) => lane.pinnedBy.length > 0).length;
	const parts: string[] = [];
	if (pool.maxLive <= 24 && width >= 50) {
		const cells = pool.lanes.map((lane) =>
			lane.running > 0
				? style.fg("accent", "■")
				: lane.pinnedBy.length > 0
					? style.fg("warning", "◆")
					: style.fg("muted", "□"),
		);
		const free = Math.max(0, pool.maxLive - pool.lanes.length);
		parts.push(
			`${style.fg("dim", "kernels")} ${cells.join("")}${style.fg("dim", "·".repeat(free))} ${pool.live}/${pool.maxLive}`,
		);
	} else parts.push(`${style.fg("dim", "kernels")} ${pool.live}/${pool.maxLive}`);
	parts.push(`${busy} busy`, `${pinned} pinned`);
	if (typeof pool.evictions === "number") parts.push(`${pool.evictions} evicted`);
	if (typeof pool.memoryBytes === "number")
		parts.push(
			`mem ${formatBytes(pool.memoryBytes)}${typeof pool.memoryCapBytes === "number" && width >= 100 ? ` (cap ${formatBytes(pool.memoryCapBytes)}/kernel)` : ""}`,
		);
	return truncateToWidth(parts.join(style.fg("dim", " · ")), width, "…");
}

// ---------------------------------------------------------------------------------------------
// The three levels: footer summary, docked panel, and the focus view body.

export interface GraphTotals {
	readonly running: number;
	readonly tasks: number;
	readonly failed: number;
	readonly jobsRunning: number;
	readonly fanouts: readonly GraphNode[];
	readonly cell?: GraphNode;
	readonly nodes: number;
}

export function graphTotals(root: GraphNode): GraphTotals {
	let running = 0;
	let tasks = 0;
	let failed = 0;
	let jobsRunning = 0;
	let nodes = 0;
	const fanouts: GraphNode[] = [];
	const visit = (node: GraphNode): void => {
		nodes++;
		if (node.kind !== "turn" && node.kind !== "earlier" && (node.status === "running" || node.status === "pending"))
			running++;
		if (node.kind === "task" || node.kind === "child" || node.kind === "background") tasks++;
		if (node.status === "failed" && node.kind !== "earlier") failed++;
		if (node.kind === "job" && node.status === "running") jobsRunning++;
		if (node.kind === "fanout") fanouts.push(node);
		for (const child of node.children) if (node.kind !== "earlier") visit(child);
	};
	visit(root);
	const cells = root.children.filter((child) => child.kind === "cell");
	const cell = [...cells].reverse().find((child) => child.status === "running") ?? cells.at(-1);
	return { running, tasks, failed, jobsRunning, fanouts, nodes, ...(cell === undefined ? {} : { cell }) };
}

function countActiveTasks(root: GraphNode): number {
	let active = 0;
	const visit = (node: GraphNode): void => {
		if ((node.kind === "task" || node.kind === "child" || node.kind === "background") && !isFinished(node.status))
			active++;
		if (node.kind !== "earlier") for (const child of node.children) visit(child);
	};
	visit(root);
	return active;
}

/** True while anything in the graph runs. */
export function graphActive(root: GraphNode): boolean {
	return root.status === "running" || graphTotals(root).running > 0;
}

export interface RlmViewOptions {
	readonly style?: RlmStyle;
	readonly spinnerFrame?: number;
	/** Key hint for the focus view, e.g. "alt+r". */
	readonly focusKey?: string;
	readonly collapse?: CollapseState;
}

/** Level (a): one line while the panel is hidden; undefined when nothing runs. */
export function renderRlmFooter(
	snapshot: RlmSnapshot,
	width: number,
	options: RlmViewOptions = {},
): string | undefined {
	const graph = buildRlmGraph(snapshot);
	if (!graphActive(graph) && snapshot.rootCell?.status !== "running") return undefined;
	const style = options.style ?? PLAIN_STYLE;
	const spinner = spinnerGlyph(options.spinnerFrame ?? 0);
	const totals = graphTotals(graph);
	const parts: string[] = [];
	const time = elapsed(graph, snapshot.now);
	parts.push(`turn${time ? ` ${time}` : ""}`);
	if (totals.cell) {
		const cellIndex = graph.children.filter((child) => child.kind === "cell").indexOf(totals.cell) + 1;
		const cellTime = elapsed(totals.cell, snapshot.now);
		parts.push(`cell ${cellIndex}${totals.cell.status === "running" && cellTime ? ` ${cellTime}` : ""}`);
	}
	const fanout = totals.fanouts.find((node) => node.status === "running") ?? totals.fanouts.at(-1);
	if (fanout?.progress) {
		const finished = fanout.progress.done + fanout.progress.incomplete + fanout.progress.failed;
		parts.push(`map ${bar(finished / Math.max(1, fanout.progress.total), 5)} ${finished}/${fanout.progress.total}`);
	}
	if (totals.tasks > 0) {
		const activeTasks = countActiveTasks(graph);
		parts.push(
			`${totals.tasks} task${totals.tasks === 1 ? "" : "s"}${activeTasks > 0 ? ` (${activeTasks} active)` : ""}`,
		);
	}
	if (totals.jobsRunning > 0) parts.push(`${totals.jobsRunning} job${totals.jobsRunning === 1 ? "" : "s"}`);
	if (totals.failed > 0) parts.push(style.fg("error", `${totals.failed} failed`));
	if (typeof graph.cost === "number" && graph.cost > 0) parts.push(formatCost(graph.cost));
	const hint = options.focusKey ? style.fg("dim", ` · ${options.focusKey} graph`) : "";
	return truncateToWidth(
		`${style.fg("accent", `◆ rlm ${spinner}`)} ${style.fg("muted", parts.join(" · "))}${hint}`,
		Math.max(1, width),
		"…",
	);
}

function headerLine(graph: GraphNode, snapshot: RlmSnapshot, style: RlmStyle, spinner: string, suffix: string): string {
	const totals = graphTotals(graph);
	const state = graphActive(graph)
		? style.fg("accent", `${spinner} running`)
		: graph.status === "failed"
			? style.fg("error", "✗ failed")
			: graph.children.length === 0
				? style.fg("dim", "idle")
				: style.fg("success", "✓ done");
	const parts = [`${totals.nodes - 1} nodes`];
	if (totals.running > 0) parts.push(`${totals.running} active`);
	if (snapshot.error !== undefined) parts.push(style.fg("error", `inspection failed: ${flat(snapshot.error, 80)}`));
	return `${style.bold(style.fg("accent", "RLM"))} ${state} ${style.fg("muted", parts.join(" · "))}${suffix}`;
}

/** Level (b): the docked panel: header, gauges, the graph (bounded, windowed on live work), kernel strip. */
export function renderRlmDock(
	snapshot: RlmSnapshot,
	width: number,
	options: RlmViewOptions & { readonly maxRows?: number } = {},
): string[] {
	const style = options.style ?? PLAIN_STYLE;
	const spinner = spinnerGlyph(options.spinnerFrame ?? 0);
	const bound = Math.max(1, width);
	const graph = buildRlmGraph(snapshot);
	const hint = options.focusKey ? style.fg("dim", `  ${options.focusKey} focus`) : "";
	const lines: string[] = [truncateToWidth(headerLine(graph, snapshot, style, spinner, hint), bound, "…")];
	lines.push(...renderGauges(budgetGauges(snapshot, graph), bound, style, 2));
	// The turn row stays pinned; the rest is windowed on the newest live node.
	const [turnRow, ...rows] = layoutGraph(graph, options.collapse);
	lines.push(renderGraphRow(turnRow!, bound, { style, now: snapshot.now, spinner }));
	const maxRows = Math.max(2, (options.maxRows ?? 12) - 1);
	let focus = -1;
	rows.forEach((row, index) => {
		if (row.node.status === "running") focus = index;
	});
	const { start, end } = windowRows(rows.length, maxRows, focus === -1 ? rows.length - 1 : focus);
	if (start > 0) lines.push(style.fg("dim", truncateToWidth(`   ↑ ${start} more`, bound, "…")));
	for (const row of rows.slice(start, end))
		lines.push(renderGraphRow(row, bound, { style, now: snapshot.now, spinner }));
	if (end < rows.length) {
		const below = rows.slice(end);
		const active = below.filter((row) => !isFinished(row.node.status)).length;
		lines.push(
			style.fg(
				"dim",
				truncateToWidth(`  ↓ ${below.length} more${active > 0 ? ` (${active} active)` : ""}`, bound, "…"),
			),
		);
	}
	const strip = renderKernelStrip(snapshot, bound, style);
	if (strip) lines.push(strip);
	lines.push(...renderContextLines(snapshot.context, style));
	return lines.map((line) => truncateToWidth(line, bound, "…"));
}

/** Frame trace (from `rlm.frames {id}`) as detail lines. */
export function frameTraceDetails(trace: unknown): [string, string][] {
	if (trace === null || typeof trace !== "object" || Array.isArray(trace)) return [];
	const body = trace as Record<string, unknown>;
	const lines: [string, string][] = [];
	if (typeof body.model === "string") lines.push(["model", body.model]);
	if (typeof body.detail === "string") lines.push(["detail", flat(body.detail, 300)]);
	if (typeof body.value === "string") lines.push(["value", flat(body.value, 400)]);
	if (typeof body.error === "string") lines.push(["error", flat(body.error, 400)]);
	if (Array.isArray(body.views))
		lines.push(["views", `${body.views.length} context view${body.views.length === 1 ? "" : "s"}`]);
	if (Array.isArray(body.attempts)) lines.push(["attempts", String(body.attempts.length)]);
	const remaining = body.remaining as Record<string, unknown> | undefined;
	if (remaining && typeof remaining === "object")
		lines.push([
			"remaining",
			`calls ${remaining.calls ?? "∞"} · tokens ${remaining.tokens ?? "∞"} · depth ${remaining.depth ?? "?"}`,
		]);
	return lines;
}
