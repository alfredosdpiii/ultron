#!/usr/bin/env node
/**
 * Quality comparison: stock Pi (baseline) vs Ultron (candidate) on the frozen tasks in
 * evals/quality/tasks.mjs, with matched model and settings. Metered: every run calls the model.
 *
 *   node scripts/eval-quality.mjs [--tasks default|hard] [--model cliproxyapi/gpt-6-sol] [--trials 2]
 *                                 [--concurrency 3] [--only id,id] [--variants pi,ultron] [--out path]
 *                                 [--baseline recorded.json] [--thinking off|low|medium|high|xhigh|max]
 *                                 [--keep-failed dir] [--ultron-command "node --import ... cli.ts"]
 *   node scripts/eval-quality.mjs --tasks hard --self-check [--only id,id] [--concurrency 4]
 *
 * `--tasks` picks the frozen set: default is evals/quality/tasks.mjs, `hard` is tasks-hard.mjs.
 * `--self-check` runs no model: for every task it checks that the hidden check fails on the
 * untouched task files, passes after applying the reference solution (tasks-<set>-solutions.mjs),
 * and, when the solution changes several files, fails if any one of those files is left unfixed.
 *
 * Both agents run in RPC mode in a fresh copy of the task files with an isolated profile that holds
 * only models.json and auth.json (no extensions, skills, or memory), so the runtimes are compared,
 * not the user's setup. Hidden checks are copied in after the agent finishes and decide pass/fail.
 * Thresholds are frozen below, before any measurement.
 *
 * Evidence: every run's RPC event stream (commands sent and everything the agent printed) is written to
 * <keep>/<run>/events.jsonl, where <keep> is `--keep-failed` (default /tmp/ultron-quality-failed). A failed
 * run additionally keeps its project dir (with the hidden check files), its isolated agent dir (Ultron's
 * session files live under agent/experimental/sessions), the agent's stderr, and the hidden check output.
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
};

/** Task files and hidden files; hard tasks generate their (large) data on demand. */
function materialize(task) {
	return task.build ? task.build() : { files: task.files, hidden: task.hidden };
}

function writeTree(dir, files) {
	for (const [path, content] of Object.entries(files)) {
		mkdirSync(dirname(join(dir, path)), { recursive: true });
		writeFileSync(join(dir, path), content);
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

/** Minimal Pi RPC driver: JSONL commands on stdin, responses and events on stdout. */
function rpcSession({ command, args, cwd, env, log }) {
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
		async close() {
			child.stdin.end();
			const timer = setTimeout(() => child.kill("SIGKILL"), 10_000);
			await exited;
			clearTimeout(timer);
		},
	};
}

/** " tools rlm=3 bash=1" for the per-run line, or "" before any tool ran. */
function formatTools(toolsByName) {
	const entries = Object.entries(toolsByName ?? {}).sort((a, b) => b[1] - a[1]);
	return entries.length === 0 ? " tools none" : ` tools ${entries.map(([name, count]) => `${name}=${count}`).join(" ")}`;
}

async function runOne({ task, variant, trial, model, thinking, keepDir, commands }) {
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
	// Memory off: the baseline runs without extensions, so neither side gets cross-run memory.
	const env = {
		...process.env,
		[agentDirEnv]: agentDir,
		ULTRON_SERVER_DIR: mkdtempSync(join("/tmp", "u-q-")),
		ULTRON_HINDSIGHT_URL: "off",
	};
	const record = { task: task.id, category: task.category, variant, trial, model, passed: false };
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
			"--no-session",
			// Both variants get the same explicit level; without it each picks its own default.
			...(thinking ? ["--thinking", thinking] : []),
		],
		cwd: project,
		env,
		log: join(keep, "events.jsonl"),
	});
	try {
		let toolCalls = 0;
		// Tool calls by name measure how the model works, e.g. whether it uses the RLM REPL at all.
		const toolsByName = {};
		record.toolsByName = toolsByName;
		for (const prompt of task.prompts) {
			const events = await session.turn(prompt, RUN_TIMEOUT_MS);
			for (const event of events) {
				if (event.type !== "tool_execution_start") continue;
				toolCalls += 1;
				const name = typeof event.toolName === "string" ? event.toolName : "unknown";
				toolsByName[name] = (toolsByName[name] ?? 0) + 1;
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
		await session.close().catch(() => {});
	}
	writeTree(project, hidden);
	record.verify = await runVerify(task, project);
	record.passed = !record.error && record.verify.status === 0;
	if (!record.passed) {
		// Evidence for failed runs: the files the agent left, its session files, stderr and the hidden check output.
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
		byVariant[variant] = {
			runs: own.length,
			infrastructure: own.length - measured.length,
			passRate: measured.length ? measured.filter((record) => record.passed).length / measured.length : null,
			categories,
			medianDurationMs: median(measured.map((record) => record.durationMs)),
			totalCost: costs.length === measured.length && costs.length > 0 ? costs.reduce((a, b) => a + b, 0) : null,
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
async function checkTask(task, solution) {
	const outcomes = [];
	if (!solution) return [{ trial: "solution", ok: false, detail: "no reference solution" }];
	const { files, hidden } = materialize(task);
	const work = mkdtempSync(join(tmpdir(), `ultron-selfcheck-${task.id}-`));
	const trial = async (name, solutionFiles, expectPass) => {
		const dir = join(work, name.replace(/[^a-z0-9.-]+/gi, "_"));
		mkdirSync(dir);
		writeTree(dir, files);
		const failure = solutionFiles ? await applySolution(dir, solution, solutionFiles) : null;
		writeTree(dir, hidden);
		const started = Date.now();
		const verify = await runVerify(task, dir);
		const passed = verify.status === 0;
		const ok = !failure && passed === expectPass;
		outcomes.push({
			trial: name,
			expect: expectPass ? "pass" : "fail",
			ok,
			verifyMs: Date.now() - started,
			...(ok ? {} : { detail: failure ?? verify.output }),
		});
		rmSync(dir, { recursive: true, force: true });
	};
	try {
		await trial("unsolved", null, false);
		const solutionFiles = solution.files ?? {};
		await trial("solved", solutionFiles, true);
		const paths = Object.keys(solutionFiles);
		if (paths.length > 1 && !solution.run)
			for (const path of paths)
				await trial(`without ${path}`, Object.fromEntries(Object.entries(solutionFiles).filter(([other]) => other !== path)), false);
	} finally {
		rmSync(work, { recursive: true, force: true });
	}
	return outcomes;
}

async function selfCheck(taskSet, selected, concurrency) {
	const solutionsModule = TASK_SETS[taskSet].solutions;
	if (!solutionsModule) throw new Error(`Task set "${taskSet}" has no reference solutions to self-check`);
	const { solutions } = await import(solutionsModule);
	console.log(`Self-check: ${selected.length} ${taskSet} tasks (no model calls)`);
	const results = [];
	let cursor = 0;
	await Promise.all(
		Array.from({ length: Math.max(1, concurrency) }, async () => {
			while (cursor < selected.length) {
				const task = selected[cursor++];
				const started = Date.now();
				const outcomes = await checkTask(task, solutions[task.id]);
				const ok = outcomes.every((outcome) => outcome.ok);
				results.push({ task: task.id, category: task.category, ok, durationMs: Date.now() - started, outcomes });
				console.log(`${ok ? "OK  " : "BAD "} ${task.id} (${outcomes.length} trials, ${((Date.now() - started) / 1000).toFixed(0)}s)`);
				for (const outcome of outcomes.filter((entry) => !entry.ok))
					console.log(`     ${outcome.trial}: expected ${outcome.expect}; ${String(outcome.detail).slice(-300)}`);
			}
		}),
	);
	results.sort((a, b) => selected.findIndex((task) => task.id === a.task) - selected.findIndex((task) => task.id === b.task));
	const passed = results.every((result) => result.ok);
	const out = resolve(root, arg("out", `acceptance/quality/${new Date().toISOString().slice(0, 10)}-${taskSet}-self-check.json`));
	mkdirSync(dirname(out), { recursive: true });
	writeFileSync(out, `${JSON.stringify({ taskSet, frozenAt: FROZEN_AT, passed, results }, null, 2)}\n`);
	console.log(`${passed ? "Self-check passed" : "Self-check FAILED"}: ${results.filter((result) => result.ok).length}/${results.length} tasks. Wrote ${out}`);
	return passed ? 0 : 1;
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
	if (process.argv.includes("--self-check")) return selfCheck(taskSet, selected, Number(arg("concurrency", "4")));
	const model = arg("model", "cliproxyapi/gpt-6-sol");
	const trials = Number(arg("trials", "2"));
	const concurrency = Number(arg("concurrency", "3"));
	const variants = arg("variants", "pi,ultron").split(",");
	const thinking = arg("thinking", "") || undefined;
	const keepDir = resolve(arg("keep-failed", DEFAULT_KEEP_DIR));
	const ultronCommand = arg("ultron-command", "");
	const commands = {
		pi: VARIANTS.pi.command,
		ultron: ultronCommand ? ultronCommand.trim().split(/\s+/) : VARIANTS.ultron.command,
	};
	const out = resolve(
		root,
		arg(
			"out",
			`acceptance/quality/${new Date().toISOString().slice(0, 10)}-${taskSet}-${model.replace(/[^a-z0-9.-]+/gi, "_")}${thinking ? `-thinking-${thinking}` : ""}.json`,
		),
	);
	const jobs = selected.flatMap((task) =>
		variants.flatMap((variant) => Array.from({ length: trials }, (_, index) => ({ task, variant, trial: index + 1, model, thinking, keepDir, commands }))),
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
				console.log(
					`${record.passed ? "PASS" : record.infrastructure ? "INFRA" : "FAIL"} ${record.variant} ${record.task}#${record.trial} ${(record.durationMs / 1000).toFixed(0)}s${formatTools(record.toolsByName)}${record.error ? ` (${record.error.slice(0, 120)})` : ""}`,
				);
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
	mkdirSync(dirname(out), { recursive: true });
	writeFileSync(out, `${JSON.stringify({ taskSet, frozenAt: FROZEN_AT, thresholds: THRESHOLDS, model, thinking, ultronCommand: ultronCommand || undefined, trials, summary, records }, null, 2)}\n`);
	console.log(JSON.stringify(summary, null, 2));
	console.log(`Wrote ${out}`);
	return summary.passed ? 0 : 1;
}

if (import.meta.url === `file://${process.argv[1]}`) process.exitCode = await main();
