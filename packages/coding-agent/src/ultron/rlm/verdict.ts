/**
 * Subagent verdicts: what a finished `rlm.spawn` child claims (`rlm.finish`), the mechanical checks on that claim,
 * and the cross-check of its declared file changes against what the host saw change on disk while it ran.
 *
 * Nothing here judges quality. A verdict is rejected only for things a program can see: an unknown status, an empty
 * summary, a `passed` with no evidence or with evidence that names neither a command outcome nor a file, malformed
 * fields or oversized ones. The file check compares paths, never contents.
 *
 * Attribution under concurrency: children share the filesystem and siblings (or the parent's own cells) may write
 * while a child runs, so a change seen during a child's run is not proof that the child made it. The check is
 * therefore asymmetric:
 * - a declared file that did not change during the child's run is a false claim, whoever else was running, and
 *   marks the verdict `contradicted`;
 * - a file that changed during the run but was not declared is reported in `unreported` ("changed while this child
 *   ran, not declared by it") and never fails the verdict, because it may belong to concurrent work. Paths that
 *   another overlapping task's verdict declared are left out of it, and `concurrent` names the overlapping tasks so
 *   the parent can attribute the rest.
 */
import { isAbsolute, posix, relative, sep } from "node:path";
import { isJsonValue, type JsonValue } from "@ultron/chord";
import type { WorkspaceDiff, WorkspaceSnapshot } from "../workspace-snapshot.ts";

export const VERDICT_STATUSES = ["passed", "failed", "blocked"] as const;
export type VerdictStatus = (typeof VERDICT_STATUSES)[number];

export type Verdict = {
	status: VerdictStatus;
	summary: string;
	outputs: Record<string, JsonValue>;
	evidence: string[];
	changed_files: string[];
};

/**
 * How the host's check of a verdict came out:
 * - `verified`: the verdict is well formed and every declared file changed during the child's run;
 * - `contradicted`: a declared file did not change (see `unobserved`);
 * - `invalid`: the child's `rlm.finish` calls were rejected until no attempts were left (see `problems`);
 * - `unchecked`: the files could not be compared (no workspace, a failed or incomplete snapshot; see `reason`).
 */
export type VerdictCheckOutcome = "verified" | "contradicted" | "invalid" | "unchecked";

export type VerdictCheck = {
	outcome: VerdictCheckOutcome;
	/** Declared as changed, but unchanged during the child's run. */
	unobserved: string[];
	/** Changed during the child's run and declared by no verdict: the child's or concurrent work's. */
	unreported: string[];
	/** Declared paths the snapshot does not list (ignored or outside the workspace), so not checked. */
	unlisted: string[];
	/** Other tasks that ran at the same time (outside this child's own subtree and ancestors). */
	concurrent: string[];
	/** Why the files were not compared (`unchecked`). */
	reason?: string;
	/** Why the verdict was rejected (`invalid`). */
	problems?: string[];
	/** How many undeclared changes there were, when more than `unreported` lists. */
	unreportedCount?: number;
};

/** Rejected `rlm.finish` calls a child may fix; the next rejection ends its verdict as `invalid`. */
export const MAX_VERDICT_REJECTIONS = 2;

const LIMITS = {
	summary: 2_000,
	evidenceItems: 40,
	evidenceChars: 1_000,
	changedFiles: 500,
	outputsBytes: 32 * 1024,
	listed: 100,
};

/** Evidence a program can recognize as concrete: a command's exit status or counts, a file path, a line. */
const CONCRETE = [
	/\bexit(?:ed)?(?:\s+(?:with\s+)?(?:code|status))?\s*[:=]?\s*-?\d+\b/i,
	/\b\d+\s+(?:passed|failed|passing|failing|tests?|errors?|warnings?|skipped|files?|matches|lines?)\b/i,
	/(?:^|[\s`'"(=])(?:\.{0,2}\/)?[\w@+.-]+\/[\w@+./-]*[\w@+-]/,
	/\b[\w-]{2,}\.[A-Za-z][A-Za-z0-9]{0,5}\b/,
	/:\d+(?:[-:]\d+)?\b/,
];

export function isConcreteEvidence(item: string): boolean {
	return CONCRETE.some((pattern) => pattern.test(item));
}

/** A declared path relative to the workspace with `/` separators, or undefined when it lies outside it. */
export function workspacePath(path: string, workspace: string | undefined): string | undefined {
	let value = path.trim();
	if (isAbsolute(value)) {
		if (workspace === undefined) return undefined;
		value = relative(workspace, value);
		if (value === "" || value.startsWith("..") || isAbsolute(value)) return undefined;
	}
	if (sep !== "/") value = value.split(sep).join("/");
	value = posix.normalize(value).replace(/\/+$/, "");
	if (value === "." || value === "" || value.startsWith("../") || value === "..") return undefined;
	return value;
}

function bounded(value: string, limit = 200): string {
	const flat = value.replace(/\s+/g, " ").trim();
	return flat.length > limit ? `${flat.slice(0, limit - 1)}…` : flat;
}

/**
 * Checks an `rlm.finish` payload. Returns the normalized verdict, or every problem found (worded for the child that
 * has to fix them).
 */
export function validateVerdict(
	payload: Record<string, unknown>,
	workspace?: string,
): { verdict: Verdict } | { problems: string[] } {
	const problems: string[] = [];
	const allowed = new Set(["status", "summary", "outputs", "evidence", "changed_files"]);
	for (const key of Object.keys(payload))
		if (!allowed.has(key))
			problems.push(`unknown field "${key}" (use status, summary, outputs, evidence, changed_files)`);
	const status = payload.status;
	if (typeof status !== "string" || !(VERDICT_STATUSES as readonly string[]).includes(status))
		problems.push(
			`status must be "passed", "failed" or "blocked", not ${bounded(JSON.stringify(status) ?? "nothing")}`,
		);
	const summary = typeof payload.summary === "string" ? payload.summary.trim() : "";
	if (!summary) problems.push("summary must say in a sentence or two what was done, or what blocked it");
	else if (summary.length > LIMITS.summary)
		problems.push(`summary is ${summary.length} characters; keep it under ${LIMITS.summary} (details go in outputs)`);
	const outputs = payload.outputs === undefined || payload.outputs === null ? {} : payload.outputs;
	if (typeof outputs !== "object" || Array.isArray(outputs) || !isJsonValue(outputs))
		problems.push("outputs must be a JSON object (a dict of named results)");
	else if (JSON.stringify(outputs).length > LIMITS.outputsBytes)
		problems.push(`outputs exceed ${LIMITS.outputsBytes} bytes; write large results to a file and return its path`);
	const rawEvidence = payload.evidence === undefined || payload.evidence === null ? [] : payload.evidence;
	const evidence: string[] = [];
	if (!Array.isArray(rawEvidence)) problems.push("evidence must be a list of strings");
	else {
		for (const item of rawEvidence) {
			if (typeof item !== "string" || !item.trim()) {
				problems.push("every evidence item must be a nonempty string");
				break;
			}
			if (item.length > LIMITS.evidenceChars) {
				problems.push(`an evidence item is over ${LIMITS.evidenceChars} characters; quote the deciding lines only`);
				break;
			}
			evidence.push(item.trim());
		}
		if (rawEvidence.length > LIMITS.evidenceItems)
			problems.push(`evidence has ${rawEvidence.length} items; give at most ${LIMITS.evidenceItems}`);
	}
	if (status === "passed" && Array.isArray(rawEvidence)) {
		if (evidence.length === 0)
			problems.push(
				'"passed" needs evidence: the commands you ran with their exit codes or counts, and the files (with lines) that show the result',
			);
		else if (!evidence.some(isConcreteEvidence))
			problems.push(
				`"passed" evidence must be concrete, and none of it is: name a command with its outcome ("npm test: exit 0, 42 passed") or a file with lines ("src/app.ts:40-52 handles the empty case"), not ${bounded(JSON.stringify(evidence[0]), 80)}`,
			);
	}
	const rawFiles = payload.changed_files === undefined || payload.changed_files === null ? [] : payload.changed_files;
	const changed: string[] = [];
	if (!Array.isArray(rawFiles) || rawFiles.some((item) => typeof item !== "string" || !item.trim()))
		problems.push("changed_files must be a list of file paths (relative to the working directory)");
	else if (rawFiles.length > LIMITS.changedFiles)
		problems.push(
			`changed_files has ${rawFiles.length} paths; list at most ${LIMITS.changedFiles} (or their directories)`,
		);
	else
		for (const item of rawFiles as string[]) {
			const path = workspacePath(item, workspace) ?? item.trim();
			if (!changed.includes(path)) changed.push(path);
		}
	if (problems.length > 0) return { problems };
	return {
		verdict: {
			status: status as VerdictStatus,
			summary,
			outputs: outputs as Record<string, JsonValue>,
			evidence,
			changed_files: changed,
		},
	};
}

function covers(declared: string, path: string): boolean {
	return path === declared || path.startsWith(`${declared}/`);
}

function listed(snapshot: WorkspaceSnapshot, path: string): boolean {
	if (snapshot.entries.has(path)) return true;
	for (const entry of snapshot.entries.keys()) if (entry.startsWith(`${path}/`)) return true;
	return false;
}

export type FileCheckInput = {
	verdict: Verdict;
	workspace: string;
	before: WorkspaceSnapshot;
	after: WorkspaceSnapshot;
	diff: WorkspaceDiff;
	/** Paths declared by the verdicts of other tasks that overlapped this run. */
	explained: ReadonlySet<string>;
	concurrent: string[];
	/** Whether a path exists now (for declared paths neither snapshot lists). */
	exists: (path: string) => Promise<boolean>;
};

/** Compares a verdict's declared changes with the changes observed during the run (see the module comment). */
export async function checkFiles(input: FileCheckInput): Promise<VerdictCheck> {
	const { verdict, diff } = input;
	const unobserved: string[] = [];
	const unlisted: string[] = [];
	for (const declared of verdict.changed_files) {
		const path = workspacePath(declared, input.workspace);
		if (path === undefined) {
			unlisted.push(declared);
			continue;
		}
		if (diff.changed.some((changed) => covers(path, changed))) continue;
		if (listed(input.before, path) || listed(input.after, path)) unobserved.push(declared);
		// Neither snapshot lists it: an ignored file is not checked; a claimed file that does not exist is false.
		else if (await input.exists(path)) unlisted.push(declared);
		else unobserved.push(declared);
	}
	const declared = verdict.changed_files.map((path) => workspacePath(path, input.workspace) ?? path);
	const unreported = diff.changed.filter(
		(path) =>
			!declared.some((item) => covers(item, path)) && ![...input.explained].some((item) => covers(item, path)),
	);
	const outcome: VerdictCheckOutcome =
		unobserved.length > 0 ? "contradicted" : diff.complete ? "verified" : "unchecked";
	return {
		outcome,
		unobserved: unobserved.slice(0, LIMITS.listed),
		unreported: unreported.slice(0, LIMITS.listed),
		unlisted: unlisted.slice(0, LIMITS.listed),
		concurrent: input.concurrent.slice(0, LIMITS.listed),
		...(outcome === "unchecked" ? { reason: "the workspace has more files than a snapshot records" } : {}),
		...(unreported.length > LIMITS.listed ? { unreportedCount: unreported.length } : {}),
	};
}

/** One-line tag for a completion event: `[passed, verified]`, `[failed, contradicted]`, `[unverified]`. */
export function verdictTag(result: { verdict?: unknown; check?: unknown; unverified?: unknown }): string | undefined {
	const verdict = result.verdict as Verdict | null | undefined;
	const check = result.check as VerdictCheck | undefined;
	if (verdict) {
		const extra = [
			check?.unobserved.length ? `declared but unchanged: ${check.unobserved.slice(0, 3).join(", ")}` : "",
			check?.unreported.length ? `${check.unreported.length} undeclared change(s)` : "",
		].filter(Boolean);
		return `[${verdict.status}, ${check?.outcome ?? "unchecked"}${extra.length ? `; ${extra.join("; ")}` : ""}]`;
	}
	if (result.unverified === true) return `[unverified${check?.outcome === "invalid" ? ": verdict rejected" : ""}]`;
	return undefined;
}
