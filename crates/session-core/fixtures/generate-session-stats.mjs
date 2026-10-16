// Records the TypeScript session counters (ultron/session-stats.ts `SessionStatsRecorder`, the session value
// `ultron.module/stats` behind `/usage` and `ultron usage`): each case starts from a stored document (or none), applies
// a list of recorder calls on a clock the case sets, and records every document the recorder wrote (in order) and the
// snapshots it was asked for. The Rust test (crates/session-core/tests/session_stats.rs) replays the same calls.
// No network and no product process: the module is imported directly; offline-guard.mjs is loaded anyway.
// Usage: node crates/session-core/fixtures/generate-session-stats.mjs (writes session_stats.json beside it;
// deterministic)
import { writeFileSync } from "node:fs";
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import {
	cellApis,
	classifyNudge,
	parseSessionStats,
	SessionStatsRecorder,
	statsLane,
} from "../../../packages/coding-agent/src/ultron/session-stats.ts";

const guard = (name, phase, outcome, ms) => ({ op: "guard", record: { guard: name, phase, outcome, ms } });
const cell = (lane, code, failed = false, masked = 0) => ({ op: "cell", lane, code, failed, masked });

const counting = [
	{ op: "tick", now: 1_700_000_000_000 },
	{ op: "touch" },
	{ op: "flush" },
	{ op: "flush" },
	{ op: "tick", now: 1_700_000_000_250 },
	cell("main", "print('one')"),
	cell("main", "r = bash('ls')\nprint(read('a.txt'))\nedit('a', 'b', 'c')\nwrite('x', 'y')", false, 2),
	cell("main", "x.bash('a'); obj.read(1); my_write(2); view_image('p.png')"),
	cell("main", "rlm.load('a'); rlm.infer('q'); rlm.map([], 'p'); rlm.spawn('t')", true),
	cell("ultron.rlm-child.abc", "rlm.collect(); rlm.merge(['a']); mcp.call('s', 't')", false, 1),
	cell("ultron.rlm-child.abc", "tools.call('x'); workflows.run('w'); agents.spawn('d'); background.start('j')"),
	cell("agent:reviewer", "agents.invoke('a'); agents.register('b'); agents.list()", true, 3),
	cell("job-1", "def later():\n    bash ('echo')\nalias = bash\nalias('x')"),
	{ op: "hostCall", type: "shell.run" },
	{ op: "hostCall", type: "rlm.map" },
	{ op: "hostCall", type: "shell.run" },
	{ op: "hostCall", type: "workflows.run" },
	guard("Loki", "before_write", "allowed", 12),
	guard("Loki", "before_write", "blocked", 30),
	guard("Loki", "before_shell", "unchecked", 5000),
	guard("Loki", "before_shell", "allowed", 3),
	guard("Loki", "after_cell", "clean", 40),
	guard("Loki", "after_cell", "advisory", 41),
	guard("Loki", "after_cell", "findings", 42),
	guard("Loki", "after_cell", "unchecked", 43),
	guard("ext:lint", "before_write", "allowed", 1),
	{ op: "nudge", text: "[Ultron] You have used 8 rounds of tool calls in this turn." },
	{ op: "nudge", text: "[Ultron] You have now used 16 rounds of tool calls." },
	{ op: "nudge", text: "[Ultron] 2 subagents or tasks you started are still running: wait." },
	{ op: "nudge", text: "[Ultron] 1 subagent or task you started is still running." },
	{ op: "nudge", text: "[Ultron] 12 tool rounds in a row succeeded: save it as a skill." },
	{ op: "nudge", text: "[Ultron] something else" },
	{ op: "nudge", text: "You have used 3 rounds of tool calls" },
	{ op: "usageLimitBlock" },
	{ op: "usageLimitBlock" },
	{ op: "merge", taskId: "task-a", status: "merged" },
	{ op: "merge", taskId: "task-b", status: "conflict" },
	{ op: "merge", taskId: "task-a", status: "empty" },
	{ op: "childModel", taskId: "task-a", model: "anthropic/claude-sonnet-4-5" },
	{ op: "childModel", taskId: "task-c", model: "openai/gpt-5" },
	{ op: "tick", now: 1_700_000_001_000 },
	{ op: "externalTurnStarted", turn: "t1" },
	{ op: "externalTurnStarted", turn: "t1" },
	{ op: "tick", now: 1_700_000_001_700 },
	{ op: "externalTurnEnded", turn: "t1" },
	{ op: "externalTurnEnded", turn: "t1" },
	{ op: "externalTurnEnded", turn: "never" },
	{ op: "externalTurnStarted", turn: "t2" },
	{ op: "snapshot" },
	{ op: "flush" },
	{ op: "tick", now: 1_700_000_002_000 },
	{ op: "close" },
	{ op: "cell", lane: "main", code: "bash('after close')", failed: false, masked: 0 },
	{ op: "flush" },
	{ op: "snapshot" },
];

const bounded = [{ op: "tick", now: 10 }];
for (let i = 0; i < 205; i++) bounded.push({ op: "hostCall", type: `type.${i}` });
bounded.push({ op: "hostCall", type: "type.3" });
bounded.push({ op: "hostCall", type: "type.204" });
for (let i = 0; i < 203; i++) bounded.push({ op: "merge", taskId: `task-${i}`, status: i % 2 ? "merged" : "conflict" });
bounded.push({ op: "merge", taskId: "task-5", status: "merged" });
for (let i = 0; i < 201; i++) bounded.push({ op: "childModel", taskId: `child-${i}`, model: `p/m${i}` });
bounded.push({ op: "flush" });

const stored = {
	version: 1,
	root: "external",
	since: 5,
	updatedAt: 6,
	cells: {
		root: { count: 3, failed: 1, apis: { bash: 2, "rlm.map": 1 } },
		subagents: { count: 1, failed: 0, apis: {} },
		other: { count: 0, failed: 0, apis: {} },
	},
	hostCalls: { "shell.run": 4 },
	secretsMasked: 1,
	guards: { Loki: { checks: 2, blocked: 1, unchecked: 0, afterChecks: 1, afterFindings: 0, ms: 70 } },
	nudges: { toolRounds: 1, wait: 0, skill: 0 },
	usageLimitBlocks: 2,
	merges: { "task-z": "merged" },
	childModels: { "task-z": "anthropic/claude-haiku-4-5" },
	externalTurns: { count: 4, wallMs: 900 },
};

const malformed = {
	root: "somewhere",
	since: -4,
	updatedAt: "x",
	extra: { kept: false },
	cells: {
		root: { count: 2.5, failed: -1, apis: { bash: 3, read: 0, edit: -2, write: "4", "rlm.load": 1.5 } },
		subagents: null,
		other: "three",
	},
	hostCalls: [1, 2],
	secretsMasked: Number.MAX_SAFE_INTEGER,
	guards: { Loki: null, Other: { checks: 1, ms: "slow", blocked: 2, extra: 9 }, Third: 7 },
	nudges: { toolRounds: "1", wait: 2 },
	usageLimitBlocks: null,
	merges: { a: "merged", b: 3, c: null },
	childModels: "none",
	externalTurns: { count: 1 },
};

const resume = [{ op: "tick", now: 100 }, cell("main", "bash('x')"), { op: "hostCall", type: "shell.run" }, { op: "flush" }];

const cases = [
	{ name: "counting", root: "lane", ops: counting },
	{ name: "external-root", root: "external", ops: [{ op: "tick", now: 7 }, { op: "externalTurnStarted", turn: "a" }, { op: "close" }] },
	{ name: "bounded", root: "lane", ops: bounded },
	{ name: "resumed", root: "lane", stored, ops: resume },
	{ name: "unversioned", root: "lane", stored: { ...stored, version: undefined }, ops: resume },
	{ name: "malformed", root: "lane", stored: malformed, ops: resume },
	{ name: "foreign-version", root: "lane", stored: { ...stored, version: 2 }, ops: [...resume, { op: "snapshot" }] },
	{ name: "string-version", root: "lane", stored: { ...stored, version: "1" }, ops: [...resume, { op: "snapshot" }] },
	{ name: "not-an-object", root: "lane", stored: [1, 2, 3], ops: resume },
	{ name: "null-stored", root: "lane", stored: null, ops: resume },
	{ name: "nothing-ran", root: "lane", ops: [{ op: "tick", now: 3 }, { op: "flush" }, { op: "snapshot" }, { op: "close" }] },
	{ name: "failing-store", root: "lane", failWrites: 1, ops: [{ op: "tick", now: 3 }, { op: "usageLimitBlock" }, { op: "flush" }, { op: "flush" }, { op: "usageLimitBlock" }, { op: "flush" }] },
];

async function run(c) {
	let now = 0;
	let document = c.stored === undefined ? undefined : structuredClone(c.stored);
	if (document && typeof document === "object" && !Array.isArray(document)) {
		for (const key of Object.keys(document)) if (document[key] === undefined) delete document[key];
	}
	let failures = c.failWrites ?? 0;
	const writes = [];
	const store = {
		read: async () => (document === undefined ? undefined : structuredClone(document)),
		write: async (next) => {
			if (failures > 0) {
				failures -= 1;
				throw new Error("disk full");
			}
			document = structuredClone(next);
			writes.push(structuredClone(next));
		},
	};
	const recorder = new SessionStatsRecorder({ store, root: c.root, now: () => now, flushMs: 1_000_000_000 });
	const snapshots = [];
	for (const op of c.ops) {
		switch (op.op) {
			case "tick":
				now = op.now;
				break;
			case "touch":
				recorder.touch();
				break;
			case "cell":
				recorder.cell(op.lane, op.code, { failed: op.failed, masked: op.masked });
				break;
			case "hostCall":
				recorder.hostCall(op.type);
				break;
			case "guard":
				recorder.guard(op.record);
				break;
			case "nudge":
				recorder.nudge(op.text);
				break;
			case "usageLimitBlock":
				recorder.usageLimitBlock();
				break;
			case "merge":
				recorder.merge(op.taskId, op.status);
				break;
			case "childModel":
				recorder.childModel(op.taskId, op.model);
				break;
			case "externalTurnStarted":
				recorder.externalTurnStarted(op.turn);
				break;
			case "externalTurnEnded":
				recorder.externalTurnEnded(op.turn);
				break;
			case "flush":
				await recorder.flush();
				break;
			case "snapshot":
				snapshots.push(await recorder.snapshot());
				break;
			case "close":
				await recorder.close();
				break;
			default:
				throw new Error(`unknown op ${op.op}`);
		}
	}
	await recorder.flush();
	return { name: c.name, root: c.root, stored: c.stored ?? null, hasStored: c.stored !== undefined, failWrites: c.failWrites ?? 0, ops: c.ops, writes, snapshots };
}

const out = { generator: "crates/session-core/fixtures/generate-session-stats.mjs", cases: [] };
for (const c of cases) {
	out.cases.push(await run(c));
}
// The pure helpers the recorder uses, over the codes and texts above.
const codes = counting.filter((op) => op.op === "cell").map((op) => op.code);
out.cellApis = codes.map((code) => ({ code, apis: cellApis(code) }));
out.nudges = counting.filter((op) => op.op === "nudge").map((op) => ({ text: op.text, kind: classifyNudge(op.text) ?? null }));
out.lanes = ["main", "ultron.rlm-child.x", "ultron.rlm-childx", "agent:a", "Main", ""].map((lane) => ({ lane, kind: statsLane(lane) }));
out.parsed = [stored, malformed, { version: 2 }, [1], "x", null, 5, {}].map((saved) => ({ saved, parsed: parseSessionStats(saved) ?? null }));
assertOffline();
writeFileSync(new URL("./session_stats.json", import.meta.url), `${JSON.stringify(out, null, "\t")}\n`);
