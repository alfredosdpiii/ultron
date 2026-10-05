/**
 * From an engine result to what is posted: the verdict, a few inline comments, and the body. Pure functions;
 * nothing here talks to GitHub.
 *
 * - Verdict: REQUEST_CHANGES when a confirmed finding is at or above `blockAt` (medium by default), or one from an
 *   earlier review is still present at that level; APPROVE only when coverage was complete and there is none;
 *   otherwise COMMENT. A pull request by the reviewing account, or one that is closed or merged, always gets
 *   COMMENT.
 * - Few, heavy comments: confirmed findings are ranked by level and by the strength of their evidence (a test the
 *   host ran, source quoted from outside the diff, the diff alone) and at most `maxComments` are posted inline.
 *   A finding with diff-only evidence below high gets a slot only when one is left. Nits are never inline.
 * - An inline comment sits on a diff line: its own, or the nearest within three lines of a hunk; a finding that
 *   cannot be placed is named in the body with its file and line.
 * - Inline shape: `[level] what is wrong, with the evidence. The fix.` in plain sentences. A ```suggestion block
 *   only when the finding carries an exact replacement for its line range and every line of it is in one hunk.
 * - Body shape: what was checked and holds; what must be resolved before merge; one line on the non-blocking
 *   notes; what was not checked; timing. No headings, no tables.
 */
import { SIGNATURE } from "./config.ts";
import type { ReviewCommentInput } from "./github.ts";
import type { EngineFinding, EngineResult, Level } from "./types.ts";
import { LEVELS, levelOf } from "./types.ts";

export type Verdict = "approve" | "request_changes" | "comment";

export const DEFAULT_BLOCK_AT: Level = "medium";
export const DEFAULT_MAX_COMMENTS = 5;
export const RELOCATE_WITHIN = 3;
/** An inline comment's sentences (before any code block) aim to stay under this. */
export const COMMENT_TARGET_CHARS = 600;
const MAX_BLOCKING_LISTED = 4;
const MAX_OUTSIDE_LISTED = 3;
const MAX_NOT_CHECKED = 3;
const MAX_BODY_LINES = 60;

export interface PlanOptions {
	/** The reviewing account wrote the pull request (GitHub rejects approve/request-changes on your own PR). */
	readonly selfAuthored: boolean;
	readonly state: "open" | "closed" | "merged";
	readonly headSha: string;
	/** A re-review of the changes since this commit. */
	readonly sinceSha?: string;
	readonly signature: boolean;
	/** Findings at this level or above ask for changes (default medium). */
	readonly blockAt?: Level;
	/** Inline comments posted at most (default 5). */
	readonly maxComments?: number;
}

export interface PlannedComment extends ReviewCommentInput {
	/** Index into the result's findings. */
	readonly finding: number;
	/** The marker in the body that identifies the comment when the review is read back. */
	readonly marker: string;
	readonly relocated: boolean;
	readonly suggestion: boolean;
}

export interface ReviewPlan {
	readonly verdict: Verdict;
	readonly event: "APPROVE" | "REQUEST_CHANGES" | "COMMENT";
	readonly body: string;
	readonly comments: readonly PlannedComment[];
	/** Confirmed findings named in the body instead of inline (their line is not in the diff), by index. */
	readonly inSummary: readonly number[];
	/** Confirmed findings neither inline nor named: past the comment limit, or nits. By index. */
	readonly overCap: readonly number[];
}

type Ranges = ReadonlyArray<readonly [number, number]>;

/** Where an inline comment for `line` can go: the line itself, a diff line within three lines, or nowhere. */
export function placeLine(
	ranges: Ranges | undefined,
	line: number,
): { line: number; relocated: boolean; range: readonly [number, number] } | undefined {
	if (!ranges || ranges.length === 0) return undefined;
	let best: { line: number; distance: number; range: readonly [number, number] } | undefined;
	for (const range of ranges) {
		const [start, end] = range;
		const nearest = Math.min(Math.max(line, start), end);
		const distance = Math.abs(nearest - line);
		if (best === undefined || distance < best.distance) best = { line: nearest, distance, range };
	}
	if (best === undefined || best.distance > RELOCATE_WITHIN) return undefined;
	return { line: best.line, relocated: best.distance > 0, range: best.range };
}

function fence(code: string, info = ""): string {
	const longest = Math.max(2, ...[...code.matchAll(/`+/g)].map((match) => match[0].length));
	const ticks = "`".repeat(longest + 1);
	return `${ticks}${info}\n${code}\n${ticks}`;
}

export function commentMarker(key: string): string {
	return `<!-- ultron-autoreview:${key} -->`;
}

function short(text: string, limit: number): string {
	const flat = text.replace(/\s+/g, " ").trim();
	return flat.length <= limit ? flat : `${flat.slice(0, Math.max(1, limit - 1)).trimEnd()}…`;
}

/** `text` ending as a sentence. */
function sentence(text: string): string {
	const flat = text.replace(/\s+/g, " ").trim();
	return flat === "" || /[.!?…:]$/.test(flat) ? flat : `${flat}.`;
}

const where = (finding: { file: string; line: number }) => `\`${finding.file}:${finding.line}\``;

/** `[level] what is wrong, with the evidence. The fix.`: the sentences of an inline comment, within the target. */
export function commentText(finding: EngineFinding): string {
	const head = `[${levelOf(finding)}] ${sentence(finding.claim)}`;
	const fix = finding.replacement === undefined && finding.suggestedFix ? sentence(finding.suggestedFix) : "";
	const room = COMMENT_TARGET_CHARS - head.length - (fix ? fix.length + 1 : 0) - 1;
	// The scenario is the evidence when there is one; else the reason.
	const evidence = finding.scenario?.trim() ? finding.scenario : finding.why;
	const middle = room >= 40 && evidence ? sentence(short(evidence, room)) : "";
	return [head, middle, fix].filter(Boolean).join(" ");
}

/** The inline comment of one finding, or undefined when its line cannot be placed on the diff. */
export function planComment(
	finding: EngineFinding,
	index: number,
	ranges: Ranges | undefined,
	key: string,
): PlannedComment | undefined {
	const placed = placeLine(ranges, finding.line);
	if (placed === undefined) return undefined;
	const end = finding.endLine !== undefined && finding.endLine > finding.line ? finding.endLine : finding.line;
	const spanInHunk = !placed.relocated && end >= placed.range[0] && end <= placed.range[1];
	const multi = spanInHunk && end > finding.line;
	const suggestion = finding.replacement !== undefined && spanInHunk;
	const parts = [commentText(finding)];
	if (placed.relocated) parts.push(`This is about line ${finding.line}, which is not part of the diff.`);
	if (finding.alsoAt?.length) parts.push(`Same at ${finding.alsoAt.slice(0, 6).map(where).join(", ")}.`);
	if (suggestion) parts.push(fence(finding.replacement!, "suggestion"));
	else if (finding.replacement !== undefined)
		parts.push(
			`Suggested replacement for ${end > finding.line ? `lines ${finding.line}-${end}` : `line ${finding.line}`}:\n\n${fence(finding.replacement)}`,
		);
	const marker = commentMarker(key);
	parts.push(marker);
	return {
		finding: index,
		marker,
		relocated: placed.relocated,
		suggestion,
		path: finding.file,
		line: multi ? end : placed.line,
		side: "RIGHT",
		...(multi ? { start_line: finding.line, start_side: "RIGHT" as const } : {}),
		body: parts.join("\n\n"),
	};
}

const atOrAbove = (level: Level, threshold: Level): boolean => LEVELS.indexOf(level) <= LEVELS.indexOf(threshold);

export function decideVerdict(
	result: EngineResult,
	options: Pick<PlanOptions, "selfAuthored" | "state" | "blockAt">,
): { verdict: Verdict; reason: string } {
	const blockAt = options.blockAt ?? DEFAULT_BLOCK_AT;
	const blocking = result.findings.filter(
		(finding) => finding.verification === "confirmed" && atOrAbove(levelOf(finding), blockAt),
	).length;
	const stillOpen = result.earlier.filter(
		(item) => item.status === "still_present" && atOrAbove(levelOf(item), blockAt),
	).length;
	if (options.selfAuthored) return { verdict: "comment", reason: "this account opened the pull request" };
	if (options.state !== "open") return { verdict: "comment", reason: `the pull request is ${options.state}` };
	if (blocking > 0)
		return {
			verdict: "request_changes",
			reason: `${blocking} confirmed finding${blocking === 1 ? "" : "s"} at ${blockAt} or above`,
		};
	if (stillOpen > 0)
		return {
			verdict: "request_changes",
			reason: `${stillOpen} finding${stillOpen === 1 ? "" : "s"} from an earlier review still present`,
		};
	if (!result.complete) return { verdict: "comment", reason: "the review did not cover everything" };
	return { verdict: "approve", reason: `no confirmed finding at ${blockAt} or above` };
}

const EVENTS = { approve: "APPROVE", request_changes: "REQUEST_CHANGES", comment: "COMMENT" } as const;
const VERDICT_TITLES = { approve: "Approve", request_changes: "Request changes", comment: "Comment" } as const;
const STRENGTHS = ["test", "outside", "diff"] as const;

function seconds(ms: number): string {
	return ms < 10_000 ? `${(ms / 1000).toFixed(1)} s` : `${Math.round(ms / 1000)} s`;
}

function tokens(count: number): string {
	return count >= 1000 ? `${Math.round(count / 1000)}k` : String(count);
}

/** One line of timing and cost. */
export function costLine(result: EngineResult): string {
	const cost = result.usage.costUsd > 0 ? `, $${result.usage.costUsd.toFixed(2)}` : "";
	return `Reviewed in ${seconds(result.timing.totalMs)}: ${result.usage.frames} model calls, ${tokens(result.usage.tokens)} tokens${cost}.`;
}

/**
 * Confirmed findings, the ones to post first: first those that are serious (critical, high) or proven beyond the
 * diff, then the rest; within each by level, then by evidence strength. Returns indices into `result.findings`.
 */
export function rankFindings(result: EngineResult): number[] {
	const strength = (finding: EngineFinding) => STRENGTHS.indexOf(finding.strength ?? "diff");
	const tier = (finding: EngineFinding) =>
		atOrAbove(levelOf(finding), "high") || (finding.strength ?? "diff") !== "diff" ? 0 : 1;
	return result.findings
		.map((finding, index) => ({ finding, index }))
		.filter(({ finding }) => finding.verification === "confirmed")
		.sort(
			(a, b) =>
				tier(a.finding) - tier(b.finding) ||
				LEVELS.indexOf(levelOf(a.finding)) - LEVELS.indexOf(levelOf(b.finding)) ||
				strength(a.finding) - strength(b.finding) ||
				a.index - b.index,
		)
		.map(({ index }) => index);
}

function counted(indices: readonly number[], result: EngineResult): string {
	return LEVELS.map((level) => {
		const count = indices.filter((index) => levelOf(result.findings[index]!) === level).length;
		return count === 0 ? undefined : `${count} ${level}`;
	})
		.filter((item): item is string => item !== undefined)
		.join(", ");
}

export function planReview(result: EngineResult, options: PlanOptions): ReviewPlan {
	const blockAt = options.blockAt ?? DEFAULT_BLOCK_AT;
	const maxComments = Math.max(0, options.maxComments ?? DEFAULT_MAX_COMMENTS);
	const { verdict, reason } = decideVerdict(result, options);
	const comments: PlannedComment[] = [];
	const inSummary: number[] = [];
	const overCap: number[] = [];
	const shortSha = options.headSha.slice(0, 7);
	const ranked = rankFindings(result);
	for (const index of ranked) {
		const finding = result.findings[index]!;
		const blocking = atOrAbove(levelOf(finding), blockAt);
		let comment =
			levelOf(finding) === "nit"
				? undefined
				: planComment(finding, index, result.diffLines[finding.file], `${shortSha}-${index + 1}`);
		// The same root cause at another place that is in the diff: the comment goes there, naming this place.
		if (comment === undefined && levelOf(finding) !== "nit")
			for (const place of finding.alsoAt ?? []) {
				const moved: EngineFinding = {
					...finding,
					file: place.file,
					line: place.line,
					endLine: undefined,
					replacement: undefined,
					alsoAt: [
						{ file: finding.file, line: finding.line },
						...(finding.alsoAt ?? []).filter((other) => other !== place),
					],
				};
				comment = planComment(moved, index, result.diffLines[place.file], `${shortSha}-${index + 1}`);
				if (comment !== undefined) break;
			}
		if (comment && comments.length < maxComments) comments.push(comment);
		// What cannot be inline is named in the body when it must be resolved, or when its line is outside the diff.
		else if (blocking || (comment === undefined && levelOf(finding) !== "nit")) inSummary.push(index);
		else overCap.push(index);
	}
	const inline = new Set(comments.map((comment) => comment.finding));
	const blockers = ranked.filter((index) => atOrAbove(levelOf(result.findings[index]!), blockAt));
	const uncertain = result.findings.filter((finding) => finding.verification === "uncertain").length;

	// Paragraph 1: what was checked and holds.
	const assurance = (result.assurance ?? []).slice(0, 4).map((item) => short(item, 320));
	const scope = `${result.files} file${result.files === 1 ? "" : "s"}, +${result.added} -${result.removed}`;
	const first =
		assurance.length > 0
			? `${options.sinceSha === undefined ? "" : `Re-review of the changes since \`${options.sinceSha.slice(0, 7)}\`. `}${assurance.join(" ")}`
			: `Read ${options.sinceSha === undefined ? `the diff of \`${shortSha}\`` : `the changes from \`${options.sinceSha.slice(0, 7)}\` to \`${shortSha}\``} (${scope}) with the code around it; every finding below was checked against the source by a second pass${result.dropped.rejected ? `, which rejected ${result.dropped.rejected}` : ""}.`;

	// Paragraph 2: the verdict and what must be resolved before merge.
	const second = [`**${VERDICT_TITLES[verdict]}.**`];
	if (blockers.length > 0) {
		const named = blockers.slice(0, MAX_BLOCKING_LISTED).map((index, position) => {
			const finding = result.findings[index]!;
			const place = inline.has(index) ? `${where(finding)} (inline)` : where(finding);
			return `${blockers.length > 1 ? `(${position + 1}) ` : ""}${place} ${sentence(short(finding.claim, 220))}`;
		});
		const lead = verdict === "request_changes" ? "To resolve before merge:" : "Worth resolving:";
		second.push(`${lead} ${named.join(" ")}`);
		if (blockers.length > MAX_BLOCKING_LISTED)
			second.push(`And ${blockers.length - MAX_BLOCKING_LISTED} more at ${blockAt} or above.`);
		if (verdict !== "request_changes") second.push(`${reason[0]!.toUpperCase()}${reason.slice(1)}.`);
	} else second.push(`${reason[0]!.toUpperCase()}${reason.slice(1)}.`);
	const stillOpen = result.earlier.filter((item) => item.status === "still_present");
	const fixed = result.earlier.filter((item) => item.status === "fixed");
	if (result.earlier.length > 0) {
		const parts = [];
		if (fixed.length) parts.push(`${fixed.length} fixed (${fixed.slice(0, 4).map(where).join(", ")})`);
		if (stillOpen.length)
			parts.push(`${stillOpen.length} still present (${stillOpen.slice(0, 4).map(where).join(", ")})`);
		const other = result.earlier.length - fixed.length - stillOpen.length;
		if (other) parts.push(`${other} no longer applicable or not re-checked`);
		second.push(`Earlier findings: ${parts.join("; ")}.`);
	}

	// Paragraph 3: the non-blocking notes, in one or two lines.
	const notes: string[] = [];
	const inlineNotes = comments.filter(
		(comment) => !atOrAbove(levelOf(result.findings[comment.finding]!), blockAt),
	).length;
	if (inlineNotes > 0) notes.push(`${inlineNotes} non-blocking note${inlineNotes === 1 ? "" : "s"} inline.`);
	const outside = inSummary.filter((index) => !atOrAbove(levelOf(result.findings[index]!), blockAt));
	if (outside.length > 0) {
		const named = outside.slice(0, MAX_OUTSIDE_LISTED).map((index) => {
			const finding = result.findings[index]!;
			return `${where(finding)} [${levelOf(finding)}] ${sentence(short(finding.claim, 160))}`;
		});
		notes.push(
			`Outside the diff: ${named.join(" ")}${outside.length > MAX_OUTSIDE_LISTED ? ` And ${outside.length - MAX_OUTSIDE_LISTED} more.` : ""}`,
		);
	}
	const rest: string[] = [];
	if (overCap.length > 0)
		rest.push(
			`${overCap.length} lower-ranked finding${overCap.length === 1 ? "" : "s"} (${counted(overCap, result)})`,
		);
	if (uncertain > 0) rest.push(`${uncertain} unconfirmed`);
	if (rest.length > 0) notes.push(`Not posted: ${rest.join(" and ")}.`);
	if (result.alsoRaised.length > 0) {
		const by = [...new Set(result.alsoRaised.flatMap((item) => item.by))].slice(0, 4);
		notes.push(
			`${result.alsoRaised.length} finding${result.alsoRaised.length === 1 ? " was" : "s were"} already raised by ${by.map((name) => `@${name}`).join(", ")} and ${result.alsoRaised.length === 1 ? "is" : "are"} not repeated.`,
		);
	}

	// What was not checked: the reasons coverage is incomplete lead; routine skips follow.
	const gaps = [...result.incomplete, ...result.notChecked.filter((item) => !result.incomplete.includes(item))];
	const unchecked =
		gaps.length === 0
			? []
			: [
					`Not checked: ${gaps
						.slice(0, MAX_NOT_CHECKED)
						.map((gap) => short(gap, 200).replace(/\.$/, ""))
						.join("; ")}${gaps.length > MAX_NOT_CHECKED ? `; and ${gaps.length - MAX_NOT_CHECKED} more` : ""}.`,
				];

	const paragraphs = [
		first,
		second.join(" "),
		...(notes.length ? [notes.join(" ")] : []),
		...unchecked,
		costLine(result),
	];
	const lines = paragraphs.flatMap((paragraph, index) => (index === 0 ? [paragraph] : ["", paragraph]));
	const tail = options.signature ? ["", SIGNATURE] : [];
	const room = MAX_BODY_LINES - tail.length;
	const body = [...(lines.length > room ? lines.slice(0, room) : lines), ...tail].join("\n");
	return { verdict, event: EVENTS[verdict], body, comments, inSummary, overCap };
}

/** The plan with every inline comment named in the body instead (when GitHub rejects the inline comments). */
export function withoutInline(plan: ReviewPlan, result: EngineResult): ReviewPlan {
	if (plan.comments.length === 0) return plan;
	const named = plan.comments.map((comment) => {
		const finding = result.findings[comment.finding]!;
		return `${where(finding)} ${commentText(finding)}`;
	});
	const extra = `These could not be attached to the diff: ${named.join(" ")}`;
	const lines = plan.body.split("\n");
	const signed = lines.at(-1) === SIGNATURE;
	const head = signed ? lines.slice(0, -2) : lines;
	const body = [...head, "", extra, ...(signed ? ["", SIGNATURE] : [])].join("\n");
	return {
		...plan,
		body,
		comments: [],
		inSummary: [...plan.inSummary, ...plan.comments.map((comment) => comment.finding)],
	};
}
