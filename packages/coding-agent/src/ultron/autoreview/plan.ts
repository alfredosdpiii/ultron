/**
 * From an engine result to what is posted: the verdict, the inline comments (each validated against the diff),
 * and the summary body. Pure functions; nothing here talks to GitHub.
 *
 * - Verdict: REQUEST_CHANGES when a confirmed blocker or major finding exists (or one from an earlier review is
 *   still present); APPROVE only when coverage was complete and there is none; otherwise COMMENT. A pull request
 *   by the reviewing account, or one that is closed or merged, always gets COMMENT.
 * - Inline comments: confirmed findings only, capped by severity. A line outside the diff moves to the nearest
 *   diff line of a hunk within three lines, else the finding goes to the summary.
 * - A ```suggestion block only when the finding carries an exact replacement for its line range, the range was
 *   not moved, and every line of it is in one hunk; otherwise a plain fenced block or a sentence.
 */
import { SIGNATURE } from "./config.ts";
import type { ReviewCommentInput } from "./github.ts";
import type { EarlierStatus, EngineFinding, EngineResult, Severity } from "./types.ts";
import { SEVERITIES } from "./types.ts";

export type Verdict = "approve" | "request_changes" | "comment";

/** Inline comments posted per severity; the rest are counted in the summary. */
export const INLINE_CAPS: Readonly<Record<Severity, number>> = {
	blocker: Number.POSITIVE_INFINITY,
	major: 5,
	minor: 5,
	nit: 3,
};
export const RELOCATE_WITHIN = 3;
const MAX_UNCERTAIN = 5;
const MAX_SUMMARY_FINDINGS = 8;
const MAX_ALSO_RAISED = 5;
const MAX_EARLIER_ROWS = 10;
const MAX_NOT_CHECKED = 6;
const MAX_SUMMARY_LINES = 60;

export interface PlanOptions {
	/** The reviewing account wrote the pull request (GitHub rejects approve/request-changes on your own PR). */
	readonly selfAuthored: boolean;
	readonly state: "open" | "closed" | "merged";
	readonly headSha: string;
	/** A re-review of the changes since this commit. */
	readonly sinceSha?: string;
	readonly signature: boolean;
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
	/** Confirmed findings in the summary instead of inline (their line is not in the diff), by index. */
	readonly inSummary: readonly number[];
	/** Confirmed findings past the severity caps, by index. */
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

const TITLES: Readonly<Record<Severity, string>> = { blocker: "Blocker", major: "Major", minor: "Minor", nit: "Nit" };

export function commentMarker(key: string): string {
	return `<!-- ultron-autoreview:${key} -->`;
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
	const parts = [`**${TITLES[finding.severity]}** (${finding.category}): ${finding.claim}`];
	if (placed.relocated) parts.push(`This is about line ${finding.line}, which is not part of the diff.`);
	if (finding.why) parts.push(finding.why);
	if (suggestion) parts.push(fence(finding.replacement!, "suggestion"));
	else if (finding.replacement !== undefined)
		parts.push(
			`Suggested replacement for ${end > finding.line ? `lines ${finding.line}-${end}` : `line ${finding.line}`}:\n\n${fence(finding.replacement)}`,
		);
	else if (finding.suggestedFix) parts.push(`Suggested fix: ${finding.suggestedFix}`);
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

const BLOCKING = (severity: string): boolean => severity === "blocker" || severity === "major";

export function decideVerdict(
	result: EngineResult,
	options: Pick<PlanOptions, "selfAuthored" | "state">,
): { verdict: Verdict; reason: string } {
	const confirmed = result.findings.filter((finding) => finding.verification === "confirmed");
	const blocking = confirmed.filter((finding) => BLOCKING(finding.severity)).length;
	const stillOpen = result.earlier.filter((item) => item.status === "still_present" && BLOCKING(item.severity)).length;
	if (options.selfAuthored) return { verdict: "comment", reason: "this account opened the pull request" };
	if (options.state !== "open") return { verdict: "comment", reason: `the pull request is ${options.state}` };
	if (blocking > 0)
		return {
			verdict: "request_changes",
			reason: `${blocking} confirmed blocker or major finding${blocking === 1 ? "" : "s"}`,
		};
	if (stillOpen > 0)
		return {
			verdict: "request_changes",
			reason: `${stillOpen} blocker or major finding${stillOpen === 1 ? "" : "s"} from an earlier review still present`,
		};
	if (!result.complete) return { verdict: "comment", reason: "the review did not cover everything (see Not checked)" };
	return { verdict: "approve", reason: "no confirmed blocker or major finding" };
}

const EVENTS = { approve: "APPROVE", request_changes: "REQUEST_CHANGES", comment: "COMMENT" } as const;
const VERDICT_TITLES = { approve: "Approve", request_changes: "Request changes", comment: "Comment" } as const;
const STATUS_TITLES: Readonly<Record<EarlierStatus["status"], string>> = {
	fixed: "fixed",
	still_present: "still present",
	not_applicable: "no longer applicable",
	unknown: "not re-checked",
};

function short(text: string, limit: number): string {
	const flat = text.replace(/\s+/g, " ").trim();
	return flat.length <= limit ? flat : `${flat.slice(0, limit - 1)}…`;
}

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

export function planReview(result: EngineResult, options: PlanOptions): ReviewPlan {
	const { verdict, reason } = decideVerdict(result, options);
	const comments: PlannedComment[] = [];
	const inSummary: number[] = [];
	const overCap: number[] = [];
	const used: Record<Severity, number> = { blocker: 0, major: 0, minor: 0, nit: 0 };
	const shortSha = options.headSha.slice(0, 7);
	result.findings.forEach((finding, index) => {
		if (finding.verification !== "confirmed") return;
		if (used[finding.severity] >= INLINE_CAPS[finding.severity]) {
			overCap.push(index);
			return;
		}
		used[finding.severity] += 1;
		const comment = planComment(finding, index, result.diffLines[finding.file], `${shortSha}-${index + 1}`);
		if (comment) comments.push(comment);
		else inSummary.push(index);
	});

	const confirmed = result.findings.filter((finding) => finding.verification === "confirmed");
	const uncertain = result.findings.filter((finding) => finding.verification === "uncertain");
	const lines: string[] = [
		`**Verdict: ${VERDICT_TITLES[verdict]}.** ${reason[0]!.toUpperCase()}${reason.slice(1)}.`,
		"",
	];
	const scope =
		options.sinceSha === undefined
			? `Reviewed \`${shortSha}\``
			: `Reviewed \`${shortSha}\`, the changes since \`${options.sinceSha.slice(0, 7)}\``;
	lines.push(`${scope}: ${result.files} file${result.files === 1 ? "" : "s"}, +${result.added} -${result.removed}.`);
	const counts = SEVERITIES.map((severity) => {
		const count = confirmed.filter((finding) => finding.severity === severity).length;
		return count === 0 ? undefined : `${count} ${severity}`;
	}).filter((item): item is string => item !== undefined);
	const tally = [
		confirmed.length === 0
			? "No confirmed findings"
			: `Confirmed findings: ${counts.join(", ")} (${comments.length} inline)`,
	];
	if (uncertain.length) tally.push(`${uncertain.length} uncertain`);
	if (result.dropped.rejected) tally.push(`${result.dropped.rejected} rejected by verification`);
	lines.push(`${tally.join("; ")}.`);

	const where = (finding: { file: string; line: number }) => `\`${finding.file}:${finding.line}\``;
	if (inSummary.length) {
		lines.push("", "**Findings outside the diff**");
		for (const index of inSummary.slice(0, MAX_SUMMARY_FINDINGS)) {
			const finding = result.findings[index]!;
			lines.push(`- ${where(finding)} (${finding.severity}) ${short(finding.claim, 220)}`);
		}
		if (inSummary.length > MAX_SUMMARY_FINDINGS) lines.push(`- and ${inSummary.length - MAX_SUMMARY_FINDINGS} more`);
	}
	if (overCap.length) {
		const by = SEVERITIES.map((severity) => {
			const count = overCap.filter((index) => result.findings[index]!.severity === severity).length;
			return count === 0 ? undefined : `${count} ${severity}`;
		}).filter((item): item is string => item !== undefined);
		lines.push("", `Not shown, to keep this readable: ${by.join(", ")} finding${overCap.length === 1 ? "" : "s"}.`);
	}
	if (uncertain.length) {
		lines.push("", "**Uncertain, not confirmed** (worth a look, not counted in the verdict)");
		for (const finding of uncertain.slice(0, MAX_UNCERTAIN))
			lines.push(`- ${where(finding)} (${finding.severity}) ${short(finding.claim, 220)}`);
		if (uncertain.length > MAX_UNCERTAIN) lines.push(`- and ${uncertain.length - MAX_UNCERTAIN} more`);
	}
	if (result.alsoRaised.length) {
		lines.push("", "**Already raised by others**");
		for (const item of result.alsoRaised.slice(0, MAX_ALSO_RAISED))
			lines.push(
				`- ${where(item)} ${short(item.claim, 180)} (also raised by ${item.by.map((name) => `@${name}`).join(", ")})`,
			);
		if (result.alsoRaised.length > MAX_ALSO_RAISED)
			lines.push(`- and ${result.alsoRaised.length - MAX_ALSO_RAISED} more`);
	}
	if (result.earlier.length) {
		lines.push("", "**Earlier findings**", "", "| Finding | Status |", "| --- | --- |");
		for (const item of result.earlier.slice(0, MAX_EARLIER_ROWS))
			lines.push(
				`| ${where(item)} ${short(item.claim, 100).replace(/\|/g, "\\|")} | ${STATUS_TITLES[item.status]} |`,
			);
		if (result.earlier.length > MAX_EARLIER_ROWS)
			lines.push(`| and ${result.earlier.length - MAX_EARLIER_ROWS} more | |`);
	}
	// Reasons coverage is incomplete lead; routine skips (lockfiles, binaries) follow.
	const gaps = [...result.incomplete, ...result.notChecked.filter((item) => !result.incomplete.includes(item))];
	lines.push("", "**Not checked**");
	lines.push(
		"- Nothing was run: no tests, build or type check. Findings come from reading the diff and the code around it.",
	);
	for (const gap of gaps.slice(0, MAX_NOT_CHECKED)) lines.push(`- ${short(gap, 240)}`);
	if (gaps.length > MAX_NOT_CHECKED) lines.push(`- and ${gaps.length - MAX_NOT_CHECKED} more skipped files or passes`);
	lines.push("", costLine(result));
	const tail = options.signature ? ["", SIGNATURE] : [];
	const room = MAX_SUMMARY_LINES - tail.length;
	const body = [...(lines.length > room ? [...lines.slice(0, room - 1), "(summary shortened)"] : lines), ...tail].join(
		"\n",
	);
	return { verdict, event: EVENTS[verdict], body, comments, inSummary, overCap };
}

/** The plan with every inline comment folded into the summary (when GitHub rejects the inline comments). */
export function withoutInline(plan: ReviewPlan, result: EngineResult): ReviewPlan {
	if (plan.comments.length === 0) return plan;
	const extra = ["", "**Findings** (could not be attached to the diff)"];
	for (const comment of plan.comments) {
		const finding = result.findings[comment.finding]!;
		extra.push(`- \`${finding.file}:${finding.line}\` (${finding.severity}) ${short(finding.claim, 220)}`);
	}
	const lines = plan.body.split("\n");
	const signed = lines.at(-1) === SIGNATURE;
	const head = signed ? lines.slice(0, -2) : lines;
	const body = [...head, ...extra, ...(signed ? ["", SIGNATURE] : [])].join("\n");
	return {
		...plan,
		body,
		comments: [],
		inSummary: [...plan.inSummary, ...plan.comments.map((comment) => comment.finding)],
	};
}
