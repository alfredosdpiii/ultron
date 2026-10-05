/**
 * Pure parts of the autoreview benchmark: patch parsing and ground truth, the seeded sample, the reviewer command,
 * scoring, aggregation and the report. No I/O here; `cases.mjs` builds the case repositories and `run.mjs` drives
 * the reviewer. Unit tests: scripts/eval-autoreview.test.mjs.
 */

import { createHash } from "node:crypto";

export const DEFAULT_SEED = "ultron-autoreview-1";
export const DEFAULT_CASES_PER_KIND = 20;
/** A task is eligible when its gold patch is at most this large: changed (added plus removed) lines, and files. */
export const MAX_CHANGED_LINES = 150;
export const MAX_FILES = 4;
/** A finding counts as located when it overlaps a ground-truth hunk widened by this many lines on each side. */
export const LOCATION_TOLERANCE = 5;
/** The tight variant: the lines the diff actually changed, widened by this many lines. */
export const TIGHT_TOLERANCE = 2;
/** At most this many findings of one review are sent to the judge. */
export const MAX_JUDGED_FINDINGS = 8;

export const KINDS = ["buggy", "clean"];
export const SEVERITIES = ["blocker", "major", "minor", "nit"];
export const VERDICTS = ["approve", "request_changes", "comment"];
export const EXPECTED_VERDICT = { buggy: "request_changes", clean: "approve" };

// ---------------------------------------------------------------------------------------------------------------
// Patches

const DIFF_HEADER = /^diff --git (?:"a\/(.*)" "b\/(.*)"|a\/(.*) b\/(.*))$/;
const HUNK_HEADER = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/;

/**
 * A git diff as files with hunks. Each file: `{ path, oldPath, newPath, status, binary, hunks, raw }`, `status` one
 * of modified, added, deleted, renamed, and `raw` the file's own part of the diff text (a patch of that file alone).
 * Each hunk: `{ oldStart, oldLines, newStart, newLines, lines }` with `lines` the body lines including their
 * leading ` `, `+` or `-`.
 */
export function parsePatch(text) {
	const files = [];
	let file = null;
	let hunk = null;
	let remainingOld = 0;
	let remainingNew = 0;
	const lines = String(text ?? "").split("\n");
	for (let index = 0; index < lines.length; index++) {
		const line = lines[index];
		const inHunk = hunk && (remainingOld > 0 || remainingNew > 0);
		if (!inHunk) {
			const header = DIFF_HEADER.exec(line);
			if (header) {
				const oldPath = header[1] ?? header[3];
				const newPath = header[2] ?? header[4];
				file = { path: newPath, oldPath, newPath, status: "modified", binary: false, hunks: [], rawLines: [line] };
				files.push(file);
				hunk = null;
				continue;
			}
		}
		if (!file) continue;
		file.rawLines.push(line);
		if (inHunk) {
			if (line.startsWith("\\")) continue;
			hunk.lines.push(line);
			if (line.startsWith("+")) remainingNew--;
			else if (line.startsWith("-")) remainingOld--;
			else {
				remainingOld--;
				remainingNew--;
			}
			continue;
		}
		const at = HUNK_HEADER.exec(line);
		if (at) {
			hunk = {
				oldStart: Number(at[1]),
				oldLines: at[2] === undefined ? 1 : Number(at[2]),
				newStart: Number(at[3]),
				newLines: at[4] === undefined ? 1 : Number(at[4]),
				lines: [],
			};
			remainingOld = hunk.oldLines;
			remainingNew = hunk.newLines;
			file.hunks.push(hunk);
		} else if (line.startsWith("new file mode")) file.status = "added";
		else if (line.startsWith("deleted file mode")) file.status = "deleted";
		else if (line.startsWith("rename from") || line.startsWith("copy from")) file.status = "renamed";
		else if (line.startsWith("GIT binary patch") || line.startsWith("Binary files")) file.binary = true;
	}
	return files.map(({ rawLines, ...rest }) => {
		while (rawLines.length && rawLines.at(-1) === "") rawLines.pop();
		return { ...rest, raw: `${rawLines.join("\n")}\n` };
	});
}

/** `{ files, hunks, added, removed, changed }` of a parsed patch. */
export function patchStats(files) {
	let added = 0;
	let removed = 0;
	let hunks = 0;
	for (const file of files) {
		hunks += file.hunks.length;
		for (const hunk of file.hunks) {
			for (const line of hunk.lines) {
				if (line.startsWith("+")) added++;
				else if (line.startsWith("-")) removed++;
			}
		}
	}
	return { files: files.length, hunks, added, removed, changed: added + removed };
}

/** Sorted line numbers as inclusive `[start, end]` runs of consecutive numbers. */
function runs(numbers) {
	const sorted = [...new Set(numbers)].sort((a, b) => a - b);
	const out = [];
	for (const number of sorted) {
		if (out.length && number === out.at(-1)[1] + 1) out.at(-1)[1] = number;
		else out.push([number, number]);
	}
	return out;
}

/**
 * Where a diff's changes sit in one side of it, per file, in that side's line numbers.
 *
 * `side` names which side of `files` is the reviewed head: "new" for the diff as given, "old" for its reverse (the
 * buggy case reviews the reverse of the gold patch, whose head is the gold patch's old side). Per file:
 *
 * - `hunks`: each hunk's whole span on that side, context lines included (`[start, end]`, inclusive).
 * - `changed`: the lines that side gained (runs of consecutive lines), and where a block of the diff only removed
 *   lines, the two lines around the place they were removed from, since removed lines have no number in the head.
 * - `status`: "modified"; "added" when the file exists only in the head (one hunk, the whole file); "deleted" when
 *   the file does not exist in the head (no ranges: only the file itself can be matched).
 */
export function changeRanges(files, side = "new") {
	const out = [];
	for (const file of files) {
		const gone = side === "new" ? "deleted" : "added";
		const fresh = side === "new" ? "added" : "deleted";
		const path = side === "new" ? file.newPath : file.oldPath;
		if (file.status === gone) {
			out.push({ file: side === "new" ? file.oldPath : file.newPath, status: "deleted", hunks: [], changed: [] });
			continue;
		}
		const gained = side === "new" ? "+" : "-";
		const lost = side === "new" ? "-" : "+";
		const hunks = [];
		const marks = [];
		for (const hunk of file.hunks) {
			const start = side === "new" ? hunk.newStart : hunk.oldStart;
			const count = side === "new" ? hunk.newLines : hunk.oldLines;
			// A hunk with no lines on this side names the line before the place its lines were removed from.
			hunks.push(count > 0 ? [start, start + count - 1] : [Math.max(1, start), start + 1]);
			let line = count > 0 ? start : start + 1;
			// A block is a run of changed lines between context lines. Lines this side gained mark themselves; a
			// block that only removed lines marks the two lines around the place they were removed from.
			let block = null;
			const close = () => {
				if (block && !block.gained) marks.push(Math.max(1, block.at - 1), block.at);
				block = null;
			};
			for (const body of hunk.lines) {
				if (body.startsWith(gained)) {
					block ??= { at: line, gained: false };
					block.gained = true;
					marks.push(line++);
				} else if (body.startsWith(lost)) block ??= { at: line, gained: false };
				else {
					close();
					line++;
				}
			}
			close();
		}
		out.push({ file: path, status: file.status === fresh ? "added" : "modified", hunks, changed: runs(marks) });
	}
	return out;
}

/** Why a dataset row cannot be a case, or null when it can. */
export function ineligibleReason(instance, { maxLines = MAX_CHANGED_LINES, maxFiles = MAX_FILES } = {}) {
	const files = parsePatch(instance.patch);
	if (!files.length || files.every((file) => !file.hunks.length)) return "no text hunks";
	if (files.some((file) => file.binary)) return "binary change";
	if (files.some((file) => file.status === "renamed")) return "rename";
	const stats = patchStats(files);
	if (stats.changed > maxLines) return `more than ${maxLines} changed lines`;
	if (stats.files > maxFiles) return `more than ${maxFiles} files`;
	return null;
}

// ---------------------------------------------------------------------------------------------------------------
// Sample

function digest(text) {
	return createHash("sha256").update(text).digest("hex");
}

const byKey = (a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0);

/**
 * Every eligible instance in one seeded order that goes round the repositories: the repositories in a seeded order,
 * each repository's instances in a seeded order, then round 1 takes the first instance of every repository, round 2
 * the second of every repository that still has one, and so on. Small repositories are therefore represented as well
 * as large ones until they run out. The order does not depend on how many cases are drawn.
 */
export function caseOrder(instances, { seed = DEFAULT_SEED, maxLines, maxFiles } = {}) {
	const byRepo = new Map();
	const seen = new Set();
	for (const instance of instances) {
		if (seen.has(instance.instance_id) || ineligibleReason(instance, { maxLines, maxFiles })) continue;
		seen.add(instance.instance_id);
		if (!byRepo.has(instance.repo)) byRepo.set(instance.repo, []);
		byRepo.get(instance.repo).push({ instance, key: digest(`${seed}:instance:${instance.instance_id}`) });
	}
	const repos = [...byRepo.entries()]
		.map(([repo, entries]) => ({ repo, entries: entries.sort(byKey), key: digest(`${seed}:repo:${repo}`) }))
		.sort(byKey);
	const order = [];
	for (let round = 0; repos.some((repo) => repo.entries.length > round); round++) {
		for (const repo of repos) if (repo.entries.length > round) order.push(repo.entries[round].instance);
	}
	return order;
}

/** A case's name: its kind and the task it is built from. */
export function caseId(kind, instanceId) {
	return `${kind}-${instanceId}`;
}

/**
 * The fixed sample: the first `2 * perKind` instances of `caseOrder`, alternately a buggy and a clean case, so both
 * kinds are spread over the repositories, no task is used twice, and a smaller draw is a prefix of a larger one.
 * Each case: `{ id, kind, instanceId, repo, baseCommit, stats, truth }` plus the dataset row as `instance`.
 */
export function sampleCases(instances, { perKind = DEFAULT_CASES_PER_KIND, seed = DEFAULT_SEED, maxLines, maxFiles } = {}) {
	return caseOrder(instances, { seed, maxLines, maxFiles })
		.slice(0, 2 * perKind)
		.map((instance, index) => caseSpec(instance, KINDS[index % 2]));
}

/**
 * One case from a dataset row. `truth` is where the reviewed diff's changes sit in its head: for a buggy case the
 * reverse of the gold patch (head numbering is the gold patch's old side, the original buggy file), for a clean
 * case the gold patch itself.
 */
export function caseSpec(instance, kind) {
	const files = parsePatch(instance.patch);
	return {
		id: caseId(kind, instance.instance_id),
		kind,
		instanceId: instance.instance_id,
		repo: instance.repo,
		baseCommit: instance.base_commit,
		stats: patchStats(files),
		truth: changeRanges(files, kind === "buggy" ? "old" : "new"),
		instance,
	};
}

/** `{ repo: count }` of a list of cases. */
export function repoCounts(cases) {
	const counts = {};
	for (const item of cases) counts[item.repo] = (counts[item.repo] ?? 0) + 1;
	return Object.fromEntries(Object.entries(counts).sort(([a], [b]) => (a < b ? -1 : 1)));
}

// ---------------------------------------------------------------------------------------------------------------
// Reviewer command

/** A command line as argv: whitespace separates, single and double quotes group, a backslash escapes outside single quotes. */
export function splitCommand(text) {
	const out = [];
	let current = "";
	let open = false;
	let quote = null;
	const source = String(text ?? "");
	for (let index = 0; index < source.length; index++) {
		const char = source[index];
		if (quote) {
			if (char === quote) quote = null;
			else if (char === "\\" && quote === '"' && index + 1 < source.length) current += source[++index];
			else current += char;
		} else if (char === "'" || char === '"') {
			quote = char;
			open = true;
		} else if (char === "\\" && index + 1 < source.length) {
			current += source[++index];
			open = true;
		} else if (/\s/.test(char)) {
			if (open || current) out.push(current);
			current = "";
			open = false;
		} else current += char;
	}
	if (quote) throw new Error(`unbalanced quote in command: ${text}`);
	if (open || current) out.push(current);
	return out;
}

/** The contract's flags for one review, as argv. */
export function contractArgs({ repo, base, head, model, verifyModel, budget }) {
	return [
		"--repo-dir",
		repo,
		"--base",
		base,
		"--head",
		head,
		...(model ? ["--model", model] : []),
		...(verifyModel ? ["--verify-model", verifyModel] : []),
		...(budget ? ["--budget", String(budget)] : []),
		"--json",
		"--dry-run",
	];
}

const PLACEHOLDER = /\{(repo|base|head|model|verifyModel|budget)\}/g;

/**
 * The argv of one review.
 *
 * Without `template` it is Ultron's own reviewer: `<ultron...> autoreview review` plus the contract's flags. With a
 * template (`--reviewer-cmd`) the template is split into argv and its placeholders `{repo}`, `{base}`, `{head}`,
 * `{model}`, `{verifyModel}` and `{budget}` are filled in (an unset one becomes the empty string); a template with
 * no `{repo}` placeholder gets the contract's flags appended instead, so a reviewer that takes the same flags as
 * Ultron's needs no placeholders at all.
 */
export function reviewerArgv({ template, ultron = ["ultron"] }, values) {
	if (!template) {
		// An arm written `provider/model@level` runs the finder and verifier frames at that thinking level;
		// `@find/verify` sets them separately.
		const [find, verify = find] = String(values.thinking ?? "").split("/");
		const thinking = find ? ["--thinking", find, "--verify-thinking", verify] : [];
		// AUTOREVIEW_BENCH_EXTRA_ARGS: extra reviewer flags for every arm of a run (e.g. "--mode both").
		const extra = (process.env.AUTOREVIEW_BENCH_EXTRA_ARGS ?? "").split(/\s+/).filter(Boolean);
		return [...ultron, "autoreview", "review", ...contractArgs(values), ...thinking, ...extra];
	}
	const argv = splitCommand(template);
	if (!argv.length) throw new Error("empty --reviewer-cmd");
	if (!template.includes("{repo}")) return [...argv, ...contractArgs(values)];
	return argv.map((part) => part.replace(PLACEHOLDER, (_, name) => String(values[name] ?? "")));
}

/** How `--ultron <path>` is started: a script through this Node, anything else as an executable. */
export function ultronCommand(path, node = process.execPath) {
	if (!path) return ["ultron"];
	return /\.(?:[cm]?js|ts)$/.test(path) ? [node, path] : [path];
}

// ---------------------------------------------------------------------------------------------------------------
// Reviews

function lastJsonObject(text) {
	const trimmed = String(text ?? "").trim();
	if (!trimmed) return null;
	try {
		return JSON.parse(trimmed);
	} catch {}
	// Tolerate stray lines before the object: the last line that starts an object which parses to the end.
	const lines = trimmed.split("\n");
	for (let index = lines.length - 1; index >= 0; index--) {
		if (!lines[index].trimStart().startsWith("{")) continue;
		try {
			return JSON.parse(lines.slice(index).join("\n"));
		} catch {}
	}
	return null;
}

const number = (value) => (typeof value === "number" && Number.isFinite(value) ? value : null);

/**
 * A reviewer's stdout as a review, or why it is not one. Findings are normalised: an unknown severity becomes
 * "nit", `endLine` defaults to `line`, a finding without a file is kept with `file: ""` (it can never be located).
 */
export function parseReview(stdout) {
	const value = lastJsonObject(stdout);
	if (!value || typeof value !== "object" || Array.isArray(value)) return { ok: false, error: "no JSON object on stdout" };
	if (!VERDICTS.includes(value.verdict)) return { ok: false, error: `unknown verdict: ${JSON.stringify(value.verdict)}` };
	if (!Array.isArray(value.findings)) return { ok: false, error: "findings is not an array" };
	const findings = value.findings.map((finding) => {
		const line = number(Number(finding?.line));
		const endLine = number(Number(finding?.endLine ?? Number.NaN));
		return {
			file: typeof finding?.file === "string" ? finding.file : "",
			line,
			endLine: endLine !== null && line !== null && endLine >= line ? endLine : line,
			severity: SEVERITIES.includes(finding?.severity) ? finding.severity : "nit",
			category: typeof finding?.category === "string" ? finding.category : "",
			claim: typeof finding?.claim === "string" ? finding.claim : "",
			why: typeof finding?.why === "string" ? finding.why : "",
			...(typeof finding?.suggestedFix === "string" ? { suggestedFix: finding.suggestedFix } : {}),
			verification: finding?.verification === "confirmed" ? "confirmed" : "uncertain",
			confidence: number(finding?.confidence),
			// Optional fields of newer reviewers, kept for analysis (level, origin, and where the comment would go).
			...(typeof finding?.level === "string" ? { level: finding.level } : {}),
			...(typeof finding?.source === "string" ? { source: finding.source } : {}),
			...(typeof finding?.posted === "string" ? { posted: finding.posted } : {}),
			...(Number.isFinite(finding?.rank) ? { rank: finding.rank } : {}),
		};
	});
	return {
		ok: true,
		review: {
			verdict: value.verdict,
			complete: value.complete !== false,
			findings,
			dropped: { rejected: number(value.dropped?.rejected) ?? 0, duplicates: number(value.dropped?.duplicates) ?? 0 },
			timing: {
				totalMs: number(value.timing?.totalMs),
				scopeMs: number(value.timing?.scopeMs),
				findMs: number(value.timing?.findMs),
				verifyMs: number(value.timing?.verifyMs),
			},
			usage: {
				inputTokens: number(value.usage?.inputTokens),
				outputTokens: number(value.usage?.outputTokens),
				costUsd: number(value.usage?.costUsd),
				frames: number(value.usage?.frames),
			},
			model: typeof value.model === "string" ? value.model : null,
			verifyModel: typeof value.verifyModel === "string" ? value.verifyModel : null,
			notChecked: Array.isArray(value.notChecked) ? value.notChecked : [],
		},
	};
}

/** A finding's path as the diff names it: no `./`, no `a/` or `b/` prefix, no repository directory in front. */
export function normalizePath(file, repoDir = "") {
	let path = String(file ?? "").replaceAll("\\", "/");
	const root = repoDir.replace(/\/+$/, "");
	if (root && path.startsWith(`${root}/`)) path = path.slice(root.length + 1);
	path = path.replace(/^(?:\.\/)+/, "");
	return path;
}

function samePath(finding, truthFile, repoDir) {
	const path = normalizePath(finding.file, repoDir);
	return path === truthFile || path === `a/${truthFile}` || path === `b/${truthFile}`;
}

/**
 * Whether a finding points at the changed region: its file is a ground-truth file and its lines `line..endLine`
 * overlap one of that file's ranges widened by `tolerance`. `ranges` picks the hunk spans ("hunks") or the changed
 * lines only ("changed"). A file the diff deletes has no lines in the head: any finding on that file is located.
 */
export function located(finding, truth, { ranges = "hunks", tolerance = LOCATION_TOLERANCE, repoDir = "" } = {}) {
	for (const entry of truth) {
		if (!samePath(finding, entry.file, repoDir)) continue;
		if (entry.status === "deleted") return true;
		if (finding.line === null) continue;
		const end = finding.endLine ?? finding.line;
		for (const [from, to] of entry[ranges]) {
			if (finding.line <= to + tolerance && end >= from - tolerance) return true;
		}
	}
	return false;
}

const isConfirmed = (finding) => finding.verification === "confirmed";
const isSerious = (finding) => finding.severity === "blocker" || finding.severity === "major";
const isNotable = (finding) => isSerious(finding) || finding.severity === "minor";

/** The findings of a buggy case's review that go to the judge: confirmed, minor or worse, most severe first, capped. */
export function judgeCandidates(review, max = MAX_JUDGED_FINDINGS) {
	return review.findings
		.map((finding, index) => ({ finding, index }))
		.filter(({ finding }) => isConfirmed(finding) && isNotable(finding))
		.sort((a, b) => SEVERITIES.indexOf(a.finding.severity) - SEVERITIES.indexOf(b.finding.severity) || a.index - b.index)
		.slice(0, max);
}

/**
 * The score of one review of one case.
 *
 * Buggy: `caught` when a confirmed blocker or major finding is located in a ground-truth hunk (+-5 lines);
 * `caughtLoose` also accepts minor; `caughtTight` is `caught` against the changed lines only (+-2 lines).
 * `judged`, when judgements (`[{ index, match }]`, by finding index) are given, is whether a confirmed blocker or
 * major finding was judged to describe the defect, wherever it points; `judgedLoose` also accepts minor.
 * Clean: `falseAlarm` when any confirmed blocker or major finding exists; `approved` when the verdict is approve.
 * Both: `verdictCorrect` (buggy expects request_changes, clean expects approve) and findings by severity.
 */
export function scoreCase(spec, review, { repoDir = "", judgements = null } = {}) {
	const bySeverity = Object.fromEntries(SEVERITIES.map((severity) => [severity, 0]));
	const confirmedBySeverity = { ...bySeverity };
	for (const finding of review.findings) {
		bySeverity[finding.severity]++;
		if (isConfirmed(finding)) confirmedBySeverity[finding.severity]++;
	}
	const score = {
		kind: spec.kind,
		verdict: review.verdict,
		verdictCorrect: review.verdict === EXPECTED_VERDICT[spec.kind],
		complete: review.complete,
		findings: review.findings.length,
		bySeverity,
		confirmedBySeverity,
	};
	const confirmed = review.findings.filter(isConfirmed);
	if (spec.kind === "clean") {
		score.falseAlarm = confirmed.some(isSerious);
		score.approved = review.verdict === "approve";
		return score;
	}
	const hit = (finding) => located(finding, spec.truth, { repoDir });
	score.caught = confirmed.some((finding) => isSerious(finding) && hit(finding));
	score.caughtLoose = confirmed.some((finding) => isNotable(finding) && hit(finding));
	score.caughtTight = confirmed.some(
		(finding) => isSerious(finding) && located(finding, spec.truth, { repoDir, ranges: "changed", tolerance: TIGHT_TOLERANCE }),
	);
	score.locatedFindings = review.findings.flatMap((finding, index) => (hit(finding) ? [index] : []));
	if (judgements) {
		const matched = new Set(judgements.filter((entry) => entry.match === true).map((entry) => entry.index));
		const judgedHit = (finding, index) => isConfirmed(finding) && matched.has(index);
		score.judged = review.findings.some((finding, index) => isSerious(finding) && judgedHit(finding, index));
		score.judgedLoose = review.findings.some((finding, index) => isNotable(finding) && judgedHit(finding, index));
		score.judgeErrors = judgements.filter((entry) => typeof entry.match !== "boolean").length;
	}
	return score;
}

// ---------------------------------------------------------------------------------------------------------------
// Judge

export const JUDGE_SYSTEM =
	"You grade one finding of an automated code review against a known defect. Answer with one JSON object and nothing else.";

const clip = (text, max) => (text.length > max ? `${text.slice(0, max)}\n[... cut ...]` : text);

/**
 * The judge's prompt for one finding: the defect as the task's problem statement, the fix upstream made (the gold
 * patch), and the finding. The answer must be `{"match": true|false, "reason": "..."}`.
 */
export function judgePrompt(spec, finding) {
	return [
		"A code change introduced a defect. The defect, as it was later reported:",
		"<defect_report>",
		clip(spec.instance.problem_statement.trim(), 12_000),
		"</defect_report>",
		"",
		"The fix that was later applied (the reviewed change is the reverse of this patch):",
		"<fix>",
		clip(spec.instance.patch.trim(), 12_000),
		"</fix>",
		"",
		"A reviewer of the change that introduced the defect reported this finding:",
		"<finding>",
		JSON.stringify(
			{
				file: finding.file,
				line: finding.line,
				endLine: finding.endLine,
				severity: finding.severity,
				category: finding.category,
				claim: finding.claim,
				why: finding.why,
				...(finding.suggestedFix ? { suggestedFix: finding.suggestedFix } : {}),
			},
			null,
			2,
		),
		"</finding>",
		"",
		"Does the finding describe this defect: the same faulty behaviour or its direct cause in the changed code?",
		"A finding about another problem, a style remark, or a vague warning that would fit any change is not a match.",
		'Answer with exactly one JSON object: {"match": true or false, "reason": "one sentence"}',
	].join("\n");
}

/** `{ match, reason }` from the judge's reply, or null when the reply holds no such object. */
export function parseJudgeReply(text) {
	const source = String(text ?? "");
	const candidates = source.match(/\{[^{}]*\}/g) ?? [];
	for (const candidate of candidates.reverse()) {
		try {
			const value = JSON.parse(candidate);
			if (typeof value.match === "boolean") return { match: value.match, reason: typeof value.reason === "string" ? value.reason : "" };
		} catch {}
	}
	return null;
}

// ---------------------------------------------------------------------------------------------------------------
// Aggregation

/** Nearest-rank percentile of a list of numbers, or null for an empty list. */
export function percentile(values, p) {
	const sorted = values.filter((value) => typeof value === "number" && Number.isFinite(value)).sort((a, b) => a - b);
	if (!sorted.length) return null;
	return sorted[Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1))];
}

const rate = (count, total) => (total ? count / total : null);
const present = (values) => values.filter((value) => typeof value === "number" && Number.isFinite(value));
const total = (values) => present(values).reduce((sum, value) => sum + value, 0);
const mean = (values) => (present(values).length ? total(values) / present(values).length : null);
const count = (records, test) => records.filter(test).length;

/**
 * Per arm, over all its records (every trial of every case). A record is `{ arm, case, kind, trial, status,
 * wallMs, score?, review? }`; only records with `status: "ok"` are scored, the others are counted as `errors`.
 */
export function summarize(records, arms) {
	const out = {};
	for (const arm of arms) {
		const mine = records.filter((record) => record.arm === arm);
		const ok = mine.filter((record) => record.status === "ok");
		const buggy = ok.filter((record) => record.kind === "buggy");
		const clean = ok.filter((record) => record.kind === "clean");
		const judgedRuns = buggy.filter((record) => typeof record.score.judged === "boolean");
		const severities = (set, field) =>
			Object.fromEntries(SEVERITIES.map((severity) => [severity, total(set.map((record) => record.score[field][severity]))]));
		const seconds = ok.map((record) => record.wallMs / 1000);
		const reviews = ok.map((record) => record.review);
		out[arm] = {
			runs: mine.length,
			scored: ok.length,
			errors: mine.length - ok.length,
			incomplete: count(ok, (record) => record.score.complete === false),
			buggy: {
				runs: buggy.length,
				caught: count(buggy, (record) => record.score.caught),
				caughtLoose: count(buggy, (record) => record.score.caughtLoose),
				caughtTight: count(buggy, (record) => record.score.caughtTight),
				recall: rate(
					count(buggy, (record) => record.score.caught),
					buggy.length,
				),
				recallLoose: rate(
					count(buggy, (record) => record.score.caughtLoose),
					buggy.length,
				),
				recallTight: rate(
					count(buggy, (record) => record.score.caughtTight),
					buggy.length,
				),
				judgedRuns: judgedRuns.length,
				recallJudged: rate(
					count(judgedRuns, (record) => record.score.judged),
					judgedRuns.length,
				),
				recallJudgedLoose: rate(
					count(judgedRuns, (record) => record.score.judgedLoose),
					judgedRuns.length,
				),
				verdictCorrect: count(buggy, (record) => record.score.verdictCorrect),
				findings: severities(buggy, "bySeverity"),
			},
			clean: {
				runs: clean.length,
				falseAlarms: count(clean, (record) => record.score.falseAlarm),
				falseAlarmRate: rate(
					count(clean, (record) => record.score.falseAlarm),
					clean.length,
				),
				approved: count(clean, (record) => record.score.approved),
				verdictCorrect: count(clean, (record) => record.score.verdictCorrect),
				findings: severities(clean, "bySeverity"),
				confirmedFindings: severities(clean, "confirmedBySeverity"),
			},
			verdictAccuracy: rate(
				count(ok, (record) => record.score.verdictCorrect),
				ok.length,
			),
			findingsPerReview: mean(ok.map((record) => record.score.findings)),
			seconds: { p50: percentile(seconds, 50), p90: percentile(seconds, 90), max: percentile(seconds, 100) },
			reviewerTimingMs: {
				total: mean(reviews.map((review) => review.timing.totalMs)),
				scope: mean(reviews.map((review) => review.timing.scopeMs)),
				find: mean(reviews.map((review) => review.timing.findMs)),
				verify: mean(reviews.map((review) => review.timing.verifyMs)),
			},
			tokens: {
				input: total(reviews.map((review) => review.usage.inputTokens)),
				output: total(reviews.map((review) => review.usage.outputTokens)),
				inputPerReview: mean(reviews.map((review) => review.usage.inputTokens)),
				outputPerReview: mean(reviews.map((review) => review.usage.outputTokens)),
			},
			costUsd: { total: total(reviews.map((review) => review.usage.costUsd)), perReview: mean(reviews.map((review) => review.usage.costUsd)) },
			framesPerReview: mean(reviews.map((review) => review.usage.frames)),
		};
	}
	return out;
}

// ---------------------------------------------------------------------------------------------------------------
// Report

/** Text with the home directory written as `~`, so recorded commands carry no local paths. */
export function withoutHome(text, home) {
	return home ? String(text).split(home).join("~") : String(text);
}

/** A model or arm name as part of a file name. */
export function slug(text) {
	return String(text).replace(/[^a-z0-9.-]+/gi, "_");
}

/** `acceptance/quality` file stem: the date, the benchmark, the sample size and the arms. */
export function resultStem({ date, perKind, arms }) {
	return `${date}-autoreview-bench-${perKind}x2-${arms.map(slug).join("+")}`;
}

const percent = (value) => (value === null || value === undefined ? "-" : `${(value * 100).toFixed(0)}%`);
const fraction = (hits, of) => (of ? `${hits}/${of} (${percent(hits / of)})` : "-");
const fixed = (value, digits = 1) => (value === null || value === undefined ? "-" : value.toFixed(digits));
const thousands = (value) => (value === null || value === undefined ? "-" : Math.round(value).toLocaleString("en-US"));
const dollars = (value) => (value === null || value === undefined ? "-" : `$${value.toFixed(value < 1 ? 3 : 2)}`);
const cell = (text) => String(text).replaceAll("|", "\\|").replaceAll("\n", " ");

function table(header, rows) {
	return [`| ${header.join(" | ")} |`, `| ${header.map(() => "---").join(" | ")} |`, ...rows.map((row) => `| ${row.map(cell).join(" | ")} |`)];
}

const MARK = { true: "yes", false: "no", undefined: "-" };

/** The markdown rendering of a result file. */
export function renderMarkdown(result) {
	const lines = [
		`# Autoreview benchmark: ${result.arms.map((arm) => arm.name).join(", ")}`,
		"",
		`${result.date}. ${result.sample.cases.length} cases (${result.sample.buggy} buggy, ${result.sample.clean} clean) from ${result.dataset}, seed \`${result.sample.seed}\`, ${result.trials} trial${result.trials === 1 ? "" : "s"} per case.`,
		`Reviewer: \`${result.reviewer.command}\`${result.reviewer.version ? ` (${result.reviewer.version})` : ""}.`,
		result.judge ? `Judge: \`${result.judge.model}\`, one call per confirmed finding of minor severity or worse on buggy cases.` : "Judge: none (judged recall not measured).",
		"",
		"## Summary",
		"",
		...table(
			[
				"arm",
				"recall (blocker/major)",
				"recall (+minor)",
				"recall (changed lines)",
				"recall (judged)",
				"false alarms",
				"verdict accuracy",
				"findings / review",
				"p50 s",
				"p90 s",
				"max s",
				"tokens in / out per review",
				"cost per review",
				"errors",
			],
			result.arms.map(({ name }) => {
				const summary = result.summary[name];
				return [
					name,
					fraction(summary.buggy.caught, summary.buggy.runs),
					fraction(summary.buggy.caughtLoose, summary.buggy.runs),
					fraction(summary.buggy.caughtTight, summary.buggy.runs),
					summary.buggy.judgedRuns ? percent(summary.buggy.recallJudged) : "-",
					fraction(summary.clean.falseAlarms, summary.clean.runs),
					percent(summary.verdictAccuracy),
					fixed(summary.findingsPerReview),
					fixed(summary.seconds.p50),
					fixed(summary.seconds.p90),
					fixed(summary.seconds.max),
					`${thousands(summary.tokens.inputPerReview)} / ${thousands(summary.tokens.outputPerReview)}`,
					dollars(summary.costUsd.perReview),
					summary.errors,
				];
			}),
		),
		"",
		"Recall is over buggy cases: a confirmed finding inside a ground-truth hunk (+-5 lines) at blocker or major severity; `+minor` also",
		"accepts minor; `changed lines` requires the finding within 2 lines of a line the diff changed; `judged` is a judge model's verdict that",
		"a confirmed blocker or major finding describes the reported defect. False alarms are clean cases with a confirmed blocker or major finding.",
		"",
		"## Detail per arm",
		"",
		...table(
			[
				"arm",
				"buggy: request_changes",
				"clean: approve",
				"clean findings (blocker/major/minor/nit)",
				"buggy findings (blocker/major/minor/nit)",
				"reviewer ms (scope/find/verify)",
				"frames / review",
				"tokens in / out total",
				"cost total",
				"incomplete",
			],
			result.arms.map(({ name }) => {
				const summary = result.summary[name];
				const severities = (counts) => SEVERITIES.map((severity) => counts[severity]).join("/");
				const timing = summary.reviewerTimingMs;
				return [
					name,
					fraction(summary.buggy.verdictCorrect, summary.buggy.runs),
					fraction(summary.clean.approved, summary.clean.runs),
					severities(summary.clean.findings),
					severities(summary.buggy.findings),
					`${thousands(timing.scope)}/${thousands(timing.find)}/${thousands(timing.verify)}`,
					fixed(summary.framesPerReview),
					`${thousands(summary.tokens.input)} / ${thousands(summary.tokens.output)}`,
					dollars(summary.costUsd.total),
					summary.incomplete,
				];
			}),
		),
		"",
		"## Cases",
		"",
		...table(
			["case", "repository", "files", "hunks", "changed lines", "extra hunks"],
			result.sample.cases.map((item) => [item.id, item.repo, item.stats.files, item.stats.hunks, item.stats.changed, (item.noise ?? []).reduce((sum, entry) => sum + entry.hunks, 0)]),
		),
		"",
		"## Runs",
		"",
		...table(
			["arm", "case", "trial", "status", "verdict", "caught", "+minor", "changed lines", "judged", "false alarm", "findings (b/M/m/n)", "seconds", "tokens in/out", "cost"],
			result.records.map((record) => {
				const score = record.score;
				if (!score) return [record.arm, record.case, record.trial, `${record.status}: ${record.error ?? ""}`, "-", "-", "-", "-", "-", "-", "-", fixed(record.wallMs / 1000), "-", "-"];
				return [
					record.arm,
					record.case,
					record.trial,
					score.complete ? "ok" : "ok (incomplete)",
					`${score.verdict}${score.verdictCorrect ? "" : " (wrong)"}`,
					MARK[score.caught],
					MARK[score.caughtLoose],
					MARK[score.caughtTight],
					MARK[score.judged],
					MARK[score.falseAlarm],
					SEVERITIES.map((severity) => score.bySeverity[severity]).join("/"),
					fixed(record.wallMs / 1000),
					`${thousands(record.review.usage.inputTokens)}/${thousands(record.review.usage.outputTokens)}`,
					dollars(record.review.usage.costUsd),
				];
			}),
		),
		"",
	];
	return lines.join("\n");
}
