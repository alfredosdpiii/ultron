/**
 * Pure rendering for the native TUI's Jev panel: Jev's recent decisions in time order (triage routes,
 * memory recall gates, retention policy), plus availability and ledger activity. Input comes from
 * the read-only `jev.decisions` inspection request and the `agents.status` usage ledger.
 */
import { truncateToWidth } from "@earendil-works/pi-tui";
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
}

export interface JevSnapshot {
	readonly now: number;
	/** Null when the worker has not answered `jev.decisions` (older worker or no Ultron runtime). */
	readonly available: { readonly jev: boolean; readonly hindsight: boolean } | null;
	readonly decisions: readonly JevDecisionView[];
	/** Settled Jev calls in the usage ledger. */
	readonly ledgerCalls?: number;
	/** Jev calls currently reserved in the usage ledger. */
	readonly inFlight?: number;
	readonly error?: string;
}

export interface JevRenderOptions {
	readonly style?: RlmStyle;
	/** Maximum decision rows (newest kept). */
	readonly maxRows?: number;
}

/** A decision this recent keeps the collapsed Jev line visible. */
export const JEV_RECENT_MS = 5 * 60_000;

export function summarizeJev(snapshot: JevSnapshot): string {
	const counts = { triage: 0, recall: 0, retain: 0, errors: 0 };
	for (const decision of snapshot.decisions) {
		if (decision.kind === "triage") counts.triage++;
		else if (decision.kind === "recall") counts.recall++;
		else if (decision.kind === "retain") counts.retain++;
		if (decision.status === "error") counts.errors++;
	}
	const parts: string[] = [];
	if (snapshot.available?.jev === false) parts.push("not configured");
	if (snapshot.decisions.length === 0) parts.push("no decisions");
	if (counts.triage > 0) parts.push(`${counts.triage} triage`);
	if (counts.recall > 0) {
		const retrieved = snapshot.decisions.filter((d) => d.kind === "recall" && d.retrieve === true).length;
		parts.push(`${counts.recall} recall (${retrieved} retrieved)`);
	}
	if (counts.retain > 0) parts.push(`${counts.retain} retain`);
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

function decisionColor(decision: JevDecisionView): "accent" | "success" | "error" | "warning" | "muted" {
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
	return decision.action === "keep" ? "+" : "·";
}

export function renderJevPanel(snapshot: JevSnapshot, width: number, options: JevRenderOptions = {}): string[] {
	const style = options.style ?? PLAIN_STYLE;
	const maxRows = Math.max(1, options.maxRows ?? 12);
	const lines: string[] = [`${style.bold(style.fg("accent", "Jev"))} ${style.fg("muted", summarizeJev(snapshot))}`];
	if (snapshot.error !== undefined) lines.push(style.fg("error", `inspection failed: ${snapshot.error}`));
	const available = snapshot.available;
	if (available === null) {
		lines.push(style.fg("warning", "decision log unavailable in this session"));
	} else {
		const jev = available.jev
			? style.fg("success", "jev ✓ configured")
			: style.fg("warning", "jev ✗ not configured (set TYPESAFE_API_KEY)");
		const hindsight = available.hindsight
			? style.fg("success", "hindsight ✓ configured")
			: style.fg("warning", "hindsight ✗ not configured (set ULTRON_HINDSIGHT_URL)");
		lines.push(`${jev}${style.fg("dim", " · ")}${hindsight}`);
	}
	const ordered = [...snapshot.decisions].sort((left, right) => left.at - right.at);
	const shown = ordered.slice(-maxRows);
	if (ordered.length > shown.length) lines.push(style.fg("dim", `+${ordered.length - shown.length} earlier`));
	for (const decision of shown) {
		const age = `${formatDuration(Math.max(0, snapshot.now - decision.at))} ago`.padStart(9);
		const kind = decision.kind.padEnd(6);
		const color = decisionColor(decision);
		let line = `${style.fg("dim", age)} ${style.fg(color, glyph(decision))} ${style.fg("text", kind)} ${style.fg(color, describeDecision(decision))}`;
		if (typeof decision.durationMs === "number" && decision.status !== "unavailable") {
			line += style.fg("dim", ` ${formatDuration(decision.durationMs)}`);
		}
		if (decision.inputSha256) line += style.fg("dim", ` #${decision.inputSha256.slice(0, 6)}`);
		lines.push(line);
	}
	const bound = Math.max(1, width);
	return lines.map((line) => truncateToWidth(line, bound, "…"));
}

/** Collapsed one-liner: shown while the panel is hidden and Jev decided something recently. */
export function renderJevStatusLine(
	snapshot: JevSnapshot,
	width: number,
	options: JevRenderOptions = {},
): string | undefined {
	const latest = snapshot.decisions.reduce<JevDecisionView | undefined>(
		(best, decision) => (best === undefined || decision.at >= best.at ? decision : best),
		undefined,
	);
	const busy = typeof snapshot.inFlight === "number" && snapshot.inFlight > 0;
	if (!busy && (latest === undefined || snapshot.now - latest.at > JEV_RECENT_MS)) return undefined;
	const style = options.style ?? PLAIN_STYLE;
	const last = latest === undefined ? "" : ` · last ${latest.kind} ${describeDecision(latest)}`;
	return truncateToWidth(
		`${style.fg("accent", "Jev ▸")} ${style.fg("muted", `${summarizeJev(snapshot)}${last}`)}`,
		Math.max(1, width),
		"…",
	);
}

function record(value: unknown): Record<string, unknown> | undefined {
	return value !== null && typeof value === "object" && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

/** Parse a `jev.decisions` response; unknown fields are dropped and malformed rows skipped. */
export function parseJevDecisions(value: unknown): Pick<JevSnapshot, "available" | "decisions"> {
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
		for (const key of ["id", "inputSha256", "route", "urgency", "category", "action", "reason"]) {
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
	return {
		available:
			availability === undefined
				? null
				: { jev: availability.jev === true, hindsight: availability.hindsight === true },
		decisions,
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
