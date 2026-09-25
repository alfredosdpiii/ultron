/**
 * Pure rendering for the native TUI's live RLM panel.
 *
 * The TUI polls read-only inspection requests (`agents.status`, `instances.list`, `rlm.pool`,
 * `progress.assess`) and reads the root `rlm` tool cell from the replicated transcript. This module
 * turns that snapshot into width-bounded lines so the layout is unit-testable without a terminal.
 */
import { truncateToWidth } from "@earendil-works/pi-tui";

export type RlmTaskState = "admitted" | "running" | "completed" | "failed" | "cancelled" | "interrupted";

export interface RlmTask {
	readonly id: string;
	readonly definition: string;
	readonly state: string;
	readonly parentId?: string;
	readonly result?: { readonly status?: string; readonly value?: unknown; readonly error?: string };
}

export interface RlmUsage {
	readonly admittedTasks?: number;
	readonly remainingWallMs?: number | null;
	readonly usage?: { readonly cost?: number | null; readonly totalTokens?: number | null };
	readonly reservations?: readonly { readonly taskId?: string; readonly admittedAt?: number }[];
}

export interface RlmLimits {
	readonly maxAdmittedTasks?: number | null;
	readonly maxWallMs?: number | null;
}

export interface RlmPool {
	readonly live: number;
	readonly maxLive: number;
	readonly lanes: readonly { readonly lane: string; readonly running: number; readonly pinnedBy: readonly string[] }[];
	readonly evictions?: number;
}

export interface RlmRootCell {
	readonly toolCallId: string;
	readonly code: string;
	readonly status: "running" | "ok" | "error";
	readonly startedAt?: number;
	readonly endedAt?: number;
	/** First part of the cell's text output (used for the error line). */
	readonly output?: string;
}

export interface RlmProgress {
	readonly classification: string;
	readonly receipts: number;
}

export interface RlmTiming {
	readonly startedAt?: number;
	readonly endedAt?: number;
}

export interface RlmSnapshot {
	readonly now: number;
	readonly tasks: readonly RlmTask[];
	readonly usage?: RlmUsage | null;
	readonly limits?: RlmLimits | null;
	readonly pool?: RlmPool | null;
	readonly rootCell?: RlmRootCell | null;
	/** Task ids that belong to an open retained instance. */
	readonly retained?: ReadonlySet<string>;
	readonly progress?: ReadonlyMap<string, RlmProgress>;
	readonly timing?: ReadonlyMap<string, RlmTiming>;
	/** Last inspection failure, shown instead of stale data being mistaken for live data. */
	readonly error?: string;
}

/** Color hooks; the TUI passes the theme, tests pass identity functions. */
export interface RlmStyle {
	fg(
		color: "accent" | "success" | "error" | "warning" | "muted" | "dim" | "text" | "toolTitle" | "mdCodeBlock",
		text: string,
	): string;
	bold(text: string): string;
}

export interface RlmRenderOptions {
	readonly style?: RlmStyle;
	/** Maximum task rows before "+N more". */
	readonly maxNodes?: number;
	/** Maximum code lines shown for the root cell. */
	readonly maxCodeLines?: number;
	readonly spinnerFrame?: number;
}

export const PLAIN_STYLE: RlmStyle = { fg: (_color, text) => text, bold: (text) => text };

const SPINNER = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TERMINAL = new Set(["completed", "failed", "cancelled", "interrupted"]);
const ORDER: Record<string, number> = {
	running: 0,
	admitted: 1,
	failed: 2,
	interrupted: 3,
	completed: 4,
	cancelled: 5,
};

export function isActiveState(state: string): boolean {
	return !TERMINAL.has(state);
}

export interface RlmTreeNode {
	readonly task: RlmTask;
	readonly children: RlmTreeNode[];
	/** Parent id that is not visible in this snapshot. */
	readonly orphanOf?: string;
}

/** Group tasks under their parents. Tasks whose parent is missing become roots marked as orphans. */
export function buildTaskTree(tasks: readonly RlmTask[]): RlmTreeNode[] {
	const nodes = new Map<string, RlmTreeNode>();
	for (const task of tasks) {
		if (nodes.has(task.id)) continue;
		nodes.set(task.id, { task, children: [] });
	}
	const roots: RlmTreeNode[] = [];
	for (const node of nodes.values()) {
		const parentId = node.task.parentId;
		const parent = parentId === undefined ? undefined : nodes.get(parentId);
		if (parentId === undefined) roots.push(node);
		else if (parent === undefined || parent === node || descendsFrom(parent, node.task.id, nodes)) {
			// Missing parent (child-lane visibility, trimmed journal) or a corrupt cycle: surface it at the top.
			roots.push({ ...node, orphanOf: parentId });
		} else parent.children.push(node);
	}
	const sort = (list: RlmTreeNode[]): void => {
		list.sort((left, right) => (ORDER[left.task.state] ?? 9) - (ORDER[right.task.state] ?? 9));
		for (const node of list) sort(node.children);
	};
	sort(roots);
	return roots;
}

function descendsFrom(node: RlmTreeNode, ancestorId: string, nodes: ReadonlyMap<string, RlmTreeNode>): boolean {
	const seen = new Set<string>();
	for (let current: RlmTreeNode | undefined = node; current !== undefined; ) {
		if (current.task.id === ancestorId) return true;
		if (seen.has(current.task.id)) return true;
		seen.add(current.task.id);
		const parentId: string | undefined = current.task.parentId;
		current = parentId === undefined ? undefined : nodes.get(parentId);
	}
	return false;
}

export interface RlmCounts {
	readonly running: number;
	readonly done: number;
	readonly failed: number;
	readonly cancelled: number;
}

export function countTasks(tasks: readonly RlmTask[]): RlmCounts {
	let running = 0;
	let done = 0;
	let failed = 0;
	let cancelled = 0;
	for (const task of tasks) {
		if (isActiveState(task.state)) running++;
		else if (task.state === "completed") done++;
		else if (task.state === "cancelled") cancelled++;
		else failed++;
	}
	return { running, done, failed, cancelled };
}

/** True when something is in flight: an active task or a running root cell. */
export function hasRlmActivity(snapshot: Pick<RlmSnapshot, "tasks" | "rootCell">): boolean {
	return snapshot.rootCell?.status === "running" || snapshot.tasks.some((task) => isActiveState(task.state));
}

/** "3 running · 5 done · 1 failed · 12/24 tasks · 18m left · $0.42" (zero counts are omitted). */
export function summarizeRlm(snapshot: RlmSnapshot): string {
	const counts = countTasks(snapshot.tasks);
	const parts: string[] = [];
	if (counts.running > 0) parts.push(`${counts.running} running`);
	if (counts.done > 0) parts.push(`${counts.done} done`);
	if (counts.failed > 0) parts.push(`${counts.failed} failed`);
	if (counts.cancelled > 0) parts.push(`${counts.cancelled} cancelled`);
	if (parts.length === 0) parts.push(snapshot.rootCell?.status === "running" ? "kernel busy" : "idle");
	const admitted = snapshot.usage?.admittedTasks;
	const maxTasks = snapshot.limits?.maxAdmittedTasks;
	if (typeof admitted === "number") {
		parts.push(typeof maxTasks === "number" ? `${admitted}/${maxTasks} tasks` : `${admitted} tasks`);
	}
	const remaining = snapshot.usage?.remainingWallMs;
	if (typeof remaining === "number")
		parts.push(remaining > 0 ? `${formatDuration(remaining)} left` : "wall budget spent");
	const cost = snapshot.usage?.usage?.cost;
	if (typeof cost === "number" && cost > 0) parts.push(`$${cost.toFixed(2)}`);
	return parts.join(" · ");
}

/** Compact one-liner for the footer when the panel is hidden; undefined when nothing is in flight. */
export function renderRlmStatusLine(
	snapshot: RlmSnapshot,
	width: number,
	options: RlmRenderOptions = {},
): string | undefined {
	if (!hasRlmActivity(snapshot)) return undefined;
	const style = options.style ?? PLAIN_STYLE;
	return truncateToWidth(
		`${style.fg("accent", "RLM ▸")} ${style.fg("muted", summarizeRlm(snapshot))}`,
		Math.max(1, width),
		"…",
	);
}

/** Full panel: header, root kernel cell, task tree (bounded), and pool line. Every line fits `width`. */
export function renderRlmPanel(snapshot: RlmSnapshot, width: number, options: RlmRenderOptions = {}): string[] {
	const style = options.style ?? PLAIN_STYLE;
	const maxNodes = Math.max(1, options.maxNodes ?? 15);
	const maxCodeLines = Math.max(0, options.maxCodeLines ?? 3);
	const frame = SPINNER[Math.abs(options.spinnerFrame ?? 0) % SPINNER.length]!;
	const lines: string[] = [];

	lines.push(`${style.bold(style.fg("accent", "RLM"))} ${style.fg("muted", summarizeRlm(snapshot))}`);
	if (snapshot.error !== undefined) lines.push(style.fg("error", `inspection failed: ${oneLine(snapshot.error)}`));

	const cell = snapshot.rootCell;
	if (cell) {
		const glyph =
			cell.status === "running"
				? style.fg("accent", frame)
				: cell.status === "ok"
					? style.fg("success", "✓")
					: style.fg("error", "✗");
		const end = cell.status === "running" ? snapshot.now : cell.endedAt;
		const duration =
			cell.startedAt !== undefined && end !== undefined
				? ` ${formatDuration(Math.max(0, end - cell.startedAt))}`
				: "";
		const statusColor = cell.status === "running" ? "accent" : cell.status === "ok" ? "success" : "error";
		let head = `${glyph} ${style.fg("toolTitle", style.bold("root kernel"))} ${style.fg(statusColor, cell.status)}${style.fg("dim", duration)}`;
		if (cell.status === "error" && cell.output) head += `  ${style.fg("error", oneLine(cell.output))}`;
		lines.push(head);
		const codeLines = cell.code
			.split("\n")
			.map((line) => line.replace(/\t/g, "  ").trimEnd())
			.filter((line) => line.trim().length > 0);
		for (const line of codeLines.slice(0, maxCodeLines))
			lines.push(style.fg("dim", "│ ") + style.fg("mdCodeBlock", line));
		if (codeLines.length > maxCodeLines && maxCodeLines > 0) {
			lines.push(style.fg("dim", `│ … ${codeLines.length - maxCodeLines} more lines`));
		}
	} else {
		lines.push(style.fg("dim", "root kernel idle (no rlm cell yet)"));
	}

	const tree = buildTaskTree(snapshot.tasks);
	if (tree.length === 0) {
		lines.push(style.fg("dim", "no tasks"));
	} else {
		const rows: string[] = [];
		let total = 0;
		let hiddenActive = 0;
		const walk = (nodes: readonly RlmTreeNode[], prefix: string, greyed: boolean): void => {
			nodes.forEach((node, index) => {
				const last = index === nodes.length - 1;
				const grey = greyed || node.task.state === "cancelled";
				total++;
				if (rows.length < maxNodes) {
					rows.push(renderNode(node, `${prefix}${last ? "└─ " : "├─ "}`, grey, snapshot, style, frame));
				} else if (isActiveState(node.task.state)) hiddenActive++;
				walk(node.children, `${prefix}${last ? "   " : "│  "}`, grey);
			});
		};
		walk(tree, "", false);
		lines.push(...rows);
		if (total > rows.length) {
			const more = total - rows.length;
			lines.push(style.fg("dim", `+${more} more${hiddenActive > 0 ? ` (${hiddenActive} active)` : ""}`));
		}
	}

	const pool = snapshot.pool;
	if (pool) {
		const pinned = pool.lanes.filter((lane) => lane.pinnedBy.length > 0).length;
		const busy = pool.lanes.filter((lane) => lane.running > 0).length;
		const parts = [`kernels ${pool.live}/${pool.maxLive} live`, `${busy} busy`, `${pinned} pinned`];
		if (typeof pool.evictions === "number") parts.push(`${pool.evictions} evicted`);
		lines.push(style.fg("dim", parts.join(" · ")));
	}

	const bound = Math.max(1, width);
	return lines.map((line) => truncateToWidth(line, bound, "…"));
}

function renderNode(
	node: RlmTreeNode,
	branch: string,
	grey: boolean,
	snapshot: RlmSnapshot,
	style: RlmStyle,
	frame: string,
): string {
	const task = node.task;
	const paint = grey ? (_color: Parameters<RlmStyle["fg"]>[0], text: string) => text : style.fg;
	const glyph = stateGlyph(task.state, frame);
	const segments: string[] = [
		paint(stateColor(task.state), glyph),
		paint("text", task.definition),
		paint("dim", shortId(task.id)),
		paint(stateColor(task.state), task.state),
	];
	const timing = snapshot.timing?.get(task.id);
	if (timing?.startedAt !== undefined) {
		const end = timing.endedAt ?? (isActiveState(task.state) ? snapshot.now : undefined);
		if (end !== undefined) segments.push(paint("dim", formatDuration(Math.max(0, end - timing.startedAt))));
	}
	if (snapshot.retained?.has(task.id)) segments.push(paint("accent", "◆ retained"));
	if (node.orphanOf !== undefined) segments.push(paint("warning", `↑${shortId(node.orphanOf)}?`));
	const progress = snapshot.progress?.get(task.id);
	if (progress !== undefined && isActiveState(task.state)) {
		const color = progress.classification === "stalled" ? "warning" : "muted";
		segments.push(paint(color, `${progress.classification}·${progress.receipts}r`));
	}
	const summary = resultSummary(task);
	if (summary !== undefined) {
		segments.push(
			task.state === "completed" ? paint("muted", `→ ${summary}`) : paint(stateColor(task.state), summary),
		);
	}
	const line = style.fg("dim", branch) + segments.join(" ");
	return grey ? style.fg("dim", line) : line;
}

function stateGlyph(state: string, frame: string): string {
	switch (state) {
		case "running":
			return frame;
		case "admitted":
			return "○";
		case "completed":
			return "✓";
		case "failed":
			return "✗";
		case "interrupted":
			return "!";
		case "cancelled":
			return "⊘";
		default:
			return "?";
	}
}

function stateColor(state: string): "accent" | "muted" | "success" | "error" | "warning" | "dim" {
	switch (state) {
		case "running":
			return "accent";
		case "admitted":
			return "muted";
		case "completed":
			return "success";
		case "failed":
			return "error";
		case "interrupted":
			return "warning";
		default:
			return "dim";
	}
}

function resultSummary(task: RlmTask): string | undefined {
	const result = task.result;
	if (result === undefined) return undefined;
	if (typeof result.error === "string" && result.error.length > 0) return oneLine(result.error);
	if (result.value === undefined || result.value === null) return undefined;
	const text = typeof result.value === "string" ? result.value : safeJson(result.value);
	return oneLine(text);
}

function safeJson(value: unknown): string {
	try {
		return JSON.stringify(value) ?? String(value);
	} catch {
		return String(value);
	}
}

function oneLine(text: string): string {
	const flat = text.replace(/\s+/g, " ").trim();
	return flat.length > 200 ? `${flat.slice(0, 199)}…` : flat;
}

export function shortId(id: string): string {
	return id.replace(/^ultron-(?:task|instance)-/, "").slice(0, 8);
}

export function formatDuration(ms: number): string {
	if (ms < 1000) return `${Math.round(ms)}ms`;
	const seconds = ms / 1000;
	if (seconds < 10) return `${seconds.toFixed(1)}s`;
	if (seconds < 60) return `${Math.floor(seconds)}s`;
	const minutes = Math.floor(seconds / 60);
	if (minutes < 60) {
		const rest = Math.floor(seconds % 60);
		return rest === 0 || minutes >= 10 ? `${minutes}m` : `${minutes}m${rest}s`;
	}
	const hours = Math.floor(minutes / 60);
	const restMinutes = minutes % 60;
	return restMinutes === 0 ? `${hours}h` : `${hours}h${restMinutes}m`;
}

/**
 * Tracks first-seen and finished times. Task records carry no timestamps, so a task's start is its
 * usage reservation's admission time when available, else when the panel first saw it active; a
 * task first seen already terminal has no elapsed time rather than a misleading one.
 */
export class RlmClock {
	readonly #marks = new Map<string, { startedAt?: number; endedAt?: number }>();

	mark(key: string, active: boolean, now: number, knownStart?: number): RlmTiming {
		let mark = this.#marks.get(key);
		if (mark === undefined) {
			mark = active ? { startedAt: knownStart ?? now } : {};
			this.#marks.set(key, mark);
		} else if (active) {
			if (knownStart !== undefined) mark.startedAt = knownStart;
			mark.startedAt ??= now;
			mark.endedAt = undefined;
		} else if (mark.startedAt !== undefined && mark.endedAt === undefined) {
			mark.endedAt = now;
		}
		return { ...mark };
	}

	timings(tasks: readonly RlmTask[], usage: RlmUsage | null | undefined, now: number): Map<string, RlmTiming> {
		const admitted = new Map<string, number>();
		for (const reservation of usage?.reservations ?? []) {
			if (reservation.taskId !== undefined && typeof reservation.admittedAt === "number") {
				admitted.set(reservation.taskId, reservation.admittedAt);
			}
		}
		const result = new Map<string, RlmTiming>();
		for (const task of tasks) {
			result.set(task.id, this.mark(`task:${task.id}`, isActiveState(task.state), now, admitted.get(task.id)));
		}
		return result;
	}
}

// ---------------------------------------------------------------------------------------------
// Defensive parsing of inspection payloads (they cross a process boundary as JSON).

function record(value: unknown): Record<string, unknown> | undefined {
	return value !== null && typeof value === "object" && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

export function parseAgentsStatus(value: unknown): {
	tasks: RlmTask[];
	usage: RlmUsage | null;
	limits: RlmLimits | null;
} {
	const body = record(value);
	const tasks: RlmTask[] = [];
	for (const item of Array.isArray(body?.tasks) ? body.tasks : []) {
		const task = record(item);
		if (task === undefined || typeof task.id !== "string" || typeof task.state !== "string") continue;
		const result = record(task.result);
		tasks.push({
			id: task.id,
			definition: typeof task.definition === "string" ? task.definition : "task",
			state: task.state,
			...(typeof task.parentId === "string" ? { parentId: task.parentId } : {}),
			...(result === undefined
				? {}
				: {
						result: {
							...(typeof result.status === "string" ? { status: result.status } : {}),
							...("value" in result ? { value: result.value } : {}),
							...(typeof result.error === "string" ? { error: result.error } : {}),
						},
					}),
		});
	}
	const usage = record(body?.usage);
	const limits = record(body?.limits) ?? record(usage?.limits);
	return {
		tasks,
		usage: (usage as RlmUsage | undefined) ?? null,
		limits: (limits as RlmLimits | undefined) ?? null,
	};
}

/** Task ids owned by open retained instances (the retaining task and each invocation). */
export function parseRetained(value: unknown): Set<string> {
	const retained = new Set<string>();
	for (const item of Array.isArray(value) ? value : []) {
		const instance = record(item);
		if (instance === undefined || instance.state !== "open") continue;
		if (typeof instance.task_id === "string") retained.add(instance.task_id);
		for (const invocation of Array.isArray(instance.invocations) ? instance.invocations : []) {
			const taskId = record(invocation)?.task_id;
			if (typeof taskId === "string") retained.add(taskId);
		}
	}
	return retained;
}

export function parsePool(value: unknown): RlmPool | null {
	const body = record(value);
	if (body === undefined || typeof body.live !== "number" || typeof body.maxLive !== "number") return null;
	const lanes: { lane: string; running: number; pinnedBy: string[] }[] = [];
	for (const item of Array.isArray(body.lanes) ? body.lanes : []) {
		const lane = record(item);
		if (lane === undefined || typeof lane.lane !== "string") continue;
		lanes.push({
			lane: lane.lane,
			running: typeof lane.running === "number" ? lane.running : 0,
			pinnedBy: Array.isArray(lane.pinnedBy) ? lane.pinnedBy.filter((pin) => typeof pin === "string") : [],
		});
	}
	return {
		live: body.live,
		maxLive: body.maxLive,
		lanes,
		...(typeof body.evictions === "number" ? { evictions: body.evictions } : {}),
	};
}

export function parseProgress(value: unknown): RlmProgress | undefined {
	const body = record(value);
	if (body === undefined || typeof body.classification !== "string") return undefined;
	return { classification: body.classification, receipts: Array.isArray(body.receipts) ? body.receipts.length : 0 };
}

// ---------------------------------------------------------------------------------------------
// Root kernel cell from the replicated main-lane transcript.

interface TranscriptLike {
	readonly transcript: readonly unknown[];
	readonly operation?: {
		readonly runningTools?: readonly {
			readonly status: string;
			readonly toolCallId: string;
			readonly toolName: string;
			readonly args?: unknown;
			readonly result?: unknown;
			readonly isError?: boolean;
		}[];
	} | null;
}

function codeOf(args: unknown): string {
	const code = record(args)?.code;
	return typeof code === "string" ? code : "";
}

function textOf(result: unknown): string | undefined {
	const content = record(result)?.content;
	if (!Array.isArray(content)) return undefined;
	const text = content
		.map((part) => {
			const item = record(part);
			return item?.type === "text" && typeof item.text === "string" ? item.text : "";
		})
		.join("");
	return text.length > 0 ? text : undefined;
}

/**
 * The current or last root `rlm` cell: a running tool from the open operation first, else the last
 * `rlm` tool result in the transcript paired with its call. `clock` supplies start times for running
 * cells, which carry no timestamp of their own.
 */
export function extractRootCell(
	snapshot: TranscriptLike | undefined,
	clock: RlmClock,
	now: number,
): RlmRootCell | null {
	if (snapshot === undefined) return null;
	const running = [...(snapshot.operation?.runningTools ?? [])].reverse().find((tool) => tool.toolName === "rlm");
	if (running !== undefined) {
		const active = running.status === "running";
		const timing = clock.mark(`cell:${running.toolCallId}`, active, now);
		const output = active ? undefined : textOf(running.result);
		return {
			toolCallId: running.toolCallId,
			code: codeOf(running.args),
			status: active ? "running" : running.isError ? "error" : "ok",
			...(timing.startedAt === undefined ? {} : { startedAt: timing.startedAt }),
			...(timing.endedAt === undefined ? {} : { endedAt: timing.endedAt }),
			...(output === undefined ? {} : { output }),
		};
	}
	const entries = snapshot.transcript;
	for (let index = entries.length - 1; index >= 0; index--) {
		const message = record(record(entries[index])?.message);
		if (message?.role !== "toolResult" || message.toolName !== "rlm" || typeof message.toolCallId !== "string")
			continue;
		const toolCallId = message.toolCallId;
		let code = "";
		let startedAt: number | undefined;
		for (let back = index - 1; back >= 0; back--) {
			const assistant = record(record(entries[back])?.message);
			if (assistant?.role !== "assistant" || !Array.isArray(assistant.content)) continue;
			const call = assistant.content.map(record).find((part) => part?.type === "toolCall" && part.id === toolCallId);
			if (call === undefined) continue;
			code = codeOf(call.arguments);
			if (typeof assistant.timestamp === "number") startedAt = assistant.timestamp;
			break;
		}
		// Prefer the times observed live; fall back to message timestamps for cells from before the panel opened.
		const observed = clock.mark(`cell:${toolCallId}`, false, now);
		const endedAt = observed.endedAt ?? (typeof message.timestamp === "number" ? message.timestamp : undefined);
		const start = observed.startedAt ?? startedAt;
		const output = textOf(message);
		return {
			toolCallId,
			code,
			status: message.isError === true ? "error" : "ok",
			...(start === undefined ? {} : { startedAt: start }),
			...(endedAt === undefined ? {} : { endedAt }),
			...(output === undefined ? {} : { output }),
		};
	}
	return null;
}
