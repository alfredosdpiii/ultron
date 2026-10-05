#!/usr/bin/env node
/**
 * Autoreview benchmark: how well and how fast an automated code reviewer does on diffs with a known bug (the
 * reverse of a SWE-bench Verified gold patch) and on diffs that should pass (the gold patch itself).
 *
 *   node evals/autoreview/run.mjs --plan [--cases 20] [--seed ultron-autoreview-1] [--models a,b]
 *   node evals/autoreview/run.mjs --build-only [--cases 20]
 *   node evals/autoreview/run.mjs [--cases 20] [--seed s] [--models provider/model,provider/model]
 *                                 [--verify-model provider/model] [--budget tokens] [--trials 1] [--concurrency 2]
 *                                 [--ultron <path to cli>] [--reviewer-cmd "<command template>"]
 *                                 [--judge-model provider/model] [--limit-minutes 20] [--noise 2] [--only id,id]
 *                                 [--run-id id] [--out <dir>]
 *
 * `--plan` lists the cases and arms and runs nothing. `--build-only` builds the case repositories and stops.
 * Anything else reviews every case with every arm: with Ultron's reviewer (and with `--judge-model`) that makes
 * paid model calls. An unknown flag exits 2 before anything starts. See README.md for the method and its limits.
 */

import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { buildCase, builtCase, DEFAULT_NOISE_HUNKS, ensureCommit, upstreamUrl } from "./cases.mjs";
import { askJudge, BENCH_HOME as HOME, log, parseArgs, pool, readJson, run, scrub, userSecrets, withPrivateDirs } from "./harness.mjs";
import {
	DEFAULT_CASES_PER_KIND,
	DEFAULT_SEED,
	JUDGE_SYSTEM,
	judgeCandidates,
	judgePrompt,
	parseJudgeReply,
	parseReview,
	renderMarkdown,
	repoCounts,
	resultStem,
	reviewerArgv,
	sampleCases,
	scoreCase,
	slug,
	summarize,
	ultronCommand,
	withoutHome,
} from "./lib.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, "../..");
const SWEBENCH_HOME = process.env.ULTRON_SWEBENCH_HOME || join(homedir(), ".cache", "ultron-swebench");
const DATASET_FILE = join(HOME, "dataset.jsonl");
const DATASET = "SWE-bench/SWE-bench_Verified";
const VALUE_FLAGS = [
	"cases",
	"seed",
	"models",
	"verify-model",
	"budget",
	"trials",
	"concurrency",
	"reviewer-cmd",
	"ultron",
	"judge-model",
	"out",
	"run-id",
	"limit-minutes",
	"noise",
	"only",
];
const SWITCH_FLAGS = ["plan", "build-only"];

function positiveInteger(flags, name, fallback, { allowZero = false } = {}) {
	if (flags[name] === undefined) return fallback;
	const value = Number(flags[name]);
	if (!Number.isInteger(value) || value < (allowZero ? 0 : 1)) {
		console.error(`--${name} must be ${allowZero ? "a non-negative" : "a positive"} integer, got ${flags[name]}`);
		process.exit(2);
	}
	return value;
}

// ---------------------------------------------------------------------------------------------------------------
// Dataset and cases

async function loadDataset() {
	if (!existsSync(DATASET_FILE)) {
		const python = join(SWEBENCH_HOME, "venv", "bin", "python");
		if (!existsSync(python)) {
			console.error(`No dataset export at ${DATASET_FILE} and no virtualenv to make one: run \`node evals/swebench/run.mjs setup\` first.`);
			process.exit(1);
		}
		mkdirSync(HOME, { recursive: true });
		log("exporting the dataset with gold patches (from the local Hugging Face cache)");
		const script = join(HERE, "export_dataset.py");
		let exported = await run(python, [script, DATASET_FILE, DATASET], { env: { ...process.env, HF_DATASETS_OFFLINE: "1", HF_HUB_OFFLINE: "1" } });
		if (exported.code !== 0) {
			log("not in the local cache; downloading the public dataset");
			exported = await run(python, [script, DATASET_FILE, DATASET], { env: process.env });
		}
		if (exported.code !== 0) {
			console.error(`dataset export failed:\n${exported.stderr.slice(-2000)}`);
			process.exit(1);
		}
	}
	return readFileSync(DATASET_FILE, "utf8")
		.split("\n")
		.filter(Boolean)
		.map((line) => JSON.parse(line));
}

/** Build (or reuse) every case repository. Returns `Map<case id, { repoDir, base, head, noise }>`. */
function buildCases(specs, noiseHunks) {
	const built = new Map();
	const fetched = [];
	for (const spec of specs) {
		let entry = builtCase({ home: HOME, spec, noiseHunks });
		if (!entry) {
			let mirror;
			try {
				const ensured = ensureCommit({ home: HOME, repo: spec.repo, sha: spec.baseCommit });
				mirror = ensured.mirror;
				if (ensured.fetched) {
					fetched.push(`${spec.repo}@${spec.baseCommit.slice(0, 12)}`);
					log(`fetched ${spec.repo} ${spec.baseCommit.slice(0, 12)} from ${upstreamUrl(spec.repo)}`);
				}
			} catch (error) {
				console.error(`${spec.id}: no local copy of ${spec.repo} at ${spec.baseCommit} and fetching it from ${upstreamUrl(spec.repo)} failed: ${error.message}`);
				process.exit(1);
			}
			entry = buildCase({ home: HOME, spec, source: mirror, noiseHunks });
			log(`built ${spec.id} (${entry.noise.length} extra file change${entry.noise.length === 1 ? "" : "s"})`);
		}
		built.set(spec.id, entry);
	}
	if (fetched.length) log(`fetched ${fetched.length} commit${fetched.length === 1 ? "" : "s"} from the public upstreams`);
	return built;
}

// ---------------------------------------------------------------------------------------------------------------
// Reviews

/** One judge call: the Ultron CLI in print mode with no tools, answering with a strict JSON object. */
async function judgeFinding(ctx, spec, finding, logPath) {
	const prompt = judgePrompt(spec, finding);
	const result = await askJudge({ ultron: ctx.ultron, model: ctx.judgeModel, system: JUDGE_SYSTEM, prompt });
	writeFileSync(logPath, scrub(`${result.stdout}\n--- stderr ---\n${result.stderr.slice(-20_000)}\n`, ctx.secrets));
	const reply = result.code === 0 && !result.timedOut ? parseJudgeReply(result.stdout) : null;
	return reply ?? { match: null, reason: result.timedOut ? "judge timed out" : `no verdict (exit ${result.code})` };
}

async function judgeReview(ctx, spec, review, dir) {
	const judgements = [];
	for (const { finding, index } of judgeCandidates(review)) {
		const verdict = await judgeFinding(ctx, spec, finding, join(dir, `judge-${index}.txt`));
		judgements.push({ index, ...verdict });
	}
	return judgements;
}

async function runReview(ctx, { arm, spec, trial }) {
	const built = ctx.built.get(spec.id);
	const dir = join(ctx.runDir, slug(arm.name), spec.id, `trial-${trial}`);
	const recordPath = join(dir, "record.json");
	const base = { arm: arm.name, case: spec.id, kind: spec.kind, instanceId: spec.instanceId, repo: spec.repo, trial };
	const finish = (record) => {
		writeFileSync(recordPath, `${JSON.stringify(record, null, 2)}\n`);
		return record;
	};
	const score = (record, judgements) => ({
		...record,
		...(judgements ? { judge: judgements } : {}),
		score: scoreCase(spec, record.review, { repoDir: built.repoDir, judgements: judgements ?? null }),
	});

	const previous = readJson(recordPath);
	if (previous?.status === "ok") {
		const needsJudge = ctx.judgeModel && spec.kind === "buggy" && !previous.judge;
		if (!needsJudge) return score(previous, previous.judge);
		return finish(score(previous, await judgeReview(ctx, spec, previous.review, dir)));
	}

	mkdirSync(dir, { recursive: true });
	const argv = reviewerArgv(ctx.reviewer, {
		repo: built.repoDir,
		base: built.base,
		head: built.head,
		model: arm.model,
		thinking: arm.thinking,
		verifyModel: ctx.verifyModel,
		budget: ctx.budget,
	});
	const own = !ctx.reviewer.template;
	const result = await withPrivateDirs({ credentials: own }, (env) =>
		run(argv[0], argv.slice(1), { env, cwd: own ? built.repoDir : process.cwd(), timeoutMs: ctx.limitMs }),
	);
	writeFileSync(join(dir, "stdout.json"), scrub(result.stdout, ctx.secrets));
	writeFileSync(join(dir, "stderr.txt"), scrub(result.stderr, ctx.secrets));
	const record = { ...base, exitCode: result.code, wallMs: result.wallMs };
	if (result.timedOut) return finish({ ...record, status: "timeout", error: `no review within ${ctx.limitMs / 60_000} minutes` });
	const parsed = parseReview(result.stdout);
	if (!parsed.ok) {
		const tail = result.stderr.trim().split("\n").at(-1) ?? "";
		return finish({ ...record, status: "error", error: scrub(`${parsed.error} (exit ${result.code})${tail ? `: ${tail.slice(0, 300)}` : ""}`, ctx.secrets) });
	}
	const ok = { ...record, status: "ok", review: parsed.review };
	const judgements = ctx.judgeModel && spec.kind === "buggy" ? await judgeReview(ctx, spec, parsed.review, dir) : null;
	return finish(score(ok, judgements));
}

// ---------------------------------------------------------------------------------------------------------------
// Main

function printPlan({ specs, arms, flags, reviewerCommand, trials, noiseHunks }) {
	const seed = flags.seed ?? DEFAULT_SEED;
	console.log(`Autoreview benchmark plan: ${specs.length} cases, seed ${seed}, ${trials} trial${trials === 1 ? "" : "s"} per case and arm`);
	console.log(`Repositories: ${Object.entries(repoCounts(specs)).map(([repo, n]) => `${repo} ${n}`).join(", ")}`);
	console.log("");
	console.log(["case", "kind", "repository", "base commit", "files", "hunks", "changed", "built"].join("\t"));
	for (const spec of specs) {
		const built = builtCase({ home: HOME, spec, noiseHunks });
		console.log([spec.id, spec.kind, spec.repo, spec.baseCommit.slice(0, 12), spec.stats.files, spec.stats.hunks, spec.stats.changed, built ? "yes" : "no"].join("\t"));
	}
	console.log("");
	console.log(`Arms (${arms.length}): ${arms.map((arm) => arm.name).join(", ")}`);
	console.log(`Reviewer: ${reviewerCommand}`);
	console.log(`Judge: ${flags["judge-model"] ?? "none"}`);
	console.log(`Reviews to run: ${specs.length * arms.length * trials}`);
}

async function main() {
	const { flags, unknown } = parseArgs(process.argv.slice(2), VALUE_FLAGS, SWITCH_FLAGS);
	if (unknown.length) {
		console.error(`Unknown arguments: ${unknown.join(" ")}`);
		console.error("See the header of evals/autoreview/run.mjs for the flags.");
		process.exit(2);
	}
	const perKind = positiveInteger(flags, "cases", DEFAULT_CASES_PER_KIND);
	const trials = positiveInteger(flags, "trials", 1);
	const concurrency = positiveInteger(flags, "concurrency", 2);
	const limitMinutes = positiveInteger(flags, "limit-minutes", 20);
	const noiseHunks = positiveInteger(flags, "noise", DEFAULT_NOISE_HUNKS, { allowZero: true });
	const budget = flags.budget === undefined ? null : positiveInteger(flags, "budget", null);
	const seed = flags.seed ?? DEFAULT_SEED;
	if (flags["reviewer-cmd"] && flags.ultron && !flags["judge-model"]) {
		console.error("--ultron has no effect with --reviewer-cmd unless --judge-model is given (the judge runs on the Ultron CLI).");
		process.exit(2);
	}
	const models = (flags.models ?? "")
		.split(",")
		.map((model) => model.trim())
		.filter(Boolean);
	const arms = models.length ? models.map((spec) => ({ name: flags["verify-model"] ? `${spec}+verify:${flags["verify-model"]}` : spec, model: spec.split("@")[0], thinking: spec.split("@")[1] ?? null })) : [{ name: flags["reviewer-cmd"] ? "custom" : "default", model: null }];
	const ultron = ultronCommand(flags.ultron);
	const reviewer = { template: flags["reviewer-cmd"] ?? null, ultron };
	const reviewerCommand = withoutHome(
		reviewerArgv(reviewer, {
			repo: "<repo>",
			base: "<base>",
			head: "<head>",
			model: models.length ? "<model>" : null,
			verifyModel: flags["verify-model"],
			budget,
		}).join(" "),
		homedir(),
	);

	const instances = await loadDataset();
	let specs = sampleCases(instances, { perKind, seed });
	if (flags.only) {
		const only = new Set(flags.only.split(","));
		specs = specs.filter((spec) => only.has(spec.id) || only.has(spec.instanceId));
	}
	if (!specs.length) {
		console.error("No cases selected.");
		process.exit(1);
	}

	if (flags.plan) {
		printPlan({ specs, arms, flags, reviewerCommand, trials, noiseHunks });
		return;
	}

	const built = buildCases(specs, noiseHunks);
	if (flags["build-only"]) {
		log(`${built.size} case repositories ready under ${join(HOME, "cases")}`);
		return;
	}

	const date = new Date().toISOString().slice(0, 10);
	const stem = resultStem({ date, perKind, arms: arms.map((arm) => arm.name) });
	const runId = flags["run-id"] ?? `${stem}-${new Date().toISOString().slice(11, 19).replaceAll(":", "")}`;
	const runDir = join(HOME, "runs", runId);
	mkdirSync(runDir, { recursive: true });
	const ctx = {
		built,
		runDir,
		reviewer,
		ultron,
		verifyModel: flags["verify-model"] ?? null,
		budget,
		judgeModel: flags["judge-model"] ?? null,
		limitMs: limitMinutes * 60_000,
		secrets: reviewer.template && !flags["judge-model"] ? [] : userSecrets(),
	};
	let version = null;
	if (!reviewer.template) {
		// An Ultron without the command would take "autoreview review ..." as a prompt and call a model with it.
		// `--help` calls no model: the reviews start only when its output names the command.
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

	const jobs = [];
	for (const arm of arms) for (const spec of specs) for (let trial = 1; trial <= trials; trial++) jobs.push({ arm, spec, trial });
	log(`${jobs.length} reviews (${specs.length} cases x ${arms.length} arm${arms.length === 1 ? "" : "s"} x ${trials} trial${trials === 1 ? "" : "s"}), ${concurrency} at a time; evidence in ${runDir}`);
	const records = await pool(jobs, concurrency, async (job) => {
		const record = await runReview(ctx, job);
		const outcome = record.status !== "ok" ? `${record.status}: ${record.error}` : `${record.review.verdict}, ${record.review.findings.length} finding${record.review.findings.length === 1 ? "" : "s"}`;
		log(`${job.arm.name} ${job.spec.id} #${job.trial}: ${outcome} (${(record.wallMs / 1000).toFixed(1)}s)`);
		return record;
	});

	const result = {
		benchmark: "autoreview-bench",
		date,
		dataset: DATASET,
		runId,
		trials,
		reviewer: { command: reviewerCommand, kind: reviewer.template ? "custom" : "ultron", version, verifyModel: ctx.verifyModel, budget },
		judge: ctx.judgeModel ? { model: ctx.judgeModel, system: JUDGE_SYSTEM } : null,
		arms,
		sample: {
			seed,
			perKind,
			buggy: specs.filter((spec) => spec.kind === "buggy").length,
			clean: specs.filter((spec) => spec.kind === "clean").length,
			repos: repoCounts(specs),
			noiseHunks,
			cases: specs.map((spec) => ({
				id: spec.id,
				kind: spec.kind,
				instanceId: spec.instanceId,
				repo: spec.repo,
				baseCommit: spec.baseCommit,
				stats: spec.stats,
				truth: spec.truth,
				base: built.get(spec.id).base,
				head: built.get(spec.id).head,
				noise: built.get(spec.id).noise,
			})),
		},
		summary: summarize(
			records,
			arms.map((arm) => arm.name),
		),
		records,
	};
	const outDir = resolve(flags.out ?? join(REPO_ROOT, "acceptance", "quality"));
	mkdirSync(outDir, { recursive: true });
	// Result files carry no local paths: findings may name files under the case directory or the home directory.
	const clean = JSON.parse(withoutHome(withoutHome(JSON.stringify(result), `${join(HOME, "cases")}/`), homedir()));
	writeFileSync(join(outDir, `${stem}.json`), `${JSON.stringify(clean, null, 2)}\n`);
	writeFileSync(join(outDir, `${stem}.md`), renderMarkdown(clean));
	writeFileSync(join(runDir, "result.json"), `${JSON.stringify(result, null, 2)}\n`);
	console.log(renderMarkdown(clean).split("\n## Cases")[0]);
	log(`wrote ${join(outDir, `${stem}.json`)} and .md`);
}

main().catch((error) => {
	console.error(error.stack ?? String(error));
	process.exit(1);
});
