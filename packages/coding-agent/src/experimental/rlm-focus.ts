/**
 * Full-screen RLM graph (level c): the live graph with a selection, keyboard navigation, per-node details and
 * collapse. Navigation is a pure reducer over the laid-out rows (`applyFocusAction`) so it is testable without a
 * terminal; `RlmGraphFocus` is the TUI component that renders it and maps keys through the keybindings.
 */
import type { Component, KeybindingsManager } from "@ultron/tui";
import { truncateToWidth, visibleWidth } from "@ultron/tui";
import {
	budgetGauges,
	buildRlmGraph,
	type CollapseState,
	defaultCollapsed,
	frameTraceDetails,
	type GraphNode,
	type GraphRow,
	graphActive,
	graphTotals,
	layoutGraph,
	renderDetails,
	renderGauges,
	renderGraphRow,
	renderKernelStrip,
	spinnerGlyph,
} from "./rlm-graph.ts";
import { PLAIN_STYLE, type RlmSnapshot, type RlmStyle } from "./rlm-visualizer.ts";

export type FocusAction = "up" | "down" | "pageUp" | "pageDown" | "home" | "end" | "details" | "collapse";

export interface FocusState {
	/** Selected node key; kept across refreshes while the node exists. */
	readonly selectedKey?: string;
	/** Last selected row index, used when the selected node disappears. */
	readonly selectedIndex: number;
	/** Nodes whose details are open. */
	readonly details: ReadonlySet<string>;
	/** Collapse overrides (true collapsed, false expanded). */
	readonly collapse: CollapseState;
}

export function initialFocusState(): FocusState {
	return { selectedIndex: 0, details: new Set(), collapse: new Map() };
}

/** The selected row index for `rows`, following the node key when it still exists. */
export function selectedRow(state: FocusState, rows: readonly GraphRow[]): number {
	if (rows.length === 0) return -1;
	const byKey = state.selectedKey === undefined ? -1 : rows.findIndex((row) => row.node.key === state.selectedKey);
	return byKey !== -1 ? byKey : Math.max(0, Math.min(rows.length - 1, state.selectedIndex));
}

function select(state: FocusState, rows: readonly GraphRow[], index: number): FocusState {
	const clamped = Math.max(0, Math.min(rows.length - 1, index));
	const row = rows[clamped];
	return { ...state, selectedIndex: clamped, ...(row === undefined ? {} : { selectedKey: row.node.key }) };
}

/**
 * Apply one navigation action. `rows` is the current layout (with `state.collapse` applied); `page` is how many
 * rows a page moves. `collapse` toggles the selected subtree; on a leaf it folds the parent and selects it.
 */
export function applyFocusAction(
	state: FocusState,
	rows: readonly GraphRow[],
	action: FocusAction,
	page = 10,
): FocusState {
	const index = selectedRow(state, rows);
	if (index === -1) return state;
	const row = rows[index]!;
	switch (action) {
		case "up":
			return select(state, rows, index - 1);
		case "down":
			return select(state, rows, index + 1);
		case "pageUp":
			return select(state, rows, index - Math.max(1, page));
		case "pageDown":
			return select(state, rows, index + Math.max(1, page));
		case "home":
			return select(state, rows, 0);
		case "end":
			return select(state, rows, rows.length - 1);
		case "details": {
			const details = new Set(state.details);
			if (details.has(row.node.key)) details.delete(row.node.key);
			else details.add(row.node.key);
			return { ...select(state, rows, index), details };
		}
		case "collapse": {
			const collapse = new Map(state.collapse);
			if (row.node.children.length > 0) {
				collapse.set(row.node.key, !row.collapsed);
				return { ...select(state, rows, index), collapse };
			}
			const parentKey = row.parentKey;
			if (parentKey === undefined || parentKey === "turn") return state;
			collapse.set(parentKey, true);
			const parentIndex = rows.findIndex((candidate) => candidate.node.key === parentKey);
			return { ...state, collapse, selectedKey: parentKey, selectedIndex: Math.max(0, parentIndex) };
		}
	}
}

/** Whether a node is shown collapsed under `collapse` (for callers outside the layout). */
export function isCollapsed(node: GraphNode, collapse: CollapseState): boolean {
	return node.children.length > 0 && (collapse.get(node.key) ?? defaultCollapsed(node));
}

export interface FocusRenderOptions {
	readonly style?: RlmStyle;
	readonly spinnerFrame?: number;
	/** Hint line text (keys), shown under the header. */
	readonly hints?: string;
	/** Lazily loaded detail lines by node key (frame traces). */
	readonly extraDetails?: ReadonlyMap<string, readonly (readonly [string, string])[]>;
	/** Previous first body line, so the view scrolls only when the selection leaves it. */
	readonly scroll?: number;
}

/**
 * Render the focus view into exactly `height` lines (padded), returning the body scroll offset it used. The body
 * scrolls to keep the selected node and its open details visible.
 */
export function renderFocusView(
	snapshot: RlmSnapshot,
	state: FocusState,
	width: number,
	height: number,
	options: FocusRenderOptions = {},
): { lines: string[]; scroll: number; rows: GraphRow[] } {
	const style = options.style ?? PLAIN_STYLE;
	const bound = Math.max(1, width);
	const spinner = spinnerGlyph(options.spinnerFrame ?? 0);
	const graph = buildRlmGraph(snapshot);
	const rows = layoutGraph(graph, state.collapse);
	const selected = selectedRow(state, rows);
	const selectedKey = rows[selected]?.node.key;
	const totals = graphTotals(graph);
	const status = graphActive(graph)
		? style.fg("accent", `${spinner} running`)
		: graph.status === "failed"
			? style.fg("error", "✗ failed")
			: style.fg("success", "✓ settled");
	const head: string[] = [
		`${style.bold(style.fg("accent", "RLM graph"))} ${status} ${style.fg("muted", `${totals.nodes - 1} nodes${totals.running > 0 ? ` · ${totals.running} active` : ""}${totals.failed > 0 ? ` · ${totals.failed} failed` : ""}`)}`,
	];
	if (snapshot.error !== undefined) head.push(style.fg("error", `inspection failed: ${snapshot.error}`));
	if (options.hints) head.push(style.fg("dim", options.hints));
	head.push(...renderGauges(budgetGauges(snapshot, graph), bound, style, 2));
	const strip = renderKernelStrip(snapshot, bound, style);
	if (strip) head.push(strip);
	head.push(style.fg("dim", "─".repeat(bound)));

	const rowOptions = {
		style,
		now: snapshot.now,
		spinner,
		gutter: true,
		...(selectedKey === undefined ? {} : { selectedKey }),
	};
	const body: string[] = [];
	let selectedLine = 0;
	let selectedSpan = 1;
	rows.forEach((row, index) => {
		if (index === selected) selectedLine = body.length;
		body.push(renderGraphRow(row, bound, rowOptions));
		if (state.details.has(row.node.key)) {
			const details = renderDetails(row, options.extraDetails?.get(row.node.key), bound, rowOptions);
			body.push(...details);
			if (index === selected) selectedSpan = 1 + details.length;
		}
	});
	const room = Math.max(1, height - head.length);
	const previous = options.scroll ?? 0;
	let scroll = Math.min(previous, selectedLine);
	if (selectedLine + Math.min(selectedSpan, room) > scroll + room)
		scroll = selectedLine + Math.min(selectedSpan, room) - room;
	scroll = Math.max(0, Math.min(scroll, Math.max(0, body.length - room)));
	const visible = body.slice(scroll, scroll + room);
	if (scroll > 0 && visible.length > 0)
		visible[0] = style.fg("dim", truncateToWidth(`  ↑ ${scroll} more above`, bound, "…"));
	const hiddenBelow = body.length - (scroll + room);
	if (hiddenBelow > 0 && visible.length > 1)
		visible[visible.length - 1] = style.fg("dim", truncateToWidth(`  ↓ ${hiddenBelow} more below`, bound, "…"));
	const lines = [...head, ...visible].slice(0, Math.max(1, height)).map((line) => truncateToWidth(line, bound, "…"));
	while (lines.length < height) lines.push("");
	return { lines, scroll, rows };
}

type KeyLabels = Pick<KeybindingsManager, "getKeys" | "matches">;

const ACTIONS: readonly (readonly [FocusAction, string])[] = [
	["up", "app.rlm.graph.up"],
	["down", "app.rlm.graph.down"],
	["pageUp", "app.rlm.graph.pageUp"],
	["pageDown", "app.rlm.graph.pageDown"],
	["details", "app.rlm.graph.details"],
	["collapse", "app.rlm.graph.collapse"],
];

function keyLabel(keybindings: KeyLabels, action: string): string {
	return (keybindings.getKeys(action as never) as string[]).join("/") || "unbound";
}

/** Key hints from the live bindings, e.g. "up/k down/j move · enter details · c collapse · escape back". */
export function focusHints(keybindings: KeyLabels): string {
	return [
		`${keyLabel(keybindings, "app.rlm.graph.up")} ${keyLabel(keybindings, "app.rlm.graph.down")} move`,
		`${keyLabel(keybindings, "app.rlm.graph.details")} details`,
		`${keyLabel(keybindings, "app.rlm.graph.collapse")} collapse`,
		`${keyLabel(keybindings, "app.rlm.graph.exit")} back`,
	].join(" · ");
}

export interface RlmGraphFocusOptions {
	readonly snapshot: () => RlmSnapshot;
	readonly height: () => number;
	readonly keybindings: KeyLabels;
	readonly style?: RlmStyle;
	readonly onExit: () => void;
	readonly requestRender: () => void;
	/** Fetch one frame's trace (`rlm.frames {id}`) for its details. */
	readonly loadTrace?: (traceId: string) => Promise<unknown>;
}

/** The focus view as a TUI component: reads a live snapshot on every render, keys through the keybindings. */
export class RlmGraphFocus implements Component {
	#state = initialFocusState();
	#scroll = 0;
	#rows: GraphRow[] = [];
	readonly #extra = new Map<string, readonly (readonly [string, string])[]>();
	readonly #loading = new Set<string>();
	focused = false;
	private readonly options: RlmGraphFocusOptions;

	constructor(options: RlmGraphFocusOptions) {
		this.options = options;
	}

	get state(): FocusState {
		return this.#state;
	}

	handleInput(data: string): void {
		const keys = this.options.keybindings;
		if (keys.matches(data, "app.rlm.graph.exit" as never)) {
			this.options.onExit();
			return;
		}
		const action = ACTIONS.find(([, id]) => keys.matches(data, id as never))?.[0];
		if (action === undefined) return;
		if (this.#rows.length === 0)
			this.#rows = layoutGraph(buildRlmGraph(this.options.snapshot()), this.#state.collapse);
		const page = Math.max(1, this.options.height() - 8);
		this.#state = applyFocusAction(this.#state, this.#rows, action, page);
		if (action === "details") this.#loadDetails();
		this.options.requestRender();
	}

	#loadDetails(): void {
		const key = this.#state.selectedKey;
		const row = this.#rows.find((candidate) => candidate.node.key === key);
		const traceId = row?.node.traceId;
		const load = this.options.loadTrace;
		if (key === undefined || traceId === undefined || load === undefined) return;
		if (!this.#state.details.has(key) || this.#extra.has(key) || this.#loading.has(key)) return;
		this.#loading.add(key);
		void load(traceId)
			.then((trace) => this.#extra.set(key, frameTraceDetails(trace)))
			.catch((error: unknown) =>
				this.#extra.set(key, [["trace", `unavailable: ${error instanceof Error ? error.message : String(error)}`]]),
			)
			.finally(() => {
				this.#loading.delete(key);
				this.options.requestRender();
			});
	}

	render(width: number): string[] {
		const rendered = renderFocusView(
			this.options.snapshot(),
			this.#state,
			width,
			Math.max(6, this.options.height()),
			{
				...(this.options.style === undefined ? {} : { style: this.options.style }),
				spinnerFrame: Math.floor(Date.now() / 100),
				hints: focusHints(this.options.keybindings),
				extraDetails: this.#extra,
				scroll: this.#scroll,
			},
		);
		this.#scroll = rendered.scroll;
		this.#rows = rendered.rows;
		// Keep the selection on a real row after the graph changed shape.
		const index = selectedRow(this.#state, this.#rows);
		const row = this.#rows[index];
		if (row !== undefined && row.node.key !== this.#state.selectedKey)
			this.#state = { ...this.#state, selectedKey: row.node.key, selectedIndex: index };
		return rendered.lines.map((line) => (visibleWidth(line) > width ? truncateToWidth(line, width, "…") : line));
	}

	invalidate(): void {}
}
