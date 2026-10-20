#!/usr/bin/env node
/**
 * DeepSWE (datacurve/deep-swe, Harbor format) for ultron-rs: the swarm A/B on long-horizon tasks.
 *
 *   node evals/deepswe/run.mjs export                      the task rows from the gated parquet into the cache
 *   node evals/deepswe/run.mjs gold [--only id,id]         grade every reference patch (must all pass first)
 *   node evals/deepswe/run.mjs smoke [--arms ...]          one tiny prompt per arm in one task image
 *   node evals/deepswe/run.mjs run --run-id <id> [--arms ultron,ultron-swarm] [--only id,id] [--concurrency 2]
 *                                  [--limit-minutes 90] [--attempts 2] [--skip-gold]
 *   node evals/deepswe/run.mjs grade --run-id <id>         regrade the run's patches
 *   node evals/deepswe/run.mjs report --run-id <id> [--out <file>] [--date YYYY-MM-DD]
 *
 * `run` and `smoke` make paid model calls (cliproxyapi/gpt-6.1-sol only). Unknown flags exit 2 before anything
 * starts. Every flag takes `--binary <path>` too (default: eval-frozen/ultron-cf87c0643-portable). See README.md.
 */

import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createWriteStream, existsSync, mkdirSync, readdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import {
	AGENT_DIR,
	AGENT_ROOT,
	ARMS,
	BINARY,
	DATASET,
	DEFAULT_TASKS,
	KEY_ENV,
	KILL_LEFTOVERS,
	LABEL,
	MODEL,
	PROVIDER,
	PYTHON_MOUNT,
	REPO_DIR,
	SERVER_DIR,
	agentFiles,
	agentScript,
	buildPrompt,
	cleanPatch,
	containerName,
	diffCommand,
	gradeScript,
	gradeVerdict,
	hostStats,
	ledgerStats,
	parseArgs,
	parseReward,
	printedSessionId,
	priceTable,
	renderMarkdown,
	runMetrics,
	runStats,
	runStatus,
	summarize,
	usageSummary,
} from "./lib.mjs";
import { scrubSecret } from "../swebench/lib.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const HOME = process.env.ULTRON_DEEPSWE_HOME || join(homedir(), ".cache", "ultron-deepswe");
const TASKS_FILE = join(HOME, "tasks.jsonl");
const PARQUET = process.env.ULTRON_DEEPSWE_PARQUET || resolve(REPO, "..", "deepswe", "ds", "data", "test-00000-of-00001.parquet");
const USER_MODELS = join(homedir(), ".ultron", "agent", "models.json");
const DEFAULT_BINARY = resolve(REPO, "..", "eval-frozen", "ultron-cf87c0643-portable");
const BINARY_COMMIT = "cf87c0643";
const PYTHON_HOST = join(homedir(), ".local", "share", "uv", "python", "cpython-3.12-linux-x86_64-gnu");
const PYTHON_IN_CONTAINER = `${PYTHON_MOUNT}/bin/python3.12`;
const DIRECTIVE_FILE = join(HERE, "swarm-directive.md");
const SMOKE_PROMPT =
	"Use the rlm tool: in one cell print sys.version from Python, and run `python3 --version` with bash(). Then reply with exactly two lines: the kernel's Python version and the shell's python3 version. Do not change any file.";

// ---------------------------------------------------------------------------------------------------------------
// Processes

/** Run a command to completion, capturing its output. Never through a shell. */
function run(command, args, { input, env, timeoutMs, maxBytes = 256 * 1024 * 1024 } = {}) {
	return new Promise((done) => {
		const child = spawn(command, args, { env, stdio: ["pipe", "pipe", "pipe"] });
		const chunks = { stdout: [], stderr: [] };
		let size = 0;
		let timedOut = false;
		const timer = timeoutMs
			? setTimeout(() => {
					timedOut = true;
					child.kill("SIGKILL");
				}, timeoutMs)
			: null;
		for (const stream of ["stdout", "stderr"]) {
			child[stream].on("data", (chunk) => {
				size += chunk.length;
				if (size <= maxBytes) chunks[stream].push(chunk);
			});
		}
		child.on("error", (error) => {
			if (timer) clearTimeout(timer);
			done({ code: 127, stdout: "", stderr: String(error.message), timedOut });
		});
		child.on("close", (code) => {
			if (timer) clearTimeout(timer);
			done({
				code: code ?? 1,
				stdout: Buffer.concat(chunks.stdout).toString("utf8"),
				stderr: Buffer.concat(chunks.stderr).toString("utf8"),
				timedOut,
				truncated: size > maxBytes,
			});
		});
		child.stdin.on("error", () => {});
		child.stdin.end(input ?? "");
	});
}

const docker = (args, options) => run("docker", args, options);

/** Stream a long process's stdout to a file line by line, stderr (first MB) to another. */
function runToFiles(command, args, { env, stdoutPath, stderrPath, deadlineMs }) {
	return new Promise((done) => {
		const child = spawn(command, args, { env, stdio: ["ignore", "pipe", "pipe"] });
		const out = createWriteStream(stdoutPath);
		const err = createWriteStream(stderrPath);
		let stderrBytes = 0;
		let hitDeadline = false;
		const timer = setTimeout(() => {
			hitDeadline = true;
			child.kill("SIGKILL");
		}, deadlineMs);
		createInterface({ input: child.stdout, crlfDelay: Number.POSITIVE_INFINITY }).on("line", (line) => out.write(`${line}\n`));
		child.stderr.on("data", (chunk) => {
			if (stderrBytes < 1024 * 1024) err.write(chunk);
			stderrBytes += chunk.length;
		});
		child.on("error", () => {});
		child.on("close", (code) => {
			clearTimeout(timer);
			out.end(() => err.end(() => done({ code: code ?? 1, hitDeadline })));
		});
	});
}

const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

function log(message) {
	console.log(`[${new Date().toISOString().slice(11, 19)}] ${message}`);
}

function writeJson(path, value) {
	mkdirSync(dirname(path), { recursive: true });
	writeFileSync(path, `${JSON.stringify(value, null, "\t")}\n`);
}

function readJson(path) {
	try {
		return JSON.parse(readFileSync(path, "utf8"));
	} catch {
		return null;
	}
}

function walkFiles(dir) {
	if (!existsSync(dir)) return [];
	const files = [];
	for (const entry of readdirSync(dir, { withFileTypes: true })) {
		const path = join(dir, entry.name);
		if (entry.isDirectory()) files.push(...walkFiles(path));
		else if (entry.isFile()) files.push(path);
	}
	return files;
}

function scrubDir(dir, secrets) {
	let scrubbed = 0;
	for (const file of walkFiles(dir)) {
		const content = readFileSync(file);
		const held = secrets.filter((secret) => secret && secret.length >= 6 && content.includes(secret));
		if (held.length === 0) continue;
		writeFileSync(file, held.reduce((text, secret) => scrubSecret(text, secret), content.toString("utf8")));
		scrubbed++;
	}
	return scrubbed;
}

async function pool(jobs, concurrency, worker) {
	const queue = [...jobs];
	const runners = Array.from({ length: Math.max(1, concurrency) }, async () => {
		while (queue.length > 0) await worker(queue.shift());
	});
	await Promise.all(runners);
}

// ---------------------------------------------------------------------------------------------------------------
// Context

function loadTasks() {
	if (!existsSync(TASKS_FILE)) throw new Error(`no ${TASKS_FILE}: run \`node evals/deepswe/run.mjs export\` first`);
	return readFileSync(TASKS_FILE, "utf8")
		.split("\n")
		.filter(Boolean)
		.map((line) => JSON.parse(line));
}

function selectTasks(flags) {
	const ids = flags.only ? flags.only.split(",") : DEFAULT_TASKS;
	const tasks = loadTasks();
	return ids.map((id) => {
		const task = tasks.find((candidate) => candidate.task_id === id);
		if (!task) throw new Error(`task ${id} is not in ${TASKS_FILE}`);
		return task;
	});
}

function readUserModels() {
	const models = JSON.parse(readFileSync(USER_MODELS, "utf8"));
	const provider = models.providers?.[PROVIDER];
	if (!provider?.apiKey || !provider.baseUrl) throw new Error(`${PROVIDER} provider with apiKey and baseUrl not found in models.json`);
	return { models, key: provider.apiKey, baseUrl: provider.baseUrl };
}

async function createContext(flags, { needBinary = true } = {}) {
	const binary = resolve(flags.binary || DEFAULT_BINARY);
	if (needBinary && !existsSync(binary)) throw new Error(`no ultron binary at ${binary}`);
	const version = existsSync(binary)
		? await run(binary, ["--version"], { timeoutMs: 30_000, env: { PATH: "/usr/bin:/bin", HOME: "/nonexistent" } })
		: { stdout: "", stderr: "" };
	const proxy = readUserModels();
	const arms = flags.arms ? flags.arms.split(",") : ARMS;
	for (const arm of arms) if (!ARMS.includes(arm)) throw new Error(`unknown arm ${arm} (expected ${ARMS.join(", ")})`);
	return {
		binary,
		binaryInfo: { name: basename(binary), version: (version.stdout || version.stderr).trim().split("\n").pop() || null, commit: BINARY_COMMIT },
		python: realpathSync(PYTHON_HOST),
		proxy,
		price: priceTable(proxy.models),
		directive: existsSync(DIRECTIVE_FILE) ? readFileSync(DIRECTIVE_FILE, "utf8") : null,
		arms,
		runId: flags["run-id"] ?? null,
		runDir: flags["run-id"] ? join(HOME, "runs", flags["run-id"]) : null,
		concurrency: Number(flags.concurrency ?? 2),
		attempts: Number(flags.attempts ?? 2),
		limitMinutes: flags["limit-minutes"] ? Number(flags["limit-minutes"]) : null,
	};
}

// ---------------------------------------------------------------------------------------------------------------
// Containers (only ever ones this harness started: named and labelled ultron-deepswe)

/** The containers this process started and has not removed yet: the only ones its cleanup may remove. */
const started = new Set();

async function removeContainer(name) {
	await docker(["rm", "-f", name]);
	started.delete(name);
}

async function startContainer(ctx, task, name, { network, runId, agent = false }) {
	await docker(["rm", "-f", name]);
	started.add(name);
	const created = await docker([
		"run",
		"-d",
		"--name",
		name,
		"--label",
		`${LABEL}=${runId}`,
		"--network",
		network,
		"--memory",
		`${task.memory_mb ?? 8192}m`,
		"--cpus",
		String(task.cpus ?? 2),
		...(agent ? ["-v", `${ctx.binary}:${BINARY}:ro`, "-v", `${ctx.python}:${PYTHON_MOUNT}:ro`] : []),
		task.docker_image,
		"sleep",
		"infinity",
	]);
	if (created.code !== 0) throw new Error(`container start failed: ${created.stderr.trim().slice(-500)}`);
}

async function writeInto(name, path, content) {
	const written = await docker(["exec", "-i", name, "bash", "-c", `mkdir -p "$(dirname '${path}')" && cat > '${path}'`], { input: content });
	if (written.code !== 0) throw new Error(`could not write ${path}: ${written.stderr.trim().slice(-300)}`);
}

// ---------------------------------------------------------------------------------------------------------------
// One agent run

async function runAgentOnce(ctx, task, arm, attempt, { dir, prompt, limitSeconds, runId }) {
	rmSync(dir, { recursive: true, force: true });
	mkdirSync(dir, { recursive: true });
	const name = containerName(runId, "agent", task.task_id, arm);
	const record = {
		task_id: task.task_id,
		language: task.language,
		arm,
		attempt,
		model: MODEL,
		status: "harness_error",
		startedAt: new Date().toISOString(),
		network: "host (departs from the task's allow_internet=false: the proxy is on 127.0.0.1)",
		limitSeconds,
	};
	try {
		await startContainer(ctx, task, name, { network: "host", runId, agent: true });
		const made = await docker(["exec", name, "mkdir", "-p", AGENT_DIR, SERVER_DIR, `${AGENT_ROOT}/home`]);
		if (made.code !== 0) throw new Error(`container setup failed: ${made.stderr.trim().slice(-300)}`);
		const files = {
			...agentFiles(arm, { userModels: ctx.proxy.models, baseUrl: ctx.proxy.baseUrl, directive: ctx.directive }),
			[`${AGENT_ROOT}/prompt.txt`]: prompt,
			[`${AGENT_ROOT}/run.sh`]: agentScript({ limitSeconds, python: PYTHON_IN_CONTAINER }),
		};
		for (const [path, content] of Object.entries(files)) await writeInto(name, path, content);
		const head = await docker(["exec", name, "bash", "-c", `git config --global --add safe.directory ${REPO_DIR}; git -C ${REPO_DIR} rev-parse HEAD`]);
		if (head.code !== 0) throw new Error(`no git repository at ${REPO_DIR}: ${head.stderr.trim().slice(-300)}`);
		record.startCommit = head.stdout.trim().split("\n").pop();
		record.baseCommit = task.base_commit;
		const started = Date.now();
		const agent = await runToFiles("docker", ["exec", "-e", KEY_ENV, name, "bash", `${AGENT_ROOT}/run.sh`], {
			env: { ...process.env, [KEY_ENV]: ctx.proxy.key },
			stdoutPath: join(dir, "stdout.jsonl"),
			stderrPath: join(dir, "stderr.txt"),
			deadlineMs: (limitSeconds + 180) * 1000,
		});
		record.wallMs = Date.now() - started;
		record.exitCode = agent.code;
		record.timedOut = agent.hitDeadline || agent.code === 124 || agent.code === 137;
		await docker(["exec", name, "bash", "-c", KILL_LEFTOVERS], { timeoutMs: 60_000 });
		const diff = await docker(["exec", name, "bash", "-c", diffCommand(task.base_commit)]);
		if (diff.code !== 0 || diff.truncated) throw new Error(`git diff failed: ${diff.stderr.trim().slice(-300) || "output too large"}`);
		const cleaned = cleanPatch(diff.stdout);
		writeFileSync(join(dir, "patch.diff"), cleaned.patch);
		record.patch = { bytes: Buffer.byteLength(cleaned.patch), files: cleaned.files, dropped: cleaned.dropped };
		const copied = await docker(["cp", `${name}:${AGENT_DIR}`, join(dir, "agent")]);
		if (copied.code !== 0) record.copyError = copied.stderr.trim().slice(-300);
		// The unpacked runtime (bundled skills, docs, kernel sources) is the binary's own, the same in every run.
		rmSync(join(dir, "agent", "runtime"), { recursive: true, force: true });
		record.status = "collected";
	} catch (error) {
		record.error = String(error instanceof Error ? error.message : error).slice(0, 1000);
	} finally {
		await removeContainer(name);
	}
	record.keyScrubbedFiles = scrubDir(dir, [ctx.proxy.key]);
	if (record.status === "collected") {
		const stdout = existsSync(join(dir, "stdout.jsonl")) ? readFileSync(join(dir, "stdout.jsonl"), "utf8") : "";
		const sessionFiles = walkFiles(join(dir, "agent"))
			.filter((file) => file.endsWith(".jsonl") && file.includes("/sessions/"))
			.map((file) => ({ name: file.slice(dir.length + 1), text: readFileSync(file, "utf8") }));
		record.sessionFiles = sessionFiles.length;
		record.stats = runStats(sessionFiles, printedSessionId(stdout), ctx.price);
		if (!record.stats.turns && record.exitCode !== 0)
			record.error = (existsSync(join(dir, "stderr.txt")) ? readFileSync(join(dir, "stderr.txt"), "utf8") : "").trim().slice(-800) || null;
		record.status = runStatus({ exitCode: record.exitCode, timedOut: record.timedOut, turns: record.stats.turns, errorMessage: record.stats.errorMessage });
		await addUsage(ctx, dir, record);
	}
	record.endedAt = new Date().toISOString();
	writeJson(join(dir, "record.json"), record);
	return record;
}

/**
 * Ultron's own report of the run (`ultron usage <root session> --json`, every lane) and the host's call counters,
 * read from the copied evidence on the host. The usage command only reads session files; it runs with a scratch
 * HOME and server dir of its own all the same.
 */
async function addUsage(ctx, dir, record) {
	const sessionFiles = walkFiles(join(dir, "agent")).filter((file) => file.endsWith(".jsonl") && file.includes("/sessions/"));
	const rootId = record.stats?.rootSessionId;
	const rootFile = sessionFiles.find((file) => rootId && file.includes(rootId)) ?? sessionFiles[0];
	if (rootFile) {
		const text = readFileSync(rootFile, "utf8");
		record.host = hostStats(text);
		record.ledger = ledgerStats(text);
	}
	if (!rootId || !existsSync(ctx.binary)) return;
	const scratch = join(dir, ".usage-scratch");
	mkdirSync(scratch, { recursive: true });
	const report = await run(ctx.binary, ["usage", rootId, "--json"], {
		timeoutMs: 120_000,
		env: { PATH: "/usr/bin:/bin", HOME: scratch, ULTRON_CODING_AGENT_DIR: join(dir, "agent"), ULTRON_SERVER_DIR: join(scratch, "server"), ULTRON_HINDSIGHT_URL: "off" },
	});
	rmSync(scratch, { recursive: true, force: true });
	rmSync(join(dir, "agent", "runtime"), { recursive: true, force: true });
	if (report.code === 0) {
		writeFileSync(join(dir, "usage.json"), report.stdout);
		try {
			record.usage = usageSummary(JSON.parse(report.stdout));
		} catch (error) {
			record.usageError = String(error).slice(0, 300);
		}
	} else record.usageError = (report.stderr.trim() || `exit ${report.code}`).slice(-300);
}

async function runAgent(ctx, task, arm, options) {
	const earlier = [];
	for (let attempt = 1; ; attempt++) {
		const record = await runAgentOnce(ctx, task, arm, attempt, options);
		if (earlier.length > 0) record.earlierAttempts = earlier;
		const retry = attempt < ctx.attempts && ["harness_error", "agent_crash", "provider_error"].includes(record.status);
		if (!retry) {
			writeJson(join(options.dir, "record.json"), record);
			return record;
		}
		log(`${task.task_id} ${arm}: ${record.status} (${record.error ?? record.stats?.errorMessage ?? ""}); retrying`);
		earlier.push({ attempt, status: record.status, error: record.error ?? record.stats?.errorMessage ?? null, wallMs: record.wallMs ?? null });
		await sleep(60_000);
	}
}

// ---------------------------------------------------------------------------------------------------------------
// Grading: a second fresh container, no network, the task's verifier exactly as Harbor runs it

async function grade(ctx, task, patch, dir, { runId, kind }) {
	mkdirSync(dir, { recursive: true });
	const name = containerName(runId, kind, task.task_id);
	const result = { task_id: task.task_id, startedAt: new Date().toISOString(), network: "none" };
	try {
		await startContainer(ctx, task, name, { network: "none", runId });
		const hasPatch = patch.trim().length > 0;
		await writeInto(name, "/tests/test.sh", task.verifier_script);
		await writeInto(name, "/tests/test.patch", task.test_patch);
		if (hasPatch) await writeInto(name, "/grade/model.patch", patch);
		await writeInto(name, "/grade/grade.sh", gradeScript({ verifierTimeoutSec: task.verifier_timeout_sec ?? 1800, hasPatch }));
		const started = Date.now();
		const graded = await docker(["exec", name, "bash", "/grade/grade.sh"], { timeoutMs: ((task.verifier_timeout_sec ?? 1800) + 300) * 1000 });
		result.wallMs = Date.now() - started;
		result.exitCode = graded.code;
		writeFileSync(join(dir, "grade.log"), `${graded.stdout}\n--- stderr\n${graded.stderr}`);
		const reward = await docker(["exec", name, "cat", "/logs/verifier/reward.txt"]);
		result.reward = reward.code === 0 ? parseReward(reward.stdout) : null;
		result.verdict = gradeVerdict({ exitCode: result.exitCode, reward: result.reward });
	} catch (error) {
		result.verdict = "harness_error";
		result.error = String(error instanceof Error ? error.message : error).slice(0, 1000);
	} finally {
		await removeContainer(name);
	}
	writeJson(join(dir, "grade.json"), result);
	return result;
}

// ---------------------------------------------------------------------------------------------------------------
// Commands

async function commandExport() {
	mkdirSync(HOME, { recursive: true });
	const script = join(HOME, "export.py");
	writeFileSync(
		script,
		[
			"import duckdb, sys, json",
			"con = duckdb.connect()",
			"cur = con.execute(\"select * from read_parquet(?)\", [sys.argv[1]])",
			"cols = [d[0] for d in cur.description]",
			"with open(sys.argv[2], 'w') as f:",
			"    for r in cur.fetchall():",
			"        f.write(json.dumps(dict(zip(cols, r)), default=str) + '\\n')",
			"",
		].join("\n"),
	);
	const exported = await run("uvx", ["--with", "duckdb", "python", "-I", script, PARQUET, TASKS_FILE], { timeoutMs: 600_000 });
	if (exported.code !== 0) throw new Error(`export failed: ${exported.stderr.slice(-800)}`);
	log(`exported ${loadTasks().length} tasks to ${TASKS_FILE}`);
	return 0;
}

async function commandGold(flags) {
	const ctx = await createContext(flags, { needBinary: false });
	const tasks = selectTasks(flags);
	const results = [];
	await pool(tasks, ctx.concurrency, async (task) => {
		log(`gold ${task.task_id}: grading the reference patch`);
		const result = await grade(ctx, task, task.reference_patch, join(HOME, "gold", task.task_id), { runId: "gold", kind: "gold" });
		log(`gold ${task.task_id}: ${result.verdict} (reward ${result.reward}, ${Math.round((result.wallMs ?? 0) / 1000)}s)`);
		results.push(result);
	});
	// Merged with earlier gold runs of other tasks: `run` checks every task it runs against this file.
	const earlier = (readJson(join(HOME, "gold", "gold.json"))?.results ?? []).filter((entry) => !results.some((result) => result.task_id === entry.task_id));
	const all = [...earlier, ...results];
	writeJson(join(HOME, "gold", "gold.json"), { at: new Date().toISOString(), allPass: all.every((result) => result.verdict === "pass"), results: all });
	return results.every((result) => result.verdict === "pass") ? 0 : 1;
}

async function commandSmoke(flags) {
	const ctx = await createContext(flags);
	const task = selectTasks(flags)[0];
	const results = [];
	for (const arm of ctx.arms) {
		const dir = join(HOME, "smoke", arm);
		const record = await runAgentOnce(ctx, task, arm, 1, { dir, prompt: SMOKE_PROMPT, limitSeconds: 600, runId: "smoke" });
		log(`smoke ${arm}: ${record.status}, ${record.stats?.turns ?? 0} turns, answer: ${JSON.stringify(record.stats?.finalText ?? record.error ?? "")}`);
		results.push({ arm, status: record.status, turns: record.stats?.turns ?? 0, finalText: record.stats?.finalText ?? null, models: record.stats?.models ?? [], patchBytes: record.patch?.bytes ?? null });
	}
	writeJson(join(HOME, "smoke", "smoke.json"), { at: new Date().toISOString(), image: task.docker_image, task: task.task_id, binary: ctx.binaryInfo, results });
	return results.every((result) => result.status === "completed" && /3\.12/.test(result.finalText ?? "")) ? 0 : 1;
}

function recordDir(ctx, arm, taskId) {
	return join(ctx.runDir, arm, taskId);
}

async function gradeRecord(ctx, task, arm) {
	const dir = recordDir(ctx, arm, task.task_id);
	const record = readJson(join(dir, "record.json"));
	if (!record || record.status === "harness_error") return record;
	const patch = existsSync(join(dir, "patch.diff")) ? readFileSync(join(dir, "patch.diff"), "utf8") : "";
	log(`${task.task_id} ${arm}: grading (${Buffer.byteLength(patch)} bytes of patch)`);
	record.grade = await grade(ctx, task, patch, join(dir, "grade"), { runId: ctx.runId, kind: `grade-${arm}` });
	writeJson(join(dir, "record.json"), record);
	log(`${task.task_id} ${arm}: ${record.grade.verdict}`);
	return record;
}

async function commandRun(flags) {
	if (!flags["run-id"]) throw new Error("run needs --run-id");
	const ctx = await createContext(flags);
	const tasks = selectTasks(flags);
	if (!flags["skip-gold"]) {
		const gold = readJson(join(HOME, "gold", "gold.json"));
		const passed = new Set((gold?.results ?? []).filter((result) => result.verdict === "pass").map((result) => result.task_id));
		const missing = tasks.filter((task) => !passed.has(task.task_id)).map((task) => task.task_id);
		if (missing.length > 0) throw new Error(`gold has not passed for ${missing.join(", ")}: run \`gold\` first`);
	}
	if (ctx.arms.includes("ultron-swarm") && !ctx.directive) throw new Error(`no swarm directive at ${DIRECTIVE_FILE}`);
	// A rerun with more tasks (--only) extends the run: the manifest keeps every task and arm the run has had.
	const previous = readJson(join(ctx.runDir, "manifest.json"));
	writeJson(join(ctx.runDir, "manifest.json"), {
		runId: ctx.runId,
		createdAt: previous?.createdAt ?? new Date().toISOString(),
		dataset: DATASET,
		model: MODEL,
		binary: ctx.binaryInfo,
		arms: [...new Set([...(previous?.arms ?? []), ...ctx.arms])],
		tasks: [...new Set([...(previous?.tasks ?? []), ...tasks.map((task) => task.task_id)])],
		concurrency: ctx.concurrency,
	});
	const jobs = tasks.flatMap((task) => ctx.arms.map((arm) => ({ task, arm })));
	// Only this process's containers: another process may be running other tasks of the same run.
	const cleanup = async () => {
		for (const name of [...started]) await removeContainer(name);
	};
	process.on("SIGINT", () => cleanup().finally(() => process.exit(130)));
	process.on("SIGTERM", () => cleanup().finally(() => process.exit(143)));
	await pool(jobs, ctx.concurrency, async ({ task, arm }) => {
		const dir = recordDir(ctx, arm, task.task_id);
		const existing = readJson(join(dir, "record.json"));
		if (existing && existing.status !== "harness_error") {
			if (!existing.grade) await gradeRecord(ctx, task, arm);
			return;
		}
		const limitSeconds = ctx.limitMinutes ? ctx.limitMinutes * 60 : (task.agent_timeout_sec ?? 5400);
		log(`${task.task_id} ${arm}: starting (limit ${Math.round(limitSeconds / 60)} min)`);
		const record = await runAgent(ctx, task, arm, { dir, prompt: buildPrompt(task), limitSeconds, runId: ctx.runId });
		log(`${task.task_id} ${arm}: ${record.status} in ${Math.round((record.wallMs ?? 0) / 60000)} min, ${record.stats?.turns ?? 0} turns, ${record.patch?.bytes ?? 0} patch bytes`);
		await gradeRecord(ctx, task, arm);
	});
	await cleanup();
	return commandReport(flags, ctx);
}

async function commandGrade(flags) {
	if (!flags["run-id"]) throw new Error("grade needs --run-id");
	const ctx = await createContext(flags);
	const tasks = selectTasks(flags);
	await pool(
		tasks.flatMap((task) => ctx.arms.map((arm) => ({ task, arm }))),
		ctx.concurrency,
		({ task, arm }) => gradeRecord(ctx, task, arm),
	);
	return commandReport(flags, ctx);
}

/** A record as committed: ids, outcome, times, tokens, swarm calls. No task text, patches or paths. */
function publicRecord(record, price) {
	const stats = record.stats ?? {};
	return {
		task_id: record.task_id,
		language: record.language,
		arm: record.arm,
		status: record.status,
		attempt: record.attempt,
		earlierAttempts: record.earlierAttempts?.map(({ attempt, status, wallMs }) => ({ attempt, status, wallMs })),
		grade: record.grade ? { verdict: record.grade.verdict, reward: record.grade.reward, wallMs: record.grade.wallMs } : null,
		wallMs: record.wallMs ?? null,
		timedOut: record.timedOut ?? null,
		exitCode: record.exitCode ?? null,
		patch: record.patch ? { bytes: record.patch.bytes, files: record.patch.files.length } : null,
		metrics: runMetrics(record, price),
		usage: record.usage ?? null,
		host: record.host ? { cells: record.host.cells, hostCalls: record.host.hostCalls } : null,
		ledger: record.ledger ?? null,
		rootJournal: {
			turns: stats.rootTurns,
			cells: stats.cells,
			errorCells: stats.errorCells,
			frameCalls: stats.frameCalls,
			spawnCalls: stats.spawnCalls,
			tokens: stats.rootTokens,
			models: stats.models,
			swarmCallsInRootCode: stats.swarm?.root ?? null,
		},
	};
}

async function commandReport(flags, given) {
	if (!flags["run-id"]) throw new Error("report needs --run-id");
	const ctx = given ?? (await createContext(flags, { needBinary: false }));
	const manifest = readJson(join(ctx.runDir, "manifest.json")) ?? {};
	const taskIds = manifest.tasks ?? selectTasks(flags).map((task) => task.task_id);
	const arms = manifest.arms ?? ctx.arms;
	const records = [];
	for (const taskId of taskIds) for (const arm of arms) {
		const path = join(ctx.runDir, arm, taskId, "record.json");
		const record = readJson(path);
		if (!record) continue;
		// Records written before the usage report was collected get it now, from their evidence.
		if (record.stats && (!record.usage || !record.host || !record.ledger) && existsSync(ctx.binary)) {
			await addUsage(ctx, dirname(path), record);
			writeJson(path, record);
		}
		records.push(publicRecord(record, ctx.price));
	}
	const gold = readJson(join(HOME, "gold", "gold.json"));
	const sizes = [];
	for (const task of loadTasks().filter((candidate) => taskIds.includes(candidate.task_id))) {
		const inspected = await docker(["image", "inspect", "--format", "{{.Size}}", task.docker_image]);
		sizes.push({ task_id: task.task_id, bytes: inspected.code === 0 ? Number(inspected.stdout.trim()) : null });
	}
	const date = flags.date ?? new Date().toISOString().slice(0, 10);
	const result = {
		date,
		dataset: DATASET,
		model: MODEL,
		runId: ctx.runId,
		binary: manifest.binary ?? ctx.binaryInfo,
		arms,
		tasks: taskIds,
		directiveSha256: ctx.directive ? createHash("sha256").update(ctx.directive).digest("hex") : null,
		isolation: {
			container: "one fresh container per (task, arm) from the task's docker_image, sleep infinity, memory and cpus from the task row; removed after the run",
			network: "agent containers use --network host (the proxy is on 127.0.0.1:8317); this departs from the tasks' allow_internet=false. Grading containers use --network none.",
			agentState: "ULTRON_CODING_AGENT_DIR, ULTRON_SERVER_DIR and HOME are fresh dirs under /agent inside the container; models.json holds only the proxy provider cut to the one model, with $CLIPROXY_API_KEY in place of the key; no auth.json",
			key: `passed as ${KEY_ENV} in the environment of docker exec; never written to a file; evidence scrubbed of it`,
			swarmArm: "the same, plus the swarm directive written as APPEND_SYSTEM.md into the run's agent dir",
			python: "the REPL kernel runs on a read-only mounted python-build-standalone CPython 3.12 (ULTRON_PYTHON); the task's own toolchain is untouched",
		},
		grading: "Harbor's: tests/test.sh and tests/test.patch at /tests, `bash /tests/test.sh` after `git apply` of the prediction in /app, reward read from /logs/verifier/reward.txt",
		cost: "notional: tokens at the list prices in models.json; the proxy bills a subscription",
		gold: gold?.results?.filter((entry) => taskIds.includes(entry.task_id)).map(({ task_id, verdict, reward }) => ({ task_id, verdict, reward })) ?? null,
		imageSizes: sizes,
		summary: summarize(records, arms),
		records,
		notes: readJson(join(ctx.runDir, "notes.json")) ?? [],
	};
	const out = resolve(flags.out ?? join(REPO, "acceptance", "quality", `${date}-deepswe-swarm-${taskIds.length}-cliproxyapi_gpt-6.1-sol.json`));
	const text = JSON.stringify(result, null, "\t").split(homedir()).join("~");
	writeFileSync(out, `${text}\n`);
	writeFileSync(out.replace(/\.json$/, ".md"), renderMarkdown(JSON.parse(text)));
	log(`wrote ${out} and its .md`);
	return 0;
}

async function main() {
	const { command, flags, unknown } = parseArgs(process.argv.slice(2));
	if (unknown.length > 0 || !command) {
		console.error(`usage: run.mjs export|gold|smoke|run|grade|report [flags]; unknown: ${unknown.join(" ") || "(no command)"}`);
		return 2;
	}
	if (command === "export") return commandExport();
	if (command === "gold") return commandGold(flags);
	if (command === "smoke") return commandSmoke(flags);
	if (command === "run") return commandRun(flags);
	if (command === "grade") return commandGrade(flags);
	return commandReport(flags);
}

main().then(
	(code) => {
		process.exitCode = code;
	},
	(error) => {
		console.error(error instanceof Error ? error.message : error);
		process.exitCode = 1;
	},
);
