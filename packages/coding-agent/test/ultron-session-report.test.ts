/**
 * The session report (`/usage`, `ultron usage`) on fixture session files, in the shapes each mode writes: a native
 * root that never delegated, frames only, nested subagents with verdicts and worktrees, `ultron --claude`, and
 * `ultron claude` with and without the runtime counters. What a session did not keep must come out as null with a
 * reason, never as zero.
 */
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, test } from "vitest";
import { readSessionHeader, readSessionLog, SessionLogError } from "../src/ultron/session-log.ts";
import {
	buildSessionReport,
	depthVerdict,
	isSessionReport,
	SESSION_REPORT_SCHEMA,
	type SessionReport,
	sessionIsEmpty,
} from "../src/ultron/session-report.ts";
import {
	formatCost,
	formatTokens,
	renderSessionReport,
	renderSessionTable,
} from "../src/ultron/session-report-text.ts";
import { parseSessionStats } from "../src/ultron/session-stats.ts";
import {
	claudeHostSession,
	claudeRootSession,
	framesOnlySession,
	rootOnlySession,
	subagentSession,
} from "./support/report-sessions.ts";
import { FIXTURE_START, fixtureStats, SessionFixture } from "./support/session-fixture.ts";

const directories: string[] = [];
afterEach(() => {
	for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function sessionsRoot(): string {
	const directory = mkdtempSync(join(tmpdir(), "ultron-session-report-"));
	directories.push(directory);
	return directory;
}

async function reportOf(session: SessionFixture, options: { torn?: boolean } = {}): Promise<SessionReport> {
	const path = session.write(sessionsRoot(), { modifiedAt: FIXTURE_START + 3_600_000, ...options });
	return buildSessionReport(await readSessionLog(path));
}

const TEXT = { utc: true, home: "/home/dev" };

describe("a native session that only used the root", () => {
	test("turns, cells with their APIs, cost, guardrails and memory", async () => {
		const report = await reportOf(rootOnlySession());
		expect(report.schema).toBe(SESSION_REPORT_SCHEMA);
		expect(isSessionReport(JSON.parse(JSON.stringify(report)))).toBe(true);
		expect(report.mode).toBe("ultron");
		expect(report.session).toMatchObject({ cwd: "/home/dev/app", name: null, createdAt: FIXTURE_START });
		expect(report.turns).toEqual({ count: 2, completed: 1, aborted: 1, failed: 0, running: 0, wallMs: 120_000 });
		expect(report.root.models).toEqual([{ model: "priced/model-a", responses: 4 }]);
		expect(report.cells).toMatchObject({
			source: "transcript",
			total: { count: 3, failed: 1, apis: { bash: 1, read: 1, edit: 1 } },
			root: { count: 3, failed: 1 },
			subagents: { count: 0 },
		});
		expect(report.depth.verdict).toBe("root only");
		expect(report.depth.level).toBe(0);
		expect(report.depth.frames.count).toBe(0);
		expect(report.depth.subagents.count).toBe(0);
		expect(report.depth.workflows).toEqual({ runs: 0 });
		expect(report.usage.lanes.root).toMatchObject({ responses: 4, input: 5500, output: 360, cacheRead: 500 });
		expect(report.usage.lanes.root.totalTokens).toBe(6360);
		expect(report.usage.total.cost.reportedUsd).toBeCloseTo(0.1, 9);
		expect(report.usage.total.cost).toMatchObject({ subscriptionUsd: null, unpricedResponses: 0 });
		expect(report.usage.lanes.frames.responses).toBe(0);
		expect(report.guardrails).toEqual({
			guards: { Loki: { checks: 3, blocked: 1, unchecked: 0, afterChecks: 2, afterFindings: 1, ms: 1500 } },
			secretsMasked: 1,
			hints: { "stuck-loop": 1, "output-truncated": 2 },
			nudges: { toolRounds: 1, wait: 0, skill: 0 },
			usageLimitBlocks: 1,
			countersSince: null,
		});
		// Jev decisions an older session stored are ignored; the memory section is the store's own operations.
		expect(report.memory).toEqual({
			operations: { "prepare.recalled": 1, "prepare.skipped": 1, "propose.stored": 1 },
		});
		expect(report.unrecorded).toEqual({});
	});

	test("the printed report", async () => {
		const report = await reportOf(rootOnlySession());
		const lines = renderSessionReport(report, TEXT);
		expect(lines[1]).toMatch(/^~\/app {2}· {2}2026-09-01 10:00 to 2026-09-01 11:00 UTC {2}· {2}\d\.\d kB$/);
		expect([lines[0], ...lines.slice(2)]).toEqual([
			"Session aaaaaaaa-0001-7000-8000-000000000001  [ultron]",
			"",
			"Depth      root only",
			"Turns      2 turns, 2m00s wall (1 aborted)",
			"Root       priced/model-a (4 responses)",
			"Cells      3 cells, 1 failed",
			"           bash 1 · read 1 · edit 1",
			"Frames     0",
			"Sub-agents 0",
			"Workflows  0 runs",
			"Other work 0 typed-agent tasks · 0 background jobs",
			"",
			"Tokens and cost",
			"  lane        responses  input  output  cache read  cache write  total  cost",
			"  root                4   5.5k     360         500            0   6.4k  $0.100",
			"  frames              0      0       0           0            0      0  -",
			"  sub-agents          0      0       0           0            0      0  -",
			"  total               4   5.5k     360         500            0   6.4k  $0.100",
			"  model           responses  input  output  cache read  cache write  total  cost",
			"  priced/model-a          4   5.5k     360         500            0   6.4k  $0.100",
			"",
			"Guardrails",
			"  Loki     3 checks, 1 blocked, 0 unchecked · after cells: 2 checks, 1 finding · 2s",
			"  Secrets  1 masked",
			"  Hints    stuck-loop 1 · output-truncated 2",
			"  Nudges   tool rounds 1 · wait 0 · skill 0",
			"  Limits   1 usage-limit block",
			"",
			"Memory",
			"  Store    prepare.recalled 1 · prepare.skipped 1 · propose.stored 1",
		]);
	});

	test("a torn last line (a write in progress) is skipped; streamed-frame lists are never parsed", async () => {
		const session = rootOnlySession().pendingFrame();
		const path = session.write(sessionsRoot(), { torn: true });
		const log = await readSessionLog(path);
		expect(log.unreadableLines).toBe(1);
		// Operation metadata is deleted when a run ends; the log still knows which lane ran it.
		expect([...log.operations.values()].map((operation) => operation.lane)).toEqual(["main", "main"]);
		expect(log.values.has("pi.op.meta/op-0001")).toBe(false);
		expect(buildSessionReport(log).turns?.count).toBe(2);
	});

	test("a turn still running counts as a turn", async () => {
		const session = rootOnlySession();
		session.value("pi.op.meta", "op-live", {
			operationId: "op-live",
			lane: "main",
			startedAt: 1,
			intent: { kind: "run" },
		});
		expect((await reportOf(session)).turns).toMatchObject({ count: 3, running: 1 });
	});
});

describe("frames only", () => {
	test("depth 1 with frame outcomes, their model and an unknown cost that is not zero", async () => {
		const report = await reportOf(framesOnlySession());
		expect(report.depth.verdict).toBe("depth 1: 4 frames, 0 sub-agents");
		expect(report.depth.level).toBe(1);
		expect(report.depth.frames).toMatchObject({
			count: 4,
			complete: 1,
			incomplete: 1,
			failed: 1,
			cancelled: 1,
			running: 0,
			nested: 0,
			incompleteReasons: { budget_exhausted: 1 },
			budgetTokens: 660,
		});
		expect(report.depth.frames.byModel).toEqual([
			{ model: "cheap/mini", count: 3, tokens: 660, unmeasured: 0 },
			{ model: "not recorded", count: 1, tokens: 0, unmeasured: 0 },
		]);
		// Written before the runtime counters: how many map and infer calls made the frames is not known.
		expect(report.depth.frames.calls).toBeNull();
		expect(report.depth.workflows).toBeNull();
		expect(report.guardrails.guards).toBeNull();
		expect(report.guardrails.usageLimitBlocks).toBeNull();
		expect(Object.keys(report.unrecorded).sort()).toEqual([
			"depth.frames.calls",
			"depth.workflows",
			"guardrails.guards",
			"guardrails.usageLimitBlocks",
			"memory.operations",
		]);
		// Still read from the transcript.
		expect(report.cells?.total).toMatchObject({ count: 1, apis: { "rlm.map": 1 } });
		expect(report.guardrails.secretsMasked).toBe(0);
		expect(report.guardrails.nudges).toEqual({ toolRounds: 0, wait: 0, skill: 0 });
		expect(report.usage.lanes.frames).toMatchObject({ responses: 3, totalTokens: 660 });
		expect(report.usage.lanes.frames.cost).toEqual({
			reportedUsd: null,
			subscriptionUsd: null,
			unpricedResponses: 3,
		});
		expect(report.usage.total.cost.reportedUsd).toBeCloseTo(0.03, 9);
		expect(report.usage.total.cost.unpricedResponses).toBe(3);
		expect(report.usage.models.map((model) => model.model)).toEqual(["priced/model-a", "cheap/mini"]);
		const text = renderSessionReport(report, TEXT).join("\n");
		expect(text).toContain("Frames     4: 1 complete, 1 incomplete, 1 failed, 1 cancelled · 660 tokens");
		expect(text).toContain("incomplete: budget_exhausted 1");
		expect(text).toContain("  frames              3    600      60           0            0    660  unknown");
		expect(text).toContain("$0.030 + unknown (3 unpriced)");
		expect(text).toContain("Workflows  not recorded (this session predates Ultron's runtime counters)");
		expect(text).toContain("  Loki     not recorded");
		expect(text).not.toMatch(/\$0\.000/);
	});
});

describe("subagents", () => {
	test("nesting, verdict checks, worktrees and merges, a frame called by a subagent, other tasks", async () => {
		const report = await reportOf(subagentSession());
		expect(report.depth.verdict).toBe(
			"depth 2: 1 frame, 6 sub-agents (1 nested), 1 typed-agent task, 1 background job",
		);
		expect(report.depth.level).toBe(2);
		expect(report.depth.subagents).toMatchObject({
			count: 6,
			maxDepth: 2,
			nested: 1,
			completed: 4,
			failed: 1,
			cancelled: 0,
			interrupted: 0,
			running: 1,
			verdicts: { verified: 1, contradicted: 1, unverified: 3, invalid: 1, unchecked: 1, none: 1 },
			claims: { passed: 3, failed: 0, blocked: 0 },
		});
		expect(report.depth.subagents.byModel).toEqual([
			{ model: "priced/model-a", count: 5, tokens: 2200 + 1100 + 2200 + 1100, unmeasured: 0 },
			{ model: "priced/model-b", count: 1, tokens: 1100, unmeasured: 0 },
		]);
		expect(report.depth.subagents.worktrees).toEqual([
			{ task: "ultron-task-w", branch: "ultron/cccccccc/fix-parser", changedFiles: 2, merge: "merged" },
			{ task: "ultron-task-e", branch: "ultron/cccccccc/noop", changedFiles: 0, merge: "empty" },
		]);
		// The frame was called by subagent a: it sits one level below it.
		expect(report.depth.frames).toMatchObject({ count: 1, complete: 1, nested: 1, calls: { infer: 1, map: 0 } });
		expect(report.depth.workflows).toEqual({ runs: 2 });
		expect(report.depth.typedAgents).toEqual({ count: 1, byDefinition: { "correctness-reviewer": 1 } });
		expect(report.depth.backgroundJobs).toEqual({ count: 1, completed: 0, failed: 0, cancelled: 1, running: 0 });
		expect(report.cells).toMatchObject({
			root: { count: 2, apis: { "rlm.spawn": 1, "rlm.collect": 1, "rlm.merge": 1, "workflows.run": 1 } },
			subagents: { count: 2, apis: { write: 1, "rlm.infer": 1, "rlm.spawn": 1 } },
			total: { count: 4 },
		});
		expect(report.usage.lanes.root.responses).toBe(2);
		expect(report.usage.lanes.subagents.responses).toBe(7);
		expect(report.usage.lanes.frames.responses).toBe(1);
		expect(report.usage.total.responses).toBe(10);
		expect(report.usage.total.cost.reportedUsd).toBeCloseTo(0.2 + 7 * 0.05 + 0.01, 9);
		const text = renderSessionReport(report, TEXT).join("\n");
		expect(text).toContain("Sub-agents 6: 4 completed, 1 failed, 1 running · max depth 2 (1 nested)");
		expect(text).toContain(
			"verdicts: 1 verified · 1 contradicted · 3 unverified (1 without a verdict, 1 unchecked, 1 invalid)",
		);
		expect(text).toContain("worktree ultron/cccccccc/fix-parser: 2 files, merge merged");
		expect(text).toContain("worktree ultron/cccccccc/noop: 0 files, merge empty");
		expect(text).toContain("Frames     1: 1 complete · 1 nested · 330 tokens");
		expect(text).toContain("root 2 · sub-agents 2");
	});

	test("a merge the session did not record is not reported as unmerged", async () => {
		const session = subagentSession();
		session.value("ultron.module", "stats", fixtureStats());
		const report = await reportOf(session);
		expect(report.depth.subagents.worktrees[0]).toMatchObject({ branch: "ultron/cccccccc/fix-parser", merge: null });
		expect(renderSessionReport(report, TEXT).join("\n")).toContain("fix-parser: 2 files, merge not recorded");
	});

	test("depth verdict wording", () => {
		const none = { level: 0, frames: 0, subagents: 0, nestedSubagents: 0, typedAgents: 0, backgroundJobs: 0 };
		expect(depthVerdict(none)).toBe("root only");
		expect(depthVerdict({ ...none, level: 1, frames: 99 })).toBe("depth 1: 99 frames, 0 sub-agents");
		expect(depthVerdict({ ...none, level: 2, subagents: 6, nestedSubagents: 2 })).toBe(
			"depth 2: 0 frames, 6 sub-agents (2 nested)",
		);
		expect(depthVerdict({ ...none, level: 1, subagents: 1, frames: 1 })).toBe("depth 1: 1 frame, 1 sub-agent");
	});
});

describe("ultron --claude", () => {
	test("the root lane on Claude Code: recorded like a native session, with notional (subscription) cost", async () => {
		const report = await reportOf(claudeRootSession());
		expect(report.mode).toBe("ultron --claude");
		expect(report.turns).toMatchObject({ count: 1, completed: 1 });
		expect(report.root.models).toEqual([{ model: "claude-code/claude-opus-5-5", responses: 2 }]);
		expect(report.cells).toMatchObject({ source: "transcript", total: { count: 1 } });
		expect(report.depth.verdict).toBe("depth 1: 0 frames, 1 sub-agent");
		expect(report.depth.subagents.byModel).toEqual([
			{ model: "claude-code/claude-opus-5-5", count: 1, tokens: 8205, unmeasured: 0 },
		]);
		expect(report.usage.lanes.root.cost).toEqual({ reportedUsd: null, subscriptionUsd: 2, unpricedResponses: 0 });
		expect(report.usage.total.cost).toEqual({ reportedUsd: null, subscriptionUsd: 2.25, unpricedResponses: 0 });
		const text = renderSessionReport(report, TEXT).join("\n");
		expect(text).toContain("[ultron --claude]");
		expect(text).toContain("$2.250 (sub)");
		expect(text).toContain("(sub): made on a subscription login");
	});

	test("a provider the worker knows to be on a subscription login is priced as one", async () => {
		const path = rootOnlySession().write(sessionsRoot());
		const report = buildSessionReport(await readSessionLog(path), { subscriptionProviders: ["priced"] });
		expect(report.usage.total.cost.reportedUsd).toBeNull();
		expect(report.usage.total.cost.subscriptionUsd).toBeCloseTo(0.1, 9);
	});
});

describe("ultron claude", () => {
	test("written before the counters: depth is known, the root's turns and cells are 'not recorded'", async () => {
		const report = await reportOf(claudeHostSession(false));
		expect(report.mode).toBe("ultron claude");
		expect(report.session.name).toBe("claude code");
		expect(report.turns).toBeNull();
		expect(report.cells).toBeNull();
		expect(report.root.models).toEqual([]);
		expect(report.guardrails).toMatchObject({
			guards: null,
			secretsMasked: null,
			nudges: null,
			usageLimitBlocks: null,
		});
		expect(report.guardrails.hints).toEqual({ "poll-loop": 1 });
		for (const path of [
			"turns",
			"cells",
			"root.models",
			"guardrails.guards",
			"guardrails.secretsMasked",
			"guardrails.nudges",
		])
			expect(report.unrecorded[path], path).toEqual(expect.any(String));
		// The task journal, frame traces and usage ledger are kept in every mode.
		expect(report.depth.verdict).toBe("depth 1: 1 frame, 2 sub-agents");
		expect(report.depth.frames.byModel).toEqual([
			{ model: "claude-code/claude-haiku-5", count: 1, tokens: 10_152, unmeasured: 0 },
		]);
		expect(report.depth.subagents).toMatchObject({ count: 2, completed: 1, cancelled: 1, maxDepth: 1 });
		expect(report.depth.subagents.verdicts).toMatchObject({ verified: 1, unverified: 1, none: 1 });
		// The subagents were Claude Code processes: one reported usage to the ledger, the cancelled one did not.
		expect(report.depth.subagents.byModel).toEqual([
			{ model: "claude-code/(model not recorded)", count: 2, tokens: 1000, unmeasured: 1 },
		]);
		expect(report.usage.lanes.root.responses).toBe(0);
		expect(report.usage.lanes.subagents).toMatchObject({ responses: 1, totalTokens: 1000, unmeasured: 1 });
		expect(report.usage.lanes.subagents.cost).toEqual({
			reportedUsd: null,
			subscriptionUsd: 0.5,
			unpricedResponses: 0,
		});
		expect(report.usage.total.cost.subscriptionUsd).toBeCloseTo(0.61, 9);
		const lines = renderSessionReport(report, TEXT);
		const text = lines.join("\n");
		expect(text).toContain("Turns      not recorded (");
		expect(text).toContain("Cells      not recorded (");
		expect(text).toContain("Root       not recorded (");
		expect(text).toMatch(/^ {2}root\s+n\/r\s+n\/r\s+n\/r\s+n\/r\s+n\/r\s+n\/r\s+n\/r$/m);
		expect(text).toContain("claude-code/(model not recorded)  2 sub-agents  1.0k tokens (1 not recorded)");
		expect(text).toContain("+?: 1 sub-agent reported no usage");
		// Nothing unknown is printed as a zero count.
		expect(lines.filter((line) => /^(Turns|Cells)\s+0\b/.test(line))).toEqual([]);
	});

	test("a session whose root never delegated has no usage of its own: the table says n/r, not 0", async () => {
		const session = new SessionFixture("12121212-0012-7000-8000-000000000012", "/home/dev/app");
		session.value("pi.session.name", "", "claude code").laneModel("main", "claude-code", "claude-opus-5-5");
		session.value("ultron.module", "jobs", { version: 1, jobs: [] });
		const report = await reportOf(session);
		expect(report.mode).toBe("ultron claude");
		expect(report.depth.verdict).toBe("root only");
		expect(report.usage.total.responses).toBe(0);
		const row = renderSessionTable([report], TEXT)[1]!;
		expect(row.split(/\s{2,}/).slice(3)).toEqual([
			"ultron claude",
			"Claude Code",
			"n/r",
			"n/r",
			"0",
			"0",
			"n/r",
			"n/r",
			"root only",
		]);
	});

	test("with the runtime counters: turns, cells, masked secrets and subagent models are recorded", async () => {
		const report = await reportOf(claudeHostSession(true));
		expect(report.mode).toBe("ultron claude");
		expect(report.turns).toEqual({ count: 3, completed: 3, aborted: 0, failed: 0, running: 0, wallMs: 95_000 });
		expect(report.cells).toMatchObject({
			source: "counters",
			total: { count: 7, failed: 1, apis: { bash: 5, "rlm.map": 1, "rlm.spawn": 1 } },
		});
		expect(report.depth.frames.calls).toEqual({ infer: 0, map: 1 });
		expect(report.depth.subagents.byModel).toEqual([
			{ model: "claude-code/sonnet", count: 2, tokens: 1000, unmeasured: 1 },
		]);
		expect(report.guardrails).toMatchObject({
			guards: { Loki: { checks: 4, unchecked: 1, afterChecks: 7 } },
			secretsMasked: 2,
			nudges: { toolRounds: 0, wait: 1, skill: 0 },
			usageLimitBlocks: 0,
		});
		expect(Object.keys(report.unrecorded).sort()).toEqual(["memory.operations", "root.models"]);
		const text = renderSessionReport(report, TEXT).join("\n");
		expect(text).toContain("Turns      3 turns, 1m35s wall");
		expect(text).toContain("Cells      7 cells, 1 failed  (runtime count)");
	});
});

describe("counters that began after the session did", () => {
	test("say since when, and leave what they missed unrecorded", async () => {
		const session = rootOnlySession();
		session.value(
			"ultron.module",
			"stats",
			fixtureStats({
				since: FIXTURE_START + 30 * 60_000,
				guards: { Loki: { checks: 1, blocked: 0, unchecked: 0, afterChecks: 0, afterFindings: 0, ms: 10 } },
				hostCalls: { "workflows.run": 1 },
			}),
		);
		const report = await reportOf(session);
		expect(report.guardrails.countersSince).toBe(FIXTURE_START + 30 * 60_000);
		expect(report.guardrails.guards).toMatchObject({ Loki: { checks: 1 } });
		expect(report.depth.workflows).toBeNull();
		expect(report.unrecorded["depth.workflows"]).toBe("the runtime counters began after this session did");
		expect(renderSessionReport(report, TEXT).join("\n")).toContain("(counted since 2026-09-01 10:30)");
	});

	test("an `ultron claude` session resumed by a counting Ultron: its turns and cells are a count since then", async () => {
		const session = claudeHostSession(false);
		session.value(
			"ultron.module",
			"stats",
			fixtureStats({
				root: "external",
				since: FIXTURE_START + 30 * 60_000,
				cells: {
					root: { count: 2, failed: 0, apis: { bash: 2 } },
					subagents: { count: 0, failed: 0, apis: {} },
					other: { count: 0, failed: 0, apis: {} },
				},
				externalTurns: { count: 1, wallMs: 20_000 },
			}),
		);
		const report = await reportOf(session);
		expect(report.turns?.count).toBe(1);
		expect(report.cells?.total.count).toBe(2);
		expect(report.guardrails.countersSince).toBe(FIXTURE_START + 30 * 60_000);
		const text = renderSessionReport(report, TEXT).join("\n");
		expect(text).toContain("Turns      1 turn, 20s wall (counted since 2026-09-01 10:30)");
		expect(text).toContain("Cells      2 cells, 0 failed  (runtime count) (counted since 2026-09-01 10:30)");
	});
});

describe("usage that is not a model response on a lane", () => {
	test("a compaction's usage goes to its lane; a row with no entry (imported history) is kept apart", async () => {
		const session = rootOnlySession();
		const usage = {
			input: 400,
			output: 40,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 440,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0.005 },
		};
		session.raw(
			{
				kind: "entry",
				id: "compaction-1",
				parentId: session.tip("main"),
				type: "compaction",
				summary: "s",
				retainedTail: [],
				tokensBefore: 1,
				fromHook: false,
				timestamp: FIXTURE_START + 500_000,
			},
			{ kind: "usage", id: "u-c1", usage, entryId: "compaction-1", adjustment: false },
		);
		session.raw({
			kind: "usage",
			id: "u-import",
			usage: { ...usage, cost: undefined },
			entryId: null,
			adjustment: true,
		});
		const report = await reportOf(session);
		expect(report.usage.lanes.root).toMatchObject({ responses: 5, totalTokens: 6360 + 440 });
		expect(report.usage.lanes.root.cost.reportedUsd).toBeCloseTo(0.105, 9);
		expect(report.usage.lanes.other).toMatchObject({ responses: 1, totalTokens: 440 });
		expect(report.usage.lanes.other.cost).toEqual({ reportedUsd: null, subscriptionUsd: null, unpricedResponses: 1 });
		expect(report.usage.models.map((model) => model.model)).toEqual(["priced/model-a", "imported history"]);
		// The root's models are the ones that answered, not the summary call.
		expect(report.root.models).toEqual([{ model: "priced/model-a", responses: 4 }]);
		expect(report.usage.total.responses).toBe(6);
	});
});

describe("session files that are not reportable", () => {
	test("a Pi-format file and a non-session file are refused by name", async () => {
		const directory = sessionsRoot();
		const pi = join(directory, "pi.jsonl");
		writeFileSync(
			pi,
			`${JSON.stringify({ type: "session", version: 3, id: "x", timestamp: "2026-01-01T00:00:00Z", cwd: "/" })}\n`,
		);
		await expect(readSessionLog(pi)).rejects.toThrow(SessionLogError);
		await expect(readSessionLog(pi)).rejects.toThrow(/Pi-format session \(version 3\)/);
		const other = join(directory, "other.jsonl");
		writeFileSync(other, '{"hello":1}\n');
		await expect(readSessionHeader(other)).rejects.toThrow(/not a native Ultron session file/);
		const empty = join(directory, "empty.jsonl");
		writeFileSync(empty, "");
		await expect(readSessionLog(empty)).rejects.toThrow(/is empty/);
	});

	test("a session in which nothing ran is empty; the skill catalog alone does not count", async () => {
		const session = new SessionFixture("00000000-0000-7000-8000-000000000000", "/home/dev/app");
		session.laneModel("main", "priced", "model-a").value("ultron.module", "skills", { format: 1, catalog: [] });
		const log = await readSessionLog(session.write(sessionsRoot()));
		expect(sessionIsEmpty(log)).toBe(true);
		const report = buildSessionReport(log);
		expect(report.depth.verdict).toBe("root only");
		expect(report.turns).toMatchObject({ count: 0 });
		// Nothing ran, so nothing was missed: the counters are zero rather than "not recorded".
		expect(report.guardrails).toMatchObject({
			guards: {},
			secretsMasked: 0,
			usageLimitBlocks: 0,
			countersSince: null,
		});
		expect(report.depth.workflows).toEqual({ runs: 0 });
		expect(Object.keys(report.unrecorded)).toEqual(["memory.operations"]);
		expect(sessionIsEmpty(await readSessionLog(rootOnlySession().write(sessionsRoot())))).toBe(false);
	});
});

describe("text helpers", () => {
	test("tokens and cost", () => {
		expect([0, 999, 1000, 12_345, 999_949, 999_950, 1_234_567, 123_456_789].map(formatTokens)).toEqual([
			"0",
			"999",
			"1.0k",
			"12.3k",
			"999.9k",
			"1.00M",
			"1.23M",
			"123.5M",
		]);
		const cost = (reportedUsd: number | null, subscriptionUsd: number | null, unpricedResponses = 0) => ({
			reportedUsd,
			subscriptionUsd,
			unpricedResponses,
		});
		expect(formatCost(cost(null, null), 0)).toBe("-");
		expect(formatCost(cost(null, null, 3), 100)).toBe("unknown");
		expect(formatCost(cost(1.23456, null), 100)).toBe("$1.235");
		expect(formatCost(cost(12.3456, null), 100)).toBe("$12.35");
		expect(formatCost(cost(null, 3.287), 100)).toBe("$3.287 (sub)");
		expect(formatCost(cost(null, 0), 100)).toBe("subscription");
		expect(formatCost(cost(0.5, 3.287, 2), 100)).toBe("$0.500 + $3.287 (sub) + unknown (2 unpriced)");
	});

	test("the table of sessions", async () => {
		const reports = [
			await reportOf(subagentSession()),
			await reportOf(framesOnlySession()),
			await reportOf(claudeHostSession(false)),
			await reportOf(claudeRootSession()),
			await reportOf(rootOnlySession()),
		];
		expect(renderSessionTable(reports, TEXT)).toEqual([
			"LAST ACTIVE       ID             CWD    MODE             MODEL                        TURNS  CELLS  FRAMES  SUBS    TOKENS  COST                           DEPTH",
			"2026-09-01 11:00  cccccccc-0003  ~/lib  ultron           priced/model-a                   1      4       1     6     14.9k  $0.560                         depth 2: 1 frame, 6 sub-agents (1 nested), 1 typed-agent task, 1 background job",
			"2026-09-01 11:00  bbbbbbbb-0002  ~/app  ultron           priced/model-a                   1      1       4     0      1.9k  $0.030 + unknown (3 unpriced)  depth 1: 4 frames, 0 sub-agents",
			"2026-09-01 11:00  eeeeeeee-0005  ~/app  ultron claude    Claude Code                    n/r    n/r       1     2  11.2k +?  $0.610 (sub)                   depth 1: 1 frame, 2 sub-agents",
			"2026-09-01 11:00  dddddddd-0004  ~/app  ultron --claude  claude-code/claude-opus-5-5      1      1       0     1    108.0k  $2.250 (sub)                   depth 1: 0 frames, 1 sub-agent",
			"2026-09-01 11:00  aaaaaaaa-0001  ~/app  ultron           priced/model-a                   2      3       0     0      6.4k  $0.100                         root only",
			"n/r: not recorded by that session (never zero). Tokens and cost of an `ultron claude` session leave out the root, which is Claude Code's.",
		]);
	});
});

describe("stored counters", () => {
	test("a malformed or newer document is not read as counters", () => {
		expect(parseSessionStats(undefined)).toBeUndefined();
		expect(parseSessionStats([])).toBeUndefined();
		expect(parseSessionStats({ version: 2 })).toBeUndefined();
		expect(
			parseSessionStats({ version: 1, cells: { root: { count: "x", failed: -1, apis: { bash: 2, bad: "y" } } } }),
		).toMatchObject({
			root: "lane",
			cells: { root: { count: 0, failed: 0, apis: { bash: 2 } }, subagents: { count: 0 } },
			secretsMasked: 0,
			merges: {},
		});
	});
});
