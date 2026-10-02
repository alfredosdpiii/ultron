import assert from "node:assert/strict";
import { test } from "node:test";
import {
	agentScript,
	armSpec,
	buildPrompt,
	cleanPatch,
	codexConfigToml,
	codexStats,
	DEFAULT_SEED,
	evalVerdict,
	headToHead,
	isStreamingNoise,
	ledgerStats,
	modelsJson,
	networkLookups,
	notionalCost,
	piStats,
	prediction,
	priceTable,
	renderMarkdown,
	repoCounts,
	runStatus,
	sample,
	sampleOrder,
	scrubSecret,
	summarize,
	ultronStats,
} from "../evals/swebench/lib.mjs";
import { requestFacts, responseFacts, splitRunPath } from "../evals/swebench/recorder.mjs";

/** A dataset shaped like Verified: one dominant repository and a tail of small ones. */
function dataset() {
	const sizes = { "django/django": 231, "sympy/sympy": 75, "sphinx-doc/sphinx": 44, "astropy/astropy": 22, "psf/requests": 8, "pallets/flask": 1 };
	const rows = [];
	for (const [repo, count] of Object.entries(sizes))
		for (let index = 0; index < count; index++)
			rows.push({ instance_id: `${repo.replace("/", "__")}-${index}`, repo, problem_statement: `issue ${index}` });
	return rows;
}

test("sampleOrder: deterministic, a permutation, and independent of input order", () => {
	const rows = dataset();
	const order = sampleOrder(rows, "seed-a");
	assert.equal(order.length, rows.length);
	assert.equal(new Set(order).size, rows.length);
	assert.deepEqual(sampleOrder([...rows].reverse(), "seed-a"), order);
	assert.notDeepEqual(sampleOrder(rows, "seed-b"), order);
	assert.deepEqual(sampleOrder(rows), sampleOrder(rows, DEFAULT_SEED));
});

test("sample: a larger sample extends a smaller one", () => {
	const rows = dataset();
	const ten = sample(rows, 10, "seed-a").map((row) => row.instance_id);
	const fifty = sample(rows, 50, "seed-a").map((row) => row.instance_id);
	assert.deepEqual(fifty.slice(0, 10), ten);
});

test("sample: every prefix holds each repository's share to within one instance", () => {
	const rows = dataset();
	const total = repoCounts(rows);
	for (const seed of ["seed-a", "seed-b", DEFAULT_SEED]) {
		for (const n of [10, 50, 137]) {
			const counts = repoCounts(sample(rows, n, seed));
			for (const [repo, size] of Object.entries(total)) {
				const share = (n * size) / rows.length;
				assert.ok(Math.abs((counts[repo] ?? 0) - share) <= 1.5, `${seed} n=${n} ${repo}: ${counts[repo] ?? 0} vs ${share.toFixed(2)}`);
			}
		}
	}
});

test("buildPrompt: the issue text and nothing that gives the answer away", () => {
	const prompt = buildPrompt({
		instance_id: "django__django-1",
		repo: "django/django",
		problem_statement: "  Widget crashes on overflow.\n",
		patch: "GOLD-PATCH",
		test_patch: "TEST-PATCH",
		hints_text: "HINT",
		FAIL_TO_PASS: ["test_secret_name"],
	});
	assert.match(prompt, /django\/django repository at \/testbed/);
	assert.match(prompt, /<issue>\nWidget crashes on overflow\.\n<\/issue>/);
	for (const leak of ["GOLD-PATCH", "TEST-PATCH", "HINT", "test_secret_name"]) assert.ok(!prompt.includes(leak));
});

const USER_MODELS = {
	providers: {
		cliproxyapi: {
			baseUrl: "http://127.0.0.1:8317/v1",
			apiKey: "sk-secret-key",
			api: "openai-completions",
			compat: { supportsReasoningEffort: true },
			models: [
				{ id: "other-model", cost: { input: 9, output: 9, cacheRead: 9 } },
				{ id: "gpt-6.1-sol", reasoning: true, cost: { input: 2, output: 10, cacheRead: 0.1 } },
			],
		},
		another: { apiKey: "other-secret" },
	},
};

test("modelsJson: one provider, one model, the key only as an environment reference", () => {
	const config = modelsJson(USER_MODELS, "http://127.0.0.1:9000/run/x/v1");
	assert.deepEqual(Object.keys(config.providers), ["cliproxyapi"]);
	const provider = config.providers.cliproxyapi;
	assert.equal(provider.baseUrl, "http://127.0.0.1:9000/run/x/v1");
	assert.equal(provider.apiKey, "$CLIPROXY_API_KEY");
	assert.deepEqual(provider.models.map((model) => model.id), ["gpt-6.1-sol"]);
	assert.equal(provider.api, "openai-completions");
	assert.ok(!JSON.stringify(config).includes("secret"));
	assert.throws(() => modelsJson({ providers: {} }, "http://x"), /no "cliproxyapi" provider/);
	assert.deepEqual(priceTable(USER_MODELS), { input: 2, output: 10, cacheRead: 0.1 });
});

test("codexConfigToml: a custom provider on the proxy, key from the environment", () => {
	const toml = codexConfigToml("http://127.0.0.1:9000/run/x/v1");
	assert.match(toml, /^model = "gpt-6\.1-sol"$/m);
	assert.match(toml, /^model_provider = "cliproxyapi"$/m);
	assert.match(toml, /^model_reasoning_effort = "medium"$/m);
	assert.match(toml, /^\[model_providers\.cliproxyapi\]$/m);
	assert.match(toml, /^base_url = "http:\/\/127\.0\.0\.1:9000\/run\/x\/v1"$/m);
	assert.match(toml, /^env_key = "CLIPROXY_API_KEY"$/m);
	assert.match(toml, /^wire_api = "responses"$/m);
});

test("agentScript: testbed environment, private dirs, wall-clock limit, no key", () => {
	const ultron = agentScript("ultron", { limitSeconds: 1800 });
	assert.match(ultron, /conda activate testbed/);
	assert.match(ultron, /export ULTRON_CODING_AGENT_DIR='\/agent\/ultron-agent'/);
	assert.match(ultron, /export ULTRON_SERVER_DIR='\/agent\/ultron-server'/);
	assert.match(ultron, /export ULTRON_PYTHON='\/opt\/agent\/python\/bin\/python3'/);
	assert.match(ultron, /export ULTRON_LOKI_AUTOINIT='off'/);
	assert.match(ultron, /export ULTRON_HINDSIGHT_URL='off'/);
	assert.match(ultron, /exec timeout --signal=TERM --kill-after=30 1800s .*cli\.js' '--mode' 'json' '-p' '--model' 'cliproxyapi\/gpt-6\.1-sol' '--' "\$PROMPT" < \/dev\/null/);
	// The default thinking level is left alone for Ultron and Pi.
	assert.ok(!ultron.includes("--thinking") && !agentScript("pi", { limitSeconds: 60 }).includes("--thinking"));
	const codex = agentScript("codex", { limitSeconds: 60 });
	assert.match(codex, /export CODEX_HOME='\/agent\/codex-home'/);
	assert.match(codex, /'exec' '--json' '--sandbox' 'danger-full-access' '--cd' '\/testbed' "\$PROMPT"/);
	assert.match(agentScript("pi", { limitSeconds: 60 }), /export PI_CODING_AGENT_DIR='\/agent\/pi-agent'/);
	for (const arm of ["ultron", "codex", "pi"]) assert.ok(!/API_KEY=/.test(agentScript(arm, { limitSeconds: 60 })));
	assert.throws(() => armSpec("claude"), /unknown arm/);
});

const DIFF = [
	"diff --git a/django/forms/widgets.py b/django/forms/widgets.py",
	"index 1111111..2222222 100644",
	"--- a/django/forms/widgets.py",
	"+++ b/django/forms/widgets.py",
	"@@ -1,3 +1,3 @@",
	" a",
	"-b",
	"+c",
	" d",
	"diff --git a/.loki/policy.toml b/.loki/policy.toml",
	"new file mode 100644",
	"index 0000000..3333333",
	"--- /dev/null",
	"+++ b/.loki/policy.toml",
	"@@ -0,0 +1 @@",
	"+mode = 1",
	"diff --git a/logo.png b/logo.png",
	"new file mode 100644",
	"index 0000000..4444444",
	"Binary files /dev/null and b/logo.png differ",
	"diff --git a/repro.py b/repro.py",
	"new file mode 100644",
	"index 0000000..5555555",
	"--- /dev/null",
	"+++ b/repro.py",
	"@@ -0,0 +1 @@",
	"+print(1)",
	"",
].join("\n");

test("cleanPatch: drops .loki/ and binary sections, keeps the rest byte for byte", () => {
	const { patch, files, dropped } = cleanPatch(DIFF);
	assert.deepEqual(files, ["django/forms/widgets.py", "repro.py"]);
	assert.deepEqual(dropped, [
		{ path: ".loki/policy.toml", reason: "excluded (.loki/)" },
		{ path: "logo.png", reason: "binary" },
	]);
	assert.ok(patch.startsWith("diff --git a/django/forms/widgets.py"));
	assert.ok(patch.endsWith("+print(1)\n"));
	assert.ok(!patch.includes(".loki") && !patch.includes("logo.png"));
	assert.equal(patch.split("\n").filter((line) => line.startsWith("diff --git")).length, 2);
});

test("cleanPatch: an untouched diff round-trips, an empty one stays empty", () => {
	const plain = DIFF.split("\n").slice(0, 9).join("\n");
	assert.equal(cleanPatch(`${plain}\n`).patch, `${plain}\n`);
	assert.deepEqual(cleanPatch(""), { patch: "", files: [], dropped: [] });
	assert.deepEqual(cleanPatch("\n"), { patch: "", files: [], dropped: [] });
	// A file merely named like the excluded directory is kept.
	assert.deepEqual(cleanPatch(DIFF.replaceAll(".loki/policy.toml", ".lokirc")).files, ["django/forms/widgets.py", ".lokirc", "repro.py"]);
});

test("prediction: the line the official evaluation reads", () => {
	assert.deepEqual(prediction({ instance_id: "a__b-1", arm: "ultron" }, "PATCH"), {
		instance_id: "a__b-1",
		model_name_or_path: "ultron",
		model_patch: "PATCH",
	});
});

test("scrubSecret: removes every occurrence; ignores a secret too short to search for", () => {
	assert.equal(scrubSecret("a sk-12345678 b sk-12345678", "sk-12345678"), "a <redacted> b <redacted>");
	assert.equal(scrubSecret("abc", "b"), "abc");
	assert.equal(scrubSecret("abc", ""), "abc");
});

const usage = (input, output, cacheRead) => ({ input, output, cacheRead, cacheWrite: 0, reasoning: 0, cost: { total: 0.01 } });

function ultronSession() {
	const assistant = (id, code, stopReason = "toolUse") => ({
		kind: "entry",
		id,
		type: "message",
		message: {
			role: "assistant",
			content: code ? [{ type: "toolCall", name: "rlm", arguments: { code } }] : [{ type: "text", text: "Fixed." }],
			stopReason,
		},
	});
	return [
		JSON.stringify({ v: 4, kind: "header", id: "root-1" }),
		JSON.stringify([
			assistant("e1", "out = await bash('git fetch origin')\nprint(await read('a.py'))"),
			{ kind: "usage", id: "u1", usage: usage(100, 10, 0), entryId: "e1" },
		]),
		JSON.stringify({ kind: "entry", id: "t1", type: "message", message: { role: "toolResult", isError: true } }),
		JSON.stringify([
			assistant("e2", "h = await rlm.spawn('look')\nr = await rlm.infer('x')\nawait edit('a.py', 'a', 'b')"),
			{ kind: "usage", id: "u2", usage: usage(20, 5, 100), entryId: "e2" },
		]),
		// The journal may repeat a record; it counts once.
		JSON.stringify([assistant("e2", "ignored"), { kind: "usage", id: "u2", usage: usage(20, 5, 100) }]),
		JSON.stringify([assistant("e3", null, "stop"), { kind: "usage", id: "u3", usage: usage(30, 7, 120) }]),
		'{"kind":"entry","id":"cut-off',
	].join("\n");
}

test("ultronStats: cells, helpers, frames and tokens from the session journal", () => {
	const child = JSON.stringify([
		{ kind: "entry", id: "c1", type: "message", message: { role: "assistant", content: [{ type: "toolCall", name: "rlm", arguments: { code: "await bash('ls')" } }] } },
		{ kind: "usage", id: "cu1", usage: usage(50, 5, 0) },
	]);
	const stats = ultronStats(
		[
			{ name: "sessions/x_root-1.jsonl", text: ultronSession() },
			{ name: "sessions/y_child-9.jsonl", text: child },
		],
		"root-1",
	);
	assert.equal(stats.turns, 4);
	assert.equal(stats.cells, 2);
	assert.equal(stats.toolCalls, 3);
	assert.deepEqual(stats.toolsByName, { rlm: 3 });
	assert.equal(stats.errorCells, 1);
	assert.equal(stats.frameCalls, 2);
	assert.equal(stats.spawnCalls, 1);
	assert.deepEqual(stats.helpers, { bash: 1, read: 1, "rlm.spawn": 1, "rlm.infer": 1, edit: 1 });
	assert.equal(stats.childSessions, 1);
	assert.equal(stats.childTurns, 1);
	assert.deepEqual(stats.tokens, { input: 200, cacheRead: 220, output: 27, reasoning: 0, total: 447 });
	assert.equal(stats.lastStopReason, "stop");
	assert.equal(stats.finalText, "Fixed.");
	assert.equal(stats.commands.length, 3);
	assert.deepEqual(networkLookups(stats.commands), ["out = await bash('git fetch origin') print(await read('a.py'))"]);
});

test("piStats: turns, tools and tokens from the json event stream", () => {
	const events = [
		{ type: "session", id: "s" },
		{ type: "message_end", message: { role: "user", content: [{ type: "text", text: "hi" }] } },
		{ type: "message_end", message: { role: "assistant", content: [{ type: "toolCall", name: "bash" }], usage: usage(1000, 20, 0), stopReason: "toolUse" } },
		{ type: "tool_execution_start", toolName: "bash", args: { command: "python -m pytest tests" } },
		{ type: "tool_execution_start", toolName: "edit", args: { path: "a.py" } },
		{ type: "message_end", message: { role: "assistant", content: [{ type: "text", text: "Done." }], usage: usage(50, 10, 1000), stopReason: "stop" } },
	];
	const stats = piStats(events.map((event) => JSON.stringify(event)).join("\n"));
	assert.equal(stats.turns, 2);
	assert.equal(stats.toolCalls, 2);
	assert.deepEqual(stats.toolsByName, { bash: 1, edit: 1 });
	assert.deepEqual(stats.tokens, { input: 1050, cacheRead: 1000, output: 30, reasoning: 0, total: 2080 });
	assert.equal(stats.reportedCostUsd, 0.02);
	assert.equal(stats.finalText, "Done.");
	assert.equal(stats.errorMessage, null);
	assert.deepEqual(stats.commands, ["python -m pytest tests"]);
	const failed = piStats(JSON.stringify({ type: "message_end", message: { role: "assistant", content: [], stopReason: "error", errorMessage: "429 rate limited" } }));
	assert.equal(failed.errorMessage, "429 rate limited");
	assert.ok(isStreamingNoise('{"type":"message_update","x":1}'));
	assert.ok(!isStreamingNoise('{"type":"message_end","x":1}'));
});

test("codexStats: cached tokens split out of input, turns from the rollout", () => {
	const stdout = [
		{ type: "thread.started", thread_id: "t" },
		{ type: "item.completed", item: { type: "error", message: "Model metadata for `gpt-6.1-sol` not found." } },
		{ type: "item.completed", item: { type: "command_execution", command: "/bin/bash -lc 'curl https://github.com/x/y'", exit_code: 0 } },
		{ type: "item.completed", item: { type: "file_change", changes: [] } },
		{ type: "item.completed", item: { type: "agent_message", text: "Patched." } },
		{ type: "turn.completed", usage: { input_tokens: 24000, cached_input_tokens: 15000, output_tokens: 150, reasoning_output_tokens: 30 } },
	];
	const rollout = [
		{ type: "event_msg", payload: { type: "token_count", info: { total_token_usage: {} } } },
		{ type: "event_msg", payload: { type: "token_count", info: { total_token_usage: {} } } },
		{ type: "event_msg", payload: { type: "token_count", info: null } },
		{ type: "response_item", payload: { type: "function_call" } },
	];
	const stats = codexStats(stdout.map((event) => JSON.stringify(event)).join("\n"), rollout.map((event) => JSON.stringify(event)).join("\n"));
	assert.equal(stats.turns, 2);
	assert.equal(stats.toolCalls, 2);
	assert.deepEqual(stats.toolsByName, { command_execution: 1, file_change: 1 });
	assert.deepEqual(stats.tokens, { input: 9000, cacheRead: 15000, output: 150, reasoning: 30, total: 24150 });
	assert.equal(stats.completed, true);
	assert.equal(stats.errorMessage, null);
	assert.equal(stats.finalText, "Patched.");
	assert.equal(stats.warnings.length, 1);
	assert.equal(networkLookups(stats.commands).length, 1);
	const failed = codexStats(JSON.stringify({ type: "turn.failed", error: { message: "stream disconnected" } }));
	assert.equal(failed.completed, false);
	assert.equal(failed.errorMessage, "stream disconnected");
});

test("networkLookups: fetches and downloads, not local installs or test runs", () => {
	const hits = networkLookups([
		"git fetch origin main",
		"curl -s https://api.github.com/repos/django/django/pulls/1",
		"pip install requests",
		"open https://github.com/django/django/pull/123/files",
		"pip install -e .",
		"python -m pytest tests/test_x.py",
		"git diff HEAD",
		"python -m pip install .",
	]);
	assert.equal(hits.length, 4);
});

test("notionalCost: list prices per million tokens", () => {
	const cost = notionalCost({ input: 1_000_000, cacheRead: 2_000_000, output: 100_000 }, { input: 2, output: 10, cacheRead: 0.1 });
	assert.equal(Number(cost.toFixed(6)), 3.2);
});

test("recorder: run paths, request facts and usage from both OpenAI wire formats", () => {
	assert.deepEqual(splitRunPath("/run/ultron.django__django-1.2/v1/chat/completions"), { runId: "ultron.django__django-1.2", path: "/v1/chat/completions" });
	assert.deepEqual(splitRunPath("/v1/models"), { runId: null, path: "/v1/models" });
	assert.deepEqual(requestFacts(JSON.stringify({ model: "m", reasoning_effort: "medium", stream: true, tools: [1], messages: [1, 2] })), {
		model: "m",
		reasoningEffort: "medium",
		stream: true,
		tools: 1,
		items: 2,
	});
	assert.equal(requestFacts(JSON.stringify({ model: "m", reasoning: { effort: "high" }, input: [1] })).reasoningEffort, "high");
	assert.deepEqual(requestFacts("not json"), {});
	const chat = [
		'data: {"id":"1","model":"gpt-6.1-sol","choices":[{"delta":{"content":"hi"}}]}',
		"",
		'data: {"id":"1","model":"gpt-6.1-sol","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":64},"completion_tokens_details":{"reasoning_tokens":8}}}',
		"",
		"data: [DONE]",
		"",
	].join("\n");
	assert.deepEqual(responseFacts(chat), { model: "gpt-6.1-sol", usage: { input: 100, cachedInput: 64, output: 20, reasoning: 8 } });
	const responses = [
		"event: response.created",
		'data: {"type":"response.created","response":{"model":"gpt-6.1-sol","usage":null}}',
		"",
		"event: response.completed",
		'data: {"type":"response.completed","response":{"model":"gpt-6.1-sol","usage":{"input_tokens":12000,"input_tokens_details":{"cached_tokens":11000},"output_tokens":40,"output_tokens_details":{"reasoning_tokens":0}}}}',
		"",
	].join("\n");
	assert.deepEqual(responseFacts(responses), { model: "gpt-6.1-sol", usage: { input: 12000, cachedInput: 11000, output: 40, reasoning: 0 } });
	assert.deepEqual(responseFacts('{"error":{"message":"rate limited"}}'), { model: null, usage: null });
});

test("ledgerStats: outcomes, models and tokens counted alike for every arm", () => {
	const lines = [
		{ method: "POST", path: "/v1/responses", status: 200, request: { model: "gpt-6.1-sol", reasoningEffort: "medium" }, responseModel: "gpt-6.1-sol", usage: { input: 100, cachedInput: 60, output: 10, reasoning: 2 } },
		{ method: "POST", path: "/v1/responses", status: 429, request: { model: "gpt-6.1-sol", reasoningEffort: "medium" } },
		{ method: "POST", path: "/v1/responses", status: 502, request: { model: "gpt-6.1-sol", reasoningEffort: "medium" } },
		{ method: "POST", path: "/v1/responses", status: 200, request: { model: "gpt-6.1-sol", reasoningEffort: "medium" }, responseModel: "gpt-6.1-sol", usage: null },
		{ method: "GET", path: "/v1/models", status: 200 },
	];
	const stats = ledgerStats(lines.map((line) => JSON.stringify(line)).join("\n"));
	assert.equal(stats.requests, 4);
	assert.equal(stats.ok, 2);
	assert.equal(stats.rateLimited, 1);
	assert.equal(stats.failed, 1);
	assert.equal(stats.missingUsage, 1);
	assert.deepEqual(stats.requestModels, ["gpt-6.1-sol"]);
	assert.deepEqual(stats.responseModels, ["gpt-6.1-sol"]);
	assert.deepEqual(stats.efforts, ["medium"]);
	assert.deepEqual(stats.tokens, { input: 40, cacheRead: 60, output: 10, reasoning: 2, total: 110 });
});

test("runStatus: timeouts, provider errors and crashes are told apart from a finished run", () => {
	const quiet = { rateLimited: 0, failed: 0, ok: 5 };
	assert.equal(runStatus({ exitCode: 0, timedOut: false, turns: 5, errorMessage: null, ledger: quiet }), "completed");
	assert.equal(runStatus({ exitCode: 124, timedOut: false, turns: 50, errorMessage: null, ledger: quiet }), "timeout");
	assert.equal(runStatus({ exitCode: 1, timedOut: true, turns: 50, errorMessage: "x", ledger: quiet }), "timeout");
	assert.equal(runStatus({ exitCode: 0, timedOut: false, turns: 3, errorMessage: "429 Too Many Requests", ledger: quiet }), "provider_error");
	assert.equal(runStatus({ exitCode: 0, timedOut: false, turns: 3, errorMessage: "bad", ledger: { rateLimited: 2, failed: 0, ok: 3 } }), "provider_error");
	assert.equal(runStatus({ exitCode: 1, timedOut: false, turns: 0, errorMessage: null, ledger: { rateLimited: 0, failed: 4, ok: 0 } }), "provider_error");
	assert.equal(runStatus({ exitCode: 1, timedOut: false, turns: 3, errorMessage: null, ledger: quiet }), "agent_crash");
	assert.equal(runStatus({ exitCode: 0, timedOut: false, turns: 0, errorMessage: null, ledger: quiet }), "agent_crash");
});

test("evalVerdict: the official report's verdict per instance", () => {
	const report = { resolved_ids: ["a"], unresolved_ids: ["b"], empty_patch_ids: ["c"], error_ids: ["d"], incomplete_ids: ["e"] };
	assert.deepEqual(["a", "b", "c", "d", "e", "f"].map((id) => evalVerdict(report, id)), ["resolved", "unresolved", "empty_patch", "error", "error", "not_evaluated"]);
	assert.equal(evalVerdict(null, "a"), "not_evaluated");
});

function records() {
	const tokens = { input: 1000, cacheRead: 5000, output: 200, reasoning: 0, total: 6200 };
	const base = { wallMs: 60_000, turns: 10, toolCalls: 9, tokens, notionalCostUsd: 0.5, ledger: { rateLimited: 0, tokens } };
	return [
		{ ...base, instance_id: "t1", arm: "ultron", status: "completed", verdict: "resolved" },
		{ ...base, instance_id: "t2", arm: "ultron", status: "timeout", verdict: "unresolved", wallMs: 1_800_000 },
		{ ...base, instance_id: "t3", arm: "ultron", status: "provider_error", verdict: "empty_patch", ledger: { rateLimited: 3, tokens } },
		{ ...base, instance_id: "t4", arm: "ultron", status: "completed", verdict: "error" },
		{ ...base, instance_id: "t1", arm: "codex", status: "completed", verdict: "resolved" },
		{ ...base, instance_id: "t2", arm: "codex", status: "completed", verdict: "resolved" },
		{ ...base, instance_id: "t3", arm: "codex", status: "completed", verdict: "empty_patch" },
		{ ...base, instance_id: "t4", arm: "codex", status: "completed", verdict: "unresolved" },
	];
}

test("summarize: harness failures and evaluation errors are not counted as unresolved", () => {
	const summary = summarize(records(), ["ultron", "codex", "pi"]);
	assert.deepEqual(Object.keys(summary), ["ultron", "codex"]);
	const ultron = summary.ultron;
	assert.equal(ultron.tasks, 4);
	assert.equal(ultron.resolved, 1);
	assert.equal(ultron.unresolved, 1);
	assert.equal(ultron.timeouts, 1);
	assert.deepEqual(ultron.harnessFailures, [{ instance_id: "t3", status: "provider_error" }]);
	assert.deepEqual(ultron.evalErrors, ["t4"]);
	assert.equal(ultron.resolvedRate, 0.25);
	assert.equal(ultron.resolvedRateOfScored, 0.5);
	assert.equal(ultron.wallSeconds, 1980);
	assert.equal(ultron.turns, 40);
	assert.equal(ultron.tokens.total, 24800);
	assert.equal(ultron.notionalCostUsd, 2);
	assert.equal(ultron.rateLimited, 3);
	assert.equal(summary.codex.resolved, 2);
	assert.equal(summary.codex.unresolved, 2);
	assert.equal(summary.codex.emptyPatches, 1);
	assert.deepEqual(summary.codex.harnessFailures, []);
});

test("headToHead and renderMarkdown", () => {
	assert.deepEqual(headToHead(records(), "ultron", "codex"), { both: 1, onlyA: [], onlyB: ["t2"], neither: 2 });
	const markdown = renderMarkdown({
		runId: "pilot10",
		date: "2026-10-02",
		model: "cliproxyapi/gpt-6.1-sol",
		swebenchVersion: "5.0.2",
		sample: { n: 4, seed: "s" },
		limits: { wallClockMinutes: 30, concurrency: 2 },
		summary: summarize(records(), ["ultron", "codex"]),
		headToHead: { "ultron vs codex": headToHead(records(), "ultron", "codex") },
		harnessFailures: [{ instance_id: "t3", arm: "ultron", status: "provider_error", verdict: "empty_patch", error: "429" }],
		verification: { ultron: { ok: true, responseModels: ["gpt-6.1-sol"], paths: ["/v1/chat/completions"], reasoningEfforts: ["medium"], requests: 2 } },
		goldCheck: { resolved: 4, of: 4 },
		records: [
			...records(),
			{
				instance_id: "t5",
				arm: "pi",
				ultron: { cells: 5, errorCells: 1, helpers: { bash: 4, edit: 1 }, frameCalls: 0, childSessions: 0, loki: { beforeWriteChecks: 1, blocked: 0, afterCellFindings: 1 } },
			},
		],
	});
	assert.match(markdown, /- only codex \(not ultron\): t2/);
	assert.match(markdown, /- ultron t3: provider_error, verdict empty_patch \(429\)/);
	assert.match(markdown, /- ultron: verified on gpt-6\.1-sol via \/v1\/chat\/completions, reasoning effort medium/);
	assert.match(markdown, /gold patches of the same tasks: 4\/4 resolved/);
	assert.match(markdown, /\| t5 \| 5 \| 1 \| bash 4, edit 1 \| 0 \| 0 \| 1 \/ 0 \/ 1 \|/);
	assert.match(markdown, /\| ultron \| 1\/4 \| 1 \| 1 \| 1 \| 1 \| 33m00s \|/);
	assert.match(markdown, /\| t2 \| no \[timeout\], 30m00s, 10, 6,200 \| yes, 1m00s, 10, 6,200 \|/);
	assert.match(markdown, /Cost is notional/);
});
