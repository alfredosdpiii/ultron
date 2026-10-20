/**
 * The pure parts of the DeepSWE harness: task selection, arm configuration, the in-container scripts, patch
 * cleaning, reward parsing, usage and swarm-call parsing, aggregation and the report. Nothing here touches Docker,
 * the network or the filesystem, so all of it is unit tested (scripts/eval-deepswe.test.mjs). The side effects live
 * in run.mjs.
 */

import { cleanPatch as cleanSwebenchPatch, modelsJson, notionalCost, priceTable, ultronStats } from "../swebench/lib.mjs";
import { countSwarmCalls } from "../../scripts/eval-quality.mjs";

export { modelsJson, notionalCost, priceTable };

// ---------------------------------------------------------------------------------------------------------------
// Constants of the comparison

export const DATASET = "datacurve/deep-swe";
export const MODEL_ID = "gpt-6.1-sol";
export const PROVIDER = "cliproxyapi";
export const MODEL = `${PROVIDER}/${MODEL_ID}`;
/** The environment variable the agent reads the proxy key from (models.json says `$CLIPROXY_API_KEY`). */
export const KEY_ENV = "CLIPROXY_API_KEY";
/** The two arms: ultron-rs as installed, and the same with the swarm directive as APPEND_SYSTEM.md. */
export const ARMS = ["ultron", "ultron-swarm"];
/**
 * The four tasks of the A/B: four languages, multi-file feature requests. `sqlfmt-create-table-ddl-formatting`
 * replaced `narwhals-rolling-window-suite`, whose gold patch fails in its image: `test.sh base` has 6 failures
 * unrelated to the task (polars DeprecationWarnings in the interchange tests, a get_column assertion).
 */
export const DEFAULT_TASKS = [
	"sqlfmt-create-table-ddl-formatting",
	"arcane-drift-detection-baselines",
	"kysely-window-grouping-helpers",
	"wasmi-trap-coredumps",
];
/** Where things live inside a container. The repository is Harbor's /app. */
export const REPO_DIR = "/app";
export const MOUNT = "/opt/ultron";
export const BINARY = `${MOUNT}/ultron`;
export const PYTHON_MOUNT = `${MOUNT}/python`;
/** Per-run agent state; dies with the container. */
export const AGENT_ROOT = "/agent";
export const AGENT_DIR = `${AGENT_ROOT}/agent`;
export const SERVER_DIR = `${AGENT_ROOT}/server`;
export const HOME_DIR = `${AGENT_ROOT}/home`;
/** The agent's runtime state never belongs to the prediction. */
export const EXCLUDED_PATCH_PREFIXES = [".loki/", ".ultron/", ".pi/"];
/** Container names start with this, so the harness only ever touches its own containers. */
export const CONTAINER_PREFIX = "ultron-deepswe";
export const LABEL = "ultron-deepswe";

const VALUE_FLAGS = ["run-id", "arms", "only", "concurrency", "limit-minutes", "out", "binary", "attempts", "date"];
const SWITCH_FLAGS = ["skip-gold"];
export const COMMANDS = ["export", "gold", "smoke", "run", "grade", "report"];

/** `{ command, flags, unknown }`; anything not a known flag is `unknown` (the CLI exits 2 on it). */
export function parseArgs(argv) {
	const [command, ...rest] = argv;
	const flags = {};
	const unknown = [];
	for (let index = 0; index < rest.length; index++) {
		const name = rest[index].replace(/^--/, "");
		if (rest[index].startsWith("--") && VALUE_FLAGS.includes(name) && index + 1 < rest.length) flags[name] = rest[++index];
		else if (rest[index].startsWith("--") && SWITCH_FLAGS.includes(name)) flags[name] = true;
		else unknown.push(rest[index]);
	}
	if (command !== undefined && !COMMANDS.includes(command)) unknown.unshift(command);
	return { command, flags, unknown };
}

export function containerName(runId, kind, taskId, arm = "") {
	return [CONTAINER_PREFIX, runId, kind, arm, taskId]
		.filter(Boolean)
		.join("-")
		.toLowerCase()
		.replace(/[^a-z0-9_.-]/g, "-")
		.slice(0, 120);
}

// ---------------------------------------------------------------------------------------------------------------
// The agent run

/** Harbor gives the agent instruction.md, which is the row's problem_statement; so does this harness. */
export function buildPrompt(task) {
	return String(task.problem_statement ?? "").trim();
}

/** The environment of the agent inside the container: private state, a fresh HOME, the kernel's Python. */
export function agentEnv({ python }) {
	return {
		HOME: HOME_DIR,
		XDG_CONFIG_HOME: `${HOME_DIR}/.config`,
		XDG_CACHE_HOME: `${HOME_DIR}/.cache`,
		XDG_DATA_HOME: `${HOME_DIR}/.local/share`,
		XDG_STATE_HOME: `${HOME_DIR}/.local/state`,
		ULTRON_CODING_AGENT_DIR: AGENT_DIR,
		ULTRON_SERVER_DIR: SERVER_DIR,
		ULTRON_HINDSIGHT_URL: "off",
		ULTRON_LOKI_AUTOINIT: "off",
		ULTRON_PYTHON: python,
	};
}

function shellQuote(value) {
	return `'${String(value).replace(/'/g, "'\\''")}'`;
}

/**
 * The script that runs the agent in the container (`bash /agent/run.sh`): the environment, then ultron in json
 * print mode on the prompt, under `timeout` at the wall limit. The key arrives in the environment of `docker exec`.
 */
export function agentScript({ limitSeconds, python, prompt = `${AGENT_ROOT}/prompt.txt` }) {
	const env = agentEnv({ python });
	return [
		"#!/bin/bash",
		"set -u",
		...Object.entries(env).map(([name, value]) => `export ${name}=${shellQuote(value)}`),
		`mkdir -p "$HOME" "$ULTRON_CODING_AGENT_DIR" "$ULTRON_SERVER_DIR"`,
		`cd ${REPO_DIR}`,
		`exec timeout --kill-after=60 ${Math.round(limitSeconds)} ${BINARY} --mode json -p --model ${MODEL} "$(cat ${prompt})"`,
		"",
	].join("\n");
}

/** Files written into a run's agent dir: models.json (no key) and, for the swarm arm, APPEND_SYSTEM.md. */
export function agentFiles(arm, { userModels, baseUrl, directive }) {
	const files = { [`${AGENT_DIR}/models.json`]: `${JSON.stringify(modelsJson(userModels, baseUrl), null, "\t")}\n` };
	if (arm === "ultron-swarm") {
		if (!directive) throw new Error("the ultron-swarm arm needs the swarm directive");
		files[`${AGENT_DIR}/APPEND_SYSTEM.md`] = directive;
	}
	return files;
}

/**
 * Kill what the agent left running (kernels, test runners) so nothing writes while the diff is taken: every pid of
 * the container except its init and this shell, by number.
 */
export const KILL_LEFTOVERS = 'for p in /proc/[0-9]*; do pid=${p#/proc/}; [ "$pid" = 1 ] || [ "$pid" = "$$" ] || kill -9 "$pid" 2>/dev/null; done; true';

/** The agent's change against the task's base commit, everything new included. */
export function diffCommand(baseCommit) {
	return `cd ${REPO_DIR} && git config --global --add safe.directory ${REPO_DIR} >/dev/null 2>&1; git add -A . >/dev/null 2>&1; git -c core.fileMode=false diff --cached --no-color --no-ext-diff ${baseCommit}`;
}

/** The prediction: the diff without agent state and binary sections. */
export function cleanPatch(diff) {
	return cleanSwebenchPatch(diff, EXCLUDED_PATCH_PREFIXES);
}

// ---------------------------------------------------------------------------------------------------------------
// Grading, as Harbor does it: tests/ uploaded to /tests, `bash /tests/test.sh`, reward in /logs/verifier/reward.txt

/**
 * The grading script for a fresh container of the task image: apply the patch (absent for an empty prediction),
 * then run the verifier. Exit 90 means the patch did not apply; otherwise the verifier's own status.
 */
export function gradeScript({ verifierTimeoutSec, hasPatch }) {
	return [
		"#!/bin/bash",
		"set -u",
		"mkdir -p /logs/verifier /logs/artifacts",
		"rm -f /logs/verifier/reward.txt",
		`cd ${REPO_DIR}`,
		...(hasPatch
			? [
					`if ! git apply --whitespace=nowarn /grade/model.patch; then echo "[grade] model patch did not apply"; exit 90; fi`,
					'echo "[grade] model patch applied"',
				]
			: ['echo "[grade] empty prediction"']),
		`timeout --kill-after=60 ${Math.round(verifierTimeoutSec)} bash /tests/test.sh`,
		"status=$?",
		'echo "[grade] verifier exit $status"',
		"exit $status",
		"",
	].join("\n");
}

/** The reward Harbor reads: 1 or 0 from reward.txt, null when the verifier wrote none. */
export function parseReward(text) {
	if (text === null || text === undefined) return null;
	const value = String(text).trim();
	if (value === "") return null;
	const number = Number(value);
	return Number.isFinite(number) ? number : null;
}

/** A graded record's verdict: pass, fail, patch_failed, or verifier_error (no reward written). */
export function gradeVerdict({ exitCode, reward }) {
	if (exitCode === 90) return "patch_failed";
	if (reward === null) return "verifier_error";
	return reward >= 1 ? "pass" : "fail";
}

// ---------------------------------------------------------------------------------------------------------------
// Usage

/** The root session id from the one JSON line `ultron --mode json -p` prints; null for a run cut off before it. */
export function printedSessionId(stdout) {
	for (const line of String(stdout ?? "").trim().split("\n").reverse()) {
		try {
			const value = JSON.parse(line);
			if (value?.sessionId) return value.sessionId;
		} catch {
			// not the result line
		}
	}
	return null;
}

function jsonLines(text) {
	const values = [];
	for (const line of String(text).split("\n")) {
		if (!line.trim()) continue;
		try {
			values.push(JSON.parse(line));
		} catch {
			// a line cut off by a killed process
		}
	}
	return values;
}

/** The session id in a journal's header (`kind: "session"` or a `session` record), else from the file name. */
export function sessionIdOf(file) {
	for (const line of jsonLines(file.text).slice(0, 5)) {
		for (const record of Array.isArray(line) ? line : [line]) {
			if (record && typeof record === "object" && (record.type === "session" || record.kind === "session") && typeof record.id === "string")
				return record.id;
		}
	}
	return String(file.name).split("/").pop().replace(/\.jsonl$/, "").split("_").pop();
}

/**
 * Swarm calls (scripts/eval-quality.mjs's SWARM_CALLS) in the rlm code of the root session and of every other
 * session (children), plus spawn calls in the root.
 */
export function swarmStats(files, rootSessionId) {
	const root = countSwarmCalls("");
	const children = countSwarmCalls("");
	let rootSpawnCalls = 0;
	for (const file of files) {
		const isRoot = rootSessionId ? sessionIdOf(file) === rootSessionId || file.name.includes(rootSessionId) : files.length === 1;
		const seen = new Set();
		for (const line of jsonLines(file.text)) {
			for (const record of Array.isArray(line) ? line : [line]) {
				if (record?.kind !== "entry" || record.type !== "message" || seen.has(record.id)) continue;
				seen.add(record.id);
				if (record.message?.role !== "assistant") continue;
				for (const block of record.message.content ?? []) {
					if (block.type !== "toolCall" || block.name !== "rlm") continue;
					const code = String(block.arguments?.code ?? "");
					countSwarmCalls(code, isRoot ? root : children);
					if (isRoot) rootSpawnCalls += code.match(/\b(?:rlm|agents)\.spawn\s*\(/g)?.length ?? 0;
				}
			}
		}
	}
	const total = Object.fromEntries(Object.keys(root).map((name) => [name, root[name] + children[name]]));
	return { root, children, total, rootSpawnCalls, totalCalls: Object.values(total).reduce((a, b) => a + b, 0) };
}

/** Turns, tokens and children of a run from its session files (swebench's ultronStats), plus swarm calls. */
export function runStats(files, rootSessionId, price) {
	const root = rootSessionId ?? (files.length > 0 ? sessionIdOf([...files].sort((a, b) => a.name.localeCompare(b.name))[0]) : null);
	const named = files.map((file) => ({ ...file, name: `${file.name}#${sessionIdOf(file)}` }));
	const stats = ultronStats(named, root);
	const rootFiles = named.filter((file) => (root ? file.name.includes(root) : true));
	const rootTokens = ultronStats(rootFiles, root).tokens;
	const swarm = swarmStats(files, root);
	return {
		rootSessionId: root,
		turns: stats.turns,
		rootTurns: stats.turns - stats.childTurns,
		childTurns: stats.childTurns,
		childSessions: stats.childSessions,
		cells: stats.cells,
		errorCells: stats.errorCells,
		frameCalls: stats.frameCalls,
		spawnCalls: stats.spawnCalls,
		helpers: stats.helpers,
		tokens: stats.tokens,
		rootTokens,
		childTokens: Object.fromEntries(Object.keys(stats.tokens).map((name) => [name, stats.tokens[name] - (rootTokens[name] ?? 0)])),
		notionalCostUsd: price ? Number(notionalCost(stats.tokens, price).toFixed(4)) : null,
		models: stats.models,
		lastStopReason: stats.lastStopReason,
		errorMessage: stats.errorMessage,
		finalText: stats.finalText ? stats.finalText.slice(0, 1500) : null,
		swarm,
	};
}

/** The swarm-style host APIs among the host's call counters (`ultron.module` stats `hostCalls`). */
const SWARM_HOST_API = /^(?:swarm|board|notes)\.|^rlm\.(?:ask|partial|spawn|collect)$/;

/**
 * The session's own counters, from the last `ultron.module` / `stats` value in the journal: cells per lane and every
 * host API call by name (children's included). `swarmCalls` keeps the swarm-style APIs only.
 */
export function hostStats(text) {
	let stats = null;
	for (const line of jsonLines(text)) {
		for (const record of Array.isArray(line) ? line : [line]) {
			if (record?.kind === "value" && record.op === "set" && record.namespace === "ultron.module" && record.key === "stats") stats = record.value;
		}
	}
	if (!stats) return null;
	const hostCalls = stats.hostCalls ?? {};
	const swarmCalls = Object.fromEntries(Object.entries(hostCalls).filter(([name]) => SWARM_HOST_API.test(name)));
	return {
		cells: { root: stats.cells?.root?.count ?? 0, subagents: stats.cells?.subagents?.count ?? 0, other: stats.cells?.other?.count ?? 0 },
		hostCalls,
		swarmCalls,
		swarmCallTotal: Object.values(swarmCalls).reduce((a, b) => a + b, 0),
	};
}

/**
 * The host's usage ledger (the last `ultron.usage` / `root` value of the journal): responses, tokens and cost per
 * budget root. A turn's root (`turn:<id>`) carries the root lane and the children spawned inside the turn; kept
 * children (`keep=True`) outlive the turn and are charged to the session root (`ultron-root`). The session report
 * (`ultron usage`) sees the root lane only, so this is the one place the whole tree's usage is recorded.
 */
export function ledgerStats(text) {
	let ledger = null;
	for (const line of jsonLines(text)) {
		for (const record of Array.isArray(line) ? line : [line]) {
			if (record?.kind === "value" && record.op === "set" && record.namespace === "ultron.usage" && record.key === "root") ledger = record.value;
		}
	}
	if (!ledger?.roots) return null;
	const roots = {};
	const total = { responses: 0, tokens: 0, costUsd: 0 };
	for (const [id, root] of Object.entries(ledger.roots)) {
		const turns = root.turns ?? {};
		const entry = { responses: turns.count ?? 0, tokens: turns.tokens ?? 0, costUsd: turns.cost ?? 0 };
		roots[id.startsWith("turn:") ? "turn" : id] = entry;
		total.responses += entry.responses;
		total.tokens += entry.tokens;
		total.costUsd += entry.costUsd;
	}
	total.costUsd = Number(total.costUsd.toFixed(4));
	return { roots, total };
}

function laneTokens(lane) {
	if (!lane) return null;
	return { responses: lane.responses ?? 0, input: lane.input ?? 0, cacheRead: lane.cacheRead ?? 0, cacheWrite: lane.cacheWrite ?? 0, output: lane.output ?? 0, total: lane.totalTokens ?? 0 };
}

/**
 * What `ultron usage <session> --json` (schema ultron.session-report/1) says of a run: tokens per lane (the root,
 * frames, sub-agents, other: session naming and the like) and in total, sub-agents and frames spawned, cells.
 */
export function usageSummary(report) {
	if (!report || report.schema !== "ultron.session-report/1") return null;
	const lanes = report.usage?.lanes ?? {};
	const subagents = report.depth?.subagents ?? {};
	return {
		turns: report.turns?.count ?? null,
		tokens: laneTokens(report.usage?.total),
		lanes: { root: laneTokens(lanes.root), frames: laneTokens(lanes.frames), subagents: laneTokens(lanes.subagents), other: laneTokens(lanes.other) },
		subagents: { count: subagents.count ?? 0, maxDepth: subagents.maxDepth ?? 0, completed: subagents.completed ?? 0, failed: subagents.failed ?? 0 },
		frames: report.depth?.frames?.count ?? 0,
		depth: report.depth?.verdict ?? null,
		cells: { root: report.cells?.root?.count ?? 0, subagents: report.cells?.subagents?.count ?? 0, other: report.cells?.other?.count ?? 0 },
		reportedCostUsd: report.usage?.total?.cost?.reportedUsd ?? null,
	};
}

/** A run's status from how the agent ended. */
export function runStatus({ exitCode, timedOut, turns, errorMessage }) {
	if (timedOut) return "timeout";
	if (!turns) return "agent_crash";
	if (errorMessage) return "provider_error";
	if (exitCode !== 0) return "agent_error";
	return "completed";
}

export const HARNESS_FAILURES = new Set(["harness_error", "agent_crash", "provider_error"]);

// ---------------------------------------------------------------------------------------------------------------
// Report

function median(values) {
	const list = values.filter((value) => typeof value === "number");
	if (list.length === 0) return null;
	const sorted = [...list].sort((a, b) => a - b);
	const middle = Math.floor(sorted.length / 2);
	return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
}

function addLanes(...lanes) {
	const out = { input: 0, cacheRead: 0, cacheWrite: 0, output: 0, total: 0 };
	for (const lane of lanes) if (lane) for (const name of Object.keys(out)) out[name] += lane[name] ?? 0;
	return out;
}

/**
 * The scored numbers of one run. The whole tree (root and children): responses, tokens and cost from the host's
 * usage ledger (`ledger`). The root lane alone: Ultron's session report (`usage`), else the root journal (`stats`).
 * Children: the tree minus the root lane. Swarm calls: the host's counters (`host`, children's calls included);
 * children spawned: its `rlm.spawn` count. Without a ledger, the root lane's numbers stand for the tree.
 */
export function runMetrics(record, price) {
	const usage = record.usage ?? null;
	const stats = record.stats ?? {};
	const ledger = record.ledger ?? null;
	const rootLane = usage?.lanes?.root ?? stats.rootTokens ?? null;
	const rootResponses = usage?.lanes?.root?.responses ?? stats.rootTurns ?? null;
	const laneTokens = usage?.tokens ?? stats.tokens ?? null;
	const tokens = ledger?.total.tokens ?? laneTokens?.total ?? null;
	const responses = ledger?.total.responses ?? usage?.tokens?.responses ?? stats.turns ?? null;
	const cost = ledger ? ledger.total.costUsd : laneTokens && price ? Number(notionalCost(laneTokens, price).toFixed(4)) : null;
	return {
		verdict: record.grade?.verdict ?? null,
		status: record.status,
		wallMs: record.wallMs ?? null,
		rootTurns: stats.rootTurns ?? null,
		responses,
		childResponses: typeof responses === "number" && typeof rootResponses === "number" ? Math.max(0, responses - rootResponses) : null,
		tokens,
		rootTokens: rootLane?.total ?? null,
		childTokens: typeof tokens === "number" && typeof rootLane?.total === "number" ? Math.max(0, tokens - rootLane.total) : null,
		notionalCostUsd: cost,
		children: record.host?.hostCalls?.["rlm.spawn"] ?? usage?.subagents?.count ?? null,
		frames: usage?.frames ?? null,
		swarmCalls: record.host?.swarmCalls ?? null,
		swarmCallTotal: record.host?.swarmCallTotal ?? null,
		swarmCallsInRootCode: stats.swarm?.totalCalls ?? null,
	};
}

/** Per-arm totals over the records' metrics (one record per task and arm). */
export function summarize(records, arms = ARMS) {
	const out = {};
	for (const arm of arms) {
		const list = records.filter((record) => record.arm === arm).map((record) => record.metrics ?? {});
		const sum = (name) => list.reduce((total, metrics) => total + (metrics[name] ?? 0), 0);
		out[arm] = {
			runs: list.length,
			passed: list.filter((metrics) => metrics.verdict === "pass").length,
			timeouts: list.filter((metrics) => metrics.status === "timeout").length,
			harnessFailures: list.filter((metrics) => HARNESS_FAILURES.has(metrics.status)).length,
			wallMsTotal: sum("wallMs"),
			wallMsMedian: median(list.map((metrics) => metrics.wallMs)),
			responses: sum("responses"),
			tokens: sum("tokens"),
			rootTokens: sum("rootTokens"),
			childTokens: sum("childTokens"),
			notionalCostUsd: Number(sum("notionalCostUsd").toFixed(4)),
			children: sum("children"),
			swarmCalls: sum("swarmCallTotal"),
		};
	}
	return out;
}

function minutes(ms) {
	return typeof ms === "number" ? `${(ms / 60000).toFixed(1)} min` : "-";
}

function thousands(value) {
	return typeof value === "number" ? `${Math.round(value / 1000).toLocaleString("en-US")}k` : "-";
}

function callNames(calls) {
	if (!calls) return "-";
	const used = Object.entries(calls).filter(([, count]) => count > 0);
	return used.length ? used.map(([name, count]) => `${name} ${count}`).join(", ") : "none";
}

/** The Markdown rendering of a result file. */
export function renderMarkdown(result) {
	const lines = [
		`# DeepSWE swarm A/B: ${result.tasks.length} tasks, ${result.arms.join(" vs ")}`,
		"",
		`Date ${result.date}. Model \`${result.model}\` through CLIProxyAPI. Binary \`${result.binary.name}\` (${result.binary.version}, commit ${result.binary.commit}).`,
		`Dataset \`${result.dataset}\` (Harbor format); one fresh container per (task, arm) from the task image, graded in a second fresh container by the task's own verifier.`,
		"",
		"| task | language | arm | verifier | status | wall | responses (children) | tokens (root/children) | notional cost | children | swarm calls (host) |",
		"| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |",
	];
	for (const record of result.records) {
		const m = record.metrics ?? {};
		lines.push(
			`| ${record.task_id} | ${record.language} | ${record.arm} | ${m.verdict ?? "-"} | ${m.status} | ${minutes(m.wallMs)} | ${m.responses ?? "-"} (${m.childResponses ?? "-"}) | ${thousands(m.tokens)} (${thousands(m.rootTokens)}/${thousands(m.childTokens)}) | ${typeof m.notionalCostUsd === "number" ? `$${m.notionalCostUsd.toFixed(2)}` : "-"} | ${m.children ?? "-"} | ${callNames(m.swarmCalls)} |`,
		);
	}
	lines.push(
		"",
		"| arm | passed | timeouts | harness failures | median wall | total wall | responses | tokens | notional cost | children | swarm calls |",
		"| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |",
	);
	for (const [arm, totals] of Object.entries(result.summary)) {
		lines.push(
			`| ${arm} | ${totals.passed}/${totals.runs} | ${totals.timeouts} | ${totals.harnessFailures} | ${minutes(totals.wallMsMedian)} | ${minutes(totals.wallMsTotal)} | ${totals.responses} | ${thousands(totals.tokens)} | $${totals.notionalCostUsd.toFixed(2)} | ${totals.children} | ${totals.swarmCalls} |`,
		);
	}
	if (result.gold) lines.push("", `Gold check (reference patches through the same grading): ${result.gold.map((entry) => `${entry.task_id} ${entry.verdict}`).join(", ")}.`);
	if (result.notes?.length) lines.push("", ...result.notes.map((note) => `- ${note}`));
	lines.push("");
	return lines.join("\n");
}
