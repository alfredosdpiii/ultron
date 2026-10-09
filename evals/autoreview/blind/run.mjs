#!/usr/bin/env node
/**
 * Runner of the blind comparison benchmark: reviews the cases of a collected set with each arm, then has a judge
 * model compare the result with the reference reviewer's comments.
 *
 *   node evals/autoreview/blind/run.mjs --set <name> --plan [--models a,b] [--only id,id]
 *   node evals/autoreview/blind/run.mjs --set <name> [--ultron <path to cli>] [--models a,b] [--verify-model m]
 *                                       [--judge-model provider/model] [--judge-thinking level] [--concurrency 2]
 *                                       [--limit-minutes 20] [--run-id id] [--only id,id] [--reviewer-cmd "<command>"]
 *   node evals/autoreview/blind/run.mjs --set <name> --run-id <id> --redacted
 *
 * Stages, each kept per case so a rerun with the same `--run-id` does only what is missing: classify the reference
 * comments (once per set and judge model), review, match, judge the extras. Reviews and judge calls are paid model
 * calls. `--plan` lists the cases and arms and runs nothing. `--redacted` runs nothing either: it prints the
 * aggregate numbers of a finished run, with no repository, login, title, path, code or comment text. Reports stay
 * under the set directory, never in the repository. An unknown flag exits 2. See ../README.md.
 */

import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { git } from "../cases.mjs";
import { askJudge, log, parseArgs, pool, readJson, run, scrub, userSecrets, withPrivateDirs } from "../harness.mjs";
import { normalizePath, parsePatch, parseReview, reviewerArgv, slug, ultronCommand, withoutHome } from "../lib.mjs";
import { builtCase, setDir } from "./collect.mjs";
import {
	classifyPrompt,
	extraCandidates,
	extraPrompt,
	hunksNear,
	JUDGE_SYSTEM,
	matchable,
	matchCandidates,
	matchPrompt,
	parseClassification,
	parseExtra,
	parseMatch,
	regressions,
	renderMarkdown,
	renderRedacted,
	reviewKey,
	sampleShape,
	scoreCase,
	sourceWindow,
	summarize,
} from "./lib.mjs";

const VALUE_FLAGS = ["set", "ultron", "models", "verify-model", "judge-model", "judge-thinking", "concurrency", "limit-minutes", "run-id", "only", "reviewer-cmd", "baseline"];
const SWITCH_FLAGS = ["plan", "redacted"];
/** A judge reply that is not the asked JSON object is asked for again, this many times in all. */
const JUDGE_ATTEMPTS = 3;

function positiveInteger(flags, name, fallback) {
	if (flags[name] === undefined) return fallback;
	const value = Number(flags[name]);
	if (!Number.isInteger(value) || value < 1) {
		console.error(`--${name} must be a positive integer, got ${flags[name]}`);
		process.exit(2);
	}
	return value;
}

function writeJson(path, value) {
	writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
}

function privateDir(path) {
	mkdirSync(path, { recursive: true, mode: 0o700 });
	return path;
}

function loadCases(dir) {
	const file = join(dir, "cases.jsonl");
	if (!existsSync(file)) {
		console.error("This set has no cases.jsonl: run evals/autoreview/blind/collect.mjs for it first.");
		process.exit(1);
	}
	return readFileSync(file, "utf8")
		.split("\n")
		.filter(Boolean)
		.map((line) => JSON.parse(line));
}

// ---------------------------------------------------------------------------------------------------------------
// Judge

/** A file of a case repository at a commit, or null. Offline: a case repository has no remote. */
function fileAt(repoDir, commit, path) {
	try {
		return git(["-C", repoDir, "show", `${commit}:${path}`], { env: { GIT_NO_LAZY_FETCH: "1" } });
	} catch {
		return null;
	}
}

/**
 * One judge question: ask, parse, ask again when the reply is not the asked object. Every attempt's output is kept
 * in `logPath`, scrubbed of credentials. Returns the parsed answer or `{ error }`.
 */
async function ask(ctx, prompt, parse, logPath) {
	let kept = "";
	let error = "no answer";
	for (let attempt = 1; attempt <= JUDGE_ATTEMPTS; attempt++) {
		const result = await askJudge({ ultron: ctx.ultron, model: ctx.judgeModel, thinking: ctx.judgeThinking, system: JUDGE_SYSTEM, prompt });
		kept += `--- attempt ${attempt} (exit ${result.code}${result.timedOut ? ", timed out" : ""}) ---\n${result.stdout}\n--- stderr ---\n${result.stderr.slice(-20_000)}\n`;
		writeFileSync(logPath, scrub(kept, ctx.secrets), { mode: 0o600 });
		const answer = result.code === 0 && !result.timedOut ? parse(result.stdout) : null;
		if (answer) return answer;
		error = result.timedOut ? "judge timed out" : result.code === 0 ? "reply is not the asked JSON object" : `judge exited ${result.code}`;
	}
	return { error };
}

const commentWindow = (built, comment) => sourceWindow(fileAt(built.repoDir, comment.side === "LEFT" ? built.base : built.head, comment.path), comment.startLine ?? comment.line, comment.line);

/** Stage 1: the classification of a case's reference comments, kept per set and judge model. */
async function classifyCase(ctx, spec) {
	const dir = privateDir(join(ctx.dir, "classify", slug(ctx.judgeModel + (ctx.judgeThinking ? `@${ctx.judgeThinking}` : "")), spec.id));
	const file = join(dir, "classification.json");
	const previous = readJson(file) ?? [];
	const done = new Map(previous.filter((entry) => !entry.error).map((entry) => [entry.id, entry]));
	const built = ctx.built.get(spec.id);
	const out = [];
	for (const comment of spec.referenceComments) {
		if (done.has(comment.id)) {
			out.push(done.get(comment.id));
			continue;
		}
		const answer = await ask(ctx, classifyPrompt(spec, comment, commentWindow(built, comment)), parseClassification, join(dir, `classify-${comment.id}.txt`));
		out.push({ id: comment.id, ...answer });
	}
	writeJson(file, out);
	return out;
}

/** Stage 3: each matchable reference comment against our findings in its file, then, if unmatched, all of them. */
async function matchCase(ctx, spec, classification, review, dir) {
	const file = join(dir, "match.json");
	const key = reviewKey(review);
	const previous = readJson(file);
	const done = new Map((previous?.key === key ? previous.matches : []).filter((entry) => !entry.error).map((entry) => [entry.id, entry]));
	const built = ctx.built.get(spec.id);
	const comments = new Map(spec.referenceComments.map((comment) => [comment.id, comment]));
	const matches = [];
	for (const entry of matchable(classification.filter((item) => !item.error))) {
		if (done.has(entry.id)) {
			matches.push(done.get(entry.id));
			continue;
		}
		const comment = comments.get(entry.id);
		const all = matchCandidates(review, comment, "all", built.repoDir);
		const sameFile = matchCandidates(review, comment, "file", built.repoDir);
		let result = { matched: null, how: null, reason: "the review has no findings", pass: 0 };
		const window = all.length ? commentWindow(built, comment) : "";
		const pass = async (candidates, scope, number) => {
			const indexes = candidates.map((candidate) => candidate.index);
			const answer = await ask(ctx, matchPrompt(spec, comment, entry, window, candidates, scope), (text) => parseMatch(text, indexes), join(dir, `match-${entry.id}-${number}.txt`));
			return { ...answer, pass: number };
		};
		if (sameFile.length) result = await pass(sameFile, "file", 1);
		const matched = result.how === "same_issue" || result.how === "partial";
		if (!matched && !result.error && all.length > sameFile.length) {
			const second = await pass(all, "all", 2);
			// The second pass replaces the first only when it finds the issue; "same location" from the first is kept.
			if (second.error || second.how === "same_issue" || second.how === "partial" || !sameFile.length) result = second;
		}
		matches.push({ id: entry.id, ...result });
	}
	writeJson(file, { key, matches });
	return matches;
}

/** Stage 4: each confirmed finding of minor severity or worse that matched no reference comment, against the code. */
async function extrasCase(ctx, spec, review, matches, dir) {
	const file = join(dir, "extras.json");
	const { judged, skipped } = extraCandidates(review, matches);
	const key = `${reviewKey(review)}:${judged.map((entry) => entry.index).join(",")}`;
	const previous = readJson(file);
	const done = new Map((previous?.key === key ? previous.extras : []).filter((entry) => !entry.error).map((entry) => [entry.index, entry]));
	const built = ctx.built.get(spec.id);
	let diff = null;
	const extras = [];
	for (const { finding, index } of judged) {
		if (done.has(index)) {
			extras.push(done.get(index));
			continue;
		}
		const path = normalizePath(finding.file, built.repoDir);
		diff ??= parsePatch(git(["-C", built.repoDir, "diff", "--no-color", "--no-ext-diff", built.base, built.head], { env: { GIT_NO_LAZY_FETCH: "1" } }));
		const window = sourceWindow(fileAt(built.repoDir, built.head, path), finding.line, finding.endLine, 30);
		const answer = await ask(ctx, extraPrompt(spec, finding, window, hunksNear(diff, path, finding.line, finding.endLine)), parseExtra, join(dir, `extra-${index}.txt`));
		extras.push({ index, ...answer });
	}
	writeJson(file, { key, skipped, extras });
	return { extras, skipped };
}

// ---------------------------------------------------------------------------------------------------------------
// Reviews

/** Stage 2: one review through the reviewer contract. The reviewer gets the repository and the two commits only. */
async function reviewCase(ctx, arm, spec, dir) {
	const recordPath = join(dir, "record.json");
	const previous = readJson(recordPath);
	if (previous?.status === "ok") return previous;
	const built = ctx.built.get(spec.id);
	const argv = reviewerArgv(ctx.reviewer, { repo: built.repoDir, base: built.base, head: built.head, model: arm.model, thinking: arm.thinking, verifyModel: ctx.verifyModel });
	const own = !ctx.reviewer.template;
	const result = await withPrivateDirs({ credentials: own }, (env) => run(argv[0], argv.slice(1), { env, cwd: own ? built.repoDir : process.cwd(), timeoutMs: ctx.limitMs }));
	writeFileSync(join(dir, "stdout.json"), scrub(result.stdout, ctx.secrets), { mode: 0o600 });
	writeFileSync(join(dir, "stderr.txt"), scrub(result.stderr, ctx.secrets), { mode: 0o600 });
	const base = { arm: arm.name, case: spec.id, firstOnCommit: spec.priorReviews.count === 0, exitCode: result.code, wallMs: result.wallMs };
	let record;
	if (result.timedOut) record = { ...base, status: "timeout", error: `no review within ${ctx.limitMs / 60_000} minutes` };
	else {
		const parsed = parseReview(result.stdout);
		const tail = result.stderr.trim().split("\n").at(-1) ?? "";
		record = parsed.ok
			? { ...base, status: "ok", review: parsed.review }
			: { ...base, status: "error", error: scrub(`${parsed.error} (exit ${result.code})${tail ? `: ${tail.slice(0, 300)}` : ""}`, ctx.secrets) };
	}
	writeJson(recordPath, record);
	return record;
}

async function runCase(ctx, { arm, spec }) {
	const dir = privateDir(join(ctx.runDir, slug(arm.name), spec.id));
	const record = await reviewCase(ctx, arm, spec, dir);
	if (record.status !== "ok" || !ctx.judgeModel) return record;
	const classification = ctx.classification.get(spec.id);
	const matches = await matchCase(ctx, spec, classification, record.review, dir);
	const { extras, skipped } = await extrasCase(ctx, spec, record.review, matches, dir);
	return { ...record, classification, matches, extras, score: scoreCase(spec, record.review, { classification, matches, extras, extrasSkipped: skipped }) };
}

// ---------------------------------------------------------------------------------------------------------------
// Main

function printPlan({ specs, dir, arms, flags, reviewerCommand }) {
	const shape = sampleShape(specs);
	console.log(`Blind comparison plan: ${specs.length} cases from ${shape.repositories} repositories, ${shape.referenceComments} reference comments`);
	console.log("");
	console.log(["case", "files", "changed", "reference comments", "prior reviews", "built"].join("\t"));
	for (const spec of specs) console.log([spec.id, spec.files, spec.changedLines, spec.referenceComments.length, spec.priorReviews.count, builtCase(dir, spec) ? "yes" : "no"].join("\t"));
	console.log("");
	console.log(`Arms (${arms.length}): ${arms.map((arm) => arm.name).join(", ")}`);
	console.log(`Reviewer: ${reviewerCommand}`);
	console.log(`Judge: ${flags["judge-model"] ?? "none (reviews only, nothing scored)"}${flags["judge-thinking"] ? ` (thinking ${flags["judge-thinking"]})` : ""}`);
	console.log(`Reviews to run: ${specs.length * arms.length}`);
	if (flags["judge-model"]) console.log(`Judge calls: ${shape.referenceComments} to classify (once per set), then per review one or two per matched comment and one per extra finding`);
}

async function main() {
	const { flags, unknown } = parseArgs(process.argv.slice(2), VALUE_FLAGS, SWITCH_FLAGS);
	if (unknown.length) {
		console.error(`Unknown arguments: ${unknown.join(" ")}`);
		console.error("See the header of evals/autoreview/blind/run.mjs for the flags.");
		process.exit(2);
	}
	if (!flags.set) {
		console.error("--set is required");
		process.exit(2);
	}
	let dir;
	try {
		dir = setDir(flags.set);
	} catch (error) {
		console.error(error.message);
		process.exit(2);
	}
	if (flags.redacted) {
		const report = flags["run-id"] ? readJson(join(dir, "runs", flags["run-id"], "report.json")) : null;
		if (!report) {
			console.error("--redacted prints the summary of a finished run: pass its --run-id.");
			process.exit(flags["run-id"] ? 1 : 2);
		}
		console.log(renderRedacted(report));
		return;
	}
	if (flags["judge-thinking"] && !flags["judge-model"]) {
		console.error("--judge-thinking needs --judge-model");
		process.exit(2);
	}
	const concurrency = positiveInteger(flags, "concurrency", 2);
	const limitMinutes = positiveInteger(flags, "limit-minutes", 20);
	const models = (flags.models ?? "")
		.split(",")
		.map((model) => model.trim())
		.filter(Boolean);
	// The first benchmark's arm syntax: `provider/model`, `provider/model@thinking`, `provider/model@find/verify`.
	const arms = models.length
		? models.map((spec) => ({ name: flags["verify-model"] ? `${spec}+verify:${flags["verify-model"]}` : spec, model: spec.split("@")[0], thinking: spec.split("@")[1] ?? null }))
		: [{ name: flags["reviewer-cmd"] ? "custom" : "default", model: null }];
	const ultron = ultronCommand(flags.ultron);
	const reviewer = { template: flags["reviewer-cmd"] ?? null, ultron };
	const reviewerCommand = withoutHome(
		reviewerArgv(reviewer, { repo: "<repo>", base: "<base>", head: "<head>", model: models.length ? "<model>" : null, thinking: models.some((spec) => spec.includes("@")) ? "<thinking>" : null, verifyModel: flags["verify-model"] }).join(" "),
		homedir(),
	);

	let specs = loadCases(dir);
	if (flags.only) {
		const only = new Set(flags.only.split(","));
		specs = specs.filter((spec) => only.has(spec.id));
	}
	if (!specs.length) {
		console.error("No cases selected.");
		process.exit(1);
	}
	if (flags.plan) {
		printPlan({ specs, dir, arms, flags, reviewerCommand });
		return;
	}

	const built = new Map();
	for (const spec of specs) {
		const entry = builtCase(dir, spec);
		if (!entry) {
			console.error(`${spec.id}: its repository is not built: run evals/autoreview/blind/collect.mjs for this set again.`);
			process.exit(1);
		}
		built.set(spec.id, entry);
	}
	const date = new Date().toISOString().slice(0, 10);
	const runId = flags["run-id"] ?? `${date}-${new Date().toISOString().slice(11, 19).replaceAll(":", "")}-${arms.map((arm) => slug(arm.name)).join("+")}`;
	const runDir = privateDir(join(dir, "runs", runId));
	const ctx = {
		dir,
		built,
		runDir,
		reviewer,
		ultron,
		verifyModel: flags["verify-model"] ?? null,
		judgeModel: flags["judge-model"] ?? null,
		judgeThinking: flags["judge-thinking"] ?? null,
		limitMs: limitMinutes * 60_000,
		secrets: reviewer.template && !flags["judge-model"] ? [] : userSecrets(),
		classification: new Map(),
	};
	let version = null;
	if (!reviewer.template) {
		// An Ultron without the command would take "autoreview review ..." as a prompt and call a model with it.
		const probe = (args) => withPrivateDirs({ credentials: false }, (env, work) => run(ultron[0], [...ultron.slice(1), ...args], { env, cwd: work, timeoutMs: 60_000 }));
		const help = await probe(["autoreview", "--help"]);
		if (help.code !== 0 || !/autoreview/.test(help.stdout + help.stderr)) {
			console.error(`\`${withoutHome(ultron.join(" "), homedir())} autoreview --help\` does not describe an autoreview command: this Ultron does not have it.`);
			console.error("Pass --ultron <path to a build that has it>, or --reviewer-cmd for another reviewer.");
			process.exit(1);
		}
		const versionProbe = await probe(["--version"]);
		version = versionProbe.code === 0 ? `ultron ${versionProbe.stdout.trim().split("\n").at(-1)}` : null;
	}

	if (ctx.judgeModel) {
		log(`classifying the reference comments of ${specs.length} cases (kept per set and judge model)`);
		await pool(specs, concurrency, async (spec) => ctx.classification.set(spec.id, await classifyCase(ctx, spec)));
	}
	const jobs = [];
	for (const arm of arms) for (const spec of specs) jobs.push({ arm, spec });
	log(`${jobs.length} reviews (${specs.length} cases x ${arms.length} arm${arms.length === 1 ? "" : "s"}), ${concurrency} at a time; evidence under the set directory, run ${runId}`);
	const records = await pool(jobs, concurrency, async (job) => {
		const record = await runCase(ctx, job);
		const outcome = record.status !== "ok" ? `${record.status}: ${record.error}` : `${record.review.verdict}, ${record.review.findings.length} finding${record.review.findings.length === 1 ? "" : "s"}`;
		const scored = record.score ? `; reference ${record.score.substantive.sameIssue}+${record.score.substantive.partial}/${record.score.substantive.total}, extras valid ${record.score.findings.extraValid}` : "";
		log(`${job.arm.name} ${job.spec.id}: ${outcome}${scored} (${(record.wallMs / 1000).toFixed(1)}s)`);
		return record;
	});

	const result = {
		benchmark: "autoreview-blind",
		date,
		set: flags.set,
		runId,
		reviewer: { command: reviewerCommand, kind: reviewer.template ? "custom" : "ultron", version, verifyModel: ctx.verifyModel },
		judge: ctx.judgeModel ? { model: ctx.judgeModel, thinking: ctx.judgeThinking, system: JUDGE_SYSTEM } : null,
		arms,
		sample: { shape: sampleShape(specs), cases: specs.map((spec) => ({ id: spec.id, files: spec.files, changedLines: spec.changedLines, referenceComments: spec.referenceComments.length, priorReviews: spec.priorReviews, reviewState: spec.reviewState })) },
		summary: summarize(
			records,
			arms.map((arm) => arm.name),
		),
		records,
	};
	// Reports carry no local paths: findings may name files under the case directory or the home directory.
	const clean = JSON.parse(withoutHome(withoutHome(JSON.stringify(result), `${join(dir, "cases")}/`), homedir()));
	writeJson(join(runDir, "report.json"), clean);
	writeFileSync(join(runDir, "report.md"), renderMarkdown(clean), { mode: 0o600 });
	writeFileSync(join(runDir, "report.redacted.md"), renderRedacted(clean), { mode: 0o600 });
	console.log(renderRedacted(clean));
	log(`wrote report.json, report.md and report.redacted.md under the set directory, runs/${runId}`);
	// A gate: against an earlier run's report.json, a drop of more than 10 points on an arm both have exits 3.
	if (flags.baseline !== undefined) {
		const baseline = JSON.parse(readFileSync(flags.baseline, "utf8"));
		const worse = regressions(result.summary, baseline.summary);
		if (worse.length > 0) {
			console.error(`Regressions against the baseline:\n${worse.map((line) => `  ${line}`).join("\n")}`);
			process.exit(3);
		}
		log("no regression against the baseline");
	}
}

main().catch((error) => {
	console.error(error.stack ?? String(error));
	process.exit(1);
});
