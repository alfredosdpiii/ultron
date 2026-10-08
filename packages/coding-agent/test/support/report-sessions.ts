import { FIXTURE_START, fixtureStats, SessionFixture } from "./session-fixture.ts";

/**
 * A native session that never delegated: two root turns, three cells, a steer, a masked secret, and Jev decisions
 * from before Jev was removed (the report ignores them).
 */
export function rootOnlySession(): SessionFixture {
	const session = new SessionFixture("aaaaaaaa-0001-7000-8000-000000000001", "/home/dev/app");
	session.laneModel("main", "priced", "model-a");
	session.run({
		lane: "main",
		responses: [
			{
				cells: [
					{ code: 'out = await bash("ls")' },
					{ code: 'text = await read("a.py")\nawait edit("a.py", "x", "y")' },
				],
				usage: { input: 1000, output: 100, cacheRead: 500, cost: 0.02 },
			},
			{
				cells: [{ code: "1/0", error: true, output: "ZeroDivisionError near [REDACTED:github_token]" }],
				usage: { input: 1200, output: 50, cost: 0.01 },
			},
			{ usage: { input: 1300, output: 200, cost: 0.03 } },
		],
		steers: [
			"[Ultron] You have used 10 rounds of tool calls in this turn without answering. Unless one more specific call is essential, stop gathering and answer now with what you have, noting what remains unknown.",
		],
	});
	session.run({
		lane: "main",
		status: "aborted",
		responses: [{ usage: { input: 2000, output: 10, cost: 0.04 } }],
	});
	session.value("ultron.module", "hints", {
		version: 1,
		lanes: { main: { muted: [], fired: { "stuck-loop": 1, "output-truncated": 2 } } },
	});
	session.value("ultron.jev.decisions", "root", {
		version: 1,
		decisions: [
			{ id: "j1", at: 1, kind: "triage", status: "ok", durationMs: 5, inputSha256: "a", inputChars: 1 },
			{
				id: "j2",
				at: 2,
				kind: "recall",
				status: "ok",
				durationMs: 5,
				inputSha256: "a",
				inputChars: 1,
				retrieve: true,
			},
			{
				id: "j3",
				at: 3,
				kind: "recall",
				status: "ok",
				durationMs: 5,
				inputSha256: "a",
				inputChars: 1,
				retrieve: false,
			},
			{
				id: "j4",
				at: 4,
				kind: "retain",
				status: "ok",
				durationMs: 5,
				inputSha256: "a",
				inputChars: 1,
				action: "keep",
			},
			{
				id: "j5",
				at: 5,
				kind: "retain",
				status: "ok",
				durationMs: 5,
				inputSha256: "a",
				inputChars: 1,
				action: "skip",
			},
		],
	});
	session.value("ultron.memory.state", "root", {
		version: 1,
		namespace: "test",
		operations: [
			{ id: "m1", kind: "prepare", state: "recalled" },
			{ id: "m2", kind: "prepare", state: "skipped" },
			{ id: "m3", kind: "propose", state: "stored" },
		],
	});
	session.value(
		"ultron.module",
		"stats",
		fixtureStats({
			cells: {
				root: { count: 3, failed: 1, apis: { bash: 1, read: 1, edit: 1 } },
				subagents: { count: 0, failed: 0, apis: {} },
				other: { count: 0, failed: 0, apis: {} },
			},
			guards: { Loki: { checks: 3, blocked: 1, unchecked: 0, afterChecks: 2, afterFindings: 1, ms: 1500 } },
			secretsMasked: 1,
			nudges: { toolRounds: 1, wait: 0, skill: 0 },
			usageLimitBlocks: 1,
		}),
	);
	return session;
}

/** A root that fanned out frames on a cheaper, unpriced model, written before Ultron kept runtime counters. */
export function framesOnlySession(): SessionFixture {
	const session = new SessionFixture("bbbbbbbb-0002-7000-8000-000000000002", "/home/dev/app");
	session.laneModel("main", "priced", "model-a");
	session.run({
		lane: "main",
		responses: [
			{
				cells: [{ code: 'labels = await rlm.map("label this", items, contract=str)' }],
				usage: { input: 500, output: 50, cost: 0.01 },
			},
			{ usage: { input: 600, output: 60, cost: 0.02 } },
		],
	});
	const statuses = ["complete", "incomplete", "error"];
	for (let index = 1; index <= 3; index += 1) {
		const lane = `ultron.rlm-frame.ultron-task-f${index}`;
		session.laneModel(lane, "cheap", "mini");
		session.run({ lane, provider: "cheap", model: "mini", responses: [{ usage: { input: 200, output: 20 } }] });
		session.frame(`frame-${index}`, {
			status: statuses[index - 1],
			...(index === 2 ? { reason: "budget_exhausted" } : {}),
			taskId: `ultron-task-f${index}`,
			lane,
			model: "cheap/mini",
			spent: { calls: 1, tokens: 220 },
		});
	}
	session.tasks([
		{
			id: "ultron-task-f1",
			definition: "rlm-frame@1",
			state: "completed",
			result: { status: "succeeded", verification: "unverified" },
		},
		{
			id: "ultron-task-f2",
			definition: "rlm-frame@1",
			state: "completed",
			result: { status: "succeeded", verification: "unverified" },
		},
		{
			id: "ultron-task-f3",
			definition: "rlm-frame@1",
			state: "failed",
			result: { status: "failed", verification: "unverified" },
		},
		// Cancelled before its frame began: it has a task and no trace.
		{
			id: "ultron-task-f4",
			definition: "rlm-frame@1",
			state: "cancelled",
			result: { status: "cancelled", verification: "unverified" },
		},
	]);
	return session;
}

/** Nested subagents with every verdict outcome, two worktree children, a frame called by a subagent, other tasks. */
export function subagentSession(): SessionFixture {
	const session = new SessionFixture("cccccccc-0003-7000-8000-000000000003", "/home/dev/lib");
	session.laneModel("main", "priced", "model-a");
	session.run({
		lane: "main",
		responses: [
			{
				cells: [
					{
						code: 'a = await rlm.spawn("fix", name="a", depth=1)\nw = await rlm.spawn("edit", name="w", worktree=True)\nawait rlm.collect([a, w])\nawait rlm.merge([w])',
					},
					{ code: "out = await workflows.run(nodes)" },
				],
				usage: { input: 3000, output: 300, cost: 0.1 },
			},
			{ usage: { input: 3500, output: 100, cost: 0.1 } },
		],
	});
	const child = (task: string, model: string, cells: string[] = []) => {
		const lane = `ultron.rlm-child.ultron-task-${task}`;
		session.laneModel(lane, "priced", model);
		session.run({
			lane,
			model,
			responses: [
				...cells.map((code) => ({ cells: [{ code }], usage: { input: 1000, output: 100, cost: 0.05 } })),
				{ usage: { input: 1000, output: 100, cost: 0.05 } },
			],
		});
	};
	child("a", "model-a", ['b = await rlm.spawn("part", name="b")\nv = await rlm.infer("judge", context=[x])']);
	child("b", "model-b");
	child("c", "model-a");
	child("w", "model-a", ['await write("parser.py", text)']);
	child("e", "model-a");
	session.laneModel("ultron.rlm-child.ultron-task-d", "priced", "model-a");
	const frameLane = "ultron.rlm-frame.ultron-task-f";
	session.laneModel(frameLane, "priced", "model-b");
	session.run({ lane: frameLane, model: "model-b", responses: [{ usage: { input: 300, output: 30, cost: 0.01 } }] });
	session.frame("frame-of-a", {
		taskId: "ultron-task-f",
		lane: frameLane,
		model: "priced/model-b",
		callerTaskId: "ultron-task-a",
		spent: { calls: 1, tokens: 330 },
	});
	const ended = (status: string, extra: Record<string, unknown> = {}) => ({
		status,
		verification: "unverified",
		...extra,
	});
	const verdict = (status: string) => ({ status, summary: "s", outputs: {}, evidence: ["exit 0"], changed_files: [] });
	const check = (outcome: string) => ({ outcome, unobserved: [], unreported: [], unlisted: [], concurrent: [] });
	session.tasks([
		{
			id: "ultron-task-a",
			definition: "rlm-child@1",
			state: "completed",
			result: ended("succeeded", { value: "done", verdict: verdict("passed"), check: check("verified") }),
		},
		{
			id: "ultron-task-b",
			definition: "rlm-child@1",
			state: "completed",
			parentId: "ultron-task-a",
			result: ended("succeeded", { value: "done", verdict: verdict("passed"), check: check("contradicted") }),
		},
		{
			id: "ultron-task-c",
			definition: "rlm-child@1",
			state: "failed",
			// Its rlm.finish calls were all rejected: no verdict, and the check says so.
			result: ended("failed", {
				error: "gave up",
				verdict: null,
				unverified: true,
				check: { ...check("invalid"), problems: ["evidence names no command outcome or file"] },
			}),
		},
		{ id: "ultron-task-d", definition: "rlm-child@1", state: "running" },
		{
			id: "ultron-task-w",
			definition: "rlm-child@1",
			state: "completed",
			result: ended("succeeded", {
				value: "done",
				verdict: verdict("passed"),
				check: check("unchecked"),
				worktree: {
					task: "ultron-task-w",
					branch: "ultron/cccccccc/fix-parser",
					path: "/tmp/wt",
					repo: "/home/dev/lib",
					base: "abc",
					commit: "def",
					changed_files: ["parser.py", "test_parser.py"],
					diffstat: "2 files changed",
				},
			}),
		},
		{
			id: "ultron-task-e",
			definition: "rlm-child@1",
			state: "completed",
			result: ended("succeeded", {
				value: "nothing to change",
				verdict: null,
				unverified: true,
				worktree: {
					task: "ultron-task-e",
					branch: "ultron/cccccccc/noop",
					path: "/tmp/wt2",
					repo: "/home/dev/lib",
					base: "abc",
					commit: null,
					changed_files: [],
					diffstat: "",
					removed: true,
				},
			}),
		},
		{
			id: "ultron-task-t",
			definition: "correctness-reviewer@1",
			state: "completed",
			result: ended("succeeded", { value: {} }),
		},
		{ id: "ultron-task-j", definition: "background-job@1", state: "cancelled", result: ended("cancelled") },
		{
			id: "ultron-task-f",
			definition: "rlm-frame@1",
			state: "completed",
			parentId: "ultron-task-a",
			result: ended("succeeded", { value: "1" }),
		},
	]);
	session.value(
		"ultron.module",
		"stats",
		fixtureStats({
			hostCalls: { "workflows.run": 2, "rlm.infer": 1, "rlm.spawn": 3 },
			merges: { "ultron-task-w": "merged" },
			childModels: { "ultron-task-a": "priced/model-a" },
		}),
	);
	return session;
}

/** `ultron --claude`: the root lane and a subagent both on Claude Code, whose costs are notional. */
export function claudeRootSession(): SessionFixture {
	const session = new SessionFixture("dddddddd-0004-7000-8000-000000000004", "/home/dev/app");
	session.laneModel("main", "claude-code", "claude-opus-5-5");
	session.value("ultron.claude-code.lanes", "main", { sessionId: "cc-session", model: "claude-opus-5-5" });
	session.run({
		lane: "main",
		provider: "claude-code",
		model: "claude-opus-5-5",
		responses: [
			{
				cells: [{ code: 'h = await rlm.spawn("look", name="scout")\nawait rlm.collect([h])' }],
				usage: { input: 10, output: 500, cacheRead: 40_000, cacheWrite: 9000, cost: 1.5 },
			},
			{ usage: { input: 12, output: 300, cacheRead: 50_000, cost: 0.5 } },
		],
	});
	const lane = "ultron.rlm-child.ultron-task-s";
	session.laneModel(lane, "claude-code", "claude-opus-5-5");
	session.run({
		lane,
		provider: "claude-code",
		model: "claude-opus-5-5",
		responses: [{ usage: { input: 5, output: 200, cacheWrite: 8000, cost: 0.25 } }],
	});
	session.tasks([
		{
			id: "ultron-task-s",
			definition: "rlm-child@1",
			state: "completed",
			result: { status: "succeeded", value: "found", verification: "unverified", verdict: null, unverified: true },
		},
	]);
	session.value("ultron.module", "stats", fixtureStats());
	return session;
}

/**
 * `ultron claude`: Claude Code owns the root conversation, so the session has no root entries. Its frames ran on a
 * lane; its subagents were Claude Code processes, known only through the task journal and the usage ledger.
 * `counters` adds the runtime counters a current Ultron keeps; without them the session reads like one written
 * before they existed.
 */
export function claudeHostSession(counters: boolean): SessionFixture {
	const session = new SessionFixture(
		counters ? "ffffffff-0006-7000-8000-000000000006" : "eeeeeeee-0005-7000-8000-000000000005",
		"/home/dev/app",
	);
	session.value("pi.session.name", "", "claude code");
	session.laneModel("main", "claude-code", "claude-opus-5-5");
	session.value("pi.branch.tip", "main", null);
	const frameLane = "ultron.rlm-frame.ultron-task-f1";
	session.laneModel(frameLane, "claude-code", "claude-haiku-5");
	session.run({
		lane: frameLane,
		provider: "claude-code",
		model: "claude-haiku-5",
		responses: [{ usage: { input: 2, output: 150, cacheWrite: 10_000, cost: 0.11 } }],
	});
	session.frame("frame-1", {
		taskId: "ultron-task-f1",
		lane: frameLane,
		model: "claude-code/claude-haiku-5",
		spent: { calls: 1, tokens: 10_152 },
	});
	session.tasks([
		{
			id: "ultron-task-f1",
			definition: "rlm-frame@1",
			state: "completed",
			result: { status: "succeeded", verification: "unverified" },
		},
		{
			id: "ultron-task-x",
			definition: "rlm-child@1",
			state: "completed",
			result: {
				status: "succeeded",
				value: "done",
				verification: "unverified",
				verdict: { status: "passed", summary: "s", outputs: {}, evidence: ["exit 0"], changed_files: [] },
				check: { outcome: "verified", unobserved: [], unreported: [], unlisted: [], concurrent: [] },
			},
		},
		{
			id: "ultron-task-y",
			definition: "rlm-child@1",
			state: "cancelled",
			result: { status: "cancelled", verification: "unverified" },
		},
	]);
	const call = (id: string, kind: string, taskId: string, usage: Record<string, unknown>) => ({
		id,
		reservationId: `r-${id}`,
		rootId: "turn:cc-abc-1",
		kind,
		taskId,
		admittedAt: FIXTURE_START,
		settledAt: FIXTURE_START + 1000,
		status: "succeeded",
		usage: { inputTokens: null, outputTokens: null, totalTokens: null, cost: null, wallMs: 1000, ...usage },
	});
	session.value("ultron.usage", "root", {
		version: 1,
		roots: {
			"turn:cc-abc-1": {
				rootId: "turn:cc-abc-1",
				startedAt: FIXTURE_START,
				deadlineAt: FIXTURE_START + 1_800_000,
				reservations: [],
				calls: [
					call("c1", "task", "ultron-task-x", {}),
					call("c2", "model", "ultron-task-x", {
						inputTokens: 900,
						outputTokens: 100,
						totalTokens: 1000,
						cost: 0.5,
					}),
					call("c3", "task", "ultron-task-y", {}),
					call("c4", "model", "ultron-task-y", {}),
				],
				turns: { count: 3, tokens: 11_152, lastAt: FIXTURE_START + 2000, cost: 0.61, unpriced: 1 },
			},
		},
	});
	session.value("ultron.module", "hints", { version: 1, lanes: { main: { muted: [], fired: { "poll-loop": 1 } } } });
	if (counters)
		session.value(
			"ultron.module",
			"stats",
			fixtureStats({
				root: "external",
				cells: {
					root: { count: 7, failed: 1, apis: { bash: 5, "rlm.spawn": 1, "rlm.map": 1 } },
					subagents: { count: 0, failed: 0, apis: {} },
					other: { count: 0, failed: 0, apis: {} },
				},
				hostCalls: { "rlm.map": 1, "rlm.spawn": 2 },
				secretsMasked: 2,
				guards: { Loki: { checks: 4, blocked: 0, unchecked: 1, afterChecks: 7, afterFindings: 0, ms: 2400 } },
				nudges: { toolRounds: 0, wait: 1, skill: 0 },
				childModels: { "ultron-task-x": "claude-code/sonnet", "ultron-task-y": "claude-code/sonnet" },
				externalTurns: { count: 3, wallMs: 95_000 },
			}),
		);
	return session;
}
