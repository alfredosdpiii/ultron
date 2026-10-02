/**
 * The pure parts of the SWE-bench harness: sampling, prompt building, arm configuration, patch cleaning, usage
 * parsing and result aggregation. Nothing here touches Docker, the network or the filesystem, so all of it is unit
 * tested (scripts/eval-swebench.test.mjs). The side effects live in run.mjs.
 */

import { createHash } from "node:crypto";

// ---------------------------------------------------------------------------------------------------------------
// Constants of the comparison

/** swebench 5 reads image names and eval scripts from this dataset; the same 500 instances as princeton-nlp's. */
export const DATASET = "SWE-bench/SWE-bench_Verified";
export const SPLIT = "test";
/** Fixed before the first sample was drawn; changing it changes every sample. */
export const DEFAULT_SEED = "ultron-swebench-1";
export const MODEL_ID = "gpt-6.1-sol";
export const PROVIDER = "cliproxyapi";
/** The environment variable every arm reads the proxy key from. It is never written to a file. */
export const KEY_ENV = "CLIPROXY_API_KEY";
/** Ultron's and Pi's default thinking level; Codex is set to the same so all three send the same effort. */
export const REASONING_EFFORT = "medium";
export const ARMS = ["ultron", "codex", "pi"];
/** Where the read-only runtimes are mounted in every task container. */
export const MOUNT = "/opt/agent";
/** The per-run scratch directory inside the container: agent dirs, sessions, the prompt. Never mounted. */
export const AGENT_DIR = "/agent";
export const REPO_DIR = "/testbed";
/** Paths the agents' own tooling may create in the repository; never part of a prediction. */
export const EXCLUDED_PATCH_PREFIXES = [".loki/"];

// ---------------------------------------------------------------------------------------------------------------
// Sampling

function digest(text) {
	return createHash("sha256").update(text).digest("hex");
}

/** A number in [0, 1) determined by `text`. */
function unit(text) {
	return Number.parseInt(digest(text).slice(0, 12), 16) / 2 ** 48;
}

/**
 * Every instance id in one seeded order whose prefixes are all stratified by repository.
 *
 * Within a repository the instances are shuffled by a seeded hash; instance number `rank` of a repository with `c`
 * instances gets the position `(rank + offset) / c`, with one seeded offset in [0, 1) per repository; the global
 * order is by position. The first `n` ids therefore hold, for every repository, its share of `n` to within one
 * instance (systematic sampling), and the first 50 extend the first 10 instead of redrawing them.
 */
export function sampleOrder(instances, seed = DEFAULT_SEED) {
	const byRepo = new Map();
	for (const instance of instances) {
		if (!byRepo.has(instance.repo)) byRepo.set(instance.repo, []);
		byRepo.get(instance.repo).push(instance.instance_id);
	}
	const placed = [];
	for (const [repo, ids] of byRepo) {
		const offset = unit(`${seed}:repo:${repo}`);
		const shuffled = [...new Set(ids)]
			.map((id) => ({ id, key: digest(`${seed}:instance:${id}`) }))
			.sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0));
		shuffled.forEach(({ id }, rank) => {
			placed.push({ id, position: (rank + offset) / shuffled.length });
		});
	}
	placed.sort((a, b) => a.position - b.position || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
	return placed.map((entry) => entry.id);
}

/** The first `n` instances of the seeded order, as full dataset rows. */
export function sample(instances, n, seed = DEFAULT_SEED) {
	const byId = new Map(instances.map((instance) => [instance.instance_id, instance]));
	return sampleOrder(instances, seed)
		.slice(0, n)
		.map((id) => byId.get(id));
}

/** `{ repo: count }` of a list of instances, for the record of what a sample covers. */
export function repoCounts(instances) {
	const counts = {};
	for (const instance of instances) counts[instance.repo] = (counts[instance.repo] ?? 0) + 1;
	return Object.fromEntries(Object.entries(counts).sort(([a], [b]) => (a < b ? -1 : 1)));
}

// ---------------------------------------------------------------------------------------------------------------
// Prompt

/**
 * The one prompt every arm gets: the issue text and where the repository is. No hints, no gold patch, no names of
 * the failing tests.
 */
export function buildPrompt(instance) {
	return [
		`You are working in a checkout of the ${instance.repo} repository at ${REPO_DIR}. The following issue was reported against it:`,
		"",
		"<issue>",
		instance.problem_statement.trim(),
		"</issue>",
		"",
		`Resolve the issue by changing the source files under ${REPO_DIR}. Your change is scored by the project's own test suite, including tests you cannot see, so fix the underlying problem rather than one example of it, and do not break existing behaviour. Changes to test files are ignored.`,
		"",
		"The project's Python environment (the conda environment `testbed`) is already active in the shell: `python` there is the project's interpreter with the project installed in development mode, so you can run the existing tests and your own reproduction scripts. Work only from the repository and the issue text; do not look up the upstream fix. Leave your changes uncommitted in the working tree.",
	].join("\n");
}

// ---------------------------------------------------------------------------------------------------------------
// Arm configuration

/** `models.json` for Ultron and Pi: the user's proxy provider entry, cut to the one model, key from the environment. */
export function modelsJson(userModels, baseUrl) {
	const provider = userModels?.providers?.[PROVIDER];
	if (!provider) throw new Error(`models.json has no "${PROVIDER}" provider`);
	const model = (provider.models ?? []).find((candidate) => candidate.id === MODEL_ID);
	if (!model) throw new Error(`the "${PROVIDER}" provider has no model "${MODEL_ID}"`);
	const { apiKey: _apiKey, baseUrl: _baseUrl, models: _models, ...rest } = provider;
	return { providers: { [PROVIDER]: { ...rest, baseUrl, apiKey: `$${KEY_ENV}`, models: [model] } } };
}

/** The notional price table (USD per million tokens) of the model, from the same models.json entry. */
export function priceTable(userModels) {
	const model = (userModels?.providers?.[PROVIDER]?.models ?? []).find((candidate) => candidate.id === MODEL_ID);
	const cost = model?.cost ?? {};
	return { input: cost.input ?? 0, output: cost.output ?? 0, cacheRead: cost.cacheRead ?? 0 };
}

/** Codex's `config.toml`: a custom provider on the proxy (Responses API), the key from the environment. */
export function codexConfigToml(baseUrl) {
	return [
		`model = "${MODEL_ID}"`,
		`model_provider = "${PROVIDER}"`,
		`model_reasoning_effort = "${REASONING_EFFORT}"`,
		"",
		`[model_providers.${PROVIDER}]`,
		'name = "CLIProxyAPI"',
		`base_url = "${baseUrl}"`,
		`env_key = "${KEY_ENV}"`,
		'wire_api = "responses"',
		"",
	].join("\n");
}

function shellQuote(value) {
	return `'${String(value).replaceAll("'", "'\\''")}'`;
}

const ARM_SPECS = {
	ultron: {
		env: {
			ULTRON_CODING_AGENT_DIR: `${AGENT_DIR}/ultron-agent`,
			ULTRON_SERVER_DIR: `${AGENT_DIR}/ultron-server`,
			// The kernel and Loki run on the mounted Python 3.12; the repository's tests run on the testbed
			// interpreter (3.5 to 3.9 in most images) through bash().
			ULTRON_PYTHON: `${MOUNT}/python/bin/python3`,
			ULTRON_LOKI_PYTHON: `${MOUNT}/python/bin/python3`,
			ULTRON_LOKI_AUTOINIT: "off",
			ULTRON_LOKI_LOG: `${AGENT_DIR}/loki.jsonl`,
			ULTRON_HINDSIGHT_URL: "off",
		},
		command: [
			`${MOUNT}/node/bin/node`,
			`${MOUNT}/node/lib/node_modules/ultron-agent/dist/bundle/cli.js`,
			"--mode",
			"json",
			"-p",
			"--model",
			`${PROVIDER}/${MODEL_ID}`,
			"--",
		],
		configFiles: (config) => ({ [`${AGENT_DIR}/ultron-agent/models.json`]: config.modelsJson }),
		dirs: [`${AGENT_DIR}/ultron-agent`, `${AGENT_DIR}/ultron-server`],
		// What is copied out as the transcript: `[path in the container, name in the run's evidence dir]`. Never
		// the whole agent dir: it holds the kernel snapshots and their key.
		keep: [
			[`${AGENT_DIR}/ultron-agent/experimental/sessions`, "sessions"],
			[`${AGENT_DIR}/ultron-agent/traces`, "traces"],
			[`${AGENT_DIR}/loki.jsonl`, "loki.jsonl"],
		],
	},
	codex: {
		env: { CODEX_HOME: `${AGENT_DIR}/codex-home` },
		// Codex's own sandbox (bubblewrap, and the legacy Landlock one) cannot start inside an unprivileged
		// container, and `workspace-write` then fails every command. The container is the sandbox, so Codex's is
		// off; nothing is added to the container to make up for it.
		command: [`${MOUNT}/codex/bin/codex`, "exec", "--json", "--sandbox", "danger-full-access", "--cd", REPO_DIR],
		configFiles: (config) => ({ [`${AGENT_DIR}/codex-home/config.toml`]: config.codexToml }),
		dirs: [`${AGENT_DIR}/codex-home`],
		keep: [[`${AGENT_DIR}/codex-home/sessions`, "sessions"]],
	},
	pi: {
		env: { PI_CODING_AGENT_DIR: `${AGENT_DIR}/pi-agent` },
		command: [
			`${MOUNT}/pi/pi`,
			"--mode",
			"json",
			"-p",
			"--model",
			`${PROVIDER}/${MODEL_ID}`,
			"--",
		],
		configFiles: (config) => ({ [`${AGENT_DIR}/pi-agent/models.json`]: config.modelsJson }),
		dirs: [`${AGENT_DIR}/pi-agent`],
		keep: [[`${AGENT_DIR}/pi-agent/sessions`, "sessions"]],
	},
};

/** The read-only runtimes an arm needs mounted under MOUNT (ripgrep, under `tools`, goes to every arm). */
export const ARM_RUNTIMES = { ultron: ["node", "ultron", "python"], codex: ["codex"], pi: ["pi"] };

/** What an arm needs inside the container: environment, directories, config files and what to copy out. */
export function armSpec(arm) {
	const spec = ARM_SPECS[arm];
	if (!spec) throw new Error(`unknown arm "${arm}" (known: ${ARMS.join(", ")})`);
	return spec;
}

/**
 * The script that runs an arm inside the task container. It activates the task's conda environment exactly as the
 * official evaluation does, so the agent's shell sees the project's interpreter, and bounds the agent by the
 * wall-clock limit. The prompt is read from a file; the key arrives through the environment of `docker exec`.
 */
export function agentScript(arm, { limitSeconds }) {
	const spec = armSpec(arm);
	const exports = Object.entries(spec.env).map(([name, value]) => `export ${name}=${shellQuote(value)}`);
	return [
		"#!/bin/bash",
		"source /opt/miniconda3/bin/activate",
		"conda activate testbed",
		// ripgrep for every arm, after the image's own PATH so it shadows nothing.
		`export PATH="$PATH:${MOUNT}/tools"`,
		...exports,
		`cd ${REPO_DIR}`,
		`PROMPT="$(cat ${AGENT_DIR}/prompt.txt)"`,
		`exec timeout --signal=TERM --kill-after=30 ${Math.round(limitSeconds)}s ${spec.command.map(shellQuote).join(" ")} "$PROMPT" < /dev/null`,
		"",
	].join("\n");
}

/** The settings of each arm as recorded with the results. */
export function armDescriptions(versions = {}) {
	return {
		ultron: {
			version: versions.ultron ?? null,
			command: "ultron --mode json -p --model cliproxyapi/gpt-6.1-sol",
			api: "Chat Completions (/v1/chat/completions)",
			reasoningEffort: `${REASONING_EFFORT} (Ultron's default thinking level; not set by the harness)`,
			configuration:
				"fresh agent dir holding only models.json; no settings.json, skills, AGENTS.md or extensions; Hindsight off",
			python: "kernel and Loki on a mounted standalone CPython 3.12 (ULTRON_PYTHON, ULTRON_LOKI_PYTHON)",
			loki: "guard on (default mode) with the bundled engine; ULTRON_LOKI_AUTOINIT=off, so no .loki/ is created",
			limits: "the tool's defaults (no turn or token limit); the harness's wall-clock limit",
		},
		codex: {
			version: versions.codex ?? null,
			command: "codex exec --json --sandbox danger-full-access",
			api: "Responses (/v1/responses)",
			reasoningEffort: `${REASONING_EFFORT} (model_reasoning_effort in the isolated config.toml)`,
			configuration:
				"fresh CODEX_HOME holding only config.toml (custom model provider on the proxy); no AGENTS.md, prompts or MCP servers",
			sandbox:
				"Codex's own sandbox off (danger-full-access): bubblewrap and the legacy Landlock sandbox both fail in an unprivileged container; the container is the sandbox",
			modelMetadata:
				"Codex 0.152.1 has no catalog entry for gpt-6.1-sol and runs it on its fallback model metadata (it says so at the start of every run)",
			limits: "the tool's defaults; the harness's wall-clock limit",
		},
		pi: {
			version: versions.pi ?? null,
			command: "pi --mode json -p --model cliproxyapi/gpt-6.1-sol",
			api: "Chat Completions (/v1/chat/completions)",
			reasoningEffort: `${REASONING_EFFORT} (Pi's default thinking level; not set by the harness)`,
			configuration: "fresh agent dir holding only models.json; the four default tools (read, bash, edit, write)",
			limits: "the tool's defaults; the harness's wall-clock limit",
		},
	};
}

// ---------------------------------------------------------------------------------------------------------------
// Patch

const DIFF_HEADER = /^diff --git (?:"a\/(.*)" "b\/(.*)"|a\/(.*) b\/(.*))$/;

/**
 * The prediction from a raw `git diff`: file sections under an excluded prefix (`.loki/`) and binary sections are
 * dropped, since the official evaluation applies the patch with `git apply` and a binary stub ("Binary files
 * differ") makes the whole patch fail to apply. Returns the cleaned patch, the files it changes and what was
 * dropped.
 */
export function cleanPatch(diff, excludedPrefixes = EXCLUDED_PATCH_PREFIXES) {
	const sections = [];
	let current = null;
	for (const line of diff.split("\n")) {
		const header = DIFF_HEADER.exec(line);
		if (header) {
			current = { path: header[2] ?? header[4], lines: [line] };
			sections.push(current);
		} else if (current) {
			current.lines.push(line);
		}
	}
	const kept = [];
	const files = [];
	const dropped = [];
	for (const section of sections) {
		const excluded = excludedPrefixes.find(
			(prefix) => section.path === prefix.replace(/\/$/, "") || section.path.startsWith(prefix),
		);
		const binary = section.lines.some((line) => line === "GIT binary patch" || /^Binary files .* differ$/.test(line));
		if (excluded) dropped.push({ path: section.path, reason: `excluded (${excluded})` });
		else if (binary) dropped.push({ path: section.path, reason: "binary" });
		else {
			// A diff ends with a newline, so the last section's last "line" is empty; keep sections newline-free
			// at the end and join them back with exactly one.
			while (section.lines.length > 0 && section.lines[section.lines.length - 1] === "") section.lines.pop();
			kept.push(section.lines.join("\n"));
			files.push(section.path);
		}
	}
	return { patch: kept.length > 0 ? `${kept.join("\n")}\n` : "", files, dropped };
}

/** One line of the predictions file the official evaluation reads. */
export function prediction(record, patch) {
	return { instance_id: record.instance_id, model_name_or_path: record.arm, model_patch: patch };
}

// ---------------------------------------------------------------------------------------------------------------
// Secrets

/** `text` with every occurrence of `secret` replaced; a secret shorter than 6 characters is not searched for. */
export function scrubSecret(text, secret) {
	if (!secret || secret.length < 6) return text;
	return text.split(secret).join("<redacted>");
}

// ---------------------------------------------------------------------------------------------------------------
// Usage

function emptyTokens() {
	return { input: 0, cacheRead: 0, output: 0, reasoning: 0, total: 0 };
}

function addTokens(target, { input = 0, cacheRead = 0, output = 0, reasoning = 0 }) {
	target.input += input;
	target.cacheRead += cacheRead;
	target.output += output;
	target.reasoning += reasoning;
	target.total = target.input + target.cacheRead + target.output;
	return target;
}

function jsonLines(text) {
	const values = [];
	for (const line of text.split("\n")) {
		if (!line.trim()) continue;
		try {
			values.push(JSON.parse(line));
		} catch {
			// A line cut off by a killed process; the rest of the file still counts.
		}
	}
	return values;
}

/** USD for a token count at the model's list prices. Notional: the proxy bills a subscription, not tokens. */
export function notionalCost(tokens, price) {
	return (tokens.input * price.input + tokens.cacheRead * price.cacheRead + tokens.output * price.output) / 1_000_000;
}

const FRAME_CALL = /\brlm\.(?:spawn|infer|map)\s*\(/g;
const SPAWN_CALL = /\b(?:rlm|agents)\.spawn\s*\(/g;
/** `await bash(`, `await rlm.infer(`: the runtime helpers a cell calls. */
const AWAITED_CALL = /\bawait\s+([A-Za-z_][\w.]*)\s*\(/g;

/**
 * What an Ultron run did, from its session files (each a journal of JSON lines; a line is one record or an array
 * of records). `files` is `[{ name, text }]`; the root session is the one named by `rootSessionId`, any other file
 * is a sub-agent's session. Tokens come from the journal's `usage` rows (one per model call, sub-agents included);
 * cells are the root's `rlm` tool calls, `helpers` the awaited runtime helpers in their code (`bash`, `edit`, ...),
 * `commands` every cell's code (for the network scan; not kept in results).
 */
export function ultronStats(files, rootSessionId = null) {
	const stats = {
		turns: 0,
		toolCalls: 0,
		toolsByName: {},
		tokens: emptyTokens(),
		reportedCostUsd: 0,
		cells: 0,
		frameCalls: 0,
		spawnCalls: 0,
		childSessions: 0,
		childTurns: 0,
		errorCells: 0,
		helpers: {},
		lastStopReason: null,
		errorMessage: null,
		finalText: null,
		commands: [],
	};
	const seenUsage = new Set();
	const seenEntries = new Set();
	for (const file of files) {
		const isRoot = rootSessionId ? file.name.includes(rootSessionId) : files.length === 1;
		let assistantTurns = 0;
		for (const line of jsonLines(file.text)) {
			for (const record of Array.isArray(line) ? line : [line]) {
				if (!record || typeof record !== "object") continue;
				if (record.kind === "usage" && record.usage && !seenUsage.has(record.id)) {
					seenUsage.add(record.id);
					addTokens(stats.tokens, record.usage);
					stats.reportedCostUsd += record.usage.cost?.total ?? 0;
				}
				if (record.kind !== "entry" || record.type !== "message" || seenEntries.has(record.id)) continue;
				seenEntries.add(record.id);
				const message = record.message ?? {};
				if (message.role === "assistant") {
					assistantTurns++;
					for (const block of message.content ?? []) {
						if (block.type !== "toolCall") continue;
						stats.toolCalls++;
						stats.toolsByName[block.name] = (stats.toolsByName[block.name] ?? 0) + 1;
						if (block.name !== "rlm") continue;
						const code = String(block.arguments?.code ?? "");
						stats.commands.push(code);
						if (!isRoot) continue;
						stats.cells++;
						stats.frameCalls += code.match(FRAME_CALL)?.length ?? 0;
						stats.spawnCalls += code.match(SPAWN_CALL)?.length ?? 0;
						for (const [, helper] of code.matchAll(AWAITED_CALL))
							stats.helpers[helper] = (stats.helpers[helper] ?? 0) + 1;
					}
					if (isRoot) {
						stats.lastStopReason = message.stopReason ?? null;
						stats.errorMessage = message.stopReason === "error" ? (message.errorMessage ?? "error") : null;
						const text = (message.content ?? [])
							.filter((block) => block.type === "text")
							.map((block) => block.text)
							.join("\n");
						if (text) stats.finalText = text;
					}
				} else if (message.role === "toolResult" && isRoot && message.isError) {
					stats.errorCells++;
				}
			}
		}
		stats.turns += assistantTurns;
		if (!isRoot) {
			stats.childSessions++;
			stats.childTurns += assistantTurns;
		}
	}
	return stats;
}

/** What a Pi run did, from its `--mode json` event stream. */
export function piStats(text) {
	const stats = {
		turns: 0,
		toolCalls: 0,
		toolsByName: {},
		tokens: emptyTokens(),
		reportedCostUsd: 0,
		lastStopReason: null,
		errorMessage: null,
		finalText: null,
		commands: [],
	};
	for (const event of jsonLines(text)) {
		if (event.type === "tool_execution_start") {
			stats.toolCalls++;
			stats.toolsByName[event.toolName] = (stats.toolsByName[event.toolName] ?? 0) + 1;
			if (typeof event.args?.command === "string") stats.commands.push(event.args.command);
		}
		if (event.type !== "message_end" || event.message?.role !== "assistant") continue;
		const message = event.message;
		stats.turns++;
		if (message.usage) {
			addTokens(stats.tokens, message.usage);
			stats.reportedCostUsd += message.usage.cost?.total ?? 0;
		}
		stats.lastStopReason = message.stopReason ?? null;
		stats.errorMessage = message.stopReason === "error" ? (message.errorMessage ?? "error") : null;
		const text_ = (message.content ?? [])
			.filter((block) => block.type === "text")
			.map((block) => block.text)
			.join("\n");
		if (text_) stats.finalText = text_;
	}
	return stats;
}

/** Events of `pi --mode json` that repeat the whole partial message; dropped from the kept transcript. */
export function isStreamingNoise(line) {
	return line.startsWith('{"type":"message_update"') || line.startsWith('{"type":"tool_execution_update"');
}

const CODEX_NON_TOOL_ITEMS = new Set(["agent_message", "reasoning", "error", "todo_list"]);

/**
 * What a Codex run did, from `codex exec --json` (items and per-turn usage) and its rollout file (one
 * `token_count` event per model request). Codex counts cached tokens inside `input_tokens`; they are split out
 * here so the three arms are counted alike.
 */
export function codexStats(stdoutText, rolloutText = "") {
	const stats = {
		turns: 0,
		toolCalls: 0,
		toolsByName: {},
		tokens: emptyTokens(),
		completed: false,
		errorMessage: null,
		warnings: [],
		finalText: null,
		commands: [],
	};
	for (const event of jsonLines(stdoutText)) {
		if (event.type === "turn.completed") {
			stats.completed = true;
			const usage = event.usage ?? {};
			const cached = usage.cached_input_tokens ?? 0;
			addTokens(stats.tokens, {
				input: (usage.input_tokens ?? 0) - cached,
				cacheRead: cached,
				output: usage.output_tokens ?? 0,
				reasoning: usage.reasoning_output_tokens ?? 0,
			});
		} else if (event.type === "turn.failed" || event.type === "error") {
			stats.errorMessage = String(event.error?.message ?? event.message ?? "error").slice(0, 500);
		} else if (event.type === "item.completed" && event.item) {
			const item = event.item;
			if (item.type === "agent_message") stats.finalText = item.text ?? stats.finalText;
			else if (item.type === "error") stats.warnings.push(String(item.message ?? "").slice(0, 300));
			if (typeof item.command === "string") stats.commands.push(item.command);
			if (!CODEX_NON_TOOL_ITEMS.has(item.type)) {
				stats.toolCalls++;
				stats.toolsByName[item.type] = (stats.toolsByName[item.type] ?? 0) + 1;
			}
		}
	}
	for (const event of jsonLines(rolloutText)) {
		if (event.type === "event_msg" && event.payload?.type === "token_count" && event.payload.info) stats.turns++;
	}
	if (stats.completed) stats.errorMessage = null;
	return stats;
}

/**
 * A run's recorder ledger as totals: requests by outcome, the models and efforts actually sent, and tokens counted
 * the same way for every arm (the upstream's own usage blocks).
 */
export function ledgerStats(text) {
	const stats = {
		requests: 0,
		ok: 0,
		rateLimited: 0,
		failed: 0,
		requestModels: [],
		responseModels: [],
		efforts: [],
		paths: [],
		tokens: emptyTokens(),
		missingUsage: 0,
	};
	const note = (list, value) => {
		if (value !== null && value !== undefined && !list.includes(value)) list.push(value);
	};
	for (const entry of jsonLines(text)) {
		if (entry.method !== "POST") continue;
		stats.requests++;
		note(stats.paths, entry.path);
		note(stats.requestModels, entry.request?.model);
		note(stats.efforts, entry.request?.reasoningEffort);
		if (entry.status === 429) stats.rateLimited++;
		else if (entry.status !== 200) stats.failed++;
		else {
			stats.ok++;
			note(stats.responseModels, entry.responseModel);
			if (entry.usage) {
				addTokens(stats.tokens, {
					input: entry.usage.input - entry.usage.cachedInput,
					cacheRead: entry.usage.cachedInput,
					output: entry.usage.output,
					reasoning: entry.usage.reasoning,
				});
			} else stats.missingUsage++;
		}
	}
	return stats;
}

/** Shell commands that reach for the network, for a later look at whether a run looked the fix up. */
const NETWORK_PATTERNS = [
	/\bgit\s+(?:fetch|pull|clone|ls-remote|remote\s+add)\b/,
	/\b(?:curl|wget)\s+[^|;&]*https?:\/\//,
	/\bpip3?\s+(?:install|download)\b(?![^|;&]*(?:\s-e\b|\s\.\s*$|\s\.\/))/,
	/\bgithub\.com\/[\w.-]+\/[\w.-]+\/(?:pull|commit|issues)\//,
	/\b(?:urllib\.request|requests\.get|httpx\.get)\b[^\n]*https?:\/\//,
];

/** The commands in `commands` (strings) that touch the network, shortened. */
export function networkLookups(commands) {
	const hits = [];
	for (const command of commands) {
		if (NETWORK_PATTERNS.some((pattern) => pattern.test(command))) hits.push(command.replace(/\s+/g, " ").slice(0, 200));
	}
	return hits;
}

// ---------------------------------------------------------------------------------------------------------------
// Outcomes and aggregation

/**
 * How an agent run ended, apart from whether its patch resolves the task:
 * - `completed`: the agent finished by itself;
 * - `timeout`: the wall-clock limit ended it (its working tree is still the prediction);
 * - `provider_error`: its last model request failed (rate limit, proxy or upstream error); the harness retries these;
 * - `agent_crash`: the agent process failed for another reason, or never reached the model.
 * `harness_error` (image pull, container start) is set by the runner, not here.
 */
export function runStatus({ exitCode, timedOut, turns, errorMessage, ledger }) {
	if (timedOut || exitCode === 124 || exitCode === 137) return "timeout";
	const providerTrouble = ledger && ledger.rateLimited + ledger.failed > 0;
	if (errorMessage) return providerTrouble || /rate|429|5\d\d|overloaded|timeout|connection/i.test(errorMessage) ? "provider_error" : "agent_crash";
	if (exitCode !== 0) return providerTrouble && (ledger?.ok ?? 0) === 0 ? "provider_error" : "agent_crash";
	if (!turns) return "agent_crash";
	return "completed";
}

/** Statuses that say nothing about the agent's ability; their tasks are reported apart from unresolved ones. */
export const HARNESS_FAILURES = new Set(["harness_error", "provider_error", "agent_crash"]);

/**
 * The official evaluation's verdict on one instance from its report (`<model>.<run_id>.json`): `resolved`,
 * `unresolved`, `empty_patch`, `error` (the evaluation itself failed) or `not_evaluated`.
 */
export function evalVerdict(report, instanceId) {
	if (!report) return "not_evaluated";
	if ((report.resolved_ids ?? []).includes(instanceId)) return "resolved";
	if ((report.unresolved_ids ?? []).includes(instanceId)) return "unresolved";
	if ((report.empty_patch_ids ?? []).includes(instanceId)) return "empty_patch";
	if ((report.error_ids ?? []).includes(instanceId)) return "error";
	if ((report.incomplete_ids ?? []).includes(instanceId)) return "error";
	return "not_evaluated";
}

function sum(records, pick) {
	return records.reduce((total, record) => total + (pick(record) ?? 0), 0);
}

function median(values) {
	if (values.length === 0) return null;
	const sorted = [...values].sort((a, b) => a - b);
	const middle = Math.floor(sorted.length / 2);
	return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
}

/**
 * Per-arm totals over the final records (one per task and arm, each with its `verdict`). A task counts as resolved
 * only on the official verdict; harness failures and evaluation errors are counted apart from genuine misses.
 */
export function summarize(records, arms = ARMS) {
	const summary = {};
	for (const arm of arms) {
		const runs = records.filter((record) => record.arm === arm);
		if (runs.length === 0) continue;
		const harnessFailures = runs.filter((record) => HARNESS_FAILURES.has(record.status));
		const evalErrors = runs.filter((record) => !HARNESS_FAILURES.has(record.status) && record.verdict === "error");
		const resolved = runs.filter((record) => record.verdict === "resolved");
		const scored = runs.length - harnessFailures.length - evalErrors.length;
		summary[arm] = {
			tasks: runs.length,
			resolved: resolved.length,
			unresolved: runs.filter(
				(record) => !HARNESS_FAILURES.has(record.status) && ["unresolved", "empty_patch"].includes(record.verdict),
			).length,
			emptyPatches: runs.filter((record) => record.verdict === "empty_patch").length,
			timeouts: runs.filter((record) => record.status === "timeout").length,
			harnessFailures: harnessFailures.map((record) => ({ instance_id: record.instance_id, status: record.status })),
			evalErrors: evalErrors.map((record) => record.instance_id),
			resolvedRate: runs.length > 0 ? resolved.length / runs.length : null,
			resolvedRateOfScored: scored > 0 ? resolved.length / scored : null,
			wallSeconds: Math.round(sum(runs, (record) => record.wallMs) / 1000),
			medianWallSeconds: Math.round((median(runs.map((record) => record.wallMs ?? 0)) ?? 0) / 1000),
			turns: sum(runs, (record) => record.turns),
			toolCalls: sum(runs, (record) => record.toolCalls),
			tokens: runs.reduce((total, record) => addTokens(total, record.tokens ?? {}), emptyTokens()),
			ledgerTokens: runs.reduce((total, record) => addTokens(total, record.ledger?.tokens ?? {}), emptyTokens()),
			notionalCostUsd: Number(sum(runs, (record) => record.notionalCostUsd).toFixed(4)),
			rateLimited: sum(runs, (record) => record.ledger?.rateLimited),
		};
	}
	return summary;
}

/** Tasks where the arms disagree, and the head-to-head counts between two arms. */
export function headToHead(records, a, b) {
	const verdicts = (arm) =>
		new Map(records.filter((record) => record.arm === arm).map((record) => [record.instance_id, record.verdict]));
	const left = verdicts(a);
	const right = verdicts(b);
	const result = { both: 0, onlyA: [], onlyB: [], neither: 0 };
	for (const [id, verdict] of left) {
		if (!right.has(id)) continue;
		const other = right.get(id);
		if (verdict === "resolved" && other === "resolved") result.both++;
		else if (verdict === "resolved") result.onlyA.push(id);
		else if (other === "resolved") result.onlyB.push(id);
		else result.neither++;
	}
	return result;
}

function thousands(value) {
	return value === null || value === undefined ? "-" : Math.round(value).toLocaleString("en-US");
}

function duration(ms) {
	if (ms === null || ms === undefined) return "-";
	const seconds = Math.round(ms / 1000);
	return `${Math.floor(seconds / 60)}m${String(seconds % 60).padStart(2, "0")}s`;
}

const VERDICT_MARK = { resolved: "yes", unresolved: "no", empty_patch: "no (empty patch)", error: "eval error", not_evaluated: "-" };

/** The results as a Markdown document: per-arm summary, then one row per task. */
export function renderMarkdown(result) {
	const arms = Object.keys(result.summary);
	const lines = [
		`# SWE-bench Verified, ${result.sample.n} tasks, ${result.model}`,
		"",
		`Run \`${result.runId}\`, ${result.date}. Seed \`${result.sample.seed}\`, one run per task and arm, ${result.limits.wallClockMinutes} min wall-clock limit, ${result.limits.concurrency} tasks at a time. Scored by the official SWE-bench evaluation (swebench ${result.swebenchVersion}) in the prebuilt instance images.`,
		"",
		"| arm | resolved | unresolved | timeouts | harness failures | eval errors | wall time | turns | tool calls | tokens (in / cached / out) | notional cost |",
		"| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |",
	];
	for (const arm of arms) {
		const row = result.summary[arm];
		lines.push(
			`| ${arm} | ${row.resolved}/${row.tasks} | ${row.unresolved} | ${row.timeouts} | ${row.harnessFailures.length} | ${row.evalErrors.length} | ${duration(row.wallSeconds * 1000)} | ${thousands(row.turns)} | ${thousands(row.toolCalls)} | ${thousands(row.tokens.input)} / ${thousands(row.tokens.cacheRead)} / ${thousands(row.tokens.output)} | $${row.notionalCostUsd.toFixed(2)} |`,
		);
	}
	lines.push(
		"",
		"Cost is notional: tokens at the model's list prices from models.json. The proxy bills a subscription.",
		"",
		"## Per task",
		"",
		`| instance | ${arms.map((arm) => `${arm}: resolved, time, turns, tokens`).join(" | ")} |`,
		`| --- | ${arms.map(() => "---").join(" | ")} |`,
	);
	const ids = [...new Set(result.records.map((record) => record.instance_id))];
	for (const id of ids) {
		const cells = arms.map((arm) => {
			const record = result.records.find((candidate) => candidate.arm === arm && candidate.instance_id === id);
			if (!record) return "-";
			const status = record.status === "completed" ? "" : ` [${record.status}]`;
			return `${VERDICT_MARK[record.verdict] ?? record.verdict}${status}, ${duration(record.wallMs)}, ${record.turns ?? "-"}, ${thousands(record.tokens?.total)}`;
		});
		lines.push(`| ${id} | ${cells.join(" | ")} |`);
	}
	const disagreements = Object.entries(result.headToHead ?? {}).filter(([, pair]) => pair.onlyA.length + pair.onlyB.length > 0);
	if (disagreements.length > 0) {
		lines.push("", "## Where the arms differ", "");
		for (const [name, pair] of disagreements) {
			const [a, b] = name.split(" vs ");
			if (pair.onlyA.length > 0) lines.push(`- only ${a} (not ${b}): ${pair.onlyA.join(", ")}`);
			if (pair.onlyB.length > 0) lines.push(`- only ${b} (not ${a}): ${pair.onlyB.join(", ")}`);
		}
	}
	const failures = result.harnessFailures ?? [];
	lines.push("", "## Harness failures, timeouts and evaluation errors", "");
	if (failures.length === 0) lines.push("None: every run ended by itself and every prediction was evaluated.");
	for (const failure of failures)
		lines.push(`- ${failure.arm} ${failure.instance_id}: ${failure.status}, verdict ${failure.verdict}${failure.error ? ` (${failure.error.slice(0, 200)})` : ""}`);
	lines.push("", "## Checks", "");
	for (const [arm, check] of Object.entries(result.verification ?? {}))
		lines.push(
			`- ${arm}: ${check.ok ? "verified" : "NOT verified"} on ${check.responseModels.join(", ") || "no model"} via ${check.paths.join(", ")}, reasoning effort ${check.reasoningEfforts.join(", ") || "none sent"} (${check.requests} requests seen by the recorder)`,
		);
	if (result.goldCheck) lines.push(`- gold patches of the same tasks: ${result.goldCheck.resolved}/${result.goldCheck.of} resolved by the same evaluation`);
	const ultronRuns = result.records.filter((record) => record.ultron);
	if (ultronRuns.length > 0) {
		lines.push(
			"",
			"## How Ultron worked",
			"",
			"| instance | cells | failed cells | helpers awaited | frame calls | sub-agent sessions | Loki: write checks / blocked / cells with findings |",
			"| --- | --- | --- | --- | --- | --- | --- |",
		);
		for (const record of ultronRuns) {
			const { cells, errorCells, helpers, frameCalls, childSessions, loki } = record.ultron;
			const used = Object.entries(helpers)
				.map(([name, count]) => `${name} ${count}`)
				.join(", ");
			lines.push(
				`| ${record.instance_id} | ${cells} | ${errorCells} | ${used} | ${frameCalls} | ${childSessions} | ${loki.beforeWriteChecks} / ${loki.blocked} / ${loki.afterCellFindings} |`,
			);
		}
	}
	lines.push("");
	return lines.join("\n");
}
