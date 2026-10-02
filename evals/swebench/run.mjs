#!/usr/bin/env node
/**
 * SWE-bench Verified for Ultron, Codex CLI and stock Pi on one model through the local proxy, and (`--arms
 * claude,ultron-claude`) for plain Claude Code and `ultron --claude` on the user's Claude subscription.
 *
 *   node evals/swebench/run.mjs setup                       private virtualenv (swebench) and the dataset export
 *   node evals/swebench/run.mjs sample [--n 10]             print the seeded sample
 *   node evals/swebench/run.mjs verify [--arms ultron,codex,pi]
 *   node evals/swebench/run.mjs run --run-id pilot10 [--n 10] [--arms ...] [--concurrency 2] [--limit-minutes 30]
 *                                   [--only id,id] [--skip-verify] [--skip-eval] [--ultron-package <dir>]
 *                                   [--max-utilization 0.9]
 *   node evals/swebench/run.mjs eval --run-id pilot10 [--arms ...] [--gold]
 *   node evals/swebench/run.mjs report --run-id pilot10 [--out acceptance/quality/<name>.json]
 *
 * `run` is the whole thing: verify each arm, run every (task, arm) not yet recorded, score with the official
 * evaluation, write the report. It makes paid model calls; an unknown flag exits 2 before anything starts.
 * See README.md for what is isolated and how each arm is configured.
 */

import { spawn } from "node:child_process";
import {
	createWriteStream,
	existsSync,
	mkdirSync,
	openSync,
	readdirSync,
	readFileSync,
	readSync,
	closeSync,
	realpathSync,
	renameSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import {
	AGENT_DIR,
	ARM_RUNTIMES,
	ARM_STOPPERS,
	ARMS,
	CLAUDE_CONFIG_DIR,
	CLAUDE_MAX_UTILIZATION,
	CLAUDE_TOKEN_ENV,
	DATASET,
	DEFAULT_SEED,
	HARNESS_FAILURES,
	KEY_ENV,
	MODEL_ID,
	MOUNT,
	PROVIDER,
	REASONING_EFFORT,
	REPO_DIR,
	RERUN_STATUSES,
	SPLIT,
	agentScript,
	armAuth,
	armDescriptions,
	armSpec,
	buildPrompt,
	claudeAuthPlan,
	claudeCheck,
	claudeSecrets,
	claudeStats,
	claudeTranscriptStats,
	cleanPatch,
	codexConfigToml,
	codexStats,
	evalVerdict,
	headToHead,
	isClaudeAuthError,
	isStreamingNoise,
	ledgerStats,
	modelsJson,
	networkLookups,
	notionalCost,
	overLimit,
	piStats,
	prediction,
	priceTable,
	renderMarkdown,
	repoCounts,
	runModel,
	runStatus,
	sample,
	scrubSecret,
	subscriptionUse,
	summarize,
	ultronStats,
	ultronUsageSummary,
} from "./lib.mjs";
import { startRecorder } from "./recorder.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, "../..");
/** Everything the harness keeps outside the repository: virtualenv, dataset export, runs with their transcripts. */
const HOME = process.env.ULTRON_SWEBENCH_HOME || join(homedir(), ".cache", "ultron-swebench");
const VENV_PYTHON = join(HOME, "venv", "bin", "python");
const DATASET_FILE = join(HOME, "dataset.jsonl");
const USER_MODELS = process.env.ULTRON_SWEBENCH_MODELS || join(homedir(), ".ultron", "agent", "models.json");
/**
 * The one file of the user's Claude Code config dir a Claude arm may see: the login. It is only ever read (for
 * the access token's expiry and to scrub the evidence) and mounted read-only; nothing here writes to that dir.
 */
const CLAUDE_CREDENTIALS = join(process.env.CLAUDE_CONFIG_DIR || join(homedir(), ".claude"), ".credentials.json");
const VERIFY_PROMPT =
	"Run `python --version` in the shell, then reply with exactly the line it printed and nothing else. Do not change any file.";

const VALUE_FLAGS = ["n", "seed", "arms", "run-id", "concurrency", "limit-minutes", "only", "out", "memory", "attempts", "ultron-package", "max-utilization"];
const SWITCH_FLAGS = ["skip-verify", "skip-eval", "gold", "no-ledger"];

function parseArgs(argv) {
	const [command, ...rest] = argv;
	const flags = {};
	const unknown = [];
	for (let index = 0; index < rest.length; index++) {
		const name = rest[index].replace(/^--/, "");
		if (rest[index].startsWith("--") && VALUE_FLAGS.includes(name)) flags[name] = rest[++index];
		else if (rest[index].startsWith("--") && SWITCH_FLAGS.includes(name)) flags[name] = true;
		else unknown.push(rest[index]);
	}
	return { command, flags, unknown };
}

// ---------------------------------------------------------------------------------------------------------------
// Processes

/** Run a command to completion, capturing its output. Never through a shell. */
function run(command, args, { input, env, cwd, timeoutMs, maxBytes = 64 * 1024 * 1024 } = {}) {
	return new Promise((done) => {
		const child = spawn(command, args, { env, cwd, stdio: ["pipe", "pipe", "pipe"] });
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
			done({ code: 127, stdout: "", stderr: String(error.message), timedOut, truncated: false });
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

/**
 * Run the agent: stdout goes line by line to `stdoutPath` (lines `dropLine` rejects are left out), stderr to
 * `stderrPath` (first megabyte). `onDeadline` runs when the backstop timer fires; the process is expected to end
 * by itself afterwards.
 */
function runAgentProcess(command, args, { env, stdoutPath, stderrPath, dropLine, deadlineMs, onDeadline }) {
	return new Promise((done) => {
		const child = spawn(command, args, { env, stdio: ["ignore", "pipe", "pipe"] });
		const out = createWriteStream(stdoutPath);
		const err = createWriteStream(stderrPath);
		let stderrBytes = 0;
		let hitDeadline = false;
		const timer = setTimeout(() => {
			hitDeadline = true;
			Promise.resolve(onDeadline()).finally(() => setTimeout(() => child.kill("SIGKILL"), 30_000).unref());
		}, deadlineMs);
		const lines = createInterface({ input: child.stdout, crlfDelay: Number.POSITIVE_INFINITY });
		lines.on("line", (line) => {
			if (!dropLine || !dropLine(line)) out.write(`${line}\n`);
		});
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

// ---------------------------------------------------------------------------------------------------------------
// Setup: virtualenv, dataset, runtimes

async function setup() {
	mkdirSync(HOME, { recursive: true });
	if (!existsSync(VENV_PYTHON)) {
		log("creating the virtualenv (uv, a standalone CPython 3.12)");
		const venv = await run("uv", ["venv", "--python", "3.12", join(HOME, "venv")]);
		if (venv.code !== 0) throw new Error(`uv venv failed: ${venv.stderr}`);
	}
	log("installing swebench into the virtualenv");
	const install = await run("uv", ["pip", "install", "--python", VENV_PYTHON, "-r", join(HERE, "requirements.txt")]);
	if (install.code !== 0) throw new Error(`pip install failed: ${install.stderr}`);
	log(`exporting ${DATASET}`);
	const exported = await run(VENV_PYTHON, [join(HERE, "export_dataset.py"), DATASET_FILE, DATASET, SPLIT]);
	if (exported.code !== 0) throw new Error(`dataset export failed: ${exported.stderr.slice(-2000)}`);
	log(exported.stdout.trim().split("\n").pop());
}

function loadDataset() {
	if (!existsSync(DATASET_FILE)) throw new Error(`no dataset export; run: node evals/swebench/run.mjs setup`);
	return readFileSync(DATASET_FILE, "utf8")
		.split("\n")
		.filter(Boolean)
		.map((line) => JSON.parse(line));
}

function packageRoot(file, name) {
	let dir = dirname(file);
	while (dir !== dirname(dir)) {
		const manifest = join(dir, "package.json");
		if (existsSync(manifest) && JSON.parse(readFileSync(manifest, "utf8")).name === name) return dir;
		dir = dirname(dir);
	}
	throw new Error(`${file} is not inside the ${name} package`);
}

function isElf(file) {
	const handle = openSync(file, "r");
	const magic = Buffer.alloc(4);
	readSync(handle, magic, 0, 4, 0);
	closeSync(handle);
	return magic.toString("latin1") === "\x7fELF";
}

/**
 * The host installs mounted read-only into task containers: the Node binary and the installed ultron-agent package,
 * the Codex CLI's install directory, Pi's binary directory, the Claude Code binary, the standalone CPython the
 * virtualenv is built on, and a static ripgrep (the one Codex ships) for every arm. Only what the run's arms need
 * is looked up (ripgrep, and so the Codex install, always). `--ultron-package <dir>` mounts another installed
 * ultron-agent package instead of the one on PATH (a build under test, installed from its release tarball).
 */
async function resolveRuntimes({ arms = ARMS, ultronPackage: customUltron } = {}) {
	const needed = new Set(arms.flatMap((arm) => ARM_RUNTIMES[arm]));
	const which = async (name) => {
		const found = await run("which", [name]);
		if (found.code !== 0) throw new Error(`${name} is not on PATH`);
		return realpathSync(found.stdout.trim());
	};
	const version = async (command, args) => (await run(command, args)).stdout.trim().split("\n")[0];
	const mounts = {};
	const versions = {};
	if (needed.has("ultron")) {
		const node = await which("node");
		const ultronPackage = customUltron
			? packageRoot(join(realpathSync(resolve(customUltron)), "package.json"), "ultron-agent")
			: packageRoot(await which("ultron"), "ultron-agent");
		const ultronCli = join(ultronPackage, "dist", "bundle", "cli.js");
		if (!existsSync(ultronCli)) throw new Error(`no dist/bundle/cli.js in ${ultronPackage}`);
		mounts.node = [[node, `${MOUNT}/node/bin/node`]];
		mounts.ultron = [[ultronPackage, `${MOUNT}/node/lib/node_modules/ultron-agent`]];
		versions.ultron = `${await version(node, [ultronCli, "--version"])}${customUltron ? " (--ultron-package: a local build, not the installed release)" : ""}`;
	}
	const codexBinary = await which("codex");
	const codexRoot = dirname(dirname(codexBinary));
	if (!existsSync(join(codexRoot, "codex-package.json")) || !isElf(codexBinary))
		throw new Error(`unexpected Codex install layout at ${codexRoot}: need the standalone package (bin/codex, codex-path/rg)`);
	mounts.codex = [[codexRoot, `${MOUNT}/codex`]];
	if (needed.has("codex")) versions.codex = await version("codex", ["--version"]);
	if (needed.has("pi")) {
		const piBinary = await which("pi");
		if (!isElf(piBinary)) throw new Error(`pi at ${piBinary} is not a standalone binary; the harness mounts the binary release`);
		mounts.pi = [[dirname(piBinary), `${MOUNT}/pi`]];
		versions.pi = await version("pi", ["--version"]);
	}
	if (needed.has("claude")) {
		const claudeBinary = await which("claude");
		if (!isElf(claudeBinary)) throw new Error(`claude at ${claudeBinary} is not the native binary; the harness mounts the native build`);
		mounts.claude = [[claudeBinary, `${MOUNT}/claude/claude`]];
		versions.claude = await version(claudeBinary, ["--version"]);
	}
	if (needed.has("ultron")) versions.node = await version(mounts.node[0][0], ["--version"]);
	if (!existsSync(VENV_PYTHON)) throw new Error("no virtualenv; run: node evals/swebench/run.mjs setup");
	const pythonPrefix = (await run(VENV_PYTHON, ["-c", "import sys; print(sys.base_prefix)"])).stdout.trim();
	mounts.python = [[pythonPrefix, `${MOUNT}/python`]];
	const ripgrep = join(codexRoot, "codex-path", "rg");
	if (!existsSync(ripgrep)) throw new Error(`no ripgrep at ${ripgrep}`);
	mounts.tools = [[ripgrep, `${MOUNT}/tools/rg`]];
	versions.kernelPython = await version(join(pythonPrefix, "bin", "python3"), ["--version"]);
	versions.swebench = await version(VENV_PYTHON, ["-c", "import swebench; print(swebench.__version__)"]);
	versions.docker = await version("docker", ["--version"]);
	return { mounts, versions };
}

function mountArgs(runtimes, arm) {
	const args = [];
	for (const name of [...ARM_RUNTIMES[arm], "tools"])
		for (const [from, to] of runtimes.mounts[name]) args.push("-v", `${from}:${to}:ro`);
	return args;
}

/**
 * The user's Claude Code login as the harness may know it: the long-lived token of its own environment, else the
 * parsed credentials file. Held in memory only, to decide how to pass the login and to scrub it from the evidence.
 */
function readClaudeLogin() {
	const envToken = process.env[CLAUDE_TOKEN_ENV] || null;
	let credentials = null;
	if (!envToken && existsSync(CLAUDE_CREDENTIALS)) {
		try {
			credentials = JSON.parse(readFileSync(CLAUDE_CREDENTIALS, "utf8"));
		} catch {
			// An unreadable or half-written file is "no login"; the plan says so.
		}
	}
	return { envToken, credentials };
}

function readUserModels() {
	const models = JSON.parse(readFileSync(USER_MODELS, "utf8"));
	const provider = models.providers?.[PROVIDER];
	if (!provider?.apiKey || !provider.baseUrl) throw new Error(`${PROVIDER} provider with apiKey and baseUrl not found in models.json`);
	const base = new URL(provider.baseUrl);
	return { models, key: provider.apiKey, upstream: base.origin, basePath: base.pathname.replace(/\/$/, "") };
}

// ---------------------------------------------------------------------------------------------------------------
// One agent run

function containerName(runId, arm, instanceId) {
	return `swe-${runId}-${arm}-${instanceId}`.toLowerCase().replace(/[^a-z0-9_.-]/g, "-");
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

/** Remove the secrets (the proxy key, the Claude login's tokens) from every file of a run's evidence; returns how many files held one. */
function scrubDir(dir, secrets) {
	let scrubbed = 0;
	for (const file of walkFiles(dir)) {
		const content = readFileSync(file);
		const held = secrets.filter((secret) => content.includes(secret));
		if (held.length === 0) continue;
		writeFileSync(file, held.reduce((text, secret) => scrubSecret(text, secret), content.toString("utf8")));
		scrubbed++;
	}
	return scrubbed;
}

function readIfExists(path) {
	return existsSync(path) ? readFileSync(path, "utf8") : "";
}

/** The session id `ultron --mode json -p` prints with its answer; null for a run cut off before it. */
function printedSessionId(stdout) {
	try {
		return JSON.parse(stdout.trim().split("\n").pop() ?? "").sessionId ?? null;
	} catch {
		return null;
	}
}

function readJsonIfExists(path) {
	try {
		return JSON.parse(readFileSync(path, "utf8"));
	} catch {
		return null;
	}
}

/** The arm's own account of a run (turns, tool calls, tokens), read from the evidence directory. */
function collectStats(arm, dir) {
	const stdout = readIfExists(join(dir, "stdout.jsonl"));
	if (arm === "pi") return piStats(stdout);
	// Claude Code's own session transcripts: the model and effort of every response, and a second token count.
	const transcript =
		armAuth(arm) === "claude"
			? claudeTranscriptStats(
					walkFiles(join(dir, "claude-projects"))
						.filter((file) => file.endsWith(".jsonl"))
						.map((file) => readFileSync(file, "utf8")),
				)
			: null;
	if (arm === "claude") {
		const stats = claudeStats(stdout);
		// A run cut off before its `result` event has no totals of its own.
		if (!stats.hasTotals) stats.tokens = transcript.tokens;
		return { ...stats, transcript };
	}
	const sessionFiles = walkFiles(join(dir, "sessions")).filter((file) => file.endsWith(".jsonl"));
	if (arm === "codex") return codexStats(stdout, sessionFiles.map((file) => readFileSync(file, "utf8")).join("\n"));
	// A run cut off at the wall-clock limit prints nothing: the root session is then the oldest file.
	let rootSessionId = printedSessionId(stdout);
	const files = sessionFiles.sort().map((file) => ({ name: file, text: readFileSync(file, "utf8") }));
	if (!rootSessionId && files.length > 0) rootSessionId = files[0].name.split("_").pop().replace(/\.jsonl$/, "");
	const stats = ultronStats(files, rootSessionId);
	if (transcript) {
		stats.transcript = transcript;
		stats.usageReport = ultronUsageSummary(readJsonIfExists(join(dir, "usage.json")));
	}
	const loki = readIfExists(join(dir, "loki.jsonl"))
		.split("\n")
		.filter(Boolean)
		.map((line) => {
			try {
				return JSON.parse(line);
			} catch {
				return {};
			}
		});
	stats.loki = {
		beforeWriteChecks: loki.filter((entry) => entry.phase === "before_write").length,
		blocked: loki.filter((entry) => entry.phase === "before_write" && entry.outcome === "blocked").length,
		afterCellChecks: loki.filter((entry) => entry.phase === "after_cell").length,
		afterCellFindings: loki.filter((entry) => entry.phase === "after_cell" && entry.outcome === "findings").length,
		afterCellAdvisories: loki.filter((entry) => entry.phase === "after_cell" && entry.outcome === "advisory").length,
		setupNotes: loki.filter((entry) => entry.phase === "setup").map((entry) => String(entry.detail ?? entry.outcome ?? "").slice(0, 200)),
	};
	return stats;
}

/**
 * Run one arm on one task in a fresh container of the task's image and leave the evidence in
 * `<run>/<arm>/<instance>/`: the prediction (`patch.diff`), the agent's transcript, the recorder ledger and
 * `record.json`. Returns the record.
 */
async function runAgentOnce(ctx, instance, arm, attempt, { prompt, limitSeconds, dir }) {
	rmSync(dir, { recursive: true, force: true });
	mkdirSync(dir, { recursive: true });
	const onClaude = armAuth(arm) === "claude";
	const ledgerKey = `${arm}.${instance.instance_id}.${attempt}`;
	if (!onClaude) ctx.ledgers.set(ledgerKey, join(dir, "ledger.jsonl"));
	const baseUrl = onClaude
		? null
		: ctx.recorderPort
			? `http://127.0.0.1:${ctx.recorderPort}/run/${ledgerKey}${ctx.proxy.basePath}`
			: `${ctx.proxy.upstream}${ctx.proxy.basePath}`;
	const container = containerName(ctx.runId, arm, instance.instance_id);
	const spec = armSpec(arm);
	const record = {
		instance_id: instance.instance_id,
		repo: instance.repo,
		arm,
		attempt,
		status: "harness_error",
		startedAt: new Date().toISOString(),
	};
	// How the login or key reaches the agent: named in the environment of `docker exec`, or (the Claude login
	// without a long-lived token) the credentials file alone, mounted read-only into the fresh config dir.
	let authMounts = [];
	let execEnv = { names: [KEY_ENV], values: { [KEY_ENV]: ctx.proxy?.key } };
	if (onClaude) {
		const login = readClaudeLogin();
		for (const secret of claudeSecrets(login)) ctx.secrets.add(secret);
		const plan = claudeAuthPlan({ ...login, nowMs: Date.now(), limitSeconds });
		if (!plan.ok) {
			record.status = "auth_unavailable";
			record.error = plan.reason;
			writeFileSync(join(dir, "record.json"), `${JSON.stringify(record, null, "\t")}\n`);
			return record;
		}
		record.auth = { mode: plan.mode, ...(plan.secondsLeft === undefined ? {} : { accessTokenMinutesLeft: Math.floor(plan.secondsLeft / 60) }) };
		if (plan.mode === "mount") authMounts = ["-v", `${realpathSync(CLAUDE_CREDENTIALS)}:${CLAUDE_CONFIG_DIR}/.credentials.json:ro`];
		execEnv = plan.mode === "env" ? { names: [CLAUDE_TOKEN_ENV], values: {} } : { names: [], values: {} };
	}
	try {
		await docker(["rm", "-f", container]);
		const created = await docker([
			"run",
			"-d",
			"--name",
			container,
			"--label",
			`ultron-swebench=${ctx.runId}`,
			"--network",
			"host",
			"--memory",
			ctx.memory,
			...mountArgs(ctx.runtimes, arm),
			...authMounts,
			instance.image,
			"sleep",
			"infinity",
		]);
		if (created.code !== 0) throw new Error(`container start failed: ${created.stderr.trim().slice(-500)}`);
		const exec = (args, options) => docker(["exec", ...args], options);
		const made = await exec([container, "mkdir", "-p", AGENT_DIR, ...spec.dirs]);
		if (made.code !== 0) throw new Error(`container setup failed: ${made.stderr.trim().slice(-500)}`);
		const files = {
			...(onClaude
				? {}
				: spec.configFiles({
						modelsJson: `${JSON.stringify(modelsJson(ctx.proxy.models, baseUrl), null, "\t")}\n`,
						codexToml: codexConfigToml(baseUrl),
					})),
			[`${AGENT_DIR}/prompt.txt`]: prompt,
			[`${AGENT_DIR}/run.sh`]: agentScript(arm, { limitSeconds }),
		};
		for (const [path, content] of Object.entries(files)) {
			const written = await exec(["-i", container, "bash", "-c", `cat > '${path}'`], { input: content });
			if (written.code !== 0) throw new Error(`could not write ${path}: ${written.stderr.trim().slice(-300)}`);
		}
		const head = await exec([container, "git", "-C", REPO_DIR, "rev-parse", "HEAD"]);
		if (head.code !== 0) throw new Error(`no git repository at ${REPO_DIR}: ${head.stderr.trim().slice(-300)}`);
		record.startCommit = head.stdout.trim();
		record.baseCommit = instance.base_commit ?? null;

		const killAll = () => exec([container, "bash", "-c", "kill -9 -1"], { timeoutMs: 30_000 });
		const started = Date.now();
		const agent = await runAgentProcess("docker", ["exec", ...execEnv.names.flatMap((name) => ["-e", name]), container, "bash", `${AGENT_DIR}/run.sh`], {
			env: { ...process.env, ...execEnv.values },
			stdoutPath: join(dir, "stdout.jsonl"),
			stderrPath: join(dir, "stderr.txt"),
			dropLine: arm === "pi" ? isStreamingNoise : null,
			// `timeout` inside the container enforces the limit; this is the backstop if it cannot.
			deadlineMs: (limitSeconds + 120) * 1000,
			onDeadline: killAll,
		});
		record.wallMs = Date.now() - started;
		record.exitCode = agent.code;
		record.timedOut = agent.hitDeadline || agent.code === 124 || agent.code === 137;
		// Whatever the agent left running (test runners, a kernel) must not write while the diff is taken.
		await killAll();

		const diff = await exec(
			[
				container,
				"bash",
				"-c",
				`cd ${REPO_DIR} && git add -A . >/dev/null 2>&1; git -c core.fileMode=false diff --cached --no-color --no-ext-diff ${record.startCommit}`,
			],
			{ maxBytes: 256 * 1024 * 1024 },
		);
		if (diff.code !== 0 || diff.truncated) throw new Error(`git diff failed: ${diff.stderr.trim().slice(-300) || "output too large"}`);
		const cleaned = cleanPatch(diff.stdout);
		writeFileSync(join(dir, "patch.diff"), cleaned.patch);
		record.patch = { bytes: Buffer.byteLength(cleaned.patch), files: cleaned.files, dropped: cleaned.dropped };
		if (spec.usageReport) {
			// Ultron's own report of the session (`ultron usage --json`): it only reads the session files.
			const usage = await exec(
				[
					"-w",
					REPO_DIR,
					...Object.entries(spec.env).flatMap(([name, value]) => ["-e", `${name}=${value}`]),
					container,
					...spec.usageReport(printedSessionId(readIfExists(join(dir, "stdout.jsonl")))),
				],
				{ timeoutMs: 60_000 },
			);
			if (usage.code === 0) writeFileSync(join(dir, "usage.json"), usage.stdout);
			else record.usageReportError = (usage.stderr.trim() || `exit ${usage.code}`).slice(-300);
		}
		for (const [from, to] of spec.keep) await docker(["cp", `${container}:${from}`, join(dir, to)]);
		record.status = "collected";
	} catch (error) {
		record.error = String(error instanceof Error ? error.message : error).slice(0, 1000);
	} finally {
		await docker(["rm", "-f", container]);
		// A request still in flight when the container died is recorded as it closes.
		await sleep(500);
		ctx.ledgers.delete(ledgerKey);
	}
	record.keyScrubbedFiles = scrubDir(dir, [...ctx.secrets]);
	if (record.status === "collected") {
		const { commands, finalText, reportedCostUsd, lastStopReason, errorMessage, warnings, completed, ...stats } = collectStats(arm, dir);
		const ledger = onClaude ? null : ledgerStats(readIfExists(join(dir, "ledger.jsonl")));
		const reported = typeof reportedCostUsd === "number" && Number.isFinite(reportedCostUsd) ? Number(reportedCostUsd.toFixed(4)) : null;
		Object.assign(record, {
			turns: stats.turns,
			toolCalls: stats.toolCalls,
			toolsByName: stats.toolsByName,
			tokens: stats.tokens,
			// The proxy model: tokens at list prices. Claude: what Claude Code itself reports for the run.
			notionalCostUsd: onClaude ? reported : Number(notionalCost(stats.tokens, ctx.price).toFixed(4)),
			reportedCostUsd: reportedCostUsd === undefined ? null : reported,
			ledger,
			networkCommands: networkLookups(commands ?? []),
			finalText: finalText ? finalText.slice(0, 1500) : null,
		});
		if (warnings?.length) record.warnings = [...new Set(warnings)];
		if (arm === "ultron" || arm === "ultron-claude") {
			const { cells, frameCalls, spawnCalls, childSessions, childTurns, errorCells, helpers, loki } = stats;
			record.ultron = { cells, frameCalls, spawnCalls, childSessions, childTurns, errorCells, helpers, loki };
			if (stats.usageReport) record.ultron.usageReport = stats.usageReport;
		}
		let failure = errorMessage;
		let usageLimit = false;
		if (onClaude) {
			record.claude = {
				version: stats.version ?? null,
				models: stats.models,
				apiKeySource: stats.apiKeySource ?? null,
				permissionMode: stats.permissionMode ?? null,
				mcpServers: stats.mcpServers ?? [],
				permissionDenials: stats.permissionDenials ?? [],
				reportedTurns: stats.reportedTurns ?? null,
				subagentTurns: stats.subagentTurns ?? 0,
				subagentsSpawned: stats.subagentsSpawned ?? 0,
				tokensFrom: arm === "claude" ? (stats.hasTotals ? "result event (modelUsage)" : "session transcript (no result event)") : "Ultron's session journal",
				rateLimits: stats.rateLimits,
				transcript: stats.transcript,
			};
			usageLimit = stats.usageLimit || overLimit(stats.rateLimits?.last);
			// A run that died before any response says why only on stderr.
			if (!failure && record.exitCode !== 0 && !record.timedOut && !stats.turns) failure = readIfExists(join(dir, "stderr.txt")).trim().slice(-500) || null;
		}
		if (failure) record.error = String(failure).slice(0, 1000);
		record.status = runStatus({
			exitCode: record.exitCode,
			timedOut: record.timedOut,
			turns: stats.turns,
			errorMessage: failure,
			ledger,
			usageLimit,
			authError: onClaude && isClaudeAuthError(failure),
		});
	}
	writeFileSync(join(dir, "record.json"), `${JSON.stringify(record, null, "\t")}\n`);
	return record;
}

/** Run with retries: provider errors (rate limits, proxy trouble) back off and rerun; a crash before any turn reruns once. */
async function runAgent(ctx, instance, arm, options) {
	const dir = join(ctx.runDir, arm, instance.instance_id);
	const earlier = [];
	for (let attempt = 1; ; attempt++) {
		const record = await runAgentOnce(ctx, instance, arm, attempt, { ...options, dir });
		if (earlier.length > 0) {
			record.earlierAttempts = earlier;
			writeFileSync(join(dir, "record.json"), `${JSON.stringify(record, null, "\t")}\n`);
		}
		if (record.ledger?.rateLimited) ctx.cooldownUntil = Date.now() + 60_000;
		// A spent subscription or a missing login is not retried: it ends the arms on that login (ARM_STOPPERS).
		const retry =
			attempt < ctx.attempts &&
			(record.status === "provider_error" || record.status === "harness_error" || (record.status === "agent_crash" && !record.turns));
		if (!retry) return record;
		earlier.push({ attempt, status: record.status, error: record.error ?? null, wallMs: record.wallMs ?? null });
		const kept = `${dir}.attempt-${attempt}`;
		rmSync(kept, { recursive: true, force: true });
		renameSync(dir, kept);
		const wait = 60_000 * attempt;
		log(`${arm} ${instance.instance_id}: ${record.status} (${record.error ?? "no detail"}); retrying in ${wait / 1000}s`);
		ctx.cooldownUntil = Math.max(ctx.cooldownUntil, Date.now() + wait);
		await sleep(wait);
	}
}

async function pool(jobs, concurrency, worker) {
	const queue = [...jobs];
	await Promise.all(
		Array.from({ length: concurrency }, async () => {
			while (queue.length > 0) await worker(queue.shift());
		}),
	);
}

// ---------------------------------------------------------------------------------------------------------------
// Context, verification

/** The arms a command works on: `--arms`, else the arms of the run's manifest, else the default three. */
function plannedArms(flags) {
	let arms = ARMS;
	const manifest = flags["run-id"] ? join(HOME, "runs", flags["run-id"], "manifest.json") : null;
	if (flags.arms) arms = flags.arms.split(",");
	else if (manifest && existsSync(manifest)) arms = JSON.parse(readFileSync(manifest, "utf8")).arms;
	for (const arm of arms) armSpec(arm);
	return arms;
}

async function createContext(flags, runId, arms = plannedArms(flags)) {
	// The proxy (its key, the recorder) only for arms that use it; the Claude arms bring their own login.
	const onProxy = arms.some((arm) => armAuth(arm) === "proxy");
	const onClaude = arms.some((arm) => armAuth(arm) === "claude");
	const proxy = onProxy ? readUserModels() : null;
	const runtimes = await resolveRuntimes({ arms, ultronPackage: flags["ultron-package"] });
	const runDir = join(HOME, "runs", runId);
	mkdirSync(runDir, { recursive: true });
	const ctx = {
		runId,
		runDir,
		arms,
		proxy,
		runtimes,
		price: proxy ? priceTable(proxy.models) : { input: 0, output: 0, cacheRead: 0 },
		memory: flags.memory ?? "16g",
		attempts: Number(flags.attempts ?? 3),
		ledgers: new Map(),
		// Everything that must never reach the evidence or the results: the proxy key, the Claude login's tokens.
		secrets: new Set([...(proxy ? [proxy.key] : []), ...(onClaude ? claudeSecrets(readClaudeLogin()) : [])]),
		// Claude arms: why they were stopped (by login), the last subscription windows seen, and how full a window
		// may be for a run to start.
		stopped: new Map(),
		rateLimits: null,
		maxUtilization: Number(flags["max-utilization"] ?? CLAUDE_MAX_UTILIZATION),
		cooldownUntil: 0,
		recorderPort: null,
		close: async () => {},
	};
	if (onProxy && !flags["no-ledger"]) {
		const recorder = await startRecorder({ upstream: proxy.upstream, ledgerPath: (key) => ctx.ledgers.get(key) ?? null });
		ctx.recorderPort = recorder.port;
		ctx.close = recorder.close;
	}
	return ctx;
}

/**
 * Note what a finished Claude run says about the subscription: its windows, and whether it ends the arms on that
 * login (the usage limit, or a login that is gone).
 */
function noteClaudeRun(ctx, record) {
	if (armAuth(record.arm) !== "claude") return;
	if (record.claude?.rateLimits?.last) ctx.rateLimits = { at: Date.now(), ...record.claude.rateLimits.last };
	if (ARM_STOPPERS.has(record.status) && !ctx.stopped.has("claude"))
		ctx.stopped.set("claude", `${record.status} on ${record.instance_id} (${record.arm}): ${record.error ?? "no detail"}`);
}

/** Why no Claude run may start now, or null: the arms were stopped, or a window that has not reset is too full. */
function claudeHold(ctx) {
	if (ctx.stopped.has("claude")) return ctx.stopped.get("claude");
	for (const [name, window] of Object.entries(ctx.rateLimits?.windows ?? {})) {
		if (typeof window.utilization !== "number" || window.utilization < ctx.maxUtilization) continue;
		if (typeof window.resetsAt === "number" && window.resetsAt * 1000 <= Date.now()) continue;
		const reset = typeof window.resetsAt === "number" ? `, resets ${new Date(window.resetsAt * 1000).toISOString()}` : "";
		return `the subscription's ${name} window is ${Math.round(window.utilization * 100)}% used (limit for starting a run: ${Math.round(ctx.maxUtilization * 100)}%, --max-utilization)${reset}`;
	}
	return null;
}

/**
 * One tiny prompt per arm in a real task container. Proxy arms, through the recorder: every request must name the
 * model, be answered by it, and carry the expected reasoning effort. Claude arms, from the tool's own output and
 * Claude Code's session transcript: every response from the one model, on the subscription login, at one effort,
 * and the same effort in both arms.
 */
async function verifyArms(ctx, arms, instance) {
	const results = {};
	for (const arm of arms) {
		const dir = join(ctx.runDir, "verify", arm);
		const record = await runAgentOnce(ctx, { ...instance, instance_id: "verify" }, arm, 1, {
			prompt: VERIFY_PROMPT,
			limitSeconds: 300,
			dir,
		});
		if (armAuth(arm) === "claude") {
			noteClaudeRun(ctx, record);
			const transcript = record.claude?.transcript ?? { responses: 0, models: [], efforts: [] };
			const check = claudeCheck({
				arm,
				status: record.status,
				models: record.claude?.models ?? [],
				transcript,
				apiKeySource: record.claude?.apiKeySource,
				mcpServers: record.claude?.mcpServers,
				permissionDenials: record.claude?.permissionDenials,
			});
			results[arm] = {
				ok: check.ok,
				status: record.status,
				error: record.error ?? (check.problems.join("; ") || null),
				requests: transcript.responses,
				source: "responses in Claude Code's session transcript",
				paths: [arm === "claude" ? "claude -p" : "ultron --claude (claude -p)"],
				requestModels: record.claude?.models ?? [],
				responseModels: transcript.models,
				reasoningEfforts: transcript.efforts,
				login: record.auth ?? null,
				apiKeySource: record.claude?.apiKeySource ?? null,
				subscription: record.claude?.rateLimits?.last ?? null,
				answer: record.finalText,
				warnings: record.warnings ?? [],
			};
			log(`verify ${arm}: ${check.ok ? "ok" : "FAILED"} ${JSON.stringify(results[arm])}`);
			continue;
		}
		const ledger = record.ledger ?? {};
		const ok =
			record.status === "completed" &&
			ledger.ok > 0 &&
			ledger.ok === ledger.requests &&
			ledger.requestModels.length === 1 &&
			ledger.requestModels[0] === MODEL_ID &&
			ledger.responseModels.length === 1 &&
			ledger.responseModels[0] === MODEL_ID &&
			ledger.efforts.length === 1 &&
			ledger.efforts[0] === REASONING_EFFORT;
		results[arm] = {
			ok,
			status: record.status,
			error: record.error ?? null,
			requests: ledger.requests ?? 0,
			paths: ledger.paths ?? [],
			requestModels: ledger.requestModels ?? [],
			responseModels: ledger.responseModels ?? [],
			reasoningEfforts: ledger.efforts ?? [],
			answer: record.finalText,
			warnings: record.warnings ?? [],
		};
		log(`verify ${arm}: ${ok ? "ok" : "FAILED"} ${JSON.stringify(results[arm])}`);
	}
	// The Claude arms are compared at one effort: plain Claude Code's default and Ultron's must be the same.
	const onClaude = arms.filter((arm) => armAuth(arm) === "claude" && results[arm].ok);
	const efforts = new Set(onClaude.flatMap((arm) => results[arm].reasoningEfforts));
	if (efforts.size > 1)
		for (const arm of onClaude) {
			results[arm].ok = false;
			results[arm].error = `the Claude arms ran at different efforts (${[...efforts].join(", ")}); set one for both before comparing`;
			log(`verify ${arm}: FAILED ${results[arm].error}`);
		}
	writeFileSync(join(ctx.runDir, "verify.json"), `${JSON.stringify(sanitize(results, [...ctx.secrets]), null, "\t")}\n`);
	return results;
}

// ---------------------------------------------------------------------------------------------------------------
// Official evaluation

function loadRecords(runDir, arms, ids) {
	const records = [];
	for (const id of ids)
		for (const arm of arms) {
			const file = join(runDir, arm, id, "record.json");
			if (existsSync(file)) records.push(JSON.parse(readFileSync(file, "utf8")));
		}
	return records;
}

function evalRunId(runId, arm) {
	return `${runId}.${arm}`;
}

/** Score an arm's predictions (or the gold patches) with `swebench.harness.run_evaluation` in the prebuilt images. */
async function evaluate(ctx, manifest, arm, { gold = false } = {}) {
	const evalDir = join(ctx.runDir, "eval");
	mkdirSync(evalDir, { recursive: true });
	const name = gold ? "gold" : arm;
	let predictionsPath = "gold";
	if (!gold) {
		const records = loadRecords(ctx.runDir, [arm], manifest.instanceIds);
		predictionsPath = join(ctx.runDir, arm, "predictions.jsonl");
		writeFileSync(
			predictionsPath,
			records.map((record) => `${JSON.stringify(prediction(record, readIfExists(join(ctx.runDir, arm, record.instance_id, "patch.diff"))))}\n`).join(""),
		);
		if (records.length === 0) return null;
	}
	log(`evaluating ${name} (${manifest.instanceIds.length} instances)`);
	const result = await run(
		VENV_PYTHON,
		[
			"-m",
			"swebench.harness.run_evaluation",
			"--dataset_name",
			DATASET,
			"--split",
			SPLIT,
			"--predictions_path",
			predictionsPath,
			"--max_workers",
			String(manifest.limits.concurrency),
			"--run_id",
			evalRunId(ctx.runId, name),
			"--report_dir",
			evalDir,
			"--instance_ids",
			...manifest.instanceIds,
		],
		{ cwd: evalDir, env: { ...process.env, PYTHONUNBUFFERED: "1" } },
	);
	writeFileSync(join(evalDir, `${name}.log`), `${result.stdout}\n${result.stderr}`);
	if (result.code !== 0) log(`evaluation of ${name} exited ${result.code}; see eval/${name}.log`);
	return readEvalReport(ctx.runDir, ctx.runId, name);
}

function readEvalReport(runDir, runId, name) {
	const file = join(runDir, "eval", `${name}.${evalRunId(runId, name)}.json`);
	return existsSync(file) ? JSON.parse(readFileSync(file, "utf8")) : null;
}

/** How many of the hidden tests passed, from the per-instance report; counts only, never the test names. */
function testCounts(runDir, runId, name, instanceId) {
	const file = join(runDir, "eval", "logs", "run_evaluation", evalRunId(runId, name), name, instanceId, "report.json");
	if (!existsSync(file)) return null;
	const status = JSON.parse(readFileSync(file, "utf8"))[instanceId]?.tests_status;
	if (!status) return null;
	const count = (group) => ({
		passed: status[group]?.success?.length ?? 0,
		total: (status[group]?.success?.length ?? 0) + (status[group]?.failure?.length ?? 0),
	});
	return { failToPass: count("FAIL_TO_PASS"), passToPass: count("PASS_TO_PASS") };
}

// ---------------------------------------------------------------------------------------------------------------
// Report

/** Deep copy with the home directory and the secrets (the proxy key, the Claude login's tokens) removed from every string. */
function sanitize(value, secrets) {
	const home = homedir();
	const clean = (text) => secrets.reduce((cleaned, secret) => scrubSecret(cleaned, secret), text).split(home).join("~");
	if (typeof value === "string") return clean(value);
	if (Array.isArray(value)) return value.map((item) => sanitize(item, secrets));
	if (value && typeof value === "object") return Object.fromEntries(Object.entries(value).map(([name, item]) => [name, sanitize(item, secrets)]));
	return value;
}

async function imageDiskUsage(images) {
	const sizes = [];
	for (const image of images) {
		const inspected = await docker(["image", "inspect", "--format", "{{.Size}}", image]);
		if (inspected.code === 0) sizes.push(Number(inspected.stdout.trim()));
	}
	return {
		images: sizes.length,
		sumOfImageSizesBytes: sizes.reduce((total, size) => total + size, 0),
		note: "each image's size counts the layers it shares with the others, so the disk actually used is much lower; see `docker system df -v`",
	};
}

async function report(ctx, manifest, out) {
	const records = loadRecords(ctx.runDir, manifest.arms, manifest.instanceIds);
	const reports = Object.fromEntries(manifest.arms.map((arm) => [arm, readEvalReport(ctx.runDir, ctx.runId, arm)]));
	for (const record of records) {
		record.verdict = evalVerdict(reports[record.arm], record.instance_id);
		record.tests = testCounts(ctx.runDir, ctx.runId, record.arm, record.instance_id);
		const reason = reports[record.arm]?.failure_reasons?.[record.instance_id];
		if (reason) record.evalFailureReason = reason;
		record.evidence = `${ctx.runId}/${record.arm}/${record.instance_id}`;
	}
	const gold = readEvalReport(ctx.runDir, ctx.runId, "gold");
	const dataset = loadDataset();
	const byId = new Map(dataset.map((row) => [row.instance_id, row]));
	const instances = manifest.instanceIds.map((id) => byId.get(id));
	const pairs = {};
	for (let a = 0; a < manifest.arms.length; a++)
		for (let b = a + 1; b < manifest.arms.length; b++)
			pairs[`${manifest.arms[a]} vs ${manifest.arms[b]}`] = headToHead(records, manifest.arms[a], manifest.arms[b]);
	const onProxy = manifest.arms.some((arm) => armAuth(arm) === "proxy");
	const onClaude = manifest.arms.some((arm) => armAuth(arm) === "claude");
	const descriptions = armDescriptions(manifest.versions);
	const subscription = subscriptionUse(records);
	const result = sanitize(
		{
			taskSet: "swebench-verified",
			dataset: DATASET,
			split: SPLIT,
			swebenchVersion: manifest.versions.swebench,
			runId: ctx.runId,
			date: manifest.startedAt.slice(0, 10),
			model: runModel(manifest.arms),
			sample: { seed: manifest.seed, n: manifest.instanceIds.length, instanceIds: manifest.instanceIds, repos: repoCounts(instances) },
			limits: manifest.limits,
			versions: manifest.versions,
			arms: Object.fromEntries(manifest.arms.map((arm) => [arm, descriptions[arm]])),
			isolation: {
				where: "each run in a fresh container of the task's official prebuilt image (swebench/sweb.eval.x86_64.*), --network host, removed afterwards",
				runtimes: "the installed CLIs, Node and a standalone CPython mounted read-only under /opt/agent; ripgrep for every arm",
				...(onProxy
					? {
							agentState: "agent dirs, sessions and config live in /agent inside the container; nothing of the user's ~/.ultron, ~/.pi or ~/.codex is mounted or copied except the proxy provider entry of models.json, without its key",
							key: `passed as ${KEY_ENV} in the environment of docker exec; never written to a file; evidence is scrubbed of it`,
						}
					: {}),
				...(onClaude
					? {
							claudeState: "Claude Code's config dir is a fresh /agent/claude-config inside the container (CLAUDE_CONFIG_DIR), so no user CLAUDE.md, settings, hooks, skills, plugins, MCP servers or history are there, and its sessions are written there, not into the user's config dir; Ultron's agent and server dirs are fresh dirs in /agent too",
							claudeLogin: `the login reaches the container as ${CLAUDE_TOKEN_ENV} in the environment of docker exec when the harness's environment has it, else as the credentials file alone, bind-mounted read-only into that config dir (a rename over it or a write to it fails in the container, so the user's login cannot be changed from there); a run starts only while the access token outlives the run's limit plus 15 minutes, so no container ever refreshes the login; the evidence is scrubbed of the tokens`,
							claudeLoginUsed: [...new Set(records.map((record) => record.auth?.mode).filter(Boolean))].map((mode) => (mode === "env" ? `${CLAUDE_TOKEN_ENV} in the environment` : "the credentials file, mounted read-only")),
						}
					: {}),
				prompt: "the issue text only (problem_statement): no hints, no gold or test patch, no failing-test names",
			},
			costNote: `notionalCostUsd is ${[
				...(onProxy ? ["tokens at the model's list prices in models.json; the proxy bills a subscription, not tokens"] : []),
				...(onClaude ? [`${onProxy ? "for the Claude arms, " : ""}what Claude Code reports for each run (total_cost_usd, its tokens at list prices); the runs were made on a Claude subscription, which is not billed per token`] : []),
			].join("; ")}`,
			...(subscription ? { subscription } : {}),
			...(ctx.stopped.size > 0 ? { stopped: Object.fromEntries(ctx.stopped) } : {}),
			verification: existsSync(join(ctx.runDir, "verify.json")) ? JSON.parse(readFileSync(join(ctx.runDir, "verify.json"), "utf8")) : null,
			goldCheck: gold ? { resolved: gold.resolved_ids?.length ?? 0, of: gold.submitted_instances ?? 0, notResolved: [...(gold.unresolved_ids ?? []), ...(gold.error_ids ?? [])] } : null,
			summary: summarize(records, manifest.arms),
			headToHead: pairs,
			harnessFailures: records
				.filter((record) => HARNESS_FAILURES.has(record.status) || record.verdict === "error" || record.status === "timeout")
				.map((record) => ({ instance_id: record.instance_id, arm: record.arm, status: record.status, verdict: record.verdict, error: record.error ?? null })),
			docker: await imageDiskUsage(instances.map((instance) => instance.image)),
			records,
		},
		[...ctx.secrets],
	);
	const target = resolve(REPO_ROOT, out ?? `acceptance/quality/${result.date}-swebench-verified-${ctx.runId}-${result.model.replaceAll("/", "_").replaceAll(" + ", "+")}.json`);
	writeFileSync(target, `${JSON.stringify(result, null, "\t")}\n`);
	writeFileSync(target.replace(/\.json$/, ".md"), renderMarkdown(result));
	log(`report: ${target}`);
	for (const [arm, row] of Object.entries(result.summary))
		log(`${arm}: resolved ${row.resolved}/${row.tasks}, harness failures ${row.harnessFailures.length}, wall ${row.wallSeconds}s, tokens ${row.tokens.total}`);
	return result;
}

// ---------------------------------------------------------------------------------------------------------------
// Commands

function manifestFor(ctx, flags) {
	const file = join(ctx.runDir, "manifest.json");
	if (existsSync(file)) {
		const existing = JSON.parse(readFileSync(file, "utf8"));
		// A larger --n extends the run in place: the seeded order is prefix-stable, so the tasks already run stay
		// the first ones and only the new tasks are run.
		if (flags.n && Number(flags.n) > existing.instanceIds.length) {
			const extended = sample(loadDataset(), Number(flags.n), existing.seed).map((instance) => instance.instance_id);
			if (existing.instanceIds.some((id, index) => extended[index] !== id))
				throw new Error("the run's tasks are not a prefix of the larger sample (dataset or seed changed); use a new --run-id");
			existing.instanceIds = extended;
			writeFileSync(file, `${JSON.stringify(existing, null, "\t")}\n`);
		}
		return existing;
	}
	const seed = flags.seed ?? DEFAULT_SEED;
	const n = Number(flags.n ?? 10);
	const instances = sample(loadDataset(), n, seed);
	const manifest = {
		runId: ctx.runId,
		startedAt: new Date().toISOString(),
		seed,
		instanceIds: instances.map((instance) => instance.instance_id),
		arms: (flags.arms ?? ARMS.join(",")).split(","),
		limits: {
			wallClockMinutes: Number(flags["limit-minutes"] ?? 30),
			concurrency: Number(flags.concurrency ?? 2),
			containerMemory: ctx.memory,
			attempts: ctx.attempts,
			...(ctx.arms.some((arm) => armAuth(arm) === "claude") ? { maxSubscriptionUtilization: ctx.maxUtilization } : {}),
		},
		versions: ctx.runtimes.versions,
	};
	writeFileSync(file, `${JSON.stringify(manifest, null, "\t")}\n`);
	return manifest;
}

async function pullImages(instances) {
	const failed = [];
	for (const instance of instances) {
		if ((await docker(["image", "inspect", instance.image])).code === 0) continue;
		log(`pulling ${instance.image}`);
		const pulled = await docker(["pull", "-q", instance.image]);
		if (pulled.code !== 0) {
			failed.push({ instance_id: instance.instance_id, error: pulled.stderr.trim().slice(-300) });
			log(`pull FAILED for ${instance.instance_id}: ${pulled.stderr.trim().slice(-300)}`);
		}
	}
	return failed;
}

/** The record of a task an arm never started, because the arm had been stopped. A resumed run does it again. */
function recordNotRun(ctx, instance, arm, reason) {
	const dir = join(ctx.runDir, arm, instance.instance_id);
	rmSync(dir, { recursive: true, force: true });
	mkdirSync(dir, { recursive: true });
	const record = { instance_id: instance.instance_id, repo: instance.repo, arm, attempt: 0, status: "not_run", startedAt: new Date().toISOString(), error: `not run: ${reason}` };
	writeFileSync(join(dir, "record.json"), `${JSON.stringify(record, null, "\t")}\n`);
	return record;
}

/** Whether a (task, arm) pair has a record a resumed run keeps: one of a task that was tried. */
function hasFinalRecord(ctx, arm, instanceId) {
	const record = readJsonIfExists(join(ctx.runDir, arm, instanceId, "record.json"));
	return record !== null && !RERUN_STATUSES.has(record.status);
}

async function commandRun(flags) {
	const ctx = await createContext(flags, flags["run-id"]);
	try {
		const manifest = manifestFor(ctx, flags);
		const byId = new Map(loadDataset().map((row) => [row.instance_id, row]));
		const instances = manifest.instanceIds.map((id) => byId.get(id));
		const only = flags.only ? flags.only.split(",") : null;
		const arms = flags.arms ? flags.arms.split(",") : manifest.arms;
		const pullFailures = await pullImages(instances);
		writeFileSync(join(ctx.runDir, "pull-failures.json"), `${JSON.stringify(pullFailures, null, "\t")}\n`);
		if (!flags["skip-verify"]) {
			const verified = await verifyArms(ctx, arms, instances.find((instance) => !pullFailures.some((failure) => failure.instance_id === instance.instance_id)));
			const bad = Object.entries(verified).filter(([, result]) => !result.ok);
			if (bad.length > 0) throw new Error(`verification failed for ${bad.map(([arm]) => arm).join(", ")}; nothing was run`);
		}
		const jobs = [];
		for (const instance of instances) {
			if (only && !only.includes(instance.instance_id)) continue;
			for (const arm of arms) {
				if (hasFinalRecord(ctx, arm, instance.instance_id)) continue;
				jobs.push({ instance, arm });
			}
		}
		log(`${jobs.length} agent runs to do, ${manifest.limits.concurrency} at a time`);
		await pool(jobs, manifest.limits.concurrency, async ({ instance, arm }) => {
			while (Date.now() < ctx.cooldownUntil) await sleep(5_000);
			// A Claude arm whose subscription is spent (or nearly: the rest is the user's) or whose login is gone
			// does not start; the task is recorded as not run, a harness failure, and a later run resumes it.
			const hold = armAuth(arm) === "claude" ? claudeHold(ctx) : null;
			if (hold) {
				if (!ctx.stopped.has("claude")) ctx.stopped.set("claude", hold);
				recordNotRun(ctx, instance, arm, hold);
				log(`skip  ${arm} ${instance.instance_id}: ${hold}`);
				return;
			}
			log(`start ${arm} ${instance.instance_id}`);
			const record = await runAgent(ctx, instance, arm, {
				prompt: buildPrompt(instance),
				limitSeconds: manifest.limits.wallClockMinutes * 60,
			});
			noteClaudeRun(ctx, record);
			log(
				`done  ${arm} ${instance.instance_id}: ${record.status}, ${Math.round((record.wallMs ?? 0) / 1000)}s, ${record.turns ?? 0} turns, ${record.tokens?.total ?? 0} tokens, patch ${record.patch?.bytes ?? 0} bytes${record.error ? `, error: ${record.error.slice(0, 200)}` : ""}`,
			);
		});
		if (ctx.stopped.size > 0) log(`Claude arms stopped: ${ctx.stopped.get("claude")}`);
		if (!flags["skip-eval"]) for (const arm of arms) await evaluate(ctx, manifest, arm);
		await report(ctx, manifest, flags.out);
	} finally {
		await ctx.close();
		// Nothing of a run may outlive it: containers are labelled with the run id.
		const left = await docker(["ps", "-aq", "--filter", `label=ultron-swebench=${ctx.runId}`]);
		const ids = left.stdout.split("\n").filter(Boolean);
		if (ids.length > 0) await docker(["rm", "-f", ...ids]);
	}
}

async function main() {
	const { command, flags, unknown } = parseArgs(process.argv.slice(2));
	const commands = ["setup", "sample", "verify", "run", "eval", "report"];
	if (!commands.includes(command) || unknown.length > 0) {
		console.error(`usage: node evals/swebench/run.mjs <${commands.join("|")}> [flags]; unknown: ${[command, ...unknown].join(" ")}`);
		process.exit(2);
	}
	if (command === "setup") return setup();
	if (command === "sample") {
		const picked = sample(loadDataset(), Number(flags.n ?? 10), flags.seed ?? DEFAULT_SEED);
		console.log(JSON.stringify({ seed: flags.seed ?? DEFAULT_SEED, repos: repoCounts(picked), instanceIds: picked.map((row) => row.instance_id) }, null, 2));
		return;
	}
	if (!flags["run-id"] && command !== "verify") {
		console.error("--run-id is required");
		process.exit(2);
	}
	if (command === "run") return commandRun(flags);
	const ctx = await createContext(flags, flags["run-id"] ?? "verify");
	try {
		if (command === "verify") {
			const [first] = sample(loadDataset(), 1, flags.seed ?? DEFAULT_SEED);
			await pullImages([first]);
			const results = await verifyArms(ctx, (flags.arms ?? ARMS.join(",")).split(","), first);
			process.exitCode = Object.values(results).every((result) => result.ok) ? 0 : 1;
			return;
		}
		const manifest = manifestFor(ctx, flags);
		if (command === "eval") {
			if (flags.gold) await evaluate(ctx, manifest, "gold", { gold: true });
			else for (const arm of flags.arms ? flags.arms.split(",") : manifest.arms) await evaluate(ctx, manifest, arm);
		}
		await report(ctx, manifest, flags.out);
	} finally {
		await ctx.close();
	}
}

main().catch((error) => {
	console.error(error instanceof Error ? error.message : error);
	process.exit(1);
});
