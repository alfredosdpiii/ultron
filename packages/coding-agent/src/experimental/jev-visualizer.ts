/**
 * Jev as a presence in the native TUI, not a log.
 *
 * - `renderJevPresence`: a footer line whose glyph pulses when Jev decides, with the last decision in a few words.
 * - `renderJevPanel` (/jev): health and counts, a timeline strip of recent turns, the latest recall probability and
 *   retention confidence as needles against their thresholds, the last turn as a decision pipeline (what Jev saw,
 *   what it decided, and what that caused), what Jev knows about this project (memories recalled this session), and
 *   the latest raw decisions.
 * - Transcript notes live in jev-annotations.ts.
 *
 * Input comes from the read-only `jev.decisions` inspection, the `agents.status` usage ledger, and the
 * `ultron-memory` messages in the transcript.
 */
import { truncateToWidth, visibleWidth } from "@ultron/tui";
import { DEFAULT_THRESHOLDS, type JevThresholds, type KnownMemory } from "./jev-annotations.ts";
import { formatDuration, PLAIN_STYLE, type RlmStyle } from "./rlm-visualizer.ts";

export interface JevDecisionView {
	readonly id?: string;
	readonly at: number;
	readonly kind: string;
	readonly status: string;
	readonly durationMs?: number;
	readonly inputSha256?: string;
	readonly route?: string;
	readonly routeConfidence?: number;
	readonly complexity?: number;
	readonly urgency?: string;
	readonly category?: string;
	readonly retrieve?: boolean;
	readonly probability?: number;
	readonly action?: string;
	readonly confidence?: number;
	readonly reason?: string;
	/** `auto:<runId>` (automatic memory for a root run) or `skill:<name>`. */
	readonly ref?: string;
}

export interface JevSnapshot {
	readonly now: number;
	/** Null when the worker has not answered `jev.decisions` (older worker or no Ultron runtime). */
	readonly available: { readonly jev: boolean; readonly hindsight: boolean } | null;
	readonly decisions: readonly JevDecisionView[];
	/** Gate cut-offs reported by the worker. */
	readonly thresholds?: JevThresholds;
	/** Settled Jev calls in the usage ledger. */
	readonly ledgerCalls?: number;
	/** Jev calls currently reserved in the usage ledger. */
	readonly inFlight?: number;
	readonly error?: string;
	/** Memories recalled into this session's transcript. */
	readonly memories?: readonly KnownMemory[];
	/** Memories injected per `auto:<runId>` ref (for "recalled 3"). */
	readonly recalledCounts?: ReadonlyMap<string, number>;
}

export interface JevRenderOptions {
	readonly style?: RlmStyle;
	/** Maximum raw decision rows (newest kept). */
	readonly maxRows?: number;
	/** Maximum memories in the "knows" section. */
	readonly maxMemories?: number;
	readonly spinnerFrame?: number;
}

/** A decision this recent keeps the collapsed Jev line visible. */
export const JEV_RECENT_MS = 5 * 60_000;
/** How long the presence glyph pulses after a decision. */
export const JEV_PULSE_MS = 4000;

const SPINNER = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

type Color = Parameters<RlmStyle["fg"]>[0];

export interface JevCounts {
	readonly recalls: number;
	readonly skips: number;
	readonly keeps: number;
	readonly passes: number;
	readonly refusals: number;
	readonly triage: number;
	readonly errors: number;
}

/**
 * recalls: the gate retrieved; skips: the gate said no memory; keeps: retention kept; passes: retention skipped (or a
 * keep under the threshold); refusals: retention withheld as sensitive; errors: failed or unavailable calls.
 */
export function countJev(
	decisions: readonly JevDecisionView[],
	thresholds: JevThresholds = DEFAULT_THRESHOLDS,
): JevCounts {
	const counts = { recalls: 0, skips: 0, keeps: 0, passes: 0, refusals: 0, triage: 0, errors: 0 };
	for (const decision of decisions) {
		if (decision.status !== "ok") {
			counts.errors++;
			continue;
		}
		if (decision.kind === "triage") counts.triage++;
		else if (decision.kind === "recall") {
			if (decision.retrieve) counts.recalls++;
			else counts.skips++;
		} else if (decision.kind === "retain") {
			if (decision.action === "sensitive") counts.refusals++;
			else if (kept(decision, thresholds)) counts.keeps++;
			else counts.passes++;
		}
	}
	return counts;
}

function kept(decision: JevDecisionView, thresholds: JevThresholds): boolean {
	return decision.action === "keep" && (decision.confidence ?? 1) >= thresholds.keep;
}

export function summarizeJev(snapshot: JevSnapshot): string {
	const counts = countJev(snapshot.decisions, snapshot.thresholds);
	const parts: string[] = [];
	if (snapshot.available?.jev === false) parts.push("not configured");
	if (snapshot.decisions.length === 0) parts.push("no decisions");
	if (counts.recalls + counts.skips > 0) parts.push(`${counts.recalls} recalled · ${counts.skips} skipped`);
	if (counts.keeps + counts.passes + counts.refusals > 0)
		parts.push(
			`${counts.keeps} kept · ${counts.passes} not kept${counts.refusals > 0 ? ` · ${counts.refusals} refused` : ""}`,
		);
	if (counts.triage > 0) parts.push(`${counts.triage} triage`);
	if (counts.errors > 0) parts.push(`${counts.errors} failed`);
	if (typeof snapshot.ledgerCalls === "number" && snapshot.ledgerCalls > 0)
		parts.push(`${snapshot.ledgerCalls} ledger calls`);
	if (typeof snapshot.inFlight === "number" && snapshot.inFlight > 0) parts.push(`${snapshot.inFlight} in flight`);
	return parts.join(" · ");
}

/** Plain decision text, e.g. "→ powerful 82% · debugging · cx 1.4 · normal". */
export function describeDecision(decision: JevDecisionView): string {
	if (decision.status === "unavailable") return `unavailable: ${decision.reason ?? "Jev is not configured"}`;
	if (decision.status === "error") return `failed: ${decision.reason ?? "error"}`;
	if (decision.kind === "triage") {
		const parts = [`→ ${decision.route ?? "?"}${percent(decision.routeConfidence)}`];
		if (decision.category) parts.push(decision.category);
		if (typeof decision.complexity === "number") parts.push(`cx ${decision.complexity.toFixed(1)}`);
		if (decision.urgency) parts.push(decision.urgency);
		return parts.join(" · ");
	}
	if (decision.kind === "recall") {
		const probability = typeof decision.probability === "number" ? ` p=${decision.probability.toFixed(2)}` : "";
		return `${decision.retrieve ? "retrieve" : "no recall"}${probability}`;
	}
	if (decision.kind === "retain") return `${decision.action ?? "?"}${percent(decision.confidence)}`;
	return decision.status;
}

function percent(value: number | undefined): string {
	return typeof value === "number" ? ` ${Math.round(value * 100)}%` : "";
}

function decisionColor(decision: JevDecisionView): Color {
	if (decision.status === "error") return "error";
	if (decision.status === "unavailable") return "warning";
	if (decision.kind === "triage") return "accent";
	if (decision.kind === "recall") return decision.retrieve ? "success" : "muted";
	if (decision.action === "keep") return "success";
	if (decision.action === "sensitive") return "warning";
	return "muted";
}

function glyph(decision: JevDecisionView): string {
	if (decision.status === "error") return "✗";
	if (decision.status === "unavailable") return "–";
	if (decision.kind === "triage") return "◇";
	if (decision.kind === "recall") return decision.retrieve ? "✓" : "·";
	return decision.action === "keep" ? "+" : decision.action === "sensitive" ? "!" : "·";
}

function latestOf(decisions: readonly JevDecisionView[]): JevDecisionView | undefined {
	return decisions.reduce<JevDecisionView | undefined>(
		(best, decision) => (best === undefined || decision.at >= best.at ? decision : best),
		undefined,
	);
}

/** A few words for a decision, e.g. "recalled 3 (0.83)", "kept (0.91)", "no recall (0.21)". */
export function decisionWords(
	decision: JevDecisionView,
	snapshot: Pick<JevSnapshot, "thresholds" | "recalledCounts">,
): string {
	const thresholds = snapshot.thresholds ?? DEFAULT_THRESHOLDS;
	if (decision.status === "unavailable") return "unavailable";
	if (decision.status === "error") return `failed (${decision.reason ?? "error"})`;
	const p = (value: number | undefined) => (typeof value === "number" ? ` (${value.toFixed(2)})` : "");
	if (decision.kind === "recall") {
		if (!decision.retrieve) return `no recall${p(decision.probability)}`;
		const count = decision.ref === undefined ? undefined : snapshot.recalledCounts?.get(decision.ref);
		return `recalled${count === undefined ? "" : ` ${count}`}${p(decision.probability)}`;
	}
	if (decision.kind === "retain") {
		const subject = decision.ref?.startsWith("skill:") ? `skill ${decision.ref.slice(6)} ` : "";
		if (decision.action === "sensitive") return `${subject}withheld: sensitive`;
		if (kept(decision, thresholds)) return `${subject}kept${p(decision.confidence)}`;
		return `${subject}not kept${p(decision.confidence)}`;
	}
	if (decision.kind === "triage") return `routed ${decision.route ?? "?"}${percent(decision.routeConfidence)}`;
	return decision.status;
}

/**
 * Level (a): Jev's presence in the footer. The glyph pulses for a few seconds after a decision and spins while a Jev
 * call is in flight; the text is the last decision in a few words. Undefined when Jev is off and has nothing to show.
 */
export function renderJevPresence(
	snapshot: JevSnapshot,
	width: number,
	options: JevRenderOptions = {},
): string | undefined {
	const latest = latestOf(snapshot.decisions);
	const busy = typeof snapshot.inFlight === "number" && snapshot.inFlight > 0;
	const configured = snapshot.available?.jev === true;
	if (!busy && latest === undefined && !configured) return undefined;
	const style = options.style ?? PLAIN_STYLE;
	const frame = options.spinnerFrame ?? Math.floor(snapshot.now / 100);
	const age = latest === undefined ? Number.POSITIVE_INFINITY : snapshot.now - latest.at;
	let mark: string;
	if (busy) mark = style.fg("accent", SPINNER[Math.abs(frame) % SPINNER.length]!);
	else if (age < JEV_PULSE_MS)
		mark = Math.floor(snapshot.now / 300) % 2 === 0 ? style.bold(style.fg("accent", "✦")) : style.fg("accent", "⌁");
	else mark = style.fg(latest === undefined ? "dim" : "muted", "⌁");
	let text: string;
	if (busy) text = "jev: thinking…";
	else if (latest === undefined) text = "jev: listening";
	else {
		text = `jev: ${decisionWords(latest, snapshot)}`;
		if (age >= 60_000) text += ` · ${formatDuration(age)} ago`;
	}
	const color: Color = latest && age < JEV_PULSE_MS && !busy ? decisionColor(latest) : "muted";
	return truncateToWidth(`${mark} ${style.fg(color, text)}`, Math.max(1, width), "…");
}

// ---------------------------------------------------------------------------------------------
// Turns: Jev's decisions grouped by the root run they served.

export interface JevTurn {
	readonly at: number;
	readonly ref?: string;
	readonly recall?: JevDecisionView;
	readonly retain?: JevDecisionView;
	readonly triage: readonly JevDecisionView[];
	/** Skill proposals judged during this turn. */
	readonly skills: readonly JevDecisionView[];
}

/**
 * Group decisions into turns: decisions sharing an `auto:` ref are one turn; untagged recall decisions start a turn
 * and the next untagged retention joins it; triage and skill decisions join the turn they fall in.
 */
export function groupJevTurns(decisions: readonly JevDecisionView[]): JevTurn[] {
	const ordered = [...decisions].sort((left, right) => left.at - right.at);
	type Draft = {
		at: number;
		ref?: string;
		recall?: JevDecisionView;
		retain?: JevDecisionView;
		triage: JevDecisionView[];
		skills: JevDecisionView[];
	};
	const turns: Draft[] = [];
	const byRef = new Map<string, Draft>();
	let open: Draft | undefined;
	for (const decision of ordered) {
		const ref = decision.ref;
		if (ref?.startsWith("auto:")) {
			let turn = byRef.get(ref);
			if (turn === undefined) {
				turn = { at: decision.at, ref, triage: [], skills: [] };
				byRef.set(ref, turn);
				turns.push(turn);
			}
			if (decision.kind === "recall") turn.recall ??= decision;
			else if (decision.kind === "retain") turn.retain ??= decision;
			open = turn;
			continue;
		}
		if (decision.kind === "recall") {
			open = { at: decision.at, recall: decision, triage: [], skills: [] };
			turns.push(open);
			continue;
		}
		if (decision.kind === "retain" && !ref?.startsWith("skill:")) {
			if (open !== undefined && open.retain === undefined && open.ref === undefined) open.retain = decision;
			else turns.push({ at: decision.at, retain: decision, triage: [], skills: [] });
			continue;
		}
		if (open === undefined) {
			open = { at: decision.at, triage: [], skills: [] };
			turns.push(open);
		}
		if (decision.kind === "triage") open.triage.push(decision);
		else open.skills.push(decision);
	}
	return turns;
}

interface Token {
	readonly text: string;
	readonly color: Color;
}

function turnTokens(turn: JevTurn, thresholds: JevThresholds): Token[] {
	const tokens: Token[] = [];
	for (const triage of turn.triage)
		tokens.push({
			text: triage.status === "ok" ? "T" : "T✗",
			color: triage.status === "ok" ? "accent" : "error",
		});
	if (turn.recall) {
		const recall = turn.recall;
		tokens.push(
			recall.status !== "ok"
				? { text: "R✗", color: "error" }
				: recall.retrieve
					? { text: "R✓", color: "success" }
					: { text: "R·", color: "muted" },
		);
	}
	if (turn.retain) {
		const retain = turn.retain;
		tokens.push(
			retain.status !== "ok"
				? { text: "K✗", color: "error" }
				: retain.action === "sensitive"
					? { text: "K!", color: "warning" }
					: kept(retain, thresholds)
						? { text: "K✓", color: "success" }
						: { text: "K·", color: "muted" },
		);
	}
	for (const skill of turn.skills)
		tokens.push({
			text: skill.status !== "ok" ? "S✗" : skill.action === "keep" ? "S✓" : "S·",
			color: skill.status !== "ok" ? "error" : "accent",
		});
	return tokens;
}

/** The timeline strip: one cluster per turn, oldest to newest, newest kept when it does not fit. */
export function renderJevTimeline(
	turns: readonly JevTurn[],
	width: number,
	style: RlmStyle,
	thresholds: JevThresholds = DEFAULT_THRESHOLDS,
): string {
	const label = style.fg("dim", "turns ");
	const room = Math.max(1, width - 6);
	const clusters: string[] = [];
	let used = 0;
	let dropped = 0;
	for (const turn of [...turns].reverse()) {
		const tokens = turnTokens(turn, thresholds);
		if (tokens.length === 0) continue;
		const plain = tokens.map((token) => token.text).join("");
		const cost = visibleWidth(plain) + (clusters.length === 0 ? 0 : 1);
		if (dropped > 0 || used + cost > room - 2) {
			dropped++;
			continue;
		}
		used += cost;
		clusters.unshift(tokens.map((token) => style.fg(token.color, token.text)).join(""));
	}
	if (clusters.length === 0) return `${label}${style.fg("dim", "none yet")}`;
	return truncateToWidth(
		`${label}${dropped > 0 ? style.fg("dim", "… ") : ""}${clusters.join(" ")}`,
		Math.max(1, width),
		"…",
	);
}

export const TIMELINE_LEGEND = "R recall · K keep · T triage · S skill  ✓ yes · no ! sensitive ✗ failed";

/**
 * A score against its threshold: `recall ━━━━━━━━━┃━━●━━━ 0.83 ≥ 0.65 → recalled`. The rail left of the score is
 * filled, the threshold is a bar, and the verdict is what the gate did.
 */
export function renderNeedle(
	label: string,
	value: number | undefined,
	threshold: number,
	verdict: string,
	width: number,
	style: RlmStyle,
	pass: boolean,
): string {
	const cells = Math.max(8, Math.min(24, width - 40));
	const at = (fraction: number) => Math.max(0, Math.min(cells - 1, Math.round(fraction * (cells - 1))));
	const mark = at(threshold);
	const position = value === undefined ? -1 : at(value);
	let rail = "";
	for (let index = 0; index < cells; index++) {
		if (index === position) rail += style.fg(pass ? "success" : "warning", "●");
		else if (index === mark) rail += style.fg("accent", "┃");
		else if (position >= 0 && index < position) rail += style.fg(pass ? "success" : "muted", "━");
		else rail += style.fg("dim", "─");
	}
	const score = value === undefined ? "  –" : value.toFixed(2);
	const comparison = value === undefined ? "" : ` ${value >= threshold ? "≥" : "<"} ${threshold.toFixed(2)}`;
	return truncateToWidth(
		`${style.fg("dim", label.padEnd(7))}${rail} ${style.fg("text", score)}${style.fg("dim", comparison)} ${style.fg(pass ? "success" : "muted", `→ ${verdict}`)}`,
		Math.max(1, width),
		"…",
	);
}

/**
 * Jev's reasoning for one turn as a pipeline: what it was asked, each gate with its score against the threshold, and
 * what that caused. `ask #a1b2c3 ─▶ gate 0.83≥0.65 ✓ ─▶ recalled 3 ─▶ answer ─▶ keep 0.91≥0.65 stored`.
 */
export function renderJevPipeline(
	turn: JevTurn | undefined,
	snapshot: Pick<JevSnapshot, "thresholds" | "recalledCounts">,
	width: number,
	style: RlmStyle,
): string[] {
	if (turn === undefined) return [];
	const thresholds = snapshot.thresholds ?? DEFAULT_THRESHOLDS;
	const arrow = style.fg("dim", " ─▶ ");
	const stages: string[] = [];
	const sha = turn.recall?.inputSha256 ?? turn.retain?.inputSha256;
	stages.push(style.fg("muted", `ask${sha ? ` #${sha.slice(0, 6)}` : ""}`));
	for (const triage of turn.triage)
		stages.push(
			style.fg(
				triage.status === "ok" ? "accent" : "error",
				triage.status === "ok" ? `triage ${triage.route ?? "?"}${percent(triage.routeConfidence)}` : "triage ✗",
			),
		);
	const recall = turn.recall;
	if (recall) {
		if (recall.status !== "ok") stages.push(style.fg("error", `gate ✗ ${recall.reason ?? ""}`.trim()));
		else {
			const p = recall.probability;
			const cmp =
				typeof p === "number"
					? ` ${p.toFixed(2)}${p >= thresholds.recall ? "≥" : "<"}${thresholds.recall.toFixed(2)}`
					: "";
			stages.push(style.fg(recall.retrieve ? "success" : "muted", `gate${cmp} ${recall.retrieve ? "✓" : "·"}`));
			const count = turn.ref ? snapshot.recalledCounts?.get(turn.ref) : undefined;
			stages.push(
				style.fg(
					recall.retrieve ? "success" : "dim",
					recall.retrieve ? `recalled${count === undefined ? "" : ` ${count}`}` : "no memory",
				),
			);
		}
	}
	stages.push(style.fg("muted", "answer"));
	const retain = turn.retain;
	if (retain) {
		if (retain.status !== "ok") stages.push(style.fg("error", `retain ✗ ${retain.reason ?? ""}`.trim()));
		else {
			const c = retain.confidence;
			const cmp =
				typeof c === "number"
					? ` ${c.toFixed(2)}${retain.action === "keep" ? (c >= thresholds.keep ? "≥" : "<") + thresholds.keep.toFixed(2) : ""}`
					: "";
			const verdict = retain.action === "sensitive" ? "withheld" : kept(retain, thresholds) ? "stored" : "dropped";
			const color: Color =
				retain.action === "sensitive" ? "warning" : kept(retain, thresholds) ? "success" : "muted";
			stages.push(style.fg(color, `${retain.action ?? "?"}${cmp} ${verdict}`));
		}
	} else stages.push(style.fg("dim", "retention pending"));
	// Wrap stages onto as many lines as needed (bounded to 3) instead of cutting the verdict off.
	const lines: string[] = [];
	let current = style.fg("dim", "last  ");
	for (const [index, stage] of stages.entries()) {
		const piece = index === 0 ? stage : `${arrow}${stage}`;
		if (visibleWidth(current) + visibleWidth(piece) > width && visibleWidth(current) > 6) {
			lines.push(current);
			current = `      ${arrow.trimStart()}${stage}`;
		} else current += piece;
	}
	lines.push(current);
	return lines.slice(0, 3).map((line) => truncateToWidth(line, Math.max(1, width), "…"));
}

/** Level (c): the /jev view. */
export function renderJevPanel(snapshot: JevSnapshot, width: number, options: JevRenderOptions = {}): string[] {
	const style = options.style ?? PLAIN_STYLE;
	const bound = Math.max(1, width);
	const thresholds = snapshot.thresholds ?? DEFAULT_THRESHOLDS;
	const lines: string[] = [];
	const presence = renderJevPresence(snapshot, bound, options) ?? style.fg("dim", "⌁ jev: off");
	lines.push(`${style.bold(style.fg("accent", "Jev"))} ${presence}`);
	if (snapshot.error !== undefined) lines.push(style.fg("error", `inspection failed: ${snapshot.error}`));

	// Health and counts.
	const available = snapshot.available;
	if (available === null) lines.push(style.fg("warning", "decision log unavailable in this session"));
	else {
		const jev = available.jev
			? style.fg("success", "● jev")
			: style.fg("warning", "○ jev off (set TYPESAFE_API_KEY)");
		const hindsight = available.hindsight
			? style.fg("success", "● hindsight")
			: style.fg("warning", "○ hindsight off");
		const counts = countJev(snapshot.decisions, thresholds);
		const tally = [
			`${counts.recalls} recalled`,
			`${counts.skips} skipped`,
			`${counts.keeps} kept`,
			`${counts.refusals} refused`,
			...(counts.errors > 0 ? [style.fg("error", `${counts.errors} failed`)] : []),
			...(typeof snapshot.ledgerCalls === "number" && snapshot.ledgerCalls > 0
				? [`${snapshot.ledgerCalls} calls`]
				: []),
		];
		lines.push(`${jev}  ${hindsight}  ${style.fg("muted", tally.join(" · "))}`);
	}

	const turns = groupJevTurns(snapshot.decisions);
	lines.push(renderJevTimeline(turns, bound, style, thresholds));
	if (bound >= 60) lines.push(style.fg("dim", `      ${TIMELINE_LEGEND}`));

	// Latest scores against their thresholds.
	const latestRecall = latestOf(snapshot.decisions.filter((item) => item.kind === "recall" && item.status === "ok"));
	const latestRetain = latestOf(
		snapshot.decisions.filter(
			(item) => item.kind === "retain" && item.status === "ok" && !item.ref?.startsWith("skill:"),
		),
	);
	if (latestRecall)
		lines.push(
			renderNeedle(
				"recall",
				latestRecall.probability,
				thresholds.recall,
				latestRecall.retrieve ? "recalled" : "no recall",
				bound,
				style,
				latestRecall.retrieve === true,
			),
		);
	if (latestRetain)
		lines.push(
			renderNeedle(
				"keep",
				latestRetain.confidence,
				thresholds.keep,
				latestRetain.action === "sensitive"
					? "withheld (sensitive)"
					: kept(latestRetain, thresholds)
						? "kept"
						: `not kept (${latestRetain.action})`,
				bound,
				style,
				kept(latestRetain, thresholds),
			),
		);

	lines.push(...renderJevPipeline(turns.at(-1), snapshot, bound, style));

	// What Jev knows about this project.
	const memories = snapshot.memories ?? [];
	const maxMemories = Math.max(1, options.maxMemories ?? 5);
	const scopes = [...new Set(memories.map((memory) => memory.scope).filter((scope) => scope !== undefined))];
	const knows =
		memories.length === 0
			? "nothing recalled this session yet"
			: `${memories.length} memor${memories.length === 1 ? "y" : "ies"} recalled this session${scopes.length > 0 ? ` (${scopes.join(", ")})` : ""}`;
	lines.push(`${style.fg("toolTitle", style.bold("knows"))} ${style.fg("muted", knows)}`);
	for (const memory of memories.slice(0, maxMemories)) {
		const times = memory.times > 1 ? style.fg("dim", ` ×${memory.times}`) : "";
		const label = memory.label
			? style.fg("dim", ` [${memory.label}]`)
			: memory.legacy
				? style.fg("dim", " [pi]")
				: "";
		const text = memory.text.replace(/\s+/g, " ");
		const tail = `${times}${label}`;
		const room = Math.max(8, bound - 4 - visibleWidth(tail));
		lines.push(`  ${style.fg("accent", "•")} ${style.fg("text", truncateToWidth(text, room, "…"))}${tail}`);
	}
	if (memories.length > maxMemories) lines.push(style.fg("dim", `  … ${memories.length - maxMemories} more`));

	// Raw decisions, newest last.
	const maxRows = Math.max(1, options.maxRows ?? 4);
	const ordered = [...snapshot.decisions].sort((left, right) => left.at - right.at);
	const shown = ordered.slice(-maxRows);
	if (shown.length > 0)
		lines.push(
			`${style.fg("toolTitle", style.bold("log"))} ${style.fg("dim", ordered.length > shown.length ? `latest ${shown.length} of ${ordered.length}` : `${shown.length}`)}`,
		);
	for (const decision of shown) {
		const age = `${formatDuration(Math.max(0, snapshot.now - decision.at))} ago`.padStart(9);
		const color = decisionColor(decision);
		let line = `${style.fg("dim", age)} ${style.fg(color, glyph(decision))} ${style.fg("text", decision.kind.padEnd(6))} ${style.fg(color, describeDecision(decision))}`;
		if (typeof decision.durationMs === "number" && decision.status !== "unavailable")
			line += style.fg("dim", ` ${formatDuration(decision.durationMs)}`);
		if (decision.ref) line += style.fg("dim", ` ${decision.ref.startsWith("auto:") ? "auto" : decision.ref}`);
		lines.push(line);
	}
	return lines.map((line) => truncateToWidth(line, bound, "…"));
}

function record(value: unknown): Record<string, unknown> | undefined {
	return value !== null && typeof value === "object" && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

/** Parse a `jev.decisions` response; unknown fields are dropped and malformed rows skipped. */
export function parseJevDecisions(value: unknown): Pick<JevSnapshot, "available" | "decisions" | "thresholds"> {
	const body = record(value);
	const availability = record(body?.available);
	const decisions: JevDecisionView[] = [];
	for (const item of Array.isArray(body?.decisions) ? body.decisions : []) {
		const row = record(item);
		if (row === undefined || typeof row.at !== "number" || typeof row.kind !== "string") continue;
		const pick = <T>(key: string, type: "string" | "number" | "boolean"): T | undefined =>
			typeof row[key] === type ? (row[key] as T) : undefined;
		const view: Record<string, unknown> = {
			at: row.at,
			kind: row.kind,
			status: pick<string>("status", "string") ?? "ok",
		};
		for (const key of ["id", "inputSha256", "route", "urgency", "category", "action", "reason", "ref"]) {
			const text = pick<string>(key, "string");
			if (text !== undefined) view[key] = text;
		}
		for (const key of ["durationMs", "routeConfidence", "complexity", "probability", "confidence"]) {
			const number = pick<number>(key, "number");
			if (number !== undefined) view[key] = number;
		}
		const retrieve = pick<boolean>("retrieve", "boolean");
		if (retrieve !== undefined) view.retrieve = retrieve;
		decisions.push(view as unknown as JevDecisionView);
	}
	const thresholds = record(body?.thresholds);
	return {
		available:
			availability === undefined
				? null
				: { jev: availability.jev === true, hindsight: availability.hindsight === true },
		decisions,
		...(typeof thresholds?.recall === "number" && typeof thresholds.keep === "number"
			? { thresholds: { recall: thresholds.recall, keep: thresholds.keep } }
			: {}),
	};
}

/** Jev activity from the usage ledger inside an `agents.status` response. */
export function parseJevLedger(agentsStatus: unknown): { ledgerCalls?: number; inFlight?: number } {
	const usage = record(record(agentsStatus)?.usage);
	if (usage === undefined) return {};
	const totals = record(usage.usage);
	const reservations = Array.isArray(usage.reservations) ? usage.reservations : [];
	return {
		...(typeof totals?.jevCalls === "number" ? { ledgerCalls: totals.jevCalls } : {}),
		inFlight: reservations.filter((reservation) => record(reservation)?.kind === "jev").length,
	};
}
