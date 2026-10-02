import assert from "node:assert/strict";
import { test } from "node:test";
import {
	ARM_RUNTIMES,
	ARM_STOPPERS,
	ARMS,
	agentScript,
	armAuth,
	armDescriptions,
	armModel,
	armSpec,
	buildPrompt,
	CLAUDE_ARMS,
	claudeAuthPlan,
	claudeCheck,
	claudeRateLimit,
	claudeSecrets,
	claudeStats,
	claudeTranscriptStats,
	cleanPatch,
	codexConfigToml,
	codexStats,
	DEFAULT_SEED,
	evalVerdict,
	fullestWindow,
	HARNESS_FAILURES,
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
	RERUN_STATUSES,
	renderMarkdown,
	repoCounts,
	runModel,
	runStatus,
	sample,
	sampleOrder,
	scrubSecret,
	subscriptionUse,
	summarize,
	ultronStats,
	ultronUsageSummary,
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
	assert.throws(() => armSpec("gemini"), /unknown arm "gemini" \(known: ultron, codex, pi, claude, ultron-claude\)/);
});

test("agentScript: plain Claude Code, vanilla and without a permission bypass", () => {
	const claude = agentScript("claude", { limitSeconds: 1800 });
	assert.match(claude, /conda activate testbed/);
	assert.match(claude, /export CLAUDE_CONFIG_DIR='\/agent\/claude-config'/);
	assert.match(claude, /export DISABLE_AUTOUPDATER='1'/);
	assert.match(
		claude,
		/exec timeout --signal=TERM --kill-after=30 1800s '\/opt\/agent\/claude\/claude' '-p' '--output-format' 'stream-json' '--verbose' '--model' 'claude-opus-5-5' '--strict-mcp-config' '--setting-sources' '' '--allowedTools' 'Bash,Edit,Write,NotebookEdit' '--' "\$PROMPT" < \/dev\/null/,
	);
	// The default system prompt, tools, permission mode and effort are left alone; nothing bypasses permissions.
	for (const flag of ["--dangerously-skip-permissions", "bypassPermissions", "--permission-mode", "--system-prompt", "--tools", "--effort", "--mcp-config", "IS_SANDBOX"])
		assert.ok(!claude.includes(flag), flag);
	// No credential is ever part of the script: the login is a read-only mount or an environment variable of docker exec.
	assert.ok(!/TOKEN|API_KEY|credentials/i.test(claude));
	const spec = armSpec("claude");
	assert.deepEqual(spec.dirs, ["/agent/claude-config"]);
	assert.deepEqual(spec.configFiles({}), {});
	// Only the session transcripts are copied out, never the config dir (it holds the login).
	assert.deepEqual(spec.keep, [["/agent/claude-config/projects", "claude-projects"]]);
	assert.deepEqual(ARM_RUNTIMES.claude, ["claude"]);
});

test("agentScript: ultron --claude, a clean Ultron on the same Claude Code and config dir", () => {
	const script = agentScript("ultron-claude", { limitSeconds: 1800 });
	for (const line of [
		"export ULTRON_CODING_AGENT_DIR='/agent/ultron-agent'",
		"export ULTRON_SERVER_DIR='/agent/ultron-server'",
		"export ULTRON_PYTHON='/opt/agent/python/bin/python3'",
		"export ULTRON_LOKI_PYTHON='/opt/agent/python/bin/python3'",
		"export ULTRON_LOKI_AUTOINIT='off'",
		"export ULTRON_HINDSIGHT_URL='off'",
		"export ULTRON_CLAUDE_CODE_BIN='/opt/agent/claude/claude'",
		"export CLAUDE_CONFIG_DIR='/agent/claude-config'",
		"export DISABLE_AUTOUPDATER='1'",
	])
		assert.ok(script.includes(line), line);
	assert.match(script, /1800s .*cli\.js' '--claude' '--mode' 'json' '-p' '--model' 'claude-code\/claude-opus-5-5' '--' "\$PROMPT" < \/dev\/null/);
	// Ultron's default thinking level is left alone, as in the proxy arm.
	assert.ok(!script.includes("--thinking") && !/TOKEN|API_KEY|credentials/i.test(script));
	const spec = armSpec("ultron-claude");
	assert.deepEqual(spec.dirs, ["/agent/ultron-agent", "/agent/ultron-server", "/agent/claude-config"]);
	assert.deepEqual(spec.configFiles({ modelsJson: "{}" }), {});
	assert.deepEqual(
		spec.keep.map(([, to]) => to),
		["sessions", "traces", "loki.jsonl", "claude-projects"],
	);
	assert.deepEqual(spec.usageReport("s-1").slice(-3), ["usage", "s-1", "--json"]);
	assert.deepEqual(spec.usageReport(null).slice(-2), ["usage", "--json"]);
	assert.deepEqual(ARM_RUNTIMES["ultron-claude"], ["node", "ultron", "python", "claude"]);
});

test("arms: the Claude arms are opt-in, on their own login and model", () => {
	assert.deepEqual(ARMS, ["ultron", "codex", "pi"]);
	assert.deepEqual(CLAUDE_ARMS, ["claude", "ultron-claude"]);
	assert.deepEqual([...ARMS, ...CLAUDE_ARMS].map(armAuth), ["proxy", "proxy", "proxy", "claude", "claude"]);
	assert.equal(armModel("pi"), "cliproxyapi/gpt-6.1-sol");
	assert.equal(armModel("claude"), "claude-code/claude-opus-5-5");
	assert.equal(runModel(CLAUDE_ARMS), "claude-code/claude-opus-5-5");
	assert.equal(runModel(["ultron", "claude"]), "cliproxyapi/gpt-6.1-sol + claude-code/claude-opus-5-5");
	const described = armDescriptions({ claude: "2.1.284 (Claude Code)", ultron: "0.87.22" });
	assert.equal(described.claude.version, "2.1.284 (Claude Code)");
	assert.equal(described["ultron-claude"].claudeVersion, "2.1.284 (Claude Code)");
	assert.match(described.claude.command, /^claude -p .*--allowedTools Bash,Edit,Write,NotebookEdit$/);
	assert.equal(described["ultron-claude"].command, "ultron --claude --mode json -p --model claude-code/claude-opus-5-5");
});

const LOGIN = (expiresInSeconds) => ({
	claudeAiOauth: { accessToken: "at-secret-0123456789", refreshToken: "rt-secret-0123456789", expiresAt: 1_000_000_000 + expiresInSeconds * 1000 },
});

test("claudeAuthPlan: the environment token first, else the mounted login while its token outlives the run", () => {
	const nowMs = 1_000_000_000;
	assert.deepEqual(claudeAuthPlan({ envToken: "tok-0123456789", credentials: null, nowMs, limitSeconds: 1800 }), { ok: true, mode: "env" });
	assert.deepEqual(claudeAuthPlan({ envToken: null, credentials: LOGIN(4 * 3600), nowMs, limitSeconds: 1800 }), { ok: true, mode: "mount", secondsLeft: 14400 });
	// Exactly the limit plus the 15-minute margin is enough; a second less is not.
	assert.equal(claudeAuthPlan({ envToken: null, credentials: LOGIN(1800 + 900), nowMs, limitSeconds: 1800 }).ok, true);
	const short = claudeAuthPlan({ envToken: null, credentials: LOGIN(1800 + 899), nowMs, limitSeconds: 1800 });
	assert.equal(short.ok, false);
	assert.match(short.reason, /expires in 44 min, less than the run's limit plus margin \(45 min\).*never from a container/);
	const expired = claudeAuthPlan({ envToken: null, credentials: LOGIN(-5), nowMs, limitSeconds: 300 });
	assert.equal(expired.ok, false);
	assert.match(expired.reason, /has expired/);
	for (const credentials of [null, {}, { claudeAiOauth: {} }])
		assert.match(claudeAuthPlan({ envToken: null, credentials, nowMs, limitSeconds: 300 }).reason, /no claude\.ai login/);
	// A plan never carries a secret.
	for (const seconds of [-5, 100, 99999])
		assert.ok(!JSON.stringify(claudeAuthPlan({ envToken: null, credentials: LOGIN(seconds), nowMs, limitSeconds: 300 })).includes("secret"));
	assert.deepEqual(claudeSecrets({ envToken: null, credentials: LOGIN(1) }), ["at-secret-0123456789", "rt-secret-0123456789"]);
	assert.deepEqual(claudeSecrets({ envToken: "tok-0123456789", credentials: null }), ["tok-0123456789"]);
	assert.deepEqual(claudeSecrets({ envToken: null, credentials: null }), []);
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
	assert.deepEqual(stats.tokens, { input: 200, cacheRead: 220, cacheWrite: 0, output: 27, reasoning: 0, total: 447 });
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
	assert.deepEqual(stats.tokens, { input: 1050, cacheRead: 1000, cacheWrite: 0, output: 30, reasoning: 0, total: 2080 });
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
	assert.deepEqual(stats.tokens, { input: 9000, cacheRead: 15000, cacheWrite: 0, output: 150, reasoning: 30, total: 24150 });
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
	assert.deepEqual(stats.tokens, { input: 40, cacheRead: 60, cacheWrite: 0, output: 10, reasoning: 2, total: 110 });
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
	assert.match(markdown, /\| t2 \| no \[timeout\], 30m00s, 10, 6,200, \$0\.50 \| yes, 1m00s, 10, 6,200, \$0\.50 \|/);
	assert.match(markdown, /tokens \(in \/ cached \/ out\) \| notional cost \|/);
	assert.match(markdown, /Cost is notional/);
});

// ---------------------------------------------------------------------------------------------------------------
// Claude arms: the shapes are those of Claude Code 2.1.284's `-p --output-format stream-json --verbose` output and
// of its session transcripts.

const WINDOWS = (fiveHour, sevenDay) => ({ five_hour: { utilization: fiveHour, resetsAt: 1790929800 }, seven_day: { utilization: sevenDay, resetsAt: 1791302400 } });
const rateLimitEvent = (fiveHour, extra = {}) => ({
	type: "rate_limit_event",
	rate_limit_info: { status: "allowed", resetsAt: 1790929800, rateLimitType: "five_hour", overageStatus: "allowed", isUsingOverage: false, unifiedWindows: WINDOWS(fiveHour, 0.19), ...extra },
});
const streamUsage = (output) => ({ input_tokens: 2, cache_creation_input_tokens: 100, cache_read_input_tokens: 1000, output_tokens: output });
const streamAssistant = (id, content, extra = {}) => ({ type: "assistant", message: { id, model: "claude-opus-5-5", content, usage: streamUsage(16) }, parent_tool_use_id: null, ...extra });

function claudeStream({ result = true } = {}) {
	const events = [
		{ type: "system", subtype: "init", cwd: "/testbed", model: "claude-opus-5-5", permissionMode: "default", apiKeySource: "none", claude_code_version: "2.1.284", mcp_servers: [], tools: ["Bash", "Edit"] },
		// One message, one event per content block: a turn once, two tool calls.
		streamAssistant("msg_1", [{ type: "tool_use", id: "tu_1", name: "Bash", input: { command: "git fetch origin" } }]),
		streamAssistant("msg_1", [{ type: "tool_use", id: "tu_2", name: "Read", input: { file_path: "/testbed/a.py" } }]),
		rateLimitEvent(0.71),
		{ type: "user", message: { role: "user", content: [{ type: "tool_result", tool_use_id: "tu_1", content: "ok" }] } },
		streamAssistant("msg_2", [{ type: "tool_use", id: "tu_3", name: "Task", input: { prompt: "look" } }]),
		// A subagent's response and its tool call.
		streamAssistant("msg_3", [{ type: "tool_use", id: "tu_4", name: "Bash", input: { command: "ls" } }, { type: "text", text: "sub text" }], { parent_tool_use_id: "tu_3" }),
		{ type: "assistant", message: { id: "synthetic-1", model: "<synthetic>", content: [{ type: "text", text: "noise" }] }, parent_tool_use_id: null },
		streamAssistant("msg_4", [{ type: "tool_use", id: "tu_5", name: "WebFetch", input: { url: "https://github.com/django/django/pull/1" } }]),
		streamAssistant("msg_5", [{ type: "text", text: "Fixed the widget." }]),
		rateLimitEvent(0.73),
	];
	if (result)
		events.push({
			type: "result",
			subtype: "success",
			is_error: false,
			num_turns: 6,
			result: "Fixed the widget.",
			total_cost_usd: 0.4321,
			usage: { input_tokens: 8, output_tokens: 561 },
			modelUsage: {
				"claude-opus-5-5": { inputTokens: 8, outputTokens: 561, cacheReadInputTokens: 58224, cacheCreationInputTokens: 6217, thinkingTokens: 40, costUSD: 0.43 },
				"claude-haiku-5": { inputTokens: 100, outputTokens: 10, cacheReadInputTokens: 0, cacheCreationInputTokens: 0, thinkingTokens: 0, costUSD: 0.0021 },
			},
			permission_denials: [{ tool_name: "WebFetch", tool_use_id: "tu_5", tool_input: {} }],
			subagent_stats: { spawned: 1 },
		});
	return `${events.map((event) => JSON.stringify(event)).join("\n")}\n{"type":"assistant","message":{"id":"cut`;
}

test("claudeStats: turns, tools, totals and cost from the stream's result event", () => {
	const stats = claudeStats(claudeStream());
	assert.equal(stats.turns, 5);
	assert.equal(stats.subagentTurns, 1);
	assert.equal(stats.reportedTurns, 6);
	assert.equal(stats.toolCalls, 5);
	assert.deepEqual(stats.toolsByName, { Bash: 2, Read: 1, Task: 1, WebFetch: 1 });
	// The CLI's own totals over every model it billed, cache writes apart from uncached input.
	assert.equal(stats.hasTotals, true);
	assert.deepEqual(stats.tokens, { input: 108, cacheRead: 58224, cacheWrite: 6217, output: 571, reasoning: 40, total: 65120 });
	assert.equal(stats.reportedCostUsd, 0.4321);
	// The model of the session, of every response, and every model billed: a helper model shows here.
	assert.deepEqual(stats.models, ["claude-opus-5-5", "claude-haiku-5"]);
	assert.equal(stats.version, "2.1.284");
	assert.equal(stats.apiKeySource, "none");
	assert.equal(stats.permissionMode, "default");
	assert.deepEqual(stats.mcpServers, []);
	assert.deepEqual(stats.permissionDenials, ["WebFetch"]);
	assert.equal(stats.subagentsSpawned, 1);
	assert.equal(stats.completed, true);
	assert.equal(stats.errorMessage, null);
	assert.equal(stats.usageLimit, false);
	assert.equal(stats.finalText, "Fixed the widget.");
	assert.deepEqual(stats.rateLimits.first.windows, WINDOWS(0.71, 0.19));
	assert.equal(stats.rateLimits.last.windows.five_hour.utilization, 0.73);
	assert.deepEqual(networkLookups(stats.commands), ["git fetch origin", "WebFetch https://github.com/django/django/pull/1"]);
});

test("claudeStats: a run cut off before its result has no totals; errors and the usage limit are told apart", () => {
	const cut = claudeStats(claudeStream({ result: false }));
	assert.equal(cut.turns, 5);
	assert.equal(cut.hasTotals, false);
	assert.equal(cut.tokens.total, 0);
	assert.equal(cut.reportedCostUsd, undefined);
	assert.equal(cut.completed, false);
	assert.equal(cut.finalText, "Fixed the widget.");
	const result = (extra) => JSON.stringify({ type: "result", subtype: "success", is_error: true, num_turns: 1, total_cost_usd: 0.01, ...extra });
	const limit = claudeStats(result({ result: "You've hit your limit · resets 3pm", api_error_status: 429 }));
	assert.equal(limit.usageLimit, true);
	assert.equal(limit.completed, false);
	assert.match(limit.errorMessage, /^429 You've hit your limit/);
	const overloaded = claudeStats(result({ result: "API Error: Overloaded", api_error_status: 529 }));
	assert.equal(overloaded.usageLimit, false);
	assert.equal(runStatus({ exitCode: 1, timedOut: false, turns: 1, errorMessage: overloaded.errorMessage, ledger: null }), "provider_error");
	const login = claudeStats(result({ result: "Invalid API key · Please run /login" }));
	assert.ok(isClaudeAuthError(login.errorMessage));
	assert.ok(!isClaudeAuthError(overloaded.errorMessage) && !isClaudeAuthError(null));
	// A rejected request, paid overage, or a full window each mean the subscription is spent, whatever the result says.
	for (const extra of [{ status: "rejected" }, { isUsingOverage: true }, { unifiedWindows: WINDOWS(1, 0.2) }])
		assert.equal(claudeStats(JSON.stringify(rateLimitEvent(0.99, extra))).usageLimit, true, JSON.stringify(extra));
	assert.equal(claudeStats(JSON.stringify(rateLimitEvent(0.99))).usageLimit, false);
});

test("claudeRateLimit, overLimit and fullestWindow", () => {
	assert.deepEqual(claudeRateLimit(rateLimitEvent(0.71)), { status: "allowed", overage: false, windows: WINDOWS(0.71, 0.19) });
	assert.equal(claudeRateLimit({ type: "rate_limit_event" }), null);
	// Without the unified windows, the event's own window.
	assert.deepEqual(claudeRateLimit({ rate_limit_info: { status: "allowed_warning", rateLimitType: "seven_day", utilization: 0.8, resetsAt: 5 } }), {
		status: "allowed_warning",
		overage: false,
		windows: { seven_day: { utilization: 0.8, resetsAt: 5 } },
	});
	assert.equal(overLimit(null), false);
	assert.equal(overLimit({ status: "allowed", windows: WINDOWS(0.99, 0.5) }), false);
	assert.equal(overLimit({ status: "allowed", windows: WINDOWS(0.5, 1) }), true);
	assert.equal(overLimit({ status: "rejected", windows: {} }), true);
	assert.deepEqual(fullestWindow({ windows: WINDOWS(0.4, 0.6) }), { name: "seven_day", utilization: 0.6, resetsAt: 1791302400 });
	assert.equal(fullestWindow(null), null);
});

function claudeTranscript() {
	const usage = (output) => ({ input_tokens: 2, cache_creation_input_tokens: 100, cache_read_input_tokens: 1000, output_tokens: output, output_tokens_details: { thinking_tokens: 5 } });
	const assistant = (id, output, effort = "medium") => ({ type: "assistant", effort, perTurnEffort: effort, requestId: `req_${id}`, message: { id, model: "claude-opus-5-5", content: [], usage: usage(output) } });
	return [
		{ type: "queue-operation", operation: "enqueue" },
		{ type: "user", message: { role: "user", content: "hi" } },
		{ type: "attachment", attachment: { type: "credential_org", organizationUuid: "org" } },
		// One line per content block of a message; each carries the message's usage.
		assistant("msg_1", 154),
		assistant("msg_1", 154),
		assistant("msg_2", 117),
		{ type: "assistant", message: { id: "s1", model: "<synthetic>", content: [], usage: usage(9999) } },
		{ type: "cost-state", totalCostUSD: 0.07 },
	]
		.map((line) => JSON.stringify(line))
		.join("\n");
}

test("claudeTranscriptStats: one response per message id, with the model and the effort it ran at", () => {
	const subagent = JSON.stringify({ type: "assistant", isSidechain: true, effort: "medium", message: { id: "msg_9", model: "claude-opus-5-5", usage: { input_tokens: 1, output_tokens: 10 } } });
	const stats = claudeTranscriptStats([claudeTranscript(), subagent, ""]);
	assert.equal(stats.responses, 3);
	assert.deepEqual(stats.models, ["claude-opus-5-5"]);
	assert.deepEqual(stats.efforts, ["medium"]);
	assert.deepEqual(stats.tokens, { input: 5, cacheRead: 2000, cacheWrite: 200, output: 281, reasoning: 10, total: 2486 });
	assert.deepEqual(claudeTranscriptStats([]), { responses: 0, models: [], efforts: [], tokens: { input: 0, cacheRead: 0, cacheWrite: 0, output: 0, reasoning: 0, total: 0 } });
});

test("claudeCheck: one model, the subscription login, one effort, nothing denied", () => {
	const transcript = claudeTranscriptStats([claudeTranscript()]);
	const good = { arm: "claude", status: "completed", models: ["claude-opus-5-5"], transcript, apiKeySource: "none", mcpServers: [], permissionDenials: [] };
	assert.deepEqual(claudeCheck(good), { ok: true, problems: [] });
	// Ultron names the model with its provider and serves its REPL as the one MCP server.
	assert.equal(claudeCheck({ ...good, arm: "ultron-claude", models: ["claude-code/claude-opus-5-5"], apiKeySource: undefined, mcpServers: undefined, permissionDenials: undefined }).ok, true);
	const bad = (change, pattern) => {
		const check = claudeCheck({ ...good, ...change });
		assert.equal(check.ok, false);
		assert.match(check.problems.join("; "), pattern);
	};
	bad({ status: "timeout" }, /ended as timeout/);
	bad({ models: ["claude-opus-5-5", "claude-haiku-5"] }, /models seen by the tool: claude-opus-5-5, claude-haiku-5/);
	bad({ models: [] }, /models seen by the tool: none/);
	bad({ transcript: { ...transcript, models: ["claude-sonnet-5"] } }, /transcript: claude-sonnet-5/);
	bad({ transcript: { ...transcript, responses: 0, models: [] } }, /transcript: none/);
	bad({ transcript: { ...transcript, efforts: ["medium", "high"] } }, /efforts .*medium, high/);
	bad({ transcript: { ...transcript, efforts: [] } }, /none recorded/);
	bad({ apiKeySource: "ANTHROPIC_API_KEY" }, /an API key is in use/);
	bad({ mcpServers: ["github"] }, /MCP servers loaded: github/);
	bad({ permissionDenials: ["Bash"] }, /tool calls denied: Bash/);
});

test("ultronStats: a Claude Code lane's models, cache writes, subscription windows and usage limit", () => {
	const claudeUsage = (input, output, cacheRead, cacheWrite, cost) => ({ input, output, cacheRead, cacheWrite, totalTokens: input + output + cacheRead + cacheWrite, cost: { total: cost } });
	const assistant = (id, extra) => ({ kind: "entry", id, type: "message", message: { role: "assistant", provider: "claude-code", model: "claude-opus-5-5", content: [{ type: "toolCall", name: "rlm", arguments: { code: "await bash('ls')" } }], stopReason: "toolUse", ...extra } });
	const diagnostic = (fiveHour) => ({ type: "claude_code_usage", timestamp: 1, details: { status: "allowed", windows: WINDOWS(fiveHour, 0.19) } });
	const session = [
		JSON.stringify([assistant("e1", { diagnostics: [diagnostic(0.2)] }), { kind: "usage", id: "u1", usage: claudeUsage(2, 67, 0, 3657, 0) }]),
		JSON.stringify([
			assistant("e2", { content: [{ type: "text", text: "Done." }], stopReason: "stop", diagnostics: [{ type: "claude_code_warning", error: { message: "w" } }, diagnostic(0.25)] }),
			{ kind: "usage", id: "u2", usage: claudeUsage(2, 10, 3657, 104, 0.0324) },
		]),
	].join("\n");
	const stats = ultronStats([{ name: "sessions/x_root-1.jsonl", text: session }], "root-1");
	assert.deepEqual(stats.models, ["claude-code/claude-opus-5-5"]);
	assert.deepEqual(stats.tokens, { input: 4, cacheRead: 3657, cacheWrite: 3761, output: 77, reasoning: 0, total: 7499 });
	assert.equal(Number(stats.reportedCostUsd.toFixed(4)), 0.0324);
	assert.equal(stats.rateLimits.first.windows.five_hour.utilization, 0.2);
	assert.equal(stats.rateLimits.last.windows.five_hour.utilization, 0.25);
	assert.equal(stats.usageLimit, false);
	assert.equal(stats.cells, 1);
	const failed = JSON.stringify(assistant("e3", { content: [], stopReason: "error", errorMessage: "Claude Code usage limit reached (five_hour window)", responseModel: "claude-opus-5-5-20260901", diagnostics: [{ type: "provider_usage_limit", error: { message: "x" } }] }));
	const limited = ultronStats([{ name: "sessions/x_root-1.jsonl", text: `${session}\n${failed}` }], "root-1");
	assert.equal(limited.usageLimit, true);
	assert.match(limited.errorMessage, /usage limit reached/);
	assert.deepEqual(limited.models, ["claude-code/claude-opus-5-5", "claude-opus-5-5-20260901"]);
	// The proxy arm's journal has none of this.
	const plain = ultronStats([{ name: "sessions/x_root-1.jsonl", text: ultronSession() }], "root-1");
	assert.deepEqual([plain.models, plain.usageLimit, plain.rateLimits], [[], false, null]);
});

test("ultronUsageSummary: what `ultron usage --json` says about cells, depth, models and guards", () => {
	const bucket = { responses: 9, input: 4, output: 77, cacheRead: 3657, cacheWrite: 3761, totalTokens: 7499, cost: { reportedUsd: null, subscriptionUsd: 0.5, unpricedResponses: 0 }, unmeasured: 0 };
	const report = {
		schema: "ultron.session-report/1",
		session: { id: "s", path: "/agent/ultron-agent/experimental/sessions/--testbed--/x_s.jsonl", cwd: "/testbed" },
		mode: "ultron --claude",
		turns: { count: 1, completed: 1 },
		cells: { source: "transcript", total: { count: 7, failed: 1, apis: { bash: 6 } }, root: { count: 5, failed: 1, apis: {} }, subagents: { count: 2, failed: 0, apis: {} } },
		depth: {
			verdict: "depth 1: 2 frames, 1 sub-agent",
			level: 1,
			frames: { count: 2, calls: { infer: 1, map: 1 } },
			subagents: { count: 1, verdicts: { verified: 1, contradicted: 0, unverified: 0 } },
			workflows: { runs: 0 },
			backgroundJobs: { count: 3 },
		},
		usage: { models: [{ model: "claude-code/claude-opus-5-5", ...bucket }], total: bucket },
		guardrails: { guards: { Loki: { checks: 4, blocked: 1 } }, hints: { "bash-grep": 2 }, usageLimitBlocks: 0 },
	};
	assert.deepEqual(ultronUsageSummary(report), {
		mode: "ultron --claude",
		rootTurns: 1,
		cells: { total: 7, failed: 1, root: 5, subagents: 2 },
		depth: "depth 1: 2 frames, 1 sub-agent",
		frames: 2,
		frameCalls: { infer: 1, map: 1 },
		subagents: 1,
		subagentVerdicts: { verified: 1, contradicted: 0, unverified: 0 },
		workflows: 0,
		backgroundJobs: 3,
		models: [{ model: "claude-code/claude-opus-5-5", responses: 9, totalTokens: 7499 }],
		subscriptionUsd: 0.5,
		unmeasured: 0,
		guards: { Loki: { checks: 4, blocked: 1 } },
		hints: { "bash-grep": 2 },
		usageLimitBlocks: 0,
	});
	// The session's path (a home directory on a host) is not part of what is kept.
	assert.ok(!JSON.stringify(ultronUsageSummary(report)).includes("/agent/"));
	for (const notAReport of [null, {}, { schema: "other/1" }, "text"]) assert.equal(ultronUsageSummary(notAReport), null);
	const bare = ultronUsageSummary({ schema: "ultron.session-report/1" });
	assert.deepEqual([bare.cells, bare.frames, bare.subagents, bare.models], [null, 0, 0, []]);
});

test("runStatus: a spent subscription and a lost login are harness failures that stop the Claude arms", () => {
	const base = { exitCode: 1, timedOut: false, turns: 4, errorMessage: "x", ledger: null };
	assert.equal(runStatus({ ...base, usageLimit: true }), "usage_limit");
	// Even when the limit ended the run by the clock, or it finished on paid overage.
	assert.equal(runStatus({ ...base, timedOut: true, usageLimit: true }), "usage_limit");
	assert.equal(runStatus({ exitCode: 0, timedOut: false, turns: 9, errorMessage: null, ledger: null, usageLimit: true }), "usage_limit");
	assert.equal(runStatus({ ...base, authError: true }), "auth_error");
	assert.equal(runStatus({ ...base }), "agent_crash");
	assert.equal(runStatus({ exitCode: 0, timedOut: false, turns: 9, errorMessage: null, ledger: null }), "completed");
	assert.equal(runStatus({ exitCode: 124, timedOut: false, turns: 9, errorMessage: null, ledger: null }), "timeout");
	for (const status of ["usage_limit", "auth_error", "auth_unavailable", "not_run"]) {
		assert.ok(HARNESS_FAILURES.has(status), status);
		assert.ok(RERUN_STATUSES.has(status), status);
	}
	assert.deepEqual([...ARM_STOPPERS].sort(), ["auth_error", "auth_unavailable", "usage_limit"]);
	assert.ok(!RERUN_STATUSES.has("timeout") && !RERUN_STATUSES.has("completed") && !ARM_STOPPERS.has("not_run"));
});

function claudeRecords() {
	const tokens = { input: 100, cacheRead: 50_000, cacheWrite: 6000, output: 900, reasoning: 0, total: 57_000 };
	const transcript = { responses: 6, models: ["claude-opus-5-5"], efforts: ["medium"], tokens };
	const claude = (fiveHour, denied = []) => ({ models: ["claude-opus-5-5"], permissionDenials: denied, transcript, rateLimits: { first: { status: "allowed", windows: WINDOWS(fiveHour, 0.19) }, last: { status: "allowed", windows: WINDOWS(fiveHour + 0.02, 0.2) } } });
	const base = { wallMs: 120_000, turns: 6, toolCalls: 5, tokens, ledger: null };
	const ultron = { cells: 4, errorCells: 0, helpers: { bash: 4 }, frameCalls: 0, childSessions: 0, loki: { beforeWriteChecks: 1, blocked: 0, afterCellFindings: 0 } };
	return [
		{ ...base, instance_id: "t1", arm: "claude", status: "completed", verdict: "resolved", notionalCostUsd: 0.4, startedAt: "2026-10-02T09:00:00.000Z", claude: claude(0.1, ["WebFetch"]) },
		{ ...base, instance_id: "t1", arm: "ultron-claude", status: "completed", verdict: "resolved", notionalCostUsd: 0.25, startedAt: "2026-10-02T09:00:01.000Z", claude: claude(0.1), ultron },
		{ ...base, instance_id: "t2", arm: "claude", status: "completed", verdict: "unresolved", notionalCostUsd: 0.6, startedAt: "2026-10-02T09:03:00.000Z", wallMs: 600_000, claude: claude(0.2) },
		{ ...base, instance_id: "t2", arm: "ultron-claude", status: "usage_limit", verdict: "empty_patch", notionalCostUsd: null, startedAt: "2026-10-02T09:03:01.000Z", claude: claude(0.3), ultron },
		{ instance_id: "t3", arm: "claude", status: "not_run", verdict: "empty_patch", startedAt: "2026-10-02T09:14:00.000Z", error: "not run: usage_limit on t2" },
	];
}

test("summarize, subscriptionUse and renderMarkdown for the Claude arms", () => {
	const summary = summarize(claudeRecords(), ["claude", "ultron-claude"]);
	assert.equal(summary.claude.resolved, 1);
	assert.equal(summary.claude.unresolved, 1);
	// A task the arm never started, and one ended by the usage limit, are harness failures, not misses.
	assert.deepEqual(summary.claude.harnessFailures, [{ instance_id: "t3", status: "not_run" }]);
	assert.deepEqual(summary["ultron-claude"].harnessFailures, [{ instance_id: "t2", status: "usage_limit" }]);
	assert.equal(summary["ultron-claude"].unresolved, 0);
	assert.equal(summary.claude.tokens.cacheWrite, 12_000);
	assert.equal(summary.claude.notionalCostUsd, 1);
	assert.equal(summary["ultron-claude"].notionalCostUsd, 0.25);
	const subscription = subscriptionUse(claudeRecords());
	assert.equal(subscription.first.at, "2026-10-02T09:00:00.000Z");
	assert.equal(subscription.first.windows.five_hour.utilization, 0.1);
	// The run that ended last, not the one that started last.
	assert.equal(subscription.last.at, "2026-10-02T09:13:00.000Z");
	assert.equal(subscription.last.windows.five_hour.utilization, 0.22);
	assert.equal(subscriptionUse(records()), null);
	const markdown = renderMarkdown({
		runId: "pilot10-opus",
		date: "2026-10-02",
		model: runModel(CLAUDE_ARMS),
		swebenchVersion: "5.0.2",
		sample: { n: 3, seed: "s" },
		limits: { wallClockMinutes: 30, concurrency: 2 },
		costNote: "notionalCostUsd is what Claude Code reports for each run",
		summary,
		subscription,
		headToHead: { "claude vs ultron-claude": headToHead(claudeRecords(), "claude", "ultron-claude") },
		harnessFailures: [{ instance_id: "t2", arm: "ultron-claude", status: "usage_limit", verdict: "empty_patch", error: "limit" }],
		verification: {
			claude: { ok: true, responseModels: ["claude-opus-5-5"], paths: ["claude -p"], reasoningEfforts: ["medium"], requests: 2, source: "responses in Claude Code's session transcript" },
		},
		records: claudeRecords(),
	});
	assert.match(markdown, /^# SWE-bench Verified, 3 tasks, claude-code\/claude-opus-5-5$/m);
	assert.match(markdown, /tokens \(in \/ cache read \/ cache write \/ out\) \| notional cost \|/);
	assert.match(markdown, /\| claude \| 1\/3 \| 1 \| 0 \| 1 \| 0 \| 12m00s \| 12 \| 10 \| 200 \/ 100,000 \/ 12,000 \/ 1,800 \| \$1\.00 \|/);
	assert.match(markdown, /Cost is notional: what Claude Code reports for each run\./);
	assert.match(markdown, /\| t2 \| no, 10m00s, 6, 57,000, \$0\.60 \| no \(empty patch\) \[usage_limit\], 2m00s, 6, 57,000, - \|/);
	assert.match(markdown, /\| t3 \| no \(empty patch\) \[not_run\], -, -, -, - \| - \|/);
	assert.match(markdown, /- claude: verified on claude-opus-5-5 via claude -p, reasoning effort medium \(2 responses in Claude Code's session transcript\)/);
	assert.match(markdown, /- claude: model claude-opus-5-5, effort medium \(Claude Code's session transcripts, 12 responses\); tool calls denied: WebFetch/);
	assert.match(markdown, /- ultron-claude: model claude-opus-5-5, effort medium .* tool calls denied: none/);
	assert.match(markdown, /first run to last: five_hour 10% to 22%, seven_day 19% to 20%\. The windows are the account's/);
	assert.match(markdown, /\| t1 \| 4 \| 0 \| bash 4 \| 0 \| 0 \| 1 \/ 0 \/ 0 \|/);
});
