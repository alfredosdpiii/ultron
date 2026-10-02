/**
 * `ultron usage`: which session it reports (the current directory's, an id, a path, Claude Code's session id), the
 * table of recent sessions, the JSON shapes evals read, and its errors. It runs on fixture session files only: no
 * server, no model.
 */
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { SESSION_REPORT_SCHEMA } from "../src/ultron/session-report.ts";
import { runUsageCommand, SESSION_REPORT_LIST_SCHEMA } from "../src/ultron/usage-cli.ts";
import {
	claudeHostSession,
	claudeRootSession,
	framesOnlySession,
	rootOnlySession,
	subagentSession,
} from "./support/report-sessions.ts";
import { FIXTURE_START, SessionFixture } from "./support/session-fixture.ts";

let work: string;
let agentDir: string;
let sessionsRoot: string;
const paths: Record<string, string> = {};
const HOUR = 3_600_000;

beforeEach(() => {
	work = mkdtempSync(join(tmpdir(), "ultron-usage-cli-"));
	agentDir = join(work, "agent");
	sessionsRoot = join(agentDir, "experimental", "sessions");
	mkdirSync(sessionsRoot, { recursive: true });
	// Newest first: subagents, frames, `ultron claude`, `ultron --claude`, root only.
	paths.subagents = subagentSession().write(sessionsRoot, { modifiedAt: FIXTURE_START + 5 * HOUR });
	paths.frames = framesOnlySession().write(sessionsRoot, { modifiedAt: FIXTURE_START + 4 * HOUR });
	paths.claudeHost = claudeHostSession(false).write(sessionsRoot, { modifiedAt: FIXTURE_START + 3 * HOUR });
	paths.claudeRoot = claudeRootSession().write(sessionsRoot, { modifiedAt: FIXTURE_START + 2 * HOUR });
	paths.rootOnly = rootOnlySession().write(sessionsRoot, { modifiedAt: FIXTURE_START + 1 * HOUR });
	process.exitCode = undefined;
});

afterEach(() => {
	rmSync(work, { recursive: true, force: true });
	process.exitCode = undefined;
});

async function usage(
	args: string[],
	options: { cwd?: string; env?: NodeJS.ProcessEnv } = {},
): Promise<{ out: string[]; err: string[]; code: number | undefined }> {
	const out: string[] = [];
	const err: string[] = [];
	const handled = await runUsageCommand(
		["usage", ...args],
		{ stdout: (line) => out.push(line), stderr: (line) => err.push(line) },
		{ cwd: options.cwd ?? "/home/dev/app", env: options.env ?? {}, agentDir, home: "/home/dev" },
	);
	expect(handled).toBe(true);
	const code = process.exitCode as number | undefined;
	process.exitCode = undefined;
	return { out, err, code };
}

/** Every file under `directory` with its size and modification time: what a read-only command must leave alone. */
function snapshot(directory: string): string[] {
	const found: string[] = [];
	for (const entry of readdirSync(directory, { withFileTypes: true, recursive: true })) {
		if (!entry.isFile()) continue;
		const path = join(entry.parentPath, entry.name);
		const info = statSync(path);
		found.push(`${path} ${info.size} ${info.mtimeMs}`);
	}
	return found.sort();
}

describe("ultron usage", () => {
	test("is not a usage command: other arguments are left to the rest of the CLI", async () => {
		expect(await runUsageCommand(["--help"])).toBe(false);
		expect(await runUsageCommand(["migrate"])).toBe(false);
		expect(await runUsageCommand([])).toBe(false);
	});

	test("with no session: the most recent session of the current directory in which something ran", async () => {
		// A newer session of the same directory in which nothing ran is passed over.
		new SessionFixture("99999999-0009-7000-8000-000000000009", "/home/dev/app")
			.laneModel("main", "priced", "model-a")
			.write(sessionsRoot, { modifiedAt: FIXTURE_START + 9 * HOUR });
		const app = await usage(["--utc"]);
		expect(app.code).toBeUndefined();
		expect(app.err).toEqual([]);
		expect(app.out[0]).toBe("Session bbbbbbbb-0002-7000-8000-000000000002  [ultron]");
		expect(app.out).toContain("Depth      depth 1: 4 frames, 0 sub-agents");
		const lib = await usage([], { cwd: "/home/dev/lib" });
		expect(lib.out[0]).toBe("Session cccccccc-0003-7000-8000-000000000003  [ultron]");
		// Only empty sessions: the newest of them is reported rather than nothing.
		new SessionFixture("88888888-0008-7000-8000-000000000008", "/home/dev/empty")
			.laneModel("main", "priced", "model-a")
			.write(sessionsRoot);
		expect((await usage([], { cwd: "/home/dev/empty" })).out).toContain("Depth      root only");
		const nowhere = await usage([], { cwd: "/home/dev/nowhere" });
		expect(nowhere.code).toBe(1);
		expect(nowhere.err[0]).toMatch(/^Error: No session of \/home\/dev\/nowhere under /);
	});

	test("a session by id, id prefix or path", async () => {
		const full = await usage(["dddddddd-0004-7000-8000-000000000004"]);
		expect(full.out[0]).toContain("[ultron --claude]");
		const prefix = await usage(["eeee"]);
		expect(prefix.out[0]).toBe("Session eeeeeeee-0005-7000-8000-000000000005  claude code  [ultron claude]");
		const byPath = await usage([paths.rootOnly!], { cwd: "/elsewhere" });
		expect(byPath.out[0]).toBe("Session aaaaaaaa-0001-7000-8000-000000000001  [ultron]");
		// Two sessions share this prefix once a second one exists.
		claudeHostSession(true).write(sessionsRoot);
		new SessionFixture("ffffffff-0007-7000-8000-000000000007", "/home/dev/app").write(sessionsRoot);
		const ambiguous = await usage(["ffffffff"]);
		expect(ambiguous.code).toBe(1);
		expect(ambiguous.err[0]).toBe(
			'Error: 2 sessions match "ffffffff": ffffffff-0006-7000-8000-000000000006, ffffffff-0007-7000-8000-000000000007',
		);
		const missing = await usage(["nope"]);
		expect(missing.code).toBe(1);
		expect(missing.err[0]).toMatch(/^Error: No session file or session id "nope" \(looked in /);
	});

	test("an `ultron claude` session by Claude Code's session id", async () => {
		mkdirSync(join(agentDir, "claude-code", "sessions"), { recursive: true });
		writeFileSync(
			join(agentDir, "claude-code", "sessions", "cc-1234.json"),
			JSON.stringify({ id: "eeeeeeee-0005-7000-8000-000000000005", cwd: "/home/dev/app", path: paths.claudeHost }),
		);
		const found = await usage(["cc-1234"]);
		expect(found.code).toBeUndefined();
		expect(found.out[0]).toContain("[ultron claude]");
		expect(found.out).toContain("Depth      depth 1: 1 frame, 2 sub-agents");
	});

	test("--json is the report, with a stable set of fields", async () => {
		const { out, code } = await usage(["cccc", "--json"]);
		expect(code).toBeUndefined();
		const report = JSON.parse(out.join("\n")) as Record<string, unknown>;
		expect(report.schema).toBe(SESSION_REPORT_SCHEMA);
		expect(SESSION_REPORT_SCHEMA).toBe("ultron.session-report/1");
		expect(Object.keys(report)).toEqual([
			"schema",
			"session",
			"mode",
			"turns",
			"root",
			"cells",
			"depth",
			"usage",
			"guardrails",
			"memory",
			"unrecorded",
		]);
		const depth = report.depth as Record<string, Record<string, unknown>>;
		expect(Object.keys(depth)).toEqual([
			"verdict",
			"level",
			"frames",
			"subagents",
			"workflows",
			"typedAgents",
			"backgroundJobs",
		]);
		expect(Object.keys(depth.frames!)).toEqual([
			"count",
			"complete",
			"incomplete",
			"failed",
			"cancelled",
			"running",
			"nested",
			"incompleteReasons",
			"budgetTokens",
			"byModel",
			"calls",
		]);
		expect(Object.keys(depth.subagents!)).toEqual([
			"count",
			"maxDepth",
			"nested",
			"completed",
			"failed",
			"cancelled",
			"interrupted",
			"running",
			"byModel",
			"verdicts",
			"claims",
			"worktrees",
		]);
		const usageSection = report.usage as {
			lanes: Record<string, Record<string, unknown>>;
			total: Record<string, unknown>;
		};
		expect(Object.keys(usageSection.lanes)).toEqual(["root", "frames", "subagents", "other"]);
		expect(Object.keys(usageSection.total)).toEqual([
			"responses",
			"input",
			"output",
			"cacheRead",
			"cacheWrite",
			"totalTokens",
			"cost",
			"unmeasured",
		]);
		expect(Object.keys(usageSection.total.cost as object)).toEqual([
			"reportedUsd",
			"subscriptionUsd",
			"unpricedResponses",
		]);
		expect(Object.keys(report.guardrails as object)).toEqual([
			"guards",
			"secretsMasked",
			"hints",
			"nudges",
			"usageLimitBlocks",
			"countersSince",
		]);
		// What evals count: subagents spawned, how deep, and frames.
		expect(depth.subagents).toMatchObject({ count: 6, maxDepth: 2 });
		expect(depth.frames).toMatchObject({ count: 1 });
		// A value the session did not keep is null in JSON, and `unrecorded` says why.
		const old = JSON.parse((await usage(["eeee", "--json"])).out.join("\n")) as {
			turns: unknown;
			cells: unknown;
			unrecorded: Record<string, string>;
		};
		expect(old.turns).toBeNull();
		expect(old.cells).toBeNull();
		expect(Object.keys(old.unrecorded)).toEqual(expect.arrayContaining(["turns", "cells"]));
	});

	test("--last N: a table of the most recent sessions across directories", async () => {
		const { out, code } = await usage(["--last", "3", "--utc"]);
		expect(code).toBeUndefined();
		expect(out).toEqual([
			"LAST ACTIVE       ID             CWD    MODE           MODEL           TURNS  CELLS  FRAMES  SUBS    TOKENS  COST                           DEPTH",
			"2026-09-01 15:00  cccccccc-0003  ~/lib  ultron         priced/model-a      1      4       1     6     14.9k  $0.560                         depth 2: 1 frame, 6 sub-agents (1 nested), 1 typed-agent task, 1 background job",
			"2026-09-01 14:00  bbbbbbbb-0002  ~/app  ultron         priced/model-a      1      1       4     0      1.9k  $0.030 + unknown (3 unpriced)  depth 1: 4 frames, 0 sub-agents",
			"2026-09-01 13:00  eeeeeeee-0005  ~/app  ultron claude  Claude Code       n/r    n/r       1     2  11.2k +?  $0.610 (sub)                   depth 1: 1 frame, 2 sub-agents",
			"n/r: not recorded by that session (never zero). Tokens and cost of an `ultron claude` session leave out the root, which is Claude Code's.",
		]);
		const here = await usage(["--last", "9", "--here"], { cwd: "/home/dev/lib" });
		expect(here.out).toHaveLength(2);
		expect(here.out[1]).toContain("cccccccc-0003");
	});

	test("--last skips sessions in which nothing ran, files that are not sessions, and says so", async () => {
		new SessionFixture("99999999-0009-7000-8000-000000000009", "/home/dev/app")
			.laneModel("main", "priced", "model-a")
			.write(sessionsRoot, { modifiedAt: FIXTURE_START + 9 * HOUR });
		writeFileSync(
			join(sessionsRoot, "--home-dev-app--", "legacy.jsonl"),
			`${JSON.stringify({ type: "session", version: 3, id: "x", timestamp: "2026-01-01T00:00:00Z", cwd: "/" })}\n`,
		);
		const recent = await usage(["--last", "2"]);
		expect(recent.code).toBeUndefined();
		expect(recent.out.map((line) => line.split(/\s{2,}/)[1])).toEqual([
			"ID",
			"cccccccc-0003",
			"bbbbbbbb-0002",
			undefined,
		]);
		expect(recent.out.at(-1)).toBe("(1 newer session in which nothing ran not listed; --all lists them)");
		const all = await usage(["--last", "2", "--all"]);
		expect(all.out.map((line) => line.split(/\s{2,}/)[1])).toEqual(["ID", "99999999-0009", "cccccccc-0003"]);
		const none = await usage(["--last", "5", "--sessions-root", join(work, "missing")]);
		expect(none.out).toEqual([`No sessions under ${join(work, "missing")}`]);
	});

	test("--last --json is a list of the same reports", async () => {
		const { out } = await usage(["--last", "5", "--json"]);
		const listed = JSON.parse(out.join("\n")) as {
			schema: string;
			sessions: Array<{ schema: string; session: { id: string }; mode: string }>;
		};
		expect(listed.schema).toBe(SESSION_REPORT_LIST_SCHEMA);
		expect(listed.sessions.map((session) => [session.session.id.slice(0, 8), session.mode])).toEqual([
			["cccccccc", "ultron"],
			["bbbbbbbb", "ultron"],
			["eeeeeeee", "ultron claude"],
			["dddddddd", "ultron --claude"],
			["aaaaaaaa", "ultron"],
		]);
		expect(listed.sessions.every((session) => session.schema === SESSION_REPORT_SCHEMA)).toBe(true);
		const single = JSON.parse((await usage(["aaaa", "--json"])).out.join("\n"));
		expect(listed.sessions[4]).toEqual(single);
	});

	test("the sessions directory comes from --sessions-root, the environment, or the profile", async () => {
		const other = join(work, "other-sessions");
		rootOnlySession().write(other);
		const fromEnv = await usage(["--last", "5"], { env: { ULTRON_CODING_AGENT_SESSION_DIR: other } });
		expect(fromEnv.out).toHaveLength(2);
		const fromFlag = await usage(["--last", "5", `--sessions-root=${other}`]);
		expect(fromFlag.out).toEqual(fromEnv.out);
	});

	test("usage errors exit 2 with the help text; --help prints it", async () => {
		for (const args of [["--bogus"], ["--last"], ["--last", "0"], ["a", "b"], ["aaaa", "--last", "2"], ["--here"]]) {
			const result = await usage(args);
			expect(result.code, args.join(" ")).toBe(2);
			expect(result.err[0]).toMatch(/^Error: /);
			expect(result.err.join("\n")).toContain("Usage: ultron usage [session-id | session.jsonl] [options]");
			expect(result.out).toEqual([]);
		}
		const help = await usage(["--help"]);
		expect(help.code).toBeUndefined();
		expect(help.out.join("\n")).toContain("--last <N>");
		expect(help.out.join("\n")).toContain("--json");
	});

	test("never writes: session files and the profile are byte-for-byte and time-for-time unchanged", async () => {
		// A file a worker is still appending to ends in a torn line; opening the session would rewrite it.
		const torn = rootOnlySession();
		const tornPath = join(sessionsRoot, "--home-dev-app--", "torn.jsonl");
		writeFileSync(tornPath, torn.text({ torn: true }));
		const before = snapshot(agentDir);
		const content = readFileSync(tornPath, "utf8");
		await usage([]);
		await usage(["--last", "20"]);
		await usage(["--last", "20", "--json", "--all"]);
		await usage([tornPath]);
		await usage(["cccc", "--json"]);
		expect(snapshot(agentDir)).toEqual(before);
		expect(readFileSync(tornPath, "utf8")).toBe(content);
	});
});
