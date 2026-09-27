/**
 * The RLM pane: the RLM run as dependency waves in the native TUI's side pane (`app.rlm.pane`), and the live wave
 * summary above the editor.
 *
 * A run is either the current root turn (the work each `rlm` cell started is one wave, a "same frontier"; work a
 * child started follows its parent) or one `workflows.run` call (waves are the topological levels of `dependsOn`;
 * nodes not admitted yet show as pending). Tool calls stay out of the waves except as one "N tool calls" node per cell,
 * and an `rlm.map` fan-out is one node with a progress bar.
 *
 * Pure and width-bounded: `buildDagRuns` turns an inspection snapshot into runs, `renderRlmPane` draws one run as
 * boxes side by side per wave (wrapping when a wave does not fit), arrows between waves, the dependency edges and
 * per-node cards, and `applyDagAction` is the pane's navigation reducer. `RlmPane` is the TUI component.
 */
import type { Component, KeybindingsManager } from "@ultron/tui";
import { truncateToWidth, visibleWidth } from "@ultron/tui";
import {
	bar,
	buildRlmGraph,
	count,
	type GraphKind,
	type GraphNode,
	type GraphProgress,
	type GraphStatus,
	isFinished,
	rollup,
	statusColor,
	taskStatus,
} from "./rlm-graph.ts";
import {
	formatDuration,
	PLAIN_STYLE,
	type RlmSnapshot,
	type RlmStyle,
	type RlmTask,
	type RlmWorkflowRun,
	shortId,
} from "./rlm-visualizer.ts";

// ---------------------------------------------------------------------------------------------
// Model.

export type DagNodeKind = GraphKind | "tools";

export interface DagNode {
	/** Stable across refreshes (selection and fold state are keyed by it). */
	readonly key: string;
	readonly name: string;
	readonly kind: DagNodeKind;
	readonly status: GraphStatus;
	/** Keys of the nodes this one waits for, within its run. */
	readonly dependsOn: readonly string[];
	/** Shown when the node has no dependencies (e.g. "Start node", "cell 2"). */
	readonly origin: string;
	/** First line of the task's prompt or input. */
	readonly prompt?: string;
	/** The definition (typed agents, workflow nodes) or the kind of work (rlm.map, job, tools). */
	readonly category: string;
	readonly model?: string;
	/** The node's latest assistant text. */
	readonly text?: string;
	/** Result or error summary. */
	readonly note?: string;
	readonly startedAt?: number;
	readonly endedAt?: number;
	readonly turns?: number;
	readonly toolCalls?: number;
	/** `rlm.map` fan-out or workflow progress. */
	readonly progress?: GraphProgress;
}

export interface DagRun {
	/** "turn" or `workflow:<run>`. */
	readonly key: string;
	readonly kind: "turn" | "workflow";
	/** Short id for the header. */
	readonly id: string;
	readonly title: string;
	readonly nodes: readonly DagNode[];
	/** Node keys per wave, in display order. */
	readonly waves: readonly (readonly string[])[];
	/** Dependency edges as [from, to] node keys. */
	readonly edges: readonly (readonly [string, string])[];
	readonly status: GraphStatus;
	readonly startedAt?: number;
}

function flat(text: string, limit = 200): string {
	const line = text.replace(/\s+/g, " ").trim();
	return line.length > limit ? `${line.slice(0, limit - 1)}…` : line;
}

/** The prompt of a task input preview: the `prompt` field of `{"prompt": ...}` (possibly cut), else the preview. */
export function promptOf(input: string | undefined): string | undefined {
	if (input === undefined || input.trim().length === 0) return undefined;
	try {
		const value = JSON.parse(input) as unknown;
		if (typeof value === "string") return flat(value);
		if (value !== null && typeof value === "object" && !Array.isArray(value)) {
			const prompt = (value as Record<string, unknown>).prompt ?? (value as Record<string, unknown>).task;
			if (typeof prompt === "string") return flat(prompt);
		}
	} catch {
		// A bounded preview is often cut mid-JSON; read the prompt field by prefix.
		const match = /^\{"(?:prompt|task)":"((?:[^"\\]|\\.)*)/.exec(input);
		if (match) return flat(match[1]!.replace(/\\n/g, " ").replace(/\\(.)/g, "$1"));
	}
	return flat(input);
}

function progressOf(statuses: readonly GraphStatus[]): { progress: GraphProgress; status: GraphStatus } {
	const progress = count(statuses);
	return { progress, status: statuses.length === 0 ? "pending" : rollup(progress) };
}

/** Topological levels of `dependsOn` (unknown dependencies are ignored; a cycle is cut where it closes). */
export function computeWaves(
	nodes: readonly { readonly key: string; readonly dependsOn: readonly string[] }[],
): string[][] {
	const byKey = new Map(nodes.map((node) => [node.key, node]));
	const level = new Map<string, number>();
	const visiting = new Set<string>();
	const depth = (key: string): number => {
		const known = level.get(key);
		if (known !== undefined) return known;
		if (visiting.has(key)) return 0;
		visiting.add(key);
		let value = 0;
		for (const dependency of byKey.get(key)?.dependsOn ?? [])
			if (byKey.has(dependency) && dependency !== key) value = Math.max(value, depth(dependency) + 1);
		visiting.delete(key);
		level.set(key, value);
		return value;
	};
	const waves: string[][] = [];
	for (const node of nodes) {
		const index = depth(node.key);
		while (waves.length <= index) waves.push([]);
		waves[index]!.push(node.key);
	}
	return waves.filter((wave) => wave.length > 0);
}

function taskFields(task: RlmTask | undefined): Partial<DagNode> {
	if (task === undefined) return {};
	const prompt = promptOf(task.input);
	return {
		...(prompt === undefined ? {} : { prompt }),
		...(task.model === undefined ? {} : { model: task.model }),
		...(task.lastText === undefined ? {} : { text: flat(task.lastText) }),
		...(task.turns === undefined ? {} : { turns: task.turns }),
		...(task.toolCallCount === undefined ? {} : { toolCalls: task.toolCallCount }),
	};
}

function taskName(task: RlmTask | undefined, node: GraphNode): string {
	if (task === undefined) return node.label;
	const id = shortId(task.id);
	if (node.kind === "child") return `child ${id}`;
	if (node.kind === "background") return `background ${id}`;
	return `${task.definition.replace(/@\d+$/, "")} ${id}`;
}

/** One graph node as a DAG node (tasks keep their graph key, so selection survives a refresh). */
function fromGraph(
	node: GraphNode,
	tasks: ReadonlyMap<string, RlmTask>,
	dependsOn: readonly string[],
	origin: string,
): DagNode {
	const task = node.key.startsWith("task:") ? tasks.get(node.key.slice(5)) : undefined;
	const base = {
		key: node.key,
		kind: node.kind as DagNodeKind,
		status: node.status,
		dependsOn,
		origin,
		...(node.startedAt === undefined ? {} : { startedAt: node.startedAt }),
		...(node.endedAt === undefined ? {} : { endedAt: node.endedAt }),
		...(node.note === undefined ? {} : { note: node.note.replace(/^→ /, "") }),
		...(node.progress === undefined ? {} : { progress: node.progress }),
	};
	switch (node.kind) {
		case "task":
		case "child":
		case "background":
			return { ...base, name: taskName(task, node), category: task?.definition ?? node.kind, ...taskFields(task) };
		case "fanout":
			return {
				...base,
				name: node.label,
				category: "rlm.map",
				...(node.note === undefined ? {} : { prompt: node.note }),
			};
		case "infer":
			return { ...base, name: node.label, category: "rlm.infer" };
		case "job":
			return { ...base, name: node.label, category: "job", prompt: node.label.replace(/^bash /, "") };
		case "workflow":
			return {
				...base,
				name: node.label.replace(/ \d+ nodes$/, ""),
				category: "workflows.run",
				prompt: `${node.children.length} node${node.children.length === 1 ? "" : "s"}`,
			};
		default:
			return { ...base, name: node.label, category: node.kind };
	}
}

/** One "N tool calls" node for a cell's tool calls. */
function toolsNode(key: string, tools: readonly GraphNode[], origin: string): DagNode {
	const { progress, status } = progressOf(tools.map((tool) => tool.status));
	const names = [...new Set(tools.map((tool) => tool.details.find(([label]) => label === "tool")?.[1] ?? tool.label))];
	const starts = tools.map((tool) => tool.startedAt).filter((at): at is number => at !== undefined);
	const ends = tools.map((tool) => tool.endedAt).filter((at): at is number => at !== undefined);
	return {
		key,
		name: `${tools.length} tool call${tools.length === 1 ? "" : "s"}`,
		kind: "tools",
		status,
		dependsOn: [],
		origin,
		category: "tools",
		prompt: names.join(", "),
		progress,
		...(starts.length === 0 ? {} : { startedAt: Math.min(...starts) }),
		...(isFinished(status) && ends.length > 0 ? { endedAt: Math.max(...ends) } : {}),
	};
}

/** Nested levels shown under a cell's frontier (work a child started, and so on). */
const MAX_NESTED = 3;

function turnRun(snapshot: RlmSnapshot, graph: GraphNode, tasks: ReadonlyMap<string, RlmTask>): DagRun | undefined {
	const nodes: DagNode[] = [];
	const waves: string[][] = [];
	const edges: [string, string][] = [];
	const addGroup = (children: readonly GraphNode[], origin: string, groupKey: string): void => {
		const tools = children.filter((child) => child.kind === "tool");
		const others = children.filter((child) => child.kind !== "tool" && child.kind !== "earlier");
		let level: { node: GraphNode; dag: DagNode }[] = others.map((node) => ({
			node,
			dag: fromGraph(node, tasks, [], origin),
		}));
		const top = level.map((item) => item.dag);
		if (tools.length > 0) top.push(toolsNode(`tools:${groupKey}`, tools, origin));
		if (top.length === 0) return;
		nodes.push(...top);
		waves.push(top.map((node) => node.key));
		for (let depth = 0; depth < MAX_NESTED && level.length > 0; depth++) {
			const next: { node: GraphNode; dag: DagNode }[] = [];
			for (const parent of level) {
				// A workflow's members are their own run; tool calls of a child stay in its card's counts.
				if (parent.node.kind === "workflow") continue;
				for (const child of parent.node.children) {
					if (child.kind === "tool" || child.kind === "earlier") continue;
					const dag = fromGraph(child, tasks, [parent.dag.key], origin);
					next.push({ node: child, dag });
					edges.push([parent.dag.key, dag.key]);
				}
			}
			if (next.length === 0) break;
			nodes.push(...next.map((item) => item.dag));
			waves.push(next.map((item) => item.dag.key));
			level = next;
		}
	};
	const cells = graph.children.filter((child) => child.kind === "cell");
	for (const [index, cell] of cells.entries()) addGroup(cell.children, `cell ${index + 1}`, cell.key);
	const loose = graph.children.filter((child) => child.kind !== "cell" && child.kind !== "earlier");
	if (loose.length > 0) addGroup(loose, "turn", "turn");
	if (nodes.length === 0) return undefined;
	const prompt = snapshot.turn?.prompt;
	return {
		key: "turn",
		kind: "turn",
		id: snapshot.usage?.rootId ? shortId(snapshot.usage.rootId.replace(/^root-/, "")) : "turn",
		title: prompt ? flat(prompt, 120) : "current turn",
		nodes,
		waves,
		edges,
		status: progressOf(nodes.map((node) => node.status)).status,
		...(graph.startedAt === undefined ? {} : { startedAt: graph.startedAt }),
	};
}

function endedStatus(status: string | undefined): GraphStatus {
	switch (status) {
		case "succeeded":
			return "done";
		case "skipped":
		case "cancelled":
			return "cancelled";
		case "exhausted":
		case "interrupted":
			return "incomplete";
		case undefined:
			return "pending";
		default:
			return "failed";
	}
}

function workflowRuns(snapshot: RlmSnapshot): DagRun[] {
	const members = new Map<string, Map<string, RlmTask>>();
	for (const task of snapshot.tasks) {
		if (task.workflow === undefined) continue;
		const run = members.get(task.workflow.run) ?? new Map<string, RlmTask>();
		// A revision loop admits one task per round; the newest round represents the node.
		run.set(task.workflow.node, task);
		members.set(task.workflow.run, run);
	}
	const plans = new Map<string, RlmWorkflowRun>((snapshot.workflows ?? []).map((plan) => [plan.run, plan]));
	const order = [...plans.keys(), ...[...members.keys()].filter((run) => !plans.has(run))];
	const turnStart = snapshot.turn?.startedAt;
	const runs: DagRun[] = [];
	for (const run of order) {
		const plan = plans.get(run);
		const tasks = members.get(run) ?? new Map<string, RlmTask>();
		const planned =
			plan?.nodes ??
			[...tasks.values()].map((task) => ({
				id: task.workflow!.node,
				definition: task.definition,
				dependsOn: task.workflow!.dependsOn,
				join: task.workflow!.join,
			}));
		const ids = new Set(planned.map((node) => node.id));
		const keyOf = (id: string) => `wf:${run}:${id}`;
		const nodes: DagNode[] = planned.map((node) => {
			const task = tasks.get(node.id);
			const ended = "status" in node ? (node as { status?: string }).status : undefined;
			const reason = "reason" in node ? (node as { reason?: string }).reason : undefined;
			const timing = task === undefined ? undefined : snapshot.timing?.get(task.id);
			const startedAt = task?.startedAt ?? timing?.startedAt;
			const endedAt = task?.endedAt ?? timing?.endedAt;
			const status = task !== undefined ? taskStatus(task.state) : endedStatus(ended);
			const error = task?.result?.error;
			const preview = task?.result?.preview;
			const note =
				error !== undefined
					? flat(error, 160)
					: task === undefined && ended === "skipped"
						? `skipped${reason ? `: ${reason}` : ""}`
						: task === undefined && reason !== undefined
							? flat(reason, 160)
							: preview !== undefined
								? flat(preview, 160)
								: undefined;
			return {
				key: keyOf(node.id),
				name: node.id,
				kind: "task" as const,
				status,
				dependsOn: node.dependsOn.filter((dependency) => ids.has(dependency)).map(keyOf),
				origin: "Start node",
				category: task?.definition ?? node.definition,
				...taskFields(task),
				...(note === undefined ? {} : { note }),
				...(startedAt === undefined ? {} : { startedAt }),
				...(endedAt === undefined ? {} : { endedAt }),
			};
		});
		const edges: [string, string][] = [];
		for (const node of nodes) for (const dependency of node.dependsOn) edges.push([dependency, node.key]);
		const startedAt =
			plan?.startedAt ??
			Math.min(...nodes.map((node) => node.startedAt ?? Number.POSITIVE_INFINITY).filter(Number.isFinite));
		const current =
			turnStart !== undefined && Number.isFinite(startedAt) && (startedAt as number) >= turnStart - 1000;
		runs.push({
			key: `workflow:${run}`,
			kind: "workflow",
			id: run.replace(/^wf-/, ""),
			title: current && snapshot.turn?.prompt ? flat(snapshot.turn.prompt, 120) : `workflow ${run}`,
			nodes,
			waves: computeWaves(nodes),
			edges,
			status: progressOf(nodes.map((node) => node.status)).status,
			...(Number.isFinite(startedAt) ? { startedAt: startedAt as number } : {}),
		});
	}
	return runs;
}

/** Every run the pane can show: the current turn (when it started work), then each workflow run, oldest first. */
export function buildDagRuns(snapshot: RlmSnapshot): DagRun[] {
	const graph = buildRlmGraph(snapshot);
	const tasks = new Map(snapshot.tasks.map((task) => [task.id, task]));
	const turn = turnRun(snapshot, graph, tasks);
	return [...(turn === undefined ? [] : [turn]), ...workflowRuns(snapshot)];
}

function isActive(status: GraphStatus): boolean {
	return status === "running" || status === "pending";
}

/** The run shown by default: the newest run with live work, else the newest run. */
export function defaultRun(runs: readonly DagRun[]): DagRun | undefined {
	return [...runs].reverse().find((run) => run.nodes.some((node) => isActive(node.status))) ?? runs.at(-1);
}

export interface RunCounts {
	readonly total: number;
	readonly done: number;
	readonly running: number;
	readonly pending: number;
	readonly failed: number;
	/** 1-based index of the first wave with unfinished work (the last wave when all are finished). */
	readonly wave: number;
	readonly waves: number;
}

export function runCounts(run: DagRun): RunCounts {
	const statuses = new Map(run.nodes.map((node) => [node.key, node.status]));
	let done = 0;
	let running = 0;
	let pending = 0;
	let failed = 0;
	for (const node of run.nodes) {
		if (node.status === "done") done++;
		else if (node.status === "running") running++;
		else if (node.status === "pending") pending++;
		else if (node.status === "failed") failed++;
	}
	const open = run.waves.findIndex((wave) => wave.some((key) => !isFinished(statuses.get(key) ?? "done")));
	return {
		total: run.nodes.length,
		done,
		running,
		pending,
		failed,
		wave: open === -1 ? run.waves.length : open + 1,
		waves: run.waves.length,
	};
}

// ---------------------------------------------------------------------------------------------
// Text helpers.

type Color = Parameters<RlmStyle["fg"]>[0];

/** Pad (or cut) to exactly `width` columns. */
function padTo(text: string, width: number): string {
	if (width <= 0) return "";
	const cut = visibleWidth(text) > width ? truncateToWidth(text, width, "…") : text;
	return cut + " ".repeat(Math.max(0, width - visibleWidth(cut)));
}

function cut(text: string, width: number): string {
	return visibleWidth(text) > width ? truncateToWidth(text, Math.max(0, width), "…") : text;
}

/** Longer than this is a clock problem, not an elapsed time. */
const MAX_PLAUSIBLE_MS = 7 * 24 * 60 * 60_000;

function elapsedOf(node: { startedAt?: number; endedAt?: number; status: GraphStatus }, now: number) {
	if (node.startedAt === undefined) return undefined;
	const end = node.endedAt ?? (isFinished(node.status) ? undefined : now);
	if (end === undefined) return undefined;
	const duration = Math.max(0, end - node.startedAt);
	return duration > MAX_PLAUSIBLE_MS ? undefined : formatDuration(duration);
}

export function statusWord(node: Pick<DagNode, "status" | "note">): string {
	switch (node.status) {
		case "running":
			return "● Running";
		case "pending":
			return "○ Pending";
		case "done":
			return "✓ Done";
		case "failed":
			return "✗ Failed";
		case "cancelled":
			return node.note?.startsWith("skipped") ? "⊘ Skipped" : "⊘ Cancelled";
		default:
			return "◐ Incomplete";
	}
}

function dot(status: GraphStatus): string {
	switch (status) {
		case "running":
			return "●";
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

/** Pending reads dim; everything else keeps the graph's status colors. */
function tone(status: GraphStatus): Color {
	return status === "pending" ? "dim" : statusColor(status);
}

function progressLine(progress: GraphProgress, width: number): string {
	const finished = progress.done + progress.failed + progress.incomplete;
	const text = `${finished}/${progress.total}`;
	const cells = Math.max(3, Math.min(10, width - visibleWidth(text) - 1));
	return `${bar(finished / Math.max(1, progress.total), cells)} ${text}`;
}

// ---------------------------------------------------------------------------------------------
// Pane state and navigation.

export interface RlmPaneState {
	/** Selected run (sticky while it exists); the default run otherwise. */
	readonly runKey?: string;
	readonly selectedKey?: string;
	/** Folded boxes (one line instead of three). */
	readonly folded: ReadonlySet<string>;
	/** Node detail cards shown. */
	readonly details: boolean;
	/** First visible body line. */
	readonly scroll: number;
	/** Scroll the selected node into view on the next render. */
	readonly reveal: boolean;
}

export type DagAction =
	| "next"
	| "prev"
	| "fold"
	| "details"
	| "scrollUp"
	| "scrollDown"
	| "pageUp"
	| "pageDown"
	| "prevRun"
	| "nextRun";

export function initialDagState(): RlmPaneState {
	return { folded: new Set(), details: true, scroll: 0, reveal: true };
}

export function currentRun(runs: readonly DagRun[], state: RlmPaneState): DagRun | undefined {
	return (state.runKey === undefined ? undefined : runs.find((run) => run.key === state.runKey)) ?? defaultRun(runs);
}

/** Nodes in navigation order: wave by wave, left to right. */
export function nodeOrder(run: DagRun): string[] {
	return run.waves.flat();
}

/** The selected node key: the chosen one while it exists, else the first running node, else the first node. */
export function selectedNode(run: DagRun, state: RlmPaneState): string | undefined {
	const order = nodeOrder(run);
	if (state.selectedKey !== undefined && order.includes(state.selectedKey)) return state.selectedKey;
	const byKey = new Map(run.nodes.map((node) => [node.key, node]));
	return order.find((key) => byKey.get(key)?.status === "running") ?? order[0];
}

export function applyDagAction(
	state: RlmPaneState,
	runs: readonly DagRun[],
	action: DagAction,
	page = 10,
): RlmPaneState {
	const run = currentRun(runs, state);
	if (run === undefined) return state;
	const order = nodeOrder(run);
	const selected = selectedNode(run, state);
	const index = selected === undefined ? -1 : order.indexOf(selected);
	switch (action) {
		case "next":
		case "prev": {
			if (order.length === 0) return state;
			const step = action === "next" ? 1 : -1;
			const nextIndex = index === -1 ? 0 : (index + step + order.length) % order.length;
			return { ...state, runKey: run.key, selectedKey: order[nextIndex]!, reveal: true };
		}
		case "fold": {
			if (selected === undefined) return state;
			const folded = new Set(state.folded);
			if (folded.has(selected)) folded.delete(selected);
			else folded.add(selected);
			return { ...state, runKey: run.key, selectedKey: selected, folded, reveal: true };
		}
		case "details":
			return { ...state, details: !state.details, reveal: true };
		case "scrollUp":
			return { ...state, scroll: Math.max(0, state.scroll - 1), reveal: false };
		case "scrollDown":
			return { ...state, scroll: state.scroll + 1, reveal: false };
		case "pageUp":
			return { ...state, scroll: Math.max(0, state.scroll - Math.max(1, page)), reveal: false };
		case "pageDown":
			return { ...state, scroll: state.scroll + Math.max(1, page), reveal: false };
		case "prevRun":
		case "nextRun": {
			if (runs.length < 2) return state;
			const at = runs.indexOf(run);
			const step = action === "nextRun" ? 1 : -1;
			const target = runs[(at + step + runs.length) % runs.length]!;
			const { selectedKey: _dropped, ...rest } = state;
			return { ...rest, runKey: target.key, scroll: 0, reveal: true };
		}
	}
}

// ---------------------------------------------------------------------------------------------
// Rendering.

export interface DagRenderOptions {
	readonly style?: RlmStyle;
	readonly now?: number;
	/** Key hints for the footer, e.g. "Tab/n next". */
	readonly hints?: readonly string[];
	/** Whether the pane has the input focus (the footer says how to focus it otherwise). */
	readonly focused?: boolean;
	/** Shown as the first hint, e.g. "alt+w chat". */
	readonly focusHint?: string;
	/** Inspection failure: the footer shows it instead of "Connected". */
	readonly error?: string;
	/** Tasks listed by the host, for the breadcrumb. */
	readonly taskCount?: number;
}

const GAP = 2;
const MIN_BOX = 20;
const MAX_BOX = 30;
/** A wave of one node may widen its box to fit its content, up to this. */
const MAX_SINGLE_BOX = 48;

/**
 * Boxes per row and the box width for a wave of `count` nodes in `width` columns; `natural` is the widest box
 * content (a lone box may grow to fit it).
 */
export function waveGeometry(count: number, width: number, natural = 0): { perRow: number; boxWidth: number } {
	const room = Math.max(1, width);
	const perRow = Math.max(1, Math.min(count, Math.floor((room + GAP) / (MIN_BOX + GAP))));
	const max = count === 1 ? Math.max(MAX_BOX, Math.min(MAX_SINGLE_BOX, natural)) : MAX_BOX;
	const boxWidth = Math.max(1, Math.min(max, Math.floor((room - GAP * (perRow - 1)) / perRow)));
	return { perRow, boxWidth };
}

/** Columns a node's box needs to show its lines without cutting them. */
function naturalBoxWidth(node: DagNode, run: DagRun): number {
	const names = new Map(run.nodes.map((item) => [item.key, item.name]));
	const sub =
		node.dependsOn.length > 0 ? `← ${node.dependsOn.map((key) => names.get(key) ?? key).join(", ")}` : node.origin;
	return Math.max(visibleWidth(`> [-] ${node.name}`), visibleWidth(sub), 16) + 4;
}

/** A bordered box of exactly `width` columns; narrower than 5 columns it degrades to plain lines. */
function drawBox(lines: readonly string[], width: number, border: Color, style: RlmStyle, bold = false): string[] {
	if (width < 5) return lines.map((line) => padTo(line, width));
	const inner = width - 4;
	const edge = (text: string) => (bold ? style.bold(style.fg(border, text)) : style.fg(border, text));
	return [
		edge(`┌${"─".repeat(width - 2)}┐`),
		...lines.map((line) => `${edge("│")} ${padTo(line, inner)} ${edge("│")}`),
		edge(`└${"─".repeat(width - 2)}┘`),
	];
}

function nodeBox(
	node: DagNode,
	run: DagRun,
	width: number,
	selected: boolean,
	folded: boolean,
	style: RlmStyle,
	now: number,
): string[] {
	const inner = Math.max(1, width - 4);
	const marker = selected ? style.fg("accent", ">") : " ";
	const fold = style.fg("dim", folded ? "[+]" : "[-]");
	const name = selected ? style.bold(node.name) : node.name;
	const color = tone(node.status);
	if (folded) {
		const glyph = style.fg(color, dot(node.status));
		const head = cut(`${marker} ${fold} ${name}`, Math.max(1, inner - 2));
		return drawBox([`${head} ${glyph}`], width, selected ? "accent" : "dim", style, selected);
	}
	const time = elapsedOf(node, now);
	const word = statusWord(node);
	const status =
		time !== undefined && visibleWidth(word) + 2 + visibleWidth(time) <= inner
			? `${style.fg(color, word)}  ${style.fg("dim", time)}`
			: style.fg(color, word);
	const names = new Map(run.nodes.map((item) => [item.key, item.name]));
	const sub =
		node.progress !== undefined && (node.kind === "fanout" || node.kind === "workflow" || node.kind === "tools")
			? style.fg(tone(node.status), progressLine(node.progress, inner))
			: node.dependsOn.length > 0
				? style.fg("muted", `← ${node.dependsOn.map((key) => names.get(key) ?? key).join(", ")}`)
				: style.fg("dim", node.origin);
	return drawBox([`${marker} ${fold} ${name}`, status, sub], width, selected ? "accent" : "dim", style, selected);
}

function cardLines(node: DagNode, run: DagRun, width: number, selected: boolean, style: RlmStyle, now: number) {
	const color = tone(node.status);
	const marker = selected ? `${style.fg("accent", ">")} ` : "";
	const head = `${marker}${style.fg(color, dot(node.status))} ${style.fg(selected ? "accent" : "text", node.prompt ?? node.name)}`;
	const meta = node.model === undefined ? node.category : `${node.category} · ${node.model}`;
	const time = elapsedOf(node, now);
	const waited =
		node.status === "pending" && run.startedAt !== undefined && now >= run.startedAt
			? formatDuration(now - run.startedAt)
			: undefined;
	const body =
		node.status === "failed" && node.note !== undefined
			? style.fg("error", node.note)
			: node.text !== undefined
				? style.fg("text", node.text)
				: node.note !== undefined
					? style.fg("muted", `→ ${node.note}`)
					: node.status === "pending"
						? style.fg("dim", `waiting${node.dependsOn.length > 0 ? " for dependencies" : ""}`)
						: node.progress !== undefined
							? style.fg("muted", progressLine(node.progress, width - 4))
							: style.fg("dim", "no output yet");
	const stats: string[] = [];
	if (node.status === "pending") stats.push(waited === undefined ? "waiting" : `waiting ${waited}`);
	else if (time !== undefined) stats.push(time);
	if (node.turns !== undefined) stats.push(`${node.turns} turn${node.turns === 1 ? "" : "s"}`);
	if (node.toolCalls !== undefined) stats.push(`${node.toolCalls} tool${node.toolCalls === 1 ? "" : "s"}`);
	const lines = [
		head,
		style.fg("muted", `${node.name} · ${meta}`),
		body,
		style.fg("dim", stats.length === 0 ? "—" : stats.join(" · ")),
	];
	return drawBox(lines, width, selected ? "accent" : "dim", style, selected);
}

/** Pack hint items into lines of `width`, two spaces apart. */
export function packHints(items: readonly string[], width: number): string[] {
	const lines: string[] = [];
	let current = "";
	for (const item of items) {
		const candidate = current.length === 0 ? item : `${current}  ${item}`;
		if (visibleWidth(candidate) <= width || current.length === 0) current = candidate;
		else {
			lines.push(current);
			current = item;
		}
	}
	if (current.length > 0) lines.push(current);
	return lines.map((line) => cut(line, width));
}

interface Body {
	readonly lines: string[];
	/** First and last body line of each node's box. */
	readonly spans: Map<string, readonly [number, number]>;
}

/** The scrolled body of the pane: waves of boxes with arrows, the dependencies, and the node cards. */
export function renderDagBody(
	run: DagRun,
	state: RlmPaneState,
	width: number,
	options: { style?: RlmStyle; now?: number } = {},
): Body {
	const style = options.style ?? PLAIN_STYLE;
	const now = options.now ?? Date.now();
	const bound = Math.max(1, width);
	const selected = selectedNode(run, state);
	const byKey = new Map(run.nodes.map((node) => [node.key, node]));
	const lines: string[] = [];
	const spans = new Map<string, readonly [number, number]>();
	const center = (text: string) => {
		const pad = Math.max(0, Math.floor((bound - visibleWidth(text)) / 2));
		return cut(`${" ".repeat(pad)}${text}`, bound);
	};
	run.waves.forEach((wave, waveIndex) => {
		const lone = wave.length === 1 ? naturalBoxWidth(byKey.get(wave[0]!)!, run) : 0;
		const { perRow, boxWidth } = waveGeometry(wave.length, bound, lone);
		for (let start = 0; start < wave.length; start += perRow) {
			if (start > 0) lines.push("");
			const row = wave.slice(start, start + perRow).map((key) => byKey.get(key)!);
			const boxes = row.map((node) =>
				nodeBox(node, run, boxWidth, node.key === selected, state.folded.has(node.key), style, now),
			);
			const height = Math.max(...boxes.map((box) => box.length));
			const rowWidth = row.length * boxWidth + GAP * (row.length - 1);
			const indent = " ".repeat(Math.max(0, Math.floor((bound - rowWidth) / 2)));
			const top = lines.length;
			for (let line = 0; line < height; line++)
				lines.push(
					cut(indent + boxes.map((box) => box[line] ?? " ".repeat(boxWidth)).join(" ".repeat(GAP)), bound),
				);
			for (const [index, node] of row.entries()) spans.set(node.key, [top, top + boxes[index]!.length - 1]);
		}
		if (wave.length > 1) lines.push(cut(style.fg("dim", "  · Same frontier"), bound));
		if (waveIndex < run.waves.length - 1) lines.push(center(style.fg("dim", "│")), center(style.fg("accent", "▼")));
	});
	lines.push("", style.bold(style.fg("text", "Dependencies")));
	if (run.edges.length === 0)
		lines.push(
			style.fg("dim", cut(run.kind === "turn" ? "  none · each cell's work is one frontier" : "  none", bound)),
		);
	const EDGE_LIMIT = 40;
	for (const [from, to] of run.edges.slice(0, EDGE_LIMIT))
		lines.push(
			cut(
				`  ${style.fg("text", byKey.get(from)?.name ?? from)} ${style.fg("dim", "→")} ${style.fg("text", byKey.get(to)?.name ?? to)}`,
				bound,
			),
		);
	if (run.edges.length > EDGE_LIMIT)
		lines.push(style.fg("dim", cut(`  … ${run.edges.length - EDGE_LIMIT} more`, bound)));
	if (state.details) {
		lines.push("", style.bold(style.fg("text", "Node details")));
		for (const key of nodeOrder(run)) {
			const node = byKey.get(key)!;
			lines.push(...cardLines(node, run, bound, key === selected, style, now).map((line) => cut(line, bound)));
		}
	}
	return { lines, spans };
}

/**
 * Render the pane into exactly `height` lines of at most `width` columns: a fixed header, the scrolled body and a
 * fixed footer (connection, scroll position, key hints). Returns the state with the scroll it used.
 */
export function renderRlmPane(
	runs: readonly DagRun[],
	state: RlmPaneState,
	width: number,
	height: number,
	options: DagRenderOptions = {},
): { lines: string[]; state: RlmPaneState; run?: DagRun; bodyLength: number } {
	const style = options.style ?? PLAIN_STYLE;
	const now = options.now ?? Date.now();
	const bound = Math.max(1, width);
	const rows = Math.max(1, height);
	const run = currentRun(runs, state);
	const header: string[] = [];
	if (run === undefined) {
		header.push(style.bold(style.fg("accent", "RLM")));
		header.push(style.fg("muted", "RLM pane"));
	} else {
		const counts = runCounts(run);
		const index = runs.indexOf(run) + 1;
		header.push(`${style.bold(style.fg("accent", "RLM"))} ${style.fg("dim", "·")} ${style.fg("muted", run.id)}`);
		header.push(
			`${style.fg("muted", `RLM pane / ${run.kind === "turn" ? "turn" : "workflow"}`)}  ${style.fg("dim", `Tasks (${options.taskCount ?? 0})`)}`,
		);
		const counter = `${index}/${runs.length}`;
		const title = cut(run.title, Math.max(1, bound - visibleWidth(counter) - 2));
		header.push(`${style.bold(style.fg("text", title))}  ${style.fg("dim", counter)}`);
		const word =
			counts.running > 0 ? "Running" : counts.pending > 0 ? "Pending" : counts.failed > 0 ? "Failed" : "Finished";
		const color: Color =
			counts.running > 0 ? "accent" : counts.failed > 0 ? "error" : counts.pending > 0 ? "dim" : "success";
		const extra = [
			`Done ${counts.done}/${counts.total}`,
			...(counts.running > 0 ? [`${counts.running} running`] : []),
			...(counts.failed > 0 ? [`${counts.failed} failed`] : []),
			`wave ${counts.wave}/${counts.waves}`,
		];
		header.push(`${style.fg(color, word)} ${style.fg("dim", "·")} ${style.fg("muted", extra.join(" · "))}`);
	}
	const body: Body =
		run === undefined
			? {
					lines: [
						"",
						style.fg("dim", "No RLM work in this turn yet."),
						style.fg("dim", "Spawned children and workflows.run nodes"),
						style.fg("dim", "appear here as dependency waves."),
					],
					spans: new Map(),
				}
			: renderDagBody(run, state, bound, { style, now });

	// Footer: connection and scroll position, then the key hints.
	const hints = [...(options.focusHint ? [options.focusHint] : []), ...(options.hints ?? [])];
	const connection =
		options.error !== undefined
			? style.fg("error", `● ${cut(`Inspection failed: ${options.error}`, Math.max(1, bound - 14))}`)
			: style.fg("success", "● Connected");
	let footerHints = packHints(hints, bound).map((line) => style.fg(options.focused === false ? "dim" : "muted", line));
	let separator = [style.fg("dim", "─".repeat(bound))];
	let blank = [""];
	// Short panes keep the body readable: drop the spacing, then fold the hints to one line.
	if (rows < header.length + 1 + 2 + footerHints.length + 6) {
		blank = [];
		separator = [];
		footerHints = footerHints.length > 0 ? [cut(footerHints.join("  "), bound)] : [];
	}
	const fixed = header.length + blank.length + separator.length + 1 + footerHints.length;
	const room = Math.max(1, rows - fixed);
	const maxScroll = Math.max(0, body.lines.length - room);
	let scroll = Math.max(0, Math.min(state.scroll, maxScroll));
	const selected = run === undefined ? undefined : selectedNode(run, state);
	if (state.reveal && selected !== undefined) {
		const span = body.spans.get(selected);
		if (span !== undefined) {
			const [top, bottom] = span;
			if (top < scroll) scroll = top;
			else if (bottom >= scroll + room) scroll = Math.min(top, bottom - room + 1);
		}
		scroll = Math.max(0, Math.min(scroll, maxScroll));
	}
	const visible = body.lines.slice(scroll, scroll + room);
	const total = body.lines.length;
	const position = total === 0 ? "0/0" : `${scroll + 1}-${Math.min(total, scroll + room)}/${total}`;
	const status = cut(`${connection}  ${style.fg("dim", position)}`, bound);
	const lines = [...header, ...blank, ...visible];
	while (lines.length < rows - (separator.length + 1 + footerHints.length)) lines.push("");
	lines.push(...separator, status, ...footerHints);
	// A pane shorter than its header and footer keeps the top lines.
	const out = lines.slice(0, rows).map((line) => cut(line, bound));
	while (out.length < rows) out.push("");
	return {
		lines: out,
		state: {
			...state,
			scroll,
			// Neither the run nor the node is pinned here: until the viewer picks them, the pane follows the newest
			// live run and its first running node.
			reveal: false,
		},
		...(run === undefined ? {} : { run }),
		bodyLength: total,
	};
}

// ---------------------------------------------------------------------------------------------
// The live wave summary above the editor.

export interface WaveSummaryOptions {
	readonly style?: RlmStyle;
	readonly now?: number;
	/** Node lines at most (the last becomes "+N more" when there are more). */
	readonly maxNodes?: number;
}

/** Nodes that make a run worth summarizing while they wait or run: tasks, children, workflow nodes, fan-outs. */
const SUMMARIZED: ReadonlySet<DagNodeKind> = new Set(["task", "child", "background", "workflow", "fanout"]);

/**
 * The runs whose work the RLM pane shows and that count as new work for its auto-open: runs with a spawned child, a
 * typed agent or background task, a `workflows.run` node or an `rlm.map` fan-out that is running or waiting, and,
 * while a turn runs (`since` is its start), the turn's own run and the workflow runs it started even when their work
 * already finished between two polls.
 */
export function paneWorkRuns(runs: readonly DagRun[], since?: number): DagRun[] {
	return runs.filter((run) => {
		const nodes = run.nodes.filter((node) => SUMMARIZED.has(node.kind));
		if (nodes.length === 0) return false;
		if (nodes.some((node) => isActive(node.status))) return true;
		if (since === undefined) return false;
		return run.kind === "turn" || (run.startedAt !== undefined && run.startedAt >= since - 1000);
	});
}

/**
 * "▶ <title> running wave i/N  d/T done, r running" and one line per node, while a run's children or workflow nodes
 * run or wait; empty otherwise (the footer's one-line summary remains).
 */
export function renderWaveSummary(runs: readonly DagRun[], width: number, options: WaveSummaryOptions = {}): string[] {
	const style = options.style ?? PLAIN_STYLE;
	const now = options.now ?? Date.now();
	const bound = Math.max(1, width);
	const run = [...runs]
		.reverse()
		.find((candidate) => candidate.nodes.some((node) => SUMMARIZED.has(node.kind) && isActive(node.status)));
	if (run === undefined) return [];
	const counts = runCounts(run);
	const state = counts.running > 0 ? "running" : "waiting";
	const tail = ` ${state} wave ${counts.wave}/${counts.waves}  ${counts.done}/${counts.total} done, ${counts.running} running`;
	const titleRoom = Math.max(4, bound - 2 - visibleWidth(tail));
	const lines = [
		cut(
			`${style.fg("accent", "▶")} ${style.bold(style.fg("text", cut(run.title, titleRoom)))}${style.fg("muted", tail)}`,
			bound,
		),
	];
	const rank = (status: GraphStatus) =>
		status === "running" ? 0 : status === "pending" ? 1 : status === "failed" || status === "incomplete" ? 2 : 3;
	const nodes = [...run.nodes].sort((left, right) => rank(left.status) - rank(right.status));
	const max = Math.max(1, options.maxNodes ?? 4);
	const shown = nodes.length > max ? nodes.slice(0, max - 1) : nodes;
	for (const node of shown) {
		const head = `${node.name} · category:${node.category}`;
		const time = elapsedOf(node, now);
		let tail: string;
		let said: string | undefined;
		if (node.status === "pending") {
			const waited = run.startedAt !== undefined && now >= run.startedAt ? formatDuration(now - run.startedAt) : "";
			tail = ` · waiting${waited ? ` ${waited}` : ""}`;
		} else {
			said = node.status === "failed" ? node.note : (node.text ?? (node.status === "done" ? node.note : undefined));
			tail = time ? ` · ${time}` : "";
		}
		// The time stays visible: the assistant text gets what is left of the line.
		const room = bound - 4 - visibleWidth(head) - visibleWidth(tail) - 3;
		const text = said !== undefined && room >= 8 ? ` · ${cut(said, room)}` : "";
		const glyph = node.status === "running" ? "▶" : dot(node.status);
		const color: Color = node.status === "pending" ? "dim" : node.status === "failed" ? "error" : "muted";
		lines.push(cut(`  ${style.fg(tone(node.status), glyph)} ${style.fg(color, `${head}${text}${tail}`)}`, bound));
	}
	if (nodes.length > shown.length) lines.push(cut(style.fg("dim", `  +${nodes.length - shown.length} more`), bound));
	return lines;
}

// ---------------------------------------------------------------------------------------------
// Keys and the TUI component.

type KeyLabels = Pick<KeybindingsManager, "getKeys" | "matches">;

const PANE_ACTIONS: readonly (readonly [DagAction | "close", string])[] = [
	["close", "app.rlm.pane.close"],
	["next", "app.rlm.pane.next"],
	["prev", "app.rlm.pane.prev"],
	["fold", "app.rlm.pane.fold"],
	["details", "app.rlm.pane.details"],
	["scrollUp", "app.rlm.pane.scrollUp"],
	["scrollDown", "app.rlm.pane.scrollDown"],
	["pageUp", "app.rlm.pane.pageUp"],
	["pageDown", "app.rlm.pane.pageDown"],
	["prevRun", "app.rlm.pane.prevRun"],
	["nextRun", "app.rlm.pane.nextRun"],
];

const KEY_NAMES: Record<string, string> = {
	tab: "Tab",
	"shift+tab": "Shift-Tab",
	space: "Space",
	enter: "Enter",
	escape: "Esc",
	up: "↑",
	down: "↓",
	left: "←",
	right: "→",
	pageUp: "PgUp",
	pageDown: "PgDn",
};

/** A key as the pane shows it: "Tab", "Shift-Tab", "↑", "alt+w" stays as is. */
export function keyName(key: string): string {
	return KEY_NAMES[key] ?? key;
}

function keysOf(keybindings: KeyLabels, action: string): string[] {
	return (keybindings.getKeys(action as never) as string[]).map(keyName);
}

/** Footer hints from the live bindings: "Tab/n next", "Shift-Tab/p prev", "Space/Enter fold", "d Details", ... */
export function rlmPaneHints(keybindings: KeyLabels): string[] {
	const label = (action: string) => keysOf(keybindings, action).join("/") || "unbound";
	const pair = (first: string, second: string) => {
		const a = keysOf(keybindings, first)[0] ?? "?";
		const b = keysOf(keybindings, second)[0] ?? "?";
		return a.length === 1 && b.length === 1 ? `${a}${b}` : `${a}/${b}`;
	};
	return [
		`${label("app.rlm.pane.next")} next`,
		`${label("app.rlm.pane.prev")} prev`,
		`${label("app.rlm.pane.fold")} fold`,
		`${label("app.rlm.pane.details")} Details`,
		`${pair("app.rlm.pane.scrollUp", "app.rlm.pane.scrollDown")} Scroll`,
		`${pair("app.rlm.pane.prevRun", "app.rlm.pane.nextRun")} Runs`,
		`${label("app.rlm.pane.close")} Close`,
	];
}

export interface RlmPaneOptions {
	readonly snapshot: () => RlmSnapshot;
	/** Rows the pane fills (the full terminal height beside the chat). */
	readonly height: () => number;
	readonly keybindings: KeyLabels;
	readonly style?: RlmStyle;
	/** Whether the pane has the input focus. */
	readonly focused: () => boolean;
	/** The key that moves focus between the chat and the pane (`app.rlm.pane`). */
	readonly toggleKey?: () => string | undefined;
	readonly onClose: () => void;
	readonly requestRender: () => void;
}

/** The RLM pane: reads a live snapshot on every render; one column of left padding next to the split border. */
export class RlmPane implements Component {
	#state = initialDagState();
	#runs: DagRun[] = [];
	private readonly options: RlmPaneOptions;

	constructor(options: RlmPaneOptions) {
		this.options = options;
	}

	get state(): RlmPaneState {
		return this.#state;
	}

	handleInput(data: string): void {
		const keys = this.options.keybindings;
		const action = PANE_ACTIONS.find(([, id]) => keys.matches(data, id as never))?.[0];
		if (action === undefined) return;
		if (action === "close") {
			this.options.onClose();
			return;
		}
		if (this.#runs.length === 0) this.#runs = buildDagRuns(this.options.snapshot());
		this.#state = applyDagAction(this.#state, this.#runs, action, Math.max(1, this.options.height() - 12));
		this.options.requestRender();
	}

	render(width: number): string[] {
		const snapshot = this.options.snapshot();
		this.#runs = buildDagRuns(snapshot);
		const inner = Math.max(1, width - 1);
		const focused = this.options.focused();
		const toggle = this.options.toggleKey?.();
		const rendered = renderRlmPane(this.#runs, this.#state, inner, Math.max(1, this.options.height()), {
			...(this.options.style === undefined ? {} : { style: this.options.style }),
			now: snapshot.now,
			hints: rlmPaneHints(this.options.keybindings),
			focused,
			...(toggle === undefined ? {} : { focusHint: `${toggle} ${focused ? "chat" : "focus"}` }),
			...(snapshot.error === undefined ? {} : { error: snapshot.error }),
			taskCount: snapshot.tasks.length,
		});
		this.#state = rendered.state;
		return rendered.lines.map((line) => ` ${line}`);
	}

	invalidate(): void {}
}
