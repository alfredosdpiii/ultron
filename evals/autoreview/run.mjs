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

import { spawn } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { buildCase, builtCase, DEFAULT_NOISE_HUNKS, ensureCommit, upstreamUrl } from "./cases.mjs";
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
/** Everything the benchmark keeps outside the repository: dataset export, mirrors, case repositories, evidence. */
const HOME = process.env.ULTRON_AUTOREVIEW_HOME || join(homedir(), ".cache", "ultron-autoreview-bench");
const SWEBENCH_HOME = process.env.ULTRON_SWEBENCH_HOME || join(homedir(), ".cache", "ultron-swebench");
const DATASET_FILE = join(HOME, "dataset.jsonl");
const DATASET = "SWE-bench/SWE-bench_Verified";
/** Where the reviewer's credentials come from: only `models.json` and `auth.json` are copied, into a temp dir. */
const USER_PROFILE = process.env.ULTRON_AUTOREVIEW_PROFILE || join(homedir(), ".ultron", "agent");
const CREDENTIAL_FILES = ["models.json", "auth.json"];
const JUDGE_TIMEOUT_MS = 5 * 60 * 1000;

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

function parseArgs(argv) {
	const flags = {};
	const unknown = [];
	for (let index = 0; index < argv.length; index++) {
		const name = argv[index].replace(/^--/, "");
		if (argv[index].startsWith("--") && VALUE_FLAGS.includes(name) && index + 1 < argv.length) flags[name] = argv[++index];
		else if (argv[index].startsWith("--") && SWITCH_FLAGS.includes(name)) flags[name] = true;
		else unknown.push(argv[index]);
	}
	return { flags, unknown };
}

function log(message) {
	console.error(`[${new Date().toISOString().slice(11, 19)}] ${message}`);
}

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
// Processes

/** Run a command to completion in its own process group, capturing its output. Never through a shell. */
function run(command, args, { env, cwd, timeoutMs } = {}) {
	return new Promise((done) => {
		const started = performance.now();
		const child = spawn(command, args, { env, cwd, stdio: ["ignore", "pipe", "pipe"], detached: true });
		const chunks = { stdout: [], stderr: [] };
		let timedOut = false;
		const timer = timeoutMs
			? setTimeout(() => {
					timedOut = true;
					try {
						process.kill(-child.pid, "SIGKILL");
					} catch {
						child.kill("SIGKILL");
					}
				}, timeoutMs)
			: null;
		for (const stream of ["stdout", "stderr"]) child[stream].on("data", (chunk) => chunks[stream].push(chunk));
		const finish = (code, error) => {
			if (timer) clearTimeout(timer);
			done({
				code,
				timedOut,
				wallMs: Math.round(performance.now() - started),
				stdout: Buffer.concat(chunks.stdout).toString("utf8"),
				stderr: error ? String(error.message) : Buffer.concat(chunks.stderr).toString("utf8"),
			});
		};
		child.on("error", (error) => finish(127, error));
		child.on("close", (code) => finish(code ?? 1));
	});
}

async function pool(jobs, concurrency, worker) {
	const results = new Array(jobs.length);
	let next = 0;
	await Promise.all(
		Array.from({ length: Math.min(concurrency, jobs.length) }, async () => {
			while (next < jobs.length) {
				const index = next++;
				results[index] = await worker(jobs[index], index);
			}
		}),
	);
	return results;
}

// ---------------------------------------------------------------------------------------------------------------
// Isolation

/** String values of a JSON document that look like secrets (long, no spaces), for scrubbing evidence. */
function secretsIn(value, out = []) {
	if (typeof value === "string") {
		if (value.length >= 20 && !/\s/.test(value) && !/^https?:\/\//.test(value)) out.push(value);
	} else if (value && typeof value === "object") for (const child of Object.values(value)) secretsIn(child, out);
	return out;
}

function userSecrets() {
	const secrets = [];
	for (const file of CREDENTIAL_FILES) {
		try {
			secretsIn(JSON.parse(readFileSync(join(USER_PROFILE, file), "utf8")), secrets);
		} catch {}
	}
	return secrets;
}

function scrub(text, secrets) {
	let out = text;
	for (const secret of secrets) out = out.split(secret).join("[redacted]");
	return out;
}

/**
 * Run `body(env, work)` with a private Ultron server dir and agent dir under a short temp path (socket paths are
 * limited to about 100 characters), deleted afterwards whatever happens. With `credentials`, the agent dir gets
 * copies of the user's `models.json` and `auth.json` and HOME is an empty directory, so the reviewer sees none of
 * the user's settings, skills, extensions or memory; the copies die with the temp dir and never reach the evidence.
 */
async function withPrivateDirs({ credentials }, body) {
	const work = mkdtempSync(join("/tmp", "u-ar-"));
	try {
		const agentDir = join(work, "a");
		const serverDir = join(work, "s");
		mkdirSync(agentDir);
		mkdirSync(serverDir);
		const env = { ...process.env, ULTRON_SERVER_DIR: serverDir, ULTRON_CODING_AGENT_DIR: agentDir, ULTRON_HINDSIGHT_URL: "off" };
		if (credentials) {
			for (const file of CREDENTIAL_FILES) {
				if (existsSync(join(USER_PROFILE, file))) copyFileSync(join(USER_PROFILE, file), join(agentDir, file));
			}
			const home = join(work, "h");
			mkdirSync(home);
			Object.assign(env, {
				HOME: home,
				USERPROFILE: home,
				XDG_CONFIG_HOME: join(home, ".config"),
				XDG_CACHE_HOME: join(home, ".cache"),
				XDG_DATA_HOME: join(home, ".local", "share"),
				XDG_STATE_HOME: join(home, ".local", "state"),
			});
		}
		return await body(env, work);
	} finally {
		rmSync(work, { recursive: true, force: true });
	}
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

function readJson(path) {
	try {
		return JSON.parse(readFileSync(path, "utf8"));
	} catch {
		return null;
	}
}

/** One judge call: the Ultron CLI in print mode with no tools, answering with a strict JSON object. */
async function judgeFinding(ctx, spec, finding, logPath) {
	const prompt = judgePrompt(spec, finding);
	const result = await withPrivateDirs({ credentials: true }, (env, work) =>
		run(
			ctx.ultron[0],
			[
				...ctx.ultron.slice(1),
				"-p",
				"--model",
				ctx.judgeModel,
				"--no-session",
				"--no-tools",
				"--no-extensions",
				"--no-skills",
				"--no-context-files",
				"--system-prompt",
				JUDGE_SYSTEM,
				prompt,
			],
			{ env, cwd: work, timeoutMs: JUDGE_TIMEOUT_MS },
		),
	);
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
	const { flags, unknown } = parseArgs(process.argv.slice(2));
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
	const arms = models.length ? models.map((model) => ({ name: model, model })) : [{ name: flags["reviewer-cmd"] ? "custom" : "default", model: null }];
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
