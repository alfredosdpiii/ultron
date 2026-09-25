#!/usr/bin/env node
/**
 * Quality comparison: stock Pi (baseline) vs Ultron (candidate) on the frozen tasks in
 * evals/quality/tasks.mjs, with matched model and settings. Metered: every run calls the model.
 *
 *   node scripts/eval-quality.mjs [--model cliproxyapi/gpt-6-sol] [--trials 2] [--concurrency 3]
 *                                 [--only id,id] [--variants pi,ultron] [--out path] [--baseline recorded.json]
 *
 * Both agents run in RPC mode in a fresh copy of the task files with an isolated profile that holds
 * only models.json and auth.json (no extensions, skills, or memory), so the runtimes are compared,
 * not the user's setup. Hidden checks are copied in after the agent finishes and decide pass/fail.
 * Thresholds are frozen below, before any measurement.
 */
import { spawn, spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { FROZEN_AT, tasks } from "../evals/quality/tasks.mjs";

/** Frozen before measurement. */
export const THRESHOLDS = {
	/** Candidate pass rate may be at most this many points below the baseline. */
	maxPassRateDropPoints: 10,
	/** Candidate median wall time per task at most this multiple of the baseline's. */
	maxMedianLatencyRatio: 2,
	/** Candidate total cost at most this multiple of the baseline's (when both report cost). */
	maxCostRatio: 2,
};

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const RUN_TIMEOUT_MS = 20 * 60 * 1000;

const VARIANTS = {
	pi: { command: "pi", agentDirEnv: "PI_CODING_AGENT_DIR" },
	ultron: { command: "ultron", agentDirEnv: "ULTRON_CODING_AGENT_DIR" },
};

function arg(name, fallback) {
	const index = process.argv.indexOf(`--${name}`);
	return index === -1 ? fallback : process.argv[index + 1];
}

/** Minimal Pi RPC driver: JSONL commands on stdin, responses and events on stdout. */
function rpcSession({ command, args, cwd, env }) {
	const child = spawn(command, args, { cwd, env, stdio: ["pipe", "pipe", "pipe"] });
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
			child.stdin.write(`${JSON.stringify({ ...command, id })}\n`, (error) => error && reject(error));
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

async function runOne({ task, variant, trial, model }) {
	const work = mkdtempSync(join(tmpdir(), `ultron-quality-${task.id}-${variant}-`));
	const project = join(work, "project");
	const agentDir = join(work, "agent");
	mkdirSync(project, { recursive: true });
	mkdirSync(agentDir, { recursive: true });
	const profile = join(homedir(), ".ultron", "agent");
	for (const file of ["models.json", "auth.json"])
		if (existsSync(join(profile, file))) copyFileSync(join(profile, file), join(agentDir, file));
	for (const [path, content] of Object.entries(task.files)) {
		mkdirSync(dirname(join(project, path)), { recursive: true });
		writeFileSync(join(project, path), content);
	}
	const { command, agentDirEnv } = VARIANTS[variant];
	const split = model.indexOf("/");
	const env = { ...process.env, [agentDirEnv]: agentDir, ULTRON_SERVER_DIR: mkdtempSync(join("/tmp", "u-q-")) };
	const record = { task: task.id, category: task.category, variant, trial, model, passed: false };
	const started = Date.now();
	const session = rpcSession({
		command,
		args: ["--mode", "rpc", "--provider", model.slice(0, split), "--model", model.slice(split + 1), "--no-session"],
		cwd: project,
		env,
	});
	try {
		let toolCalls = 0;
		for (const prompt of task.prompts) {
			const events = await session.turn(prompt, RUN_TIMEOUT_MS);
			toolCalls += events.filter((event) => event.type === "tool_execution_start").length;
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
	for (const [path, content] of Object.entries(task.hidden)) writeFileSync(join(project, path), content);
	const check = spawnSync("sh", ["-c", task.verify], { cwd: project, timeout: 60_000, encoding: "utf8" });
	record.passed = !record.error && check.status === 0;
	record.verify = { status: check.status, output: `${check.stdout ?? ""}${check.stderr ?? ""}`.slice(-500) };
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
	return { byVariant, gate, passed: gate.length > 0 && gate.every((entry) => entry.ok !== false) };
}

async function main() {
	const model = arg("model", "cliproxyapi/gpt-6-sol");
	const trials = Number(arg("trials", "2"));
	const concurrency = Number(arg("concurrency", "3"));
	const variants = arg("variants", "pi,ultron").split(",");
	const only = arg("only", "");
	const selected = tasks().filter((task) => !only || only.split(",").includes(task.id));
	const out = resolve(root, arg("out", `acceptance/quality/${new Date().toISOString().slice(0, 10)}-${model.replace(/[^a-z0-9.-]+/gi, "_")}.json`));
	const jobs = selected.flatMap((task) =>
		variants.flatMap((variant) => Array.from({ length: trials }, (_, index) => ({ task, variant, trial: index + 1, model }))),
	);
	console.log(`Quality comparison: ${selected.length} tasks x ${variants.join("/")} x ${trials} trials = ${jobs.length} runs (${model})`);
	const records = [];
	let cursor = 0;
	await Promise.all(
		Array.from({ length: Math.max(1, concurrency) }, async () => {
			while (cursor < jobs.length) {
				const job = jobs[cursor++];
				const record = await runOne(job);
				records.push(record);
				console.log(
					`${record.passed ? "PASS" : record.infrastructure ? "INFRA" : "FAIL"} ${record.variant} ${record.task}#${record.trial} ${(record.durationMs / 1000).toFixed(0)}s${record.error ? ` (${record.error.slice(0, 120)})` : ""}`,
				);
			}
		}),
	);
	// A baseline recorded earlier (same frozen tasks, model and trials) can be combined with a fresh candidate run.
	const baselinePath = arg("baseline", "");
	if (baselinePath) {
		const baseline = JSON.parse(readFileSync(resolve(root, baselinePath), "utf8"));
		if (baseline.frozenAt !== FROZEN_AT || baseline.model !== model || baseline.trials !== trials)
			throw new Error("Baseline was recorded with a different task set, model, or trial count");
		records.push(...baseline.records.filter((record) => !variants.includes(record.variant)));
		for (const variant of new Set(baseline.records.map((record) => record.variant)))
			if (!variants.includes(variant)) variants.push(variant);
	}
	const summary = summarize(records, variants);
	mkdirSync(dirname(out), { recursive: true });
	writeFileSync(out, `${JSON.stringify({ frozenAt: FROZEN_AT, thresholds: THRESHOLDS, model, trials, summary, records }, null, 2)}\n`);
	console.log(JSON.stringify(summary, null, 2));
	console.log(`Wrote ${out}`);
	return summary.passed ? 0 : 1;
}

if (import.meta.url === `file://${process.argv[1]}`) process.exitCode = await main();
