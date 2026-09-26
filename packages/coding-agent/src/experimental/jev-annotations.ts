/**
 * Jev in the transcript: muted one-line notes next to the turns Jev shaped. Before a turn, what memory Jev
 * injected and why (the recall gate's probability against its threshold, how many memories, the first one's first
 * words; expandable to the full list); after the answer, whether Jev kept the turn.
 *
 * Correlation: automatic memory tags its gate and retention decisions with `auto:<runId>`, the same id as the
 * `ultron-memory` message's `taskId`, so a turn with injected memory matches exactly. A turn without injected
 * memory (the gate said no) matches by time: the first automatic decision between its user message and the next.
 */
import type { Component } from "@ultron/tui";
import { truncateToWidth, visibleWidth, wrapTextWithAnsi } from "@ultron/tui";
import type { JevDecisionView } from "./jev-visualizer.ts";
import { PLAIN_STYLE, type RlmStyle } from "./rlm-visualizer.ts";

export const MEMORY_MESSAGE_TYPE = "ultron-memory";

export interface MemoryItem {
	readonly text: string;
	/** Evidence class label, e.g. "user statement". */
	readonly label?: string;
	/** From the read-only Pi bank. */
	readonly legacy?: boolean;
}

export interface MemoryNote {
	/** `auto:<runId>` of the run it was injected into. */
	readonly taskId?: string;
	readonly scope?: string;
	readonly count: number;
	readonly items: readonly MemoryItem[];
}

export interface JevThresholds {
	readonly recall: number;
	readonly keep: number;
}

export const DEFAULT_THRESHOLDS: JevThresholds = { recall: 0.65, keep: 0.65 };

function record(value: unknown): Record<string, unknown> | undefined {
	return value !== null && typeof value === "object" && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

function contentText(content: unknown): string {
	if (typeof content === "string") return content;
	if (!Array.isArray(content)) return "";
	return content
		.map((part) => {
			const item = record(part);
			return item?.type === "text" && typeof item.text === "string" ? item.text : "";
		})
		.join("\n");
}

/** Parse an `ultron-memory` custom message; undefined for any other message. */
export function parseMemoryMessage(message: unknown): MemoryNote | undefined {
	const body = record(message);
	if (body?.role !== "custom" || body.customType !== MEMORY_MESSAGE_TYPE) return undefined;
	const details = record(body.details);
	const items: MemoryItem[] = [];
	let legacy = false;
	for (const raw of contentText(body.content).split("\n")) {
		const line = raw.trim();
		if (line.startsWith("Earlier memory (from Pi")) {
			legacy = true;
			continue;
		}
		const match = /^\d+\.\s+(?:\[([^\]]+)\]\s+)?(.+)$/.exec(line);
		if (!match) continue;
		items.push({
			text: match[2]!.trim(),
			...(match[1] ? { label: match[1] } : {}),
			...(legacy ? { legacy: true } : {}),
		});
	}
	return {
		...(typeof details?.taskId === "string" ? { taskId: details.taskId } : {}),
		...(typeof details?.scope === "string" ? { scope: details.scope } : {}),
		count: typeof details?.count === "number" ? details.count : items.filter((item) => !item.legacy).length,
		items,
	};
}

export interface KnownMemory {
	readonly text: string;
	readonly label?: string;
	readonly scope?: string;
	/** Turns it was recalled into. */
	readonly times: number;
	readonly legacy?: boolean;
}

/** Every memory recalled in a transcript, deduplicated by text, most recalled first then newest. */
export function collectKnownMemories(transcript: readonly unknown[]): KnownMemory[] {
	const known = new Map<string, { item: KnownMemory; last: number }>();
	transcript.forEach((entry, index) => {
		const note = parseMemoryMessage(record(entry)?.message);
		if (!note) return;
		for (const item of note.items) {
			const key = item.text.replace(/\s+/g, " ").toLowerCase();
			const previous = known.get(key);
			known.set(key, {
				item: {
					text: item.text,
					...(item.label ? { label: item.label } : {}),
					...(note.scope ? { scope: note.scope } : {}),
					...(item.legacy ? { legacy: true } : {}),
					times: (previous?.item.times ?? 0) + 1,
				},
				last: index,
			});
		}
	});
	return [...known.values()]
		.sort((left, right) => right.item.times - left.item.times || right.last - left.last)
		.map((entry) => entry.item);
}

export interface TurnWindow {
	/** User message time. */
	readonly at: number;
	/** Next user message time (exclusive), if any. */
	readonly nextAt?: number;
	/** `auto:<runId>` from the turn's memory message. */
	readonly ref?: string;
}

/** Jev's automatic decisions for one turn: the recall gate and the retention policy. */
export function matchTurnDecisions(
	turn: TurnWindow,
	decisions: readonly JevDecisionView[],
): { recall?: JevDecisionView; retain?: JevDecisionView } {
	const automatic = (decision: JevDecisionView) => decision.ref === undefined || decision.ref.startsWith("auto:");
	// The next turn's gate can be stamped just before its user message: keep a little room at both ends.
	const inWindow = (decision: JevDecisionView, slack: number) =>
		decision.at >= turn.at - slack &&
		(turn.nextAt === undefined || decision.at < turn.nextAt - Math.min(slack, (turn.nextAt - turn.at) / 2));
	let ref = turn.ref;
	let recall = ref === undefined ? undefined : decisions.find((item) => item.kind === "recall" && item.ref === ref);
	if (recall === undefined) {
		// The gate runs as the run starts, which can stamp it just before the user message is stored.
		// The closest gate decision to the user message, so a neighbouring turn's gate never wins.
		const distance = (item: JevDecisionView) => Math.abs(item.at - turn.at);
		recall = decisions
			.filter((item) => item.kind === "recall" && automatic(item) && inWindow(item, 2000))
			.reduce<JevDecisionView | undefined>(
				(best, item) => (best === undefined || distance(item) < distance(best) ? item : best),
				undefined,
			);
		ref ??= recall?.ref;
	}
	const retain =
		(ref === undefined ? undefined : decisions.find((item) => item.kind === "retain" && item.ref === ref)) ??
		decisions.find(
			(item) =>
				item.kind === "retain" &&
				automatic(item) &&
				(ref === undefined || item.ref === undefined) &&
				inWindow(item, 0),
		);
	return { ...(recall === undefined ? {} : { recall }), ...(retain === undefined ? {} : { retain }) };
}

function score(value: number | undefined): string {
	return typeof value === "number" ? value.toFixed(2) : "?";
}

function firstWords(text: string, words = 7): string {
	const parts = text.replace(/\s+/g, " ").trim().split(" ");
	return parts.length <= words ? parts.join(" ") : `${parts.slice(0, words).join(" ")}…`;
}

export interface NoteOptions {
	readonly style?: RlmStyle;
	readonly thresholds?: JevThresholds;
	/** Show every recalled memory under the note. */
	readonly expanded?: boolean;
	/** Key that expands notes, for the hint. */
	readonly expandKey?: string;
	/** Maximum memories listed when expanded. */
	readonly maxItems?: number;
}

/** The note under a user message: what Jev recalled for this turn and why. Empty when Jev did not weigh in. */
export function renderRecallNote(
	recall: JevDecisionView | undefined,
	memory: MemoryNote | undefined,
	width: number,
	options: NoteOptions = {},
): string[] {
	if (recall === undefined && memory === undefined) return [];
	const style = options.style ?? PLAIN_STYLE;
	const threshold = options.thresholds?.recall ?? DEFAULT_THRESHOLDS.recall;
	const bound = Math.max(1, width);
	const glyph = style.fg("accent", "⌁");
	if (recall?.status === "error" || recall?.status === "unavailable") {
		return [
			truncateToWidth(
				`${glyph} ${style.fg("warning", `jev recall ${recall.status === "error" ? "failed" : "unavailable"}${recall.reason ? `: ${recall.reason}` : ""}`)}`,
				bound,
				"…",
			),
		];
	}
	const probability = recall?.probability;
	const gate =
		typeof probability === "number"
			? `p ${score(probability)} ${probability >= threshold ? "≥" : "<"} ${threshold.toFixed(2)}`
			: undefined;
	if (memory === undefined || memory.items.length === 0) {
		if (recall?.retrieve === true)
			return [
				truncateToWidth(
					`${glyph} ${style.fg("muted", `jev looked for memory${gate ? ` · ${gate}` : ""} · nothing relevant found`)}`,
					bound,
					"…",
				),
			];
		return [
			truncateToWidth(`${glyph} ${style.fg("dim", `jev: no memory needed${gate ? ` · ${gate}` : ""}`)}`, bound, "…"),
		];
	}
	const count = memory.items.length;
	const parts = [`jev recalled ${count} memor${count === 1 ? "y" : "ies"}`];
	if (gate) parts.push(gate);
	if (memory.scope) parts.push(memory.scope);
	const first = memory.items[0]!;
	const hint = options.expandKey
		? style.fg("dim", options.expanded ? `  ${options.expandKey} fold` : `  ${options.expandKey} expand`)
		: "";
	const summary = `${glyph} ${style.fg("muted", parts.join(" · "))}`;
	// The quote gives way to the hint: it is fitted into whatever room is left.
	const room = bound - visibleWidth(summary) - visibleWidth(hint) - 5;
	const quote =
		options.expanded || room < 6 ? "" : style.fg("dim", ` · “${truncateToWidth(firstWords(first.text), room, "…")}”`);
	const lines = [truncateToWidth(`${summary}${quote}${hint}`, bound, "…")];
	if (!options.expanded) return lines;
	const max = Math.max(1, options.maxItems ?? 8);
	memory.items.slice(0, max).forEach((item, index) => {
		const label = item.label ? style.fg("dim", `[${item.label}] `) : "";
		const legacy = item.legacy ? style.fg("dim", "(pi) ") : "";
		const wrapped = wrapTextWithAnsi(`${label}${legacy}${style.fg("muted", item.text)}`, Math.max(10, bound - 6));
		wrapped.slice(0, 3).forEach((line, lineIndex) => {
			const number = `${index + 1}. `;
			lines.push(
				truncateToWidth(
					`  ${style.fg("dim", lineIndex === 0 ? number : " ".repeat(number.length))}${line}`,
					bound,
					"…",
				),
			);
		});
	});
	if (memory.items.length > max) lines.push(style.fg("dim", `  … ${memory.items.length - max} more`));
	return lines;
}

/** The note after the answer: did Jev keep this turn? Empty until a retention decision exists. */
export function renderRetentionNote(
	retain: JevDecisionView | undefined,
	width: number,
	options: NoteOptions = {},
): string[] {
	if (retain === undefined) return [];
	const style = options.style ?? PLAIN_STYLE;
	const threshold = options.thresholds?.keep ?? DEFAULT_THRESHOLDS.keep;
	const glyph = style.fg("accent", "⌁");
	let text: string;
	if (retain.status === "error" || retain.status === "unavailable")
		text = style.fg(
			"warning",
			`jev retention ${retain.status === "error" ? "failed" : "unavailable"}${retain.reason ? `: ${retain.reason}` : ""}`,
		);
	else if (retain.action === "keep" && (retain.confidence ?? 1) >= threshold)
		text = style.fg("success", `jev kept this turn · keep ${score(retain.confidence)} ≥ ${threshold.toFixed(2)}`);
	else if (retain.action === "keep")
		text = style.fg("muted", `jev: not kept · keep ${score(retain.confidence)} < ${threshold.toFixed(2)}`);
	else if (retain.action === "sensitive")
		text = style.fg("warning", `jev withheld this turn · sensitive ${score(retain.confidence)}`);
	else text = style.fg("dim", `jev: not kept · ${retain.action ?? "skip"} ${score(retain.confidence)}`);
	return [truncateToWidth(`${glyph} ${text}`, Math.max(1, width), "…")];
}

/** Live source for transcript notes (the TUI's latest `jev.decisions` poll). */
export interface JevNoteSource {
	decisions(): readonly JevDecisionView[];
	thresholds(): JevThresholds | undefined;
	expanded(): boolean;
	expandKey(): string | undefined;
	style(): RlmStyle;
}

/** One turn's notes as components; the chat view fills in the window and memory as entries arrive. */
export class JevTurnNotes {
	at: number;
	nextAt: number | undefined;
	memory: MemoryNote | undefined;
	readonly #source: JevNoteSource;

	constructor(source: JevNoteSource, at: number) {
		this.#source = source;
		this.at = at;
	}

	#decisions() {
		return matchTurnDecisions(
			{
				at: this.at,
				...(this.nextAt === undefined ? {} : { nextAt: this.nextAt }),
				...(this.memory?.taskId ? { ref: this.memory.taskId } : {}),
			},
			this.#source.decisions(),
		);
	}

	#options(): NoteOptions {
		const thresholds = this.#source.thresholds();
		const expandKey = this.#source.expandKey();
		return {
			style: this.#source.style(),
			expanded: this.#source.expanded(),
			...(thresholds === undefined ? {} : { thresholds }),
			...(expandKey === undefined ? {} : { expandKey }),
		};
	}

	/** Rendered under the user message. */
	readonly recall: Component = {
		render: (width) =>
			renderRecallNote(this.#decisions().recall, this.memory, Math.max(1, width - 2), this.#options()).map(
				(line) => ` ${line}`,
			),
		invalidate() {},
	};

	/** Rendered after the turn's answer. */
	readonly retention: Component = {
		render: (width) =>
			renderRetentionNote(this.#decisions().retain, Math.max(1, width - 2), this.#options()).map(
				(line) => ` ${line}`,
			),
		invalidate() {},
	};
}
