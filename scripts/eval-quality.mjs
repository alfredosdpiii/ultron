#!/usr/bin/env node
/**
 * Quality comparison: stock Pi (baseline) vs Ultron (candidate) on the frozen tasks in
 * evals/quality/tasks.mjs, with matched model and settings. Metered: every run calls the model.
 *
 *   node scripts/eval-quality.mjs [--tasks default|hard|judged|parallel|research|delegation] [--model cliproxyapi/gpt-6-sol] [--trials 2]
 *                                 [--concurrency 3] [--only id,id] [--variants pi,ultron] [--out path]
 *                                 [--baseline recorded.json] [--thinking off|low|medium|high|xhigh|max]
 *                                 [--keep-failed dir] [--keep-all] [--ultron-command "node --import ... cli.ts"]
 *                                 [--judge-model cliproxyapi/glm-5.3-flash] [--judge-thinking low] [--no-judge]
 *   node scripts/eval-quality.mjs --tasks hard --self-check [--only id,id] [--concurrency 4]
 *   node scripts/eval-quality.mjs --tasks judged --self-check [--judge-live]
 *   node scripts/eval-quality.mjs --tasks parallel [--trials 2 --variants pi,ultron]   (prints wall time per run)
 *   node scripts/eval-quality.mjs --tasks parallel --self-check                        (about 4.5 minutes, real sleeps)
 *   node scripts/eval-quality.mjs --tasks delegation --self-check                      (about 8.5 minutes, real sleeps)
 *
 * `--tasks` picks the frozen set: default is evals/quality/tasks.mjs, `hard` is tasks-hard.mjs, `judged` is
 * tasks-judged.mjs (open-ended work scored by an LLM judge next to a light deterministic sanity check), `parallel`
 * is tasks-parallel.mjs (a slow suite to run and report while fixing two bugs: non-blocking work pays off), and
 * `research` is tasks-research.mjs (semantic judgement over a frozen ~2 MB corpus of model-written reports, scored by
 * precision and recall; the corpus is a committed fixture, so running or self-checking the set calls no model), and
 * `delegation` is tasks-delegation.mjs (six independent buggy services to fix under a wall-clock budget that one
 * sequential agent is unlikely to meet: working on them in parallel pays off; the hidden check reports how many
 * of the six passed as `metrics`).
 *
 * Wall time: a task with `timeBudgetMs` records `withinBudget` (durationMs <= budget) next to pass/fail, and the
 * summary reports per variant how many runs (and passing runs) finished within budget, with every run's time. The
 * budget is evidence only: `passed` stays correctness, and the gate below never reads it.
 * Asynchronous work: after each prompt the driver keeps following the agent while Ultron reports root-owned work
 * that can still re-invoke the model (`inspect async.pending`: a yield_after job, a spawned task, a queued
 * completion event), so an agent that ends its turn while a job runs is not cut off. Stock Pi has no such
 * inspection and is followed until idle, as before.
 * `--self-check` runs no model: for every task it checks that the hidden check fails on the
 * untouched task files, passes after applying the reference solution (tasks-<set>-solutions.mjs),
 * and, when the solution changes several files, fails if any one of those files is left unfixed. A solution's
 * `alternatives` ({name, files, run?, expect: "pass"|"fail"}, e.g. the research set's keyword baseline and empty
 * answer) must meet their expectation. Whenever the hidden check's last output line is a JSON object (precision, recall, ...) the self-check records it as `metrics`.
 * For judged tasks it also validates the rubric wiring against the solved tree with a fake judge (see
 * scripts/eval-judge.mjs). `--judge-live` additionally scores each reference solution with the real judge
 * (one or two model calls per task) to calibrate the rubric.
 *
 * LLM judge: a task with `judge: {rubric, inputs, passAt, model?}` is scored after its run by a fixed judge model
 * (`--judge-model`, default cliproxyapi/glm-5.3-flash, through the same proxy config as the agents, run by stock Pi
 * with no tools). The judge's prompt, raw replies, scores, reasons and normalized score go into `record.judge`, and
 * the summary reports them per variant next to the deterministic pass rate. `record.passed` stays the deterministic
 * check: the judge never decides a run alone and never feeds the release gate (scripts/gate.mjs).
 *
 * Uptake metrics in every record: `toolsByName` (tool calls by tool, from `tool_execution_start` events),
 * `framesSpawned` (rlm.spawn/infer/map frames: tasks whose definition starts with `rlm-`, read from Ultron's
 * `inspect agents.status`; else counted from the rlm tool's code in the events; null when neither is available,
 * as for stock Pi), `childrenSpawned` and `childDepth` (rlm.spawn subagents, definition `rlm-child`, and how deeply
 * they nest, from the same inspection; null for stock Pi), `tasksByDefinition`, and `rootUnseenBytes` (bytes loaded through handles minus bytes printed to the root, reported
 * only when Ultron exposes it in `agents.status` as `uptake.rootUnseenBytes` or `uptake.handleBytesLoaded` and
 * `uptake.handleBytesPrinted`; otherwise null). Summaries aggregate them per variant.
 *
 * Both agents run in RPC mode in a fresh copy of the task files with an isolated profile that holds
 * only models.json and auth.json (no extensions, skills, or memory) and an empty HOME of their own, so neither
 * loads the user's global skills from ~/.agents/skills; the runtimes are compared, not the user's setup. The
 * result JSON records this as `isolation` (and each record as `homeIsolated`). Hidden checks are copied in after the agent finishes and decide pass/fail.
 * Thresholds are frozen below, before any measurement.
 *
 * Evidence: every run's RPC event stream (commands sent and everything the agent printed) is written to
 * <keep>/<run>/events.jsonl, where <keep> is `--keep-failed` (default /tmp/ultron-quality-failed). A failed
 * run additionally keeps its project dir (with the hidden check files), its isolated agent dir (Ultron's
 * session files live under agent/experimental/sessions), the agent's stderr, and the hidden check output.
 * `--keep-all` keeps the same evidence for passing runs too, and runs the agent with a session instead of
 * `--no-session`, so the kept agent dir holds every lane's transcript and usage and the frame traces (cost diagnosis).
 * `--ultron-command` replaces the `ultron` binary (e.g. a worktree's source through the source resolver).
 */
import { spawn } from "node:child_process";
import {
	appendFileSync,
	copyFileSync,
	cpSync,
	existsSync,
	mkdirSync,
	mkdtempSync,
	readFileSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
	checkJudgeWiring,
	collectJudgeFiles,
	DEFAULT_JUDGE_MODEL,
	judgeRun,
	summarizeJudged,
} from "./eval-judge.mjs";

/** Frozen before measurement. */
export const THRESHOLDS = {
	/** Candidate pass rate may be at most this many points below the baseline. */
	maxPassRateDropPoints: 10,
	/** Candidate median wall time per task at most this multiple of the baseline's. */
	maxMedianLatencyRatio: 2,
	/** Candidate total cost at most this multiple of the baseline's (when both report cost). */
	maxCostRatio: 2,
};

/**
 * A comparison is only evidence when both variants actually reached the model: at most this share of either
 * variant's runs may be infrastructure outcomes (provider 429s, dead agent processes). Added after a run where
 * a proxy cooldown turned 49 of 60 runs into infrastructure and the remaining 11 still "passed" the gate.
 */
export const MAX_INFRASTRUCTURE_SHARE = 0.2;

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const RUN_TIMEOUT_MS = 20 * 60 * 1000;
const VERIFY_TIMEOUT_MS = 60_000;

const TASK_SETS = {
	default: { tasks: "../evals/quality/tasks.mjs", solutions: null },
	hard: { tasks: "../evals/quality/tasks-hard.mjs", solutions: "../evals/quality/tasks-hard-solutions.mjs" },
	judged: { tasks: "../evals/quality/tasks-judged.mjs", solutions: "../evals/quality/tasks-judged-solutions.mjs" },
	parallel: { tasks: "../evals/quality/tasks-parallel.mjs", solutions: "../evals/quality/tasks-parallel-solutions.mjs" },
	research: { tasks: "../evals/quality/tasks-research.mjs", solutions: "../evals/quality/tasks-research-solutions.mjs" },
	delegation: { tasks: "../evals/quality/tasks-delegation.mjs", solutions: "../evals/quality/tasks-delegation-solutions.mjs" },
};

/** The hidden check's last output line when it is a JSON object (research checks print their metrics there). */
export function verifyMetrics(output) {
	const last = String(output ?? "").trim().split("\n").at(-1) ?? "";
	try {
		const parsed = JSON.parse(last);
		return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : null;
	} catch {
		return null;
	}
}
const JUDGE_TIMEOUT_MS = 5 * 60 * 1000;

/**
 * The release gate (scripts/gate.mjs) reads only full comparisons of this set; hard, judged, parallel, research and
 * delegation are evidence and never gated.
 */
export const GATED_TASK_SET = "default";

/** Whether a recorded result file is one the release gate reads: a full comparison (it has gate entries) of the gated set. */
export function isGatedComparison(recorded) {
	return (recorded?.taskSet ?? "default") === GATED_TASK_SET && (recorded?.summary?.gate?.length ?? 0) > 0;
}

/** Task files and hidden files; hard tasks generate their (large) data on demand. */
function materialize(task) {
	return task.build ? task.build() : { files: task.files, hidden: task.hidden };
}

function writeTree(dir, files) {
	for (const [path, content] of Object.entries(files)) {
		mkdirSync(dirname(join(dir, path)), { recursive: true });
		// Scripts (a shebang line) are written executable, so `./run_integration.sh` works as in a real checkout.
		const executable = String(content.subarray ? content.subarray(0, 2) : content.slice(0, 2)) === "#!";
		writeFileSync(join(dir, path), content, executable ? { mode: 0o755 } : undefined);
	}
}

/** Run a shell command without blocking the event loop (other agents keep streaming meanwhile). */
function sh(command, cwd, timeoutMs) {
	return new Promise((resolveRun) => {
		const child = spawn("sh", ["-c", command], { cwd, stdio: ["ignore", "pipe", "pipe"], detached: true });
		let output = "";
		const collect = (chunk) => {
			output = (output + chunk).slice(-4000);
		};
		child.stdout.on("data", collect);
		child.stderr.on("data", collect);
		let timedOut = false;
		const timer = setTimeout(() => {
			timedOut = true;
			try {
				process.kill(-child.pid, "SIGKILL");
			} catch {}
		}, timeoutMs);
		child.on("close", (code) => {
			clearTimeout(timer);
			resolveRun({ status: timedOut ? null : code, output: `${output}${timedOut ? `\n(timed out after ${timeoutMs} ms)` : ""}`.slice(-500) });
		});
	});
}

const runVerify = (task, cwd) => sh(task.verify, cwd, task.verifyTimeoutMs ?? VERIFY_TIMEOUT_MS);

const VARIANTS = {
	pi: { command: ["pi"], agentDirEnv: "PI_CODING_AGENT_DIR" },
	ultron: { command: ["ultron"], agentDirEnv: "ULTRON_CODING_AGENT_DIR" },
};
const DEFAULT_KEEP_DIR = "/tmp/ultron-quality-failed";

function arg(name, fallback) {
	const index = process.argv.indexOf(`--${name}`);
	return index === -1 ? fallback : process.argv[index + 1];
}

/**
 * The environment an agent runs in: its isolated agent dir (models.json and auth.json only) and a fresh, empty
 * HOME inside the run's work dir. Both Pi and Ultron read user-global resources from the home directory (the
 * Agent Skills location `~/.agents/skills`, whose skills every system prompt would list), so an agent dir alone is
 * not enough: the user's global skills would leak into both variants. The XDG base directories follow the new
 * home. Auth comes from the copied files in the agent dir, and tools are found through PATH as before.
 */
export function isolatedAgentEnv({ work, agentDirEnv, agentDir, baseEnv = process.env }) {
	const home = join(work, "home");
	mkdirSync(home, { recursive: true });
	return {
		...baseEnv,
		HOME: home,
		USERPROFILE: home,
		XDG_CONFIG_HOME: join(home, ".config"),
		XDG_CACHE_HOME: join(home, ".cache"),
		XDG_DATA_HOME: join(home, ".local", "share"),
		XDG_STATE_HOME: join(home, ".local", "state"),
		[agentDirEnv]: agentDir,
	};
}

/** How every run is isolated from the user's setup; recorded in the result JSON. */
export const ISOLATION = {
	home: "per-run empty HOME inside the run's work dir (no ~/.agents, ~/.pi, ~/.ultron or other user-global resources)",
	agentDir: "per-run agent dir holding only models.json and auth.json copied from ~/.ultron/agent",
	memory: "off (ULTRON_HINDSIGHT_URL=off)",
};

/** Minimal Pi RPC driver: JSONL commands on stdin, responses and events on stdout. */
export function rpcSession({ command, args, cwd, env, log }) {
	const [binary, ...prefix] = command;
	const child = spawn(binary, [...prefix, ...args], { cwd, env, stdio: ["pipe", "pipe", "pipe"] });
	const record = (direction, line) => {
		if (log) appendFileSync(log, `${JSON.stringify({ at: Date.now(), direction, line })}\n`);
	};
	let buffer = "";
	let stderr = "";
	let nextId = 0;
	const pending = new Map();
	const listeners = new Set();
	const exited = new Promise((resolveExit) => child.on("exit", (code, signal) => resolveExit({ code, signal })));
	child.stderr.on("data", (chunk) => {
		stderr = (stderr + chunk).slice(-4000);
	});
	child.stdout.on("data", (chunk) => {
		buffer += chunk;
		let newline = buffer.indexOf("\n");
		while (newline !== -1) {
			const line = buffer.slice(0, newline);
			buffer = buffer.slice(newline + 1);
			newline = buffer.indexOf("\n");
			if (!line.trim()) continue;
			record("out", line);
			let message;
			try {
				message = JSON.parse(line);
			} catch {
				continue;
			}
			if (message.type === "response" && pending.has(message.id)) {
				pending.get(message.id)(message);
				pending.delete(message.id);
			} else {
				for (const listener of listeners) listener(message);
			}
		}
	});
	const send = (command) =>
		new Promise((resolveResponse, reject) => {
			const id = `q${++nextId}`;
			pending.set(id, resolveResponse);
			const line = JSON.stringify({ ...command, id });
			record("in", line);
			child.stdin.write(`${line}\n`, (error) => error && reject(error));
			void exited.then(({ code, signal }) =>
				reject(new Error(`agent exited (code ${code}, signal ${signal}) before responding: ${stderr}`)),
			);
		});
	return {
		send,
		stderr: () => stderr,
		/** Send a prompt and wait until the agent is idle again; returns the events of that turn. */
		async turn(message, timeoutMs) {
			const events = [];
			const done = new Promise((resolveTurn, reject) => {
				const timer = setTimeout(() => reject(new Error(`turn timed out after ${timeoutMs} ms`)), timeoutMs);
				const listener = (event) => {
					events.push(event);
					if (event.type === "agent_settled" || event.type === "agent_end") {
						clearTimeout(timer);
						listeners.delete(listener);
						resolveTurn(events);
					}
				};
				listeners.add(listener);
				void exited.then(({ code, signal }) => {
					clearTimeout(timer);
					reject(new Error(`agent exited (code ${code}, signal ${signal}) mid-turn: ${stderr}`));
				});
			});
			const response = await send({ type: "prompt", message });
			if (!response.success) throw new Error(`prompt rejected: ${response.error}`);
			const settled = await done;
			// agent_end can precede queued follow-ups; wait until the agent reports idle.
			for (let attempt = 0; attempt < 50; attempt += 1) {
				const state = await send({ type: "get_state" });
				if (!state.data?.isStreaming) break;
				await new Promise((resolveWait) => setTimeout(resolveWait, 200));
			}
			return settled;
		},
		/**
		 * After a turn: while the agent reports root-owned asynchronous work that can still re-invoke the model
		 * (Ultron's `inspect async.pending`), keep collecting the events of the runs it starts. Returns them. Agents
		 * without the inspection (stock Pi) return at once.
		 */
		async followAsync(timeoutMs) {
			const events = [];
			const listener = (event) => events.push(event);
			listeners.add(listener);
			const deadline = Date.now() + timeoutMs;
			try {
				while (Date.now() < deadline) {
					const status = await send({ type: "inspect", request: "async.pending", payload: {} }).catch(() => null);
					const state = await send({ type: "get_state" }).catch(() => null);
					const pending = status?.success === true && status.data?.pending === true;
					if (!pending && !state?.data?.isStreaming) break;
					await new Promise((resolveWait) => setTimeout(resolveWait, 500));
				}
				if (Date.now() >= deadline) throw new Error(`asynchronous work still pending after ${timeoutMs} ms`);
			} finally {
				listeners.delete(listener);
			}
			return events;
		},
		async close() {
			child.stdin.end();
			const timer = setTimeout(() => child.kill("SIGKILL"), 10_000);
			await exited;
			clearTimeout(timer);
		},
	};
}

/** Frames (rlm.spawn / rlm.infer / rlm.map) named in the rlm tool's code, for runs whose host cannot be inspected. */
const FRAME_CALL = /\brlm\.(?:spawn|infer|map)\s*\(/g;

/**
 * Uptake from Ultron's read-only inspector; null fields when the agent has no inspector (stock Pi) or the host does
 * not expose the metric yet.
 */
async function inspectUptake(session) {
	const response = await session.send({ type: "inspect", request: "agents.status", payload: {} }).catch(() => null);
	if (!response?.success || !response.data) return null;
	const tasks = Array.isArray(response.data.tasks) ? response.data.tasks : [];
	const tasksByDefinition = {};
	for (const task of tasks) {
		const name = String(task.definition ?? "unknown").replace(/@\d+$/, "");
		tasksByDefinition[name] = (tasksByDefinition[name] ?? 0) + 1;
	}
	const uptake = response.data.uptake ?? response.data.usage?.uptake;
	const rootUnseenBytes =
		typeof uptake?.rootUnseenBytes === "number"
			? uptake.rootUnseenBytes
			: typeof uptake?.handleBytesLoaded === "number" && typeof uptake?.handleBytesPrinted === "number"
				? uptake.handleBytesLoaded - uptake.handleBytesPrinted
				: null;
	return {
		framesSpawned: tasks.filter((task) => /^rlm-/.test(String(task.definition))).length,
		...childUptake(tasks),
		tasksByDefinition,
		rootUnseenBytes,
	};
}

/**
 * rlm.spawn subagents (definition `rlm-child`) among the inspected tasks, and how deeply they nest: a child of the
 * root is depth 1, a child spawned by that child depth 2. `childDepth` is 0 when nothing was spawned.
 */
export function childUptake(tasks) {
	const isChild = (task) => /^rlm-child(@\d+)?$/.test(String(task?.definition ?? ""));
	const byId = new Map(tasks.map((task) => [task.id, task]));
	let childDepth = 0;
	for (const task of tasks.filter(isChild)) {
		let depth = 0;
		const seen = new Set();
		for (let current = task; current && !seen.has(current.id); current = byId.get(current.parentId)) {
			seen.add(current.id);
			if (isChild(current)) depth += 1;
		}
		childDepth = Math.max(childDepth, depth);
	}
	return { childrenSpawned: tasks.filter(isChild).length, childDepth };
}

/** One judge call through stock Pi in RPC mode with no tools, extensions, skills or context files. */
function createModelJudge({ command, model, thinking, keepDir }) {
	const split = model.indexOf("/");
	let calls = 0;
	return async (system, prompt) => {
		const work = mkdtempSync(join(tmpdir(), "ultron-quality-judge-"));
		const agentDir = join(work, "agent");
		mkdirSync(agentDir, { recursive: true });
		const profile = join(homedir(), ".ultron", "agent");
		for (const file of ["models.json", "auth.json"])
			if (existsSync(join(profile, file))) copyFileSync(join(profile, file), join(agentDir, file));
		const session = rpcSession({
			command,
			args: [
				"--mode",
				"rpc",
				"--provider",
				model.slice(0, split),
				"--model",
				model.slice(split + 1),
				"--no-session",
				"--no-tools",
				"--no-extensions",
				"--no-skills",
				"--no-context-files",
				"--system-prompt",
				system,
				...(thinking ? ["--thinking", thinking] : []),
			],
			cwd: work,
			env: {
				...isolatedAgentEnv({ work, agentDirEnv: "PI_CODING_AGENT_DIR", agentDir }),
				ULTRON_CODING_AGENT_DIR: agentDir,
				ULTRON_HINDSIGHT_URL: "off",
			},
			log: keepDir ? join(keepDir, `judge-${Date.now()}-${++calls}.jsonl`) : undefined,
		});
		try {
			const events = await session.turn(prompt, JUDGE_TIMEOUT_MS);
			const message = events.filter((event) => event.type === "message_end" && event.message?.role === "assistant").at(-1)?.message;
			if (!message) throw new Error("judge produced no answer");
			if (message.stopReason === "error") throw new Error(`judge provider error: ${message.errorMessage ?? "unknown"}`);
			const text = (message.content ?? [])
				.filter((part) => part.type === "text")
				.map((part) => part.text)
				.join("");
			return { text, usage: message.usage ?? null };
		} finally {
			await session.close().catch(() => {});
			rmSync(work, { recursive: true, force: true });
		}
	};
}

async function runOne({ task, variant, trial, model, thinking, keepDir, keepAll, commands, judge }) {
	const work = mkdtempSync(join(tmpdir(), `ultron-quality-${task.id}-${variant}-`));
	const project = join(work, "project");
	const agentDir = join(work, "agent");
	mkdirSync(project, { recursive: true });
	mkdirSync(agentDir, { recursive: true });
	const profile = join(homedir(), ".ultron", "agent");
	for (const file of ["models.json", "auth.json"])
		if (existsSync(join(profile, file))) copyFileSync(join(profile, file), join(agentDir, file));
	const { files, hidden } = materialize(task);
	writeTree(project, files);
	const { agentDirEnv } = VARIANTS[variant];
	const command = commands[variant];
	const runName = `${task.id}-${variant}-${trial}-${new Date().toISOString().replace(/[:.]/g, "-")}`;
	const keep = join(keepDir, runName);
	mkdirSync(keep, { recursive: true });
	const split = model.indexOf("/");
	// Memory off: the baseline runs without extensions, so neither side gets cross-run memory. HOME is a fresh
	// empty dir, so neither side loads the user's global skills (see isolatedAgentEnv).
	const env = {
		...isolatedAgentEnv({ work, agentDirEnv, agentDir }),
		ULTRON_SERVER_DIR: mkdtempSync(join("/tmp", "u-q-")),
		ULTRON_HINDSIGHT_URL: "off",
	};
	const record = { task: task.id, category: task.category, variant, trial, model, passed: false, homeIsolated: true };
	const started = Date.now();
	const session = rpcSession({
		command,
		args: [
			"--mode",
			"rpc",
			"--provider",
			model.slice(0, split),
			"--model",
			model.slice(split + 1),
			// --keep-all keeps the session (every lane's transcript and usage, frame traces) in the kept agent dir.
			...(keepAll ? [] : ["--no-session"]),
			// Both variants get the same explicit level; without it each picks its own default.
			...(thinking ? ["--thinking", thinking] : []),
		],
		cwd: project,
		env,
		log: join(keep, "events.jsonl"),
	});
	record.toolsByName = {};
	record.framesSpawned = null;
	record.childrenSpawned = null;
	record.childDepth = null;
	record.rootUnseenBytes = null;
	let frameCallsInCode = 0;
	try {
		let toolCalls = 0;
		for (const prompt of task.prompts) {
			const events = await session.turn(prompt, RUN_TIMEOUT_MS);
			events.push(...(await session.followAsync(Math.max(0, RUN_TIMEOUT_MS - (Date.now() - started)))));
			toolCalls += events.filter((event) => event.type === "tool_execution_start").length;
			for (const event of events) {
				if (event.type !== "tool_execution_start") continue;
				record.toolsByName[event.toolName] = (record.toolsByName[event.toolName] ?? 0) + 1;
				if (event.toolName === "rlm") frameCallsInCode += JSON.stringify(event.args ?? "").match(FRAME_CALL)?.length ?? 0;
			}
			const failed = events.find(
				(event) => event.type === "message_end" && event.message?.role === "assistant" && event.message.stopReason === "error",
			);
			if (failed) {
				record.infrastructure = true;
				throw new Error(`provider error: ${failed.message.errorMessage ?? "unknown"}`);
			}
		}
		record.toolCalls = toolCalls;
		const uptake = await inspectUptake(session);
		if (uptake) Object.assign(record, uptake, { framesSource: "inspect" });
		else if (record.toolsByName.rlm) Object.assign(record, { framesSpawned: frameCallsInCode, framesSource: "events" });
		const stats = await session.send({ type: "get_session_stats" });
		const usage = stats.data?.usage ?? stats.data?.tokens ?? null;
		record.cost = typeof stats.data?.cost === "number" ? stats.data.cost : (usage?.cost?.total ?? null);
		record.tokens = usage?.totalTokens ?? usage?.total ?? null;
	} catch (error) {
		record.error = error instanceof Error ? error.message.slice(0, 2000) : String(error);
		// An agent process that dies is an infrastructure outcome; a turn that times out is the agent's failure.
		if (/agent exited/.test(record.error)) record.infrastructure = true;
	} finally {
		record.durationMs = Date.now() - started;
		if (task.timeBudgetMs) Object.assign(record, { timeBudgetMs: task.timeBudgetMs, withinBudget: record.durationMs <= task.timeBudgetMs });
		await session.close().catch(() => {});
	}
	// The judge reads the agent's files before the hidden check files are copied in; it never runs for
	// infrastructure outcomes (the agent never reached the model).
	if (task.judge && judge && !record.infrastructure) {
		const judgeModel = task.judge.model ?? judge.model;
		record.judge = await judgeRun({
			task,
			files: collectJudgeFiles(project, task.judge, files),
			call: judge.call(judgeModel),
			model: judgeModel,
			thinking: judge.thinking,
		});
	}
	writeTree(project, hidden);
	record.verify = await runVerify(task, project);
	// A hidden check whose last line is a JSON object reports metrics (the delegation set: services passed of six).
	const metrics = verifyMetrics(record.verify.output);
	if (metrics) record.metrics = metrics;
	record.passed = !record.error && record.verify.status === 0;
	if (!record.passed || keepAll) {
		// Evidence for failed runs (and every run with --keep-all): the files the agent left, its session files, stderr and the hidden check output.
		cpSync(project, join(keep, "project"), { recursive: true });
		cpSync(agentDir, join(keep, "agent"), { recursive: true });
		writeFileSync(join(keep, "stderr.txt"), session.stderr());
		writeFileSync(join(keep, "verify.txt"), `status: ${record.verify.status}\n${record.verify.output}\n`);
		writeFileSync(join(keep, "record.json"), `${JSON.stringify(record, null, 2)}\n`);
	}
	record.evidence = keep;
	rmSync(work, { recursive: true, force: true });
	return record;
}

function median(values) {
	if (values.length === 0) return null;
	const sorted = [...values].sort((a, b) => a - b);
	const middle = Math.floor(sorted.length / 2);
	return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
}

/** Per-variant uptake: tool calls by name, share of runs that used `rlm`, frames spawned and bytes the root never saw. */
export function summarizeUptake(records) {
	const toolsByName = {};
	for (const record of records)
		for (const [name, count] of Object.entries(record.toolsByName ?? {})) toolsByName[name] = (toolsByName[name] ?? 0) + count;
	const measuredOf = (field) => records.map((record) => record[field]).filter((value) => typeof value === "number");
	const aggregate = (field) => {
		const values = measuredOf(field);
		return values.length
			? { runs: values.length, total: values.reduce((a, b) => a + b, 0), median: median(values) }
			: { runs: 0, total: null, median: null };
	};
	return {
		toolsByName,
		rlmRunShare: records.length ? records.filter((record) => (record.toolsByName?.rlm ?? 0) > 0).length / records.length : null,
		framesSpawned: aggregate("framesSpawned"),
		childrenSpawned: aggregate("childrenSpawned"),
		rootUnseenBytes: aggregate("rootUnseenBytes"),
	};
}

/**
 * Wall time against per-task budgets (tasks with `timeBudgetMs`): runs and passing runs within budget, and every
 * run's time. Null when no record has a budget. Reported only; never part of the gate.
 */
export function summarizeTiming(records) {
	const timed = records.filter((record) => typeof record.timeBudgetMs === "number");
	if (timed.length === 0) return null;
	const passed = timed.filter((record) => record.passed);
	return {
		runs: timed.length,
		withinBudget: timed.filter((record) => record.withinBudget).length,
		passedWithinBudget: passed.filter((record) => record.withinBudget).length,
		passed: passed.length,
		medianDurationMs: median(timed.map((record) => record.durationMs)),
		perRun: timed.map((record) => ({
			task: record.task,
			trial: record.trial,
			durationMs: record.durationMs,
			timeBudgetMs: record.timeBudgetMs,
			withinBudget: record.withinBudget,
			passed: record.passed,
		})),
	};
}

/**
 * The console line for a finished run: time and outcome (formatRunTime), then subagents spawned (`children`, with
 * their nesting depth), the hidden check's per-part score when it reports one, tool calls by name and frames.
 */
export function formatRunLine(record) {
	const children =
		typeof record.childrenSpawned === "number"
			? `children ${record.childrenSpawned}${record.childrenSpawned > 0 ? ` (depth ${record.childDepth})` : ""}`
			: "children n/a";
	const metrics = record.metrics;
	const parts =
		metrics && typeof metrics.passed === "number" && typeof metrics.total === "number"
			? ` services ${metrics.passed}/${metrics.total}`
			: "";
	const judge = record.judge
		? ` judge ${record.judge.error ? `error (${record.judge.error.slice(0, 80)})` : `${record.judge.total}/${record.judge.max}`}`
		: "";
	return `${formatRunTime(record)}  ${children}${parts} tools ${JSON.stringify(record.toolsByName ?? {})} frames ${record.framesSpawned ?? "n/a"}${judge}${record.error ? ` (${record.error.slice(0, 120)})` : ""}`;
}

/** One line per run for the console: time first, then budget, outcome and tool use. */
export function formatRunTime(record) {
	const seconds = (ms) => `${(ms / 1000).toFixed(1)}s`;
	const budget =
		typeof record.timeBudgetMs === "number"
			? ` / budget ${seconds(record.timeBudgetMs)} ${record.withinBudget ? "WITHIN" : "OVER"}`
			: "";
	return `TIME ${seconds(record.durationMs).padStart(7)}${budget}  ${record.passed ? "PASS" : record.infrastructure ? "INFRA" : "FAIL"} ${record.variant} ${record.task}#${record.trial}`;
}

export function summarize(records, variants) {
	const byVariant = {};
	for (const variant of variants) {
		const own = records.filter((record) => record.variant === variant);
		const measured = own.filter((record) => !record.infrastructure);
		const categories = {};
		for (const record of measured) {
			categories[record.category] ??= { passed: 0, runs: 0 };
			categories[record.category].runs += 1;
			if (record.passed) categories[record.category].passed += 1;
		}
		const costs = measured.map((record) => record.cost).filter((cost) => typeof cost === "number");
		const judged = summarizeJudged(measured);
		const timing = summarizeTiming(measured);
		byVariant[variant] = {
			runs: own.length,
			infrastructure: own.length - measured.length,
			passRate: measured.length ? measured.filter((record) => record.passed).length / measured.length : null,
			categories,
			medianDurationMs: median(measured.map((record) => record.durationMs)),
			totalCost: costs.length === measured.length && costs.length > 0 ? costs.reduce((a, b) => a + b, 0) : null,
			uptake: summarizeUptake(measured),
			// Reported next to passRate; never part of the gate below.
			...(judged ? { judged } : {}),
			...(timing ? { timing } : {}),
		};
	}
	const baseline = byVariant.pi;
	const candidate = byVariant.ultron;
	const gate = [];
	if (baseline && candidate && baseline.passRate !== null && candidate.passRate !== null) {
		const drop = (baseline.passRate - candidate.passRate) * 100;
		gate.push({ check: "pass rate", ok: drop <= THRESHOLDS.maxPassRateDropPoints, detail: `${drop.toFixed(1)} points below baseline` });
		const latency = candidate.medianDurationMs / baseline.medianDurationMs;
		gate.push({ check: "median latency", ok: latency <= THRESHOLDS.maxMedianLatencyRatio, detail: `${latency.toFixed(2)}x baseline` });
		if (baseline.totalCost && candidate.totalCost !== null) {
			const cost = candidate.totalCost / baseline.totalCost;
			gate.push({ check: "cost", ok: cost <= THRESHOLDS.maxCostRatio, detail: `${cost.toFixed(2)}x baseline` });
		} else gate.push({ check: "cost", ok: null, detail: "cost not reported by both variants" });
	}
	for (const [variant, summary] of Object.entries(byVariant)) {
		const share = summary.runs ? summary.infrastructure / summary.runs : 1;
		if (share > MAX_INFRASTRUCTURE_SHARE)
			gate.push({
				check: "coverage",
				ok: false,
				detail: `${variant}: ${summary.infrastructure} of ${summary.runs} runs were infrastructure outcomes; rerun when the provider is healthy`,
			});
	}
	return { byVariant, gate, passed: gate.length > 0 && gate.every((entry) => entry.ok !== false) };
}

async function applySolution(dir, solution, files) {
	writeTree(dir, files);
	for (const path of solution.remove ?? []) rmSync(join(dir, path), { recursive: true, force: true });
	if (!solution.run) return null;
	const result = await sh(solution.run, dir, 10 * 60 * 1000);
	return result.status === 0 ? null : `reference solution failed: ${result.output}`;
}

/**
 * Model-free validation of a task set: the hidden check must reject the untouched task, accept the
 * reference solution, and reject the solution with any single one of its files left out.
 */
async function checkTask(task, solution, liveJudge) {
	const outcomes = [];
	if (!solution) return [{ trial: "solution", ok: false, detail: "no reference solution" }];
	const { files, hidden } = materialize(task);
	const work = mkdtempSync(join(tmpdir(), `ultron-selfcheck-${task.id}-`));
	/**
	 * One trial in its own copy of the task: apply `applied` ({files, run?, remove?}, or null for the untouched task),
	 * run the hidden check and compare with the expectation. With `expectWithinBudget` and a task `timeBudgetMs`,
	 * the time to apply the solution (its real run, e.g. a slow suite) must fall on the expected side of the budget.
	 */
	const trial = async (name, applied, expectPass, { inspect, expectWithinBudget } = {}) => {
		const own = [];
		const dir = join(work, name.replace(/[^a-z0-9.-]+/gi, "_"));
		mkdirSync(dir);
		writeTree(dir, files);
		const solveStarted = Date.now();
		const failure = applied ? await applySolution(dir, applied, applied.files ?? {}) : null;
		const solveMs = Date.now() - solveStarted;
		// Judged tasks: the judge reads the solved tree as an agent would leave it, before the hidden files.
		if (inspect) own.push(...(await inspect(dir)));
		writeTree(dir, hidden);
		const started = Date.now();
		const verify = await runVerify(task, dir);
		const passed = verify.status === 0;
		const timed = typeof expectWithinBudget === "boolean" && typeof task.timeBudgetMs === "number";
		const withinBudget = timed ? solveMs <= task.timeBudgetMs : undefined;
		const ok = !failure && passed === expectPass && (!timed || withinBudget === expectWithinBudget);
		const metrics = verifyMetrics(verify.output);
		own.push({
			trial: name,
			expect: `${expectPass ? "pass" : "fail"}${timed ? (expectWithinBudget ? ", within budget" : ", over budget") : ""}`,
			ok,
			verifyMs: Date.now() - started,
			...(metrics ? { metrics } : {}),
			...(timed ? { solveMs, timeBudgetMs: task.timeBudgetMs, withinBudget } : {}),
			...(ok
				? {}
				: {
						detail:
							failure ??
							(passed !== expectPass
								? verify.output
								: `took ${(solveMs / 1000).toFixed(1)}s against a ${(task.timeBudgetMs / 1000).toFixed(0)}s budget`),
					}),
		});
		rmSync(dir, { recursive: true, force: true });
		return own;
	};
	try {
		outcomes.push(...(await trial("unsolved", null, false)));
		const solutionFiles = solution.files ?? {};
		const judgeChecks = task.judge
			? async (dir) => {
					const wiring = await checkJudgeWiring(task, dir, files);
					if (!liveJudge) return wiring;
					// Calibration: the real judge scores the reference solution; it should reach the task's passAt.
					const judgeModel = task.judge.model ?? liveJudge.model;
					const judgement = await judgeRun({
						task,
						files: collectJudgeFiles(dir, task.judge, files),
						call: liveJudge.call(judgeModel),
						model: judgeModel,
						thinking: liveJudge.thinking,
					});
					liveJudge.judgements[task.id] = judgement;
					const ok = judgement.passed === true;
					return [
						...wiring,
						{
							trial: `live judge (${judgeModel}) scores the reference at or above ${task.judge.passAt}`,
							expect: "pass",
							ok,
							score: judgement.normalized ?? null,
							...(ok ? {} : { detail: judgement.error ?? `scored ${judgement.normalized}` }),
						},
					];
				}
			: undefined;
		// Alternatives (other correct or wrong solutions) run concurrently with the reference: timed tasks sleep for real.
		const concurrent = await Promise.all([
			trial("solved", solution, true, { inspect: judgeChecks, expectWithinBudget: solution.expectWithinBudget }),
			...(solution.alternatives ?? []).map((alternative) =>
				trial(alternative.name, alternative, alternative.expect === "pass", {
					expectWithinBudget: alternative.expectWithinBudget,
				}),
			),
		]);
		for (const own of concurrent) outcomes.push(...own);
		const paths = Object.keys(solutionFiles);
		if (paths.length > 1 && !solution.run)
			for (const path of paths)
				outcomes.push(
					...(await trial(
						`without ${path}`,
						{ ...solution, files: Object.fromEntries(Object.entries(solutionFiles).filter(([other]) => other !== path)) },
						false,
					)),
				);
	} finally {
		rmSync(work, { recursive: true, force: true });
	}
	return outcomes;
}

async function selfCheck(taskSet, selected, concurrency, liveJudge) {
	const solutionsModule = TASK_SETS[taskSet].solutions;
	if (!solutionsModule) throw new Error(`Task set "${taskSet}" has no reference solutions to self-check`);
	const { solutions } = await import(solutionsModule);
	console.log(
		`Self-check: ${selected.length} ${taskSet} tasks (${liveJudge ? `live judge ${liveJudge.model}` : "no model calls"})`,
	);
	const results = [];
	let cursor = 0;
	await Promise.all(
		Array.from({ length: Math.max(1, concurrency) }, async () => {
			while (cursor < selected.length) {
				const task = selected[cursor++];
				const started = Date.now();
				const outcomes = await checkTask(task, solutions[task.id], liveJudge);
				const ok = outcomes.every((outcome) => outcome.ok);
				results.push({ task: task.id, category: task.category, ok, durationMs: Date.now() - started, outcomes });
				const judged = liveJudge?.judgements[task.id];
				console.log(
					`${ok ? "OK  " : "BAD "} ${task.id} (${outcomes.length} trials, ${((Date.now() - started) / 1000).toFixed(0)}s)${judged ? ` judge ${judged.total ?? "?"}/${judged.max ?? "?"} ${JSON.stringify(judged.scores ?? judged.error)}` : ""}`,
				);
				for (const outcome of outcomes.filter((entry) => entry.metrics))
					console.log(`     ${outcome.trial} (expected ${outcome.expect}): ${JSON.stringify(outcome.metrics)}`);
				for (const outcome of outcomes.filter((entry) => typeof entry.solveMs === "number"))
					console.log(
						`     TIME ${(outcome.solveMs / 1000).toFixed(1)}s / budget ${(outcome.timeBudgetMs / 1000).toFixed(0)}s ${outcome.withinBudget ? "WITHIN" : "OVER"}: ${outcome.trial}`,
					);
				for (const outcome of outcomes.filter((entry) => !entry.ok))
					console.log(`     ${outcome.trial}: expected ${outcome.expect}; ${String(outcome.detail).slice(-300)}`);
			}
		}),
	);
	results.sort((a, b) => selected.findIndex((task) => task.id === a.task) - selected.findIndex((task) => task.id === b.task));
	const passed = results.every((result) => result.ok);
	const out = resolve(root, arg("out", `acceptance/quality/${new Date().toISOString().slice(0, 10)}-${taskSet}-self-check.json`));
	mkdirSync(dirname(out), { recursive: true });
	const judge = liveJudge ? { model: liveJudge.model, thinking: liveJudge.thinking ?? null, judgements: liveJudge.judgements } : undefined;
	writeFileSync(out, `${JSON.stringify({ taskSet, frozenAt: FROZEN_AT, passed, results, judge }, null, 2)}\n`);
	console.log(`${passed ? "Self-check passed" : "Self-check FAILED"}: ${results.filter((result) => result.ok).length}/${results.length} tasks. Wrote ${out}`);
	return passed ? 0 : 1;
}

/** A recorded command with the user's home directory written as `~`, so result files carry no local paths. */
export function withoutHome(text, home = homedir()) {
	return home ? text.split(home).join("~") : text;
}

/** `path`, or `path` with "-2", "-3", ... before the extension, whichever does not exist yet. */
export function freePath(path) {
	if (!existsSync(path)) return path;
	const base = path.replace(/\.json$/, "");
	for (let n = 2; ; n++) if (!existsSync(`${base}-${n}.json`)) return `${base}-${n}.json`;
}

let FROZEN_AT;

async function main() {
	const taskSet = arg("tasks", "default");
	if (!TASK_SETS[taskSet]) throw new Error(`Unknown task set "${taskSet}" (expected ${Object.keys(TASK_SETS).join(" or ")})`);
	const taskModule = await import(TASK_SETS[taskSet].tasks);
	FROZEN_AT = taskModule.FROZEN_AT;
	const only = arg("only", "");
	const selected = taskModule.tasks().filter((task) => !only || only.split(",").includes(task.id));
	if (only && selected.length !== only.split(",").length) throw new Error(`Unknown task id in --only ${only}`);
	const judgeModel = arg("judge-model", DEFAULT_JUDGE_MODEL);
	const judgeThinking = arg("judge-thinking", "low") || undefined;
	const judgeCommand = arg("judge-command", "pi").trim().split(/\s+/);
	const makeJudge = (keepDir) => ({
		model: judgeModel,
		thinking: judgeThinking,
		judgements: {},
		call: (model) => createModelJudge({ command: judgeCommand, model, thinking: judgeThinking, keepDir }),
	});
	if (process.argv.includes("--self-check"))
		return selfCheck(
			taskSet,
			selected,
			Number(arg("concurrency", "4")),
			process.argv.includes("--judge-live") ? makeJudge(undefined) : undefined,
		);
	const model = arg("model", "cliproxyapi/gpt-6-sol");
	const trials = Number(arg("trials", "2"));
	const concurrency = Number(arg("concurrency", "3"));
	const variants = arg("variants", "pi,ultron").split(",");
	const thinking = arg("thinking", "") || undefined;
	const keepDir = resolve(arg("keep-failed", DEFAULT_KEEP_DIR));
	const keepAll = process.argv.includes("--keep-all");
	const ultronCommand = arg("ultron-command", "");
	const commands = {
		pi: VARIANTS.pi.command,
		ultron: ultronCommand ? ultronCommand.trim().split(/\s+/) : VARIANTS.ultron.command,
	};
	// A default name never overwrites an earlier recorded comparison: the second run of a day gets "-2", and so on.
	const explicitOut = arg("out", "");
	const out = explicitOut
		? resolve(root, explicitOut)
		: freePath(
				resolve(
					root,
					`acceptance/quality/${new Date().toISOString().slice(0, 10)}-${taskSet}-${model.replace(/[^a-z0-9.-]+/gi, "_")}${thinking ? `-thinking-${thinking}` : ""}.json`,
				),
			);
	const judge = process.argv.includes("--no-judge") ? undefined : makeJudge(keepDir);
	const jobs = selected.flatMap((task) =>
		variants.flatMap((variant) =>
			Array.from({ length: trials }, (_, index) => ({ task, variant, trial: index + 1, model, thinking, keepDir, keepAll, commands, judge })),
		),
	);
	console.log(`Quality comparison (${taskSet} set): ${selected.length} tasks x ${variants.join("/")} x ${trials} trials = ${jobs.length} runs (${model}${thinking ? `, thinking ${thinking}` : ""})`);
	const records = [];
	let cursor = 0;
	await Promise.all(
		Array.from({ length: Math.max(1, concurrency) }, async () => {
			while (cursor < jobs.length) {
				const job = jobs[cursor++];
				const record = await runOne(job);
				records.push(record);
				console.log(formatRunLine(record));
			}
		}),
	);
	// A baseline recorded earlier (same frozen tasks, model and trials) can be combined with a fresh candidate run.
	const baselinePath = arg("baseline", "");
	if (baselinePath) {
		const baseline = JSON.parse(readFileSync(resolve(root, baselinePath), "utf8"));
		if (
			(baseline.taskSet ?? "default") !== taskSet ||
			baseline.frozenAt !== FROZEN_AT ||
			baseline.model !== model ||
			baseline.trials !== trials ||
			baseline.thinking !== thinking
		)
			throw new Error("Baseline was recorded with a different task set, model, or trial count");
		records.push(...baseline.records.filter((record) => !variants.includes(record.variant)));
		for (const variant of new Set(baseline.records.map((record) => record.variant)))
			if (!variants.includes(variant)) variants.push(variant);
	}
	const summary = summarize(records, variants);
	if (records.some((record) => typeof record.timeBudgetMs === "number")) {
		console.log("\nWall time per run:");
		for (const variant of variants)
			for (const record of records.filter((entry) => entry.variant === variant).sort((a, b) => a.trial - b.trial))
				console.log(`  ${formatRunTime(record)}`);
		for (const variant of variants) {
			const timing = summary.byVariant[variant]?.timing;
			if (timing)
				console.log(
					`  ${variant}: ${timing.withinBudget}/${timing.runs} runs within budget (${timing.passedWithinBudget} of them passed), median ${(timing.medianDurationMs / 1000).toFixed(1)}s`,
				);
		}
	}
	mkdirSync(dirname(out), { recursive: true });
	const judgeConfig = judge ? { model: judgeModel, thinking: judgeThinking ?? null, command: judgeCommand.join(" ") } : undefined;
	writeFileSync(out, `${JSON.stringify({ taskSet, frozenAt: FROZEN_AT, thresholds: THRESHOLDS, model, thinking, ultronCommand: ultronCommand ? withoutHome(ultronCommand) : undefined, trials, isolation: ISOLATION, judge: judgeConfig, summary, records }, null, 2)}\n`);
	console.log(JSON.stringify(summary, null, 2));
	console.log(`Wrote ${out}`);
	return summary.passed ? 0 : 1;
}

if (import.meta.url === `file://${process.argv[1]}`) process.exitCode = await main();
