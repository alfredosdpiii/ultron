import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
	AGENT_DIR,
	ARMS,
	DEFAULT_TASKS,
	KEY_ENV,
	agentFiles,
	agentScript,
	buildPrompt,
	cleanPatch,
	containerName,
	gradeScript,
	gradeVerdict,
	hostStats,
	ledgerStats,
	parseArgs,
	parseReward,
	printedSessionId,
	renderMarkdown,
	runMetrics,
	runStats,
	runStatus,
	sessionIdOf,
	summarize,
	swarmStats,
	usageSummary,
} from "../evals/deepswe/lib.mjs";

const userModels = {
	providers: {
		cliproxyapi: {
			baseUrl: "http://127.0.0.1:8317/v1",
			apiKey: "secret-key-123",
			api: "openai-completions",
			models: [
				{ id: "gpt-6.1-sol", cost: { input: 2, output: 10, cacheRead: 0.1 } },
				{ id: "other", cost: {} },
			],
		},
	},
};

function journal(id, cells, usage = []) {
	const lines = [JSON.stringify({ kind: "session", type: "session", id })];
	cells.forEach((code, index) => {
		lines.push(
			JSON.stringify({
				kind: "entry",
				type: "message",
				id: `${id}-m${index}`,
				message: { role: "assistant", provider: "cliproxyapi", model: "gpt-6.1-sol", stopReason: "toolUse", content: [{ type: "toolCall", name: "rlm", arguments: { code } }] },
			}),
		);
	});
	usage.forEach((u, index) => lines.push(JSON.stringify({ kind: "usage", id: `${id}-u${index}`, usage: u })));
	return lines.join("\n");
}

describe("parseArgs", () => {
	it("accepts known flags and reports unknown ones", () => {
		const parsed = parseArgs(["run", "--run-id", "ab1", "--concurrency", "2", "--skip-gold", "--bogus", "x"]);
		assert.equal(parsed.command, "run");
		assert.deepEqual(parsed.flags, { "run-id": "ab1", concurrency: "2", "skip-gold": true });
		assert.deepEqual(parsed.unknown, ["--bogus", "x"]);
	});
	it("treats an unknown command as unknown", () => {
		assert.deepEqual(parseArgs(["nope"]).unknown, ["nope"]);
	});
});

describe("arms and tasks", () => {
	it("compares exactly the two arms on four tasks", () => {
		assert.deepEqual(ARMS, ["ultron", "ultron-swarm"]);
		assert.equal(DEFAULT_TASKS.length, 4);
	});
	it("names containers with the harness prefix", () => {
		const name = containerName("ab1", "agent", "wasmi-trap-coredumps", "ultron-swarm");
		assert.ok(name.startsWith("ultron-deepswe-ab1-agent-ultron-swarm-"));
		assert.match(name, /^[a-z0-9_.-]+$/);
	});
});

describe("agent files", () => {
	it("writes models.json without the key, and the directive only for the swarm arm", () => {
		const plain = agentFiles("ultron", { userModels, baseUrl: "http://127.0.0.1:8317/v1", directive: "use swarm" });
		assert.deepEqual(Object.keys(plain), [`${AGENT_DIR}/models.json`]);
		const models = JSON.parse(plain[`${AGENT_DIR}/models.json`]);
		assert.equal(models.providers.cliproxyapi.apiKey, `$${KEY_ENV}`);
		assert.deepEqual(models.providers.cliproxyapi.models.map((m) => m.id), ["gpt-6.1-sol"]);
		assert.ok(!plain[`${AGENT_DIR}/models.json`].includes("secret-key-123"));
		const swarm = agentFiles("ultron-swarm", { userModels, baseUrl: "http://127.0.0.1:8317/v1", directive: "use swarm" });
		assert.equal(swarm[`${AGENT_DIR}/APPEND_SYSTEM.md`], "use swarm");
		assert.throws(() => agentFiles("ultron-swarm", { userModels, baseUrl: "x", directive: null }));
	});
	it("runs ultron in json print mode under timeout with private dirs", () => {
		const script = agentScript({ limitSeconds: 5400, python: "/opt/ultron/python/bin/python3.12" });
		assert.match(script, /timeout --kill-after=60 5400 \/opt\/ultron\/ultron --mode json -p --model cliproxyapi\/gpt-6.1-sol/);
		assert.match(script, /export ULTRON_CODING_AGENT_DIR='\/agent\/agent'/);
		assert.match(script, /export ULTRON_SERVER_DIR='\/agent\/server'/);
		assert.match(script, /export HOME='\/agent\/home'/);
		assert.match(script, /export ULTRON_PYTHON='\/opt\/ultron\/python\/bin\/python3.12'/);
		assert.ok(!script.includes("append-system-prompt"));
	});
	it("uses the problem statement as the prompt", () => {
		assert.equal(buildPrompt({ problem_statement: "  Add X.\n" }), "Add X.");
	});
});

describe("patch and grading", () => {
	it("drops agent state and binary sections", () => {
		const diff = [
			"diff --git a/src/x.py b/src/x.py",
			"--- a/src/x.py",
			"+++ b/src/x.py",
			"@@ -1 +1 @@",
			"-a",
			"+b",
			"diff --git a/.loki/log b/.loki/log",
			"new file mode 100644",
			"diff --git a/img.png b/img.png",
			"Binary files a/img.png and b/img.png differ",
			"",
		].join("\n");
		const cleaned = cleanPatch(diff);
		assert.deepEqual(cleaned.files, ["src/x.py"]);
		assert.equal(cleaned.dropped.length, 2);
	});
	it("applies the patch before the verifier, and skips it when empty", () => {
		const withPatch = gradeScript({ verifierTimeoutSec: 1800, hasPatch: true });
		assert.ok(withPatch.indexOf("git apply") < withPatch.indexOf("bash /tests/test.sh"));
		assert.match(withPatch, /exit 90/);
		assert.ok(!gradeScript({ verifierTimeoutSec: 1800, hasPatch: false }).includes("git apply"));
	});
	it("reads the reward and the verdict", () => {
		assert.equal(parseReward("1\n"), 1);
		assert.equal(parseReward("0"), 0);
		assert.equal(parseReward(""), null);
		assert.equal(parseReward(null), null);
		assert.equal(gradeVerdict({ exitCode: 0, reward: 1 }), "pass");
		assert.equal(gradeVerdict({ exitCode: 0, reward: 0 }), "fail");
		assert.equal(gradeVerdict({ exitCode: 90, reward: null }), "patch_failed");
		assert.equal(gradeVerdict({ exitCode: 1, reward: null }), "verifier_error");
	});
});

describe("usage", () => {
	it("finds the printed session id", () => {
		assert.equal(printedSessionId('noise\n{"kind":"prompted","sessionId":"s1","text":"ok"}\n'), "s1");
		assert.equal(printedSessionId(""), null);
	});
	it("reads session ids from the header or the file name", () => {
		assert.equal(sessionIdOf({ name: "x.jsonl", text: journal("abc", []) }), "abc");
		assert.equal(sessionIdOf({ name: "sessions/2026_def.jsonl", text: "" }), "def");
	});
	it("counts swarm calls in the root and in children apart", () => {
		const files = [
			{ name: "root.jsonl", text: journal("root", ["c = rlm.spawn('x', name='a', keep=True)\nboard.add('t')", "rlm.ask(c, 'q')"]) },
			{ name: "child.jsonl", text: journal("child", ["board.claim('a')\nboard.update(1, 'done')"]) },
		];
		const swarm = swarmStats(files, "root");
		assert.equal(swarm.root.keepSpawn, 1);
		assert.equal(swarm.root.boardAdd, 1);
		assert.equal(swarm.root.ask, 1);
		assert.equal(swarm.children.boardClaim, 1);
		assert.equal(swarm.total.boardUpdate, 1);
		assert.equal(swarm.rootSpawnCalls, 1);
		assert.equal(swarm.totalCalls, 5);
	});
	it("splits turns and tokens between root and children and prices them", () => {
		const files = [
			{ name: "a.jsonl", text: journal("root", ["print(1)", "print(2)"], [{ input: 1000, cacheRead: 0, output: 100 }]) },
			{ name: "b.jsonl", text: journal("kid", ["print(3)"], [{ input: 500, output: 50 }]) },
		];
		const stats = runStats(files, "root", { input: 2, output: 10, cacheRead: 0.1 });
		assert.equal(stats.turns, 3);
		assert.equal(stats.rootTurns, 2);
		assert.equal(stats.childTurns, 1);
		assert.equal(stats.childSessions, 1);
		assert.equal(stats.tokens.total, 1650);
		assert.equal(stats.rootTokens.total, 1100);
		assert.equal(stats.childTokens.total, 550);
		assert.equal(stats.notionalCostUsd, Number(((1500 * 2 + 150 * 10) / 1e6).toFixed(4)));
	});
	it("classifies how a run ended", () => {
		assert.equal(runStatus({ exitCode: 124, timedOut: true, turns: 9 }), "timeout");
		assert.equal(runStatus({ exitCode: 1, timedOut: false, turns: 0 }), "agent_crash");
		assert.equal(runStatus({ exitCode: 0, timedOut: false, turns: 3, errorMessage: "429" }), "provider_error");
		assert.equal(runStatus({ exitCode: 0, timedOut: false, turns: 3 }), "completed");
	});
});

describe("ultron's own counters", () => {
	it("reads the last host stats of the journal and keeps the swarm APIs", () => {
		const text = [
			JSON.stringify({ kind: "value", op: "set", namespace: "ultron.module", key: "stats", value: { cells: { root: { count: 1 } }, hostCalls: { bash: 1 } } }),
			JSON.stringify({
				kind: "value",
				op: "set",
				namespace: "ultron.module",
				key: "stats",
				value: { cells: { root: { count: 7 }, subagents: { count: 2 }, other: { count: 59 } }, hostCalls: { bash: 44, "board.add": 4, "swarm.send": 17, "rlm.spawn": 4, "files.before_write": 31 } },
			}),
		].join("\n");
		const host = hostStats(text);
		assert.deepEqual(host.cells, { root: 7, subagents: 2, other: 59 });
		assert.deepEqual(host.swarmCalls, { "board.add": 4, "swarm.send": 17, "rlm.spawn": 4 });
		assert.equal(host.swarmCallTotal, 25);
		assert.equal(hostStats(""), null);
	});
	it("summarizes the session report per lane", () => {
		const lane = (responses, total) => ({ responses, input: total / 2, cacheRead: total / 2, cacheWrite: 0, output: 0, totalTokens: total, cost: { reportedUsd: 0.1 } });
		const report = {
			schema: "ultron.session-report/1",
			turns: { count: 1 },
			cells: { root: { count: 3 }, subagents: { count: 5 }, other: { count: 0 } },
			depth: { verdict: "sub-agents", subagents: { count: 2, maxDepth: 1, completed: 2, failed: 0 }, frames: { count: 1 } },
			usage: { lanes: { root: lane(4, 100), frames: lane(1, 10), subagents: lane(6, 300), other: lane(1, 5) }, total: lane(12, 415) },
		};
		const usage = usageSummary(report);
		assert.equal(usage.tokens.total, 415);
		assert.equal(usage.subagents.count, 2);
		assert.equal(usage.lanes.subagents.total, 300);
		assert.equal(usageSummary({ schema: "other" }), null);
		const metrics = runMetrics({ status: "completed", wallMs: 5, grade: { verdict: "pass" }, usage, host: { swarmCalls: { "rlm.spawn": 2 }, swarmCallTotal: 2 }, stats: { rootTurns: 4 } }, { input: 2, output: 10, cacheRead: 0.1 });
		assert.equal(metrics.tokens, 415);
		assert.equal(metrics.rootTokens, 100);
		assert.equal(metrics.childTokens, 315);
		assert.equal(metrics.childResponses, 8);
		assert.equal(metrics.children, 2);
		assert.equal(metrics.swarmCallTotal, 2);
		assert.equal(metrics.notionalCostUsd, Number(((207.5 * 2 + 207.5 * 0.1) / 1e6).toFixed(4)));
	});
});

describe("the host's usage ledger", () => {
	it("sums the turn root and the session root, and scores the tree from it", () => {
		const ledger = (roots) => JSON.stringify({ kind: "value", op: "set", namespace: "ultron.usage", key: "root", value: { version: 1, roots } });
		const text = [
			ledger({ "turn:a": { turns: { count: 1, tokens: 10, cost: 0.01 } } }),
			ledger({ "turn:a": { turns: { count: 18, tokens: 400148, cost: 0.1496752 } }, "ultron-root": { turns: { count: 186, tokens: 5063830, cost: 1.504828 } } }),
		].join("\n");
		const parsed = ledgerStats(text);
		assert.deepEqual(parsed.roots.turn, { responses: 18, tokens: 400148, costUsd: 0.1496752 });
		assert.deepEqual(parsed.total, { responses: 204, tokens: 5463978, costUsd: 1.6545 });
		assert.equal(ledgerStats(""), null);
		const metrics = runMetrics({
			status: "completed",
			ledger: parsed,
			usage: { tokens: { responses: 19, total: 400957 }, lanes: { root: { responses: 18, total: 400148 } }, subagents: { count: 0 } },
			host: { hostCalls: { "rlm.spawn": 10 }, swarmCalls: { "rlm.spawn": 10 }, swarmCallTotal: 10 },
		});
		assert.equal(metrics.tokens, 5463978);
		assert.equal(metrics.rootTokens, 400148);
		assert.equal(metrics.childTokens, 5063830);
		assert.equal(metrics.childResponses, 186);
		assert.equal(metrics.notionalCostUsd, 1.6545);
		assert.equal(metrics.children, 10);
	});
});

describe("report", () => {
	it("summarizes per arm and renders a table", () => {
		const records = [
			{ task_id: "t1", language: "go", arm: "ultron", metrics: { verdict: "pass", status: "completed", wallMs: 60000, responses: 5, tokens: 1000, rootTokens: 1000, childTokens: 0, notionalCostUsd: 0.5, children: 0, swarmCalls: {}, swarmCallTotal: 0 } },
			{ task_id: "t1", language: "go", arm: "ultron-swarm", metrics: { verdict: "fail", status: "timeout", wallMs: 120000, responses: 9, tokens: 3000, rootTokens: 1000, childTokens: 2000, notionalCostUsd: 1.5, children: 2, swarmCalls: { "rlm.ask": 3 }, swarmCallTotal: 3 } },
		];
		const summary = summarize(records);
		assert.equal(summary.ultron.passed, 1);
		assert.equal(summary["ultron-swarm"].timeouts, 1);
		assert.equal(summary["ultron-swarm"].childTokens, 2000);
		assert.equal(summary["ultron-swarm"].swarmCalls, 3);
		const md = renderMarkdown({ date: "2026-10-20", model: "cliproxyapi/gpt-6.1-sol", dataset: "datacurve/deep-swe", binary: { name: "b", version: "0.87.39", commit: "c" }, arms: ARMS, tasks: ["t1"], records, summary, gold: [{ task_id: "t1", verdict: "pass" }] });
		assert.match(md, /\| t1 \| go \| ultron-swarm \| fail \| timeout \|/);
		assert.match(md, /rlm\.ask 3/);
		assert.match(md, /Gold check/);
	});
});
