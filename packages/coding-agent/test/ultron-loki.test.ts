/**
 * Ultron's built-in Loki integration (loki.ts):
 * - where `.loki/` may be created (auto-install eligibility) and committed (auto-commit skip reasons);
 * - the auto-commit commits `.loki/` only, keeps every other staged and unstaged change as it was, and on a rejecting
 *   hook or a failing or hanging signer leaves `.loki/` uncommitted and unstaged with the reason;
 * - with the real bundled engine (python3 >= 3.11; skipped otherwise): setup creates only `.loki/` and commits it,
 *   an edit introducing a hardcoded credential is blocked before the write, a clean edit passes, advise mode only
 *   reports, and a secret written through bash is reported after the cell;
 * - what the model is told after a cell: new findings to fix, advisory lines as FYI, missing checks once per session,
 *   and nothing about code the cell did not change (the SWE-bench pilot's noise: an untouched abstract method, the
 *   project's own imports, another function's complexity);
 * - which interpreter is passed to Loki as the project's.
 *
 * Fake credentials are built from parts at run time, so this file never holds a whole one.
 */
import { spawnSync } from "node:child_process";
import {
	chmodSync,
	existsSync,
	mkdirSync,
	mkdtempSync,
	readdirSync,
	readFileSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { getBundledLokiPath } from "../src/config.ts";
import type { LokiSettings } from "../src/core/settings-manager.ts";
import { createUltronRlmTool } from "../src/experimental/session-worker.ts";
import { FileHooks, type GuardRecord } from "../src/ultron/file-hooks.ts";
import {
	autoCommitSkipReason,
	autoInitEligibility,
	classifyLokiText,
	commitLoki,
	findProjectPython,
	findPython,
	LokiGuard,
	lokiAutoCommit,
	lokiAutoInit,
	lokiMode,
	lokiTimeoutMs,
	parseLokiReport,
	setupLoki,
} from "../src/ultron/loki.ts";

const IDENTITY = {
	GIT_AUTHOR_NAME: "Test User",
	GIT_AUTHOR_EMAIL: "test@example.com",
	GIT_COMMITTER_NAME: "Test User",
	GIT_COMMITTER_EMAIL: "test@example.com",
};
/** The environment every git and Loki process in these tests sees: no daemon, no CI, a fixed identity. */
const ENV: NodeJS.ProcessEnv = { ...process.env, ...IDENTITY, LOKI_DAEMON: "0" };
delete ENV.CI;
delete ENV.ULTRON_LOKI;
delete ENV.ULTRON_LOKI_AUTOINIT;
delete ENV.ULTRON_LOKI_AUTOCOMMIT;

/** A model-provider key shape Loki's secret rule flags, assembled at run time. */
const FAKE_KEY = ["sk", "proj", "A1b2C3d4".repeat(6)].join("-");

function git(cwd: string, ...args: string[]): string {
	const result = spawnSync("git", args, { cwd, env: ENV, encoding: "utf8" });
	if (result.status !== 0) throw new Error(`git ${args.join(" ")} failed: ${result.stderr}`);
	return result.stdout.trim();
}

function repository(parent: string, name = "repo"): string {
	const root = join(parent, name);
	mkdirSync(root, { recursive: true });
	git(root, "init", "-q", "-b", "main");
	git(root, "config", "commit.gpgsign", "false");
	writeFileSync(join(root, "a.txt"), "a\n");
	writeFileSync(join(root, "c.txt"), "c\n");
	git(root, "add", "a.txt", "c.txt");
	git(root, "commit", "-qm", "init");
	return root;
}

function fakeLokiDir(root: string): void {
	mkdirSync(join(root, ".loki"), { recursive: true });
	writeFileSync(join(root, ".loki", "loki.py"), "# engine\n");
	writeFileSync(join(root, ".loki", "loki.json"), "{}\n");
}

const options = (overrides: { env?: NodeJS.ProcessEnv; settings?: LokiSettings; home?: string } = {}) => ({
	env: overrides.env ?? ENV,
	settings: overrides.settings ?? {},
	...(overrides.home === undefined ? {} : { home: overrides.home }),
});

let work: string;
beforeEach(() => {
	work = mkdtempSync(join(tmpdir(), "ultron-loki-"));
});
afterEach(() => {
	chmodSync(work, 0o700);
	rmSync(work, { recursive: true, force: true });
});

describe("Loki switches", () => {
	test("environment overrides settings; defaults are on", () => {
		expect(lokiMode({}, {})).toBe("on");
		expect(lokiMode({ ULTRON_LOKI: "off" }, { mode: "on" })).toBe("off");
		expect(lokiMode({ ULTRON_LOKI: "advise" }, {})).toBe("advise");
		expect(lokiMode({}, { mode: "advise" })).toBe("advise");
		expect(lokiAutoInit({}, {})).toBe(true);
		expect(lokiAutoInit({ ULTRON_LOKI_AUTOINIT: "off" }, { autoInit: true })).toBe(false);
		expect(lokiAutoInit({}, { autoInit: false })).toBe(false);
		expect(lokiAutoCommit({ ULTRON_LOKI_AUTOCOMMIT: "0" }, {})).toBe(false);
		expect(lokiAutoCommit({ ULTRON_LOKI_AUTOCOMMIT: "on" }, { autoCommit: false })).toBe(true);
		expect(lokiTimeoutMs({})).toBe(5_000);
		expect(lokiTimeoutMs({ ULTRON_LOKI_TIMEOUT_MS: "250" })).toBe(250);
		expect(lokiTimeoutMs({ ULTRON_LOKI_TIMEOUT_MS: "soon" })).toBe(5_000);
	});
});

describe("auto-install eligibility", () => {
	test("a writable repository qualifies, from any directory inside it", async () => {
		const root = repository(work);
		mkdirSync(join(root, "src"));
		expect(await autoInitEligibility(join(root, "src"), options())).toEqual({ root });
	});

	test("every skip condition names its reason", async () => {
		const root = repository(work);
		const reason = async (cwd: string, overrides: Parameters<typeof options>[0] = {}) => {
			const result = await autoInitEligibility(cwd, options(overrides));
			return "reason" in result ? result.reason : undefined;
		};
		const plain = join(work, "plain");
		mkdirSync(plain);
		expect(await reason(plain)).toBe("not a Git repository");
		expect(await reason(root, { home: root })).toBe("the repository root is the home directory");
		expect(await reason(repository(join(work, "node_modules"), "pkg")), "under node_modules").toBe(
			"the repository is inside node_modules",
		);
		expect(await reason(root, { env: { ...ENV, CI: "true" } })).toBe("CI is set");
		expect(await reason(root, { env: { ...ENV, ULTRON_LOKI_AUTOINIT: "off" } })).toBe(
			"auto-install is off (ULTRON_LOKI_AUTOINIT)",
		);
		expect(await reason(root, { settings: { autoInit: false } })).toBe("auto-install is off (loki.autoInit setting)");
		expect(await reason(root, { settings: { ignoreRepos: [root] } })).toBe(
			"the repository is listed in loki.ignoreRepos",
		);
		expect(await reason(root, { settings: { ignoreRepos: ["~/repo"] }, home: work })).toBe(
			"the repository is listed in loki.ignoreRepos",
		);
		if (process.getuid?.() !== 0) {
			chmodSync(root, 0o555);
			try {
				expect(await reason(root)).toBe("the checkout is read-only");
			} finally {
				chmodSync(root, 0o755);
			}
		}
	});
});

describe("auto-commit", () => {
	test("skip reasons: switches, CI, operations in progress, detached HEAD, ignored .loki", async () => {
		const root = repository(work);
		const reason = (overrides: Parameters<typeof options>[0] = {}) => autoCommitSkipReason(root, options(overrides));
		expect(await reason()).toBeUndefined();
		expect(await reason({ env: { ...ENV, ULTRON_LOKI_AUTOCOMMIT: "off" } })).toBe(
			"auto-commit is off (ULTRON_LOKI_AUTOCOMMIT)",
		);
		expect(await reason({ settings: { autoCommit: false } })).toBe("auto-commit is off (loki.autoCommit setting)");
		expect(await reason({ env: { ...ENV, CI: "1" } })).toBe("CI is set");
		const gitDir = join(root, ".git");
		for (const [name, expected] of [
			["MERGE_HEAD", "a merge is in progress"],
			["rebase-merge", "a rebase is in progress"],
			["CHERRY_PICK_HEAD", "a cherry-pick is in progress"],
			["BISECT_LOG", "a bisect is in progress"],
		] as const) {
			const path = join(gitDir, name);
			if (name === "rebase-merge") mkdirSync(path);
			else writeFileSync(path, "");
			expect(await reason(), name).toBe(expected);
			rmSync(path, { recursive: true, force: true });
		}
		git(root, "checkout", "-q", "--detach");
		expect(await reason()).toBe("HEAD is detached");
		git(root, "checkout", "-q", "main");
		writeFileSync(join(root, ".gitignore"), ".loki/\n");
		expect(await reason()).toBe(".loki is gitignored");
	});

	test("commits .loki only and keeps other staged and unstaged changes exactly as they were", async () => {
		const root = repository(work);
		writeFileSync(join(root, "a.txt"), "a staged\n");
		writeFileSync(join(root, "b.txt"), "b new and staged\n");
		git(root, "add", "a.txt", "b.txt");
		writeFileSync(join(root, "a.txt"), "a staged, then edited again\n");
		writeFileSync(join(root, "c.txt"), "c unstaged\n");
		writeFileSync(join(root, "untracked.txt"), "untracked\n");
		const stagedBefore = git(root, "diff", "--cached");
		const unstagedBefore = git(root, "diff");
		fakeLokiDir(root);

		const outcome = await commitLoki(root, { env: ENV });
		expect(outcome).toEqual({ sha: git(root, "rev-parse", "--short", "HEAD") });
		expect(git(root, "show", "--name-only", "--format=", "HEAD").split("\n").sort()).toEqual([
			".loki/loki.json",
			".loki/loki.py",
		]);
		const message = git(root, "log", "-1", "--format=%B");
		expect(message).toBe("Add Loki guardrails");
		expect(git(root, "log", "-1", "--format=%an <%ae>")).toBe("Test User <test@example.com>");
		expect(git(root, "diff", "--cached")).toBe(stagedBefore);
		expect(git(root, "diff")).toBe(unstagedBefore);
		expect(git(root, "status", "--porcelain", "--", "untracked.txt")).toBe("?? untracked.txt");
	});

	test("a rejecting pre-commit hook leaves .loki uncommitted and unstaged, with the reason", async () => {
		const root = repository(work);
		writeFileSync(join(root, "a.txt"), "a staged\n");
		git(root, "add", "a.txt");
		const hook = join(root, ".git", "hooks", "pre-commit");
		writeFileSync(hook, "#!/bin/sh\necho 'policy: commits need a ticket' >&2\nexit 1\n");
		chmodSync(hook, 0o755);
		fakeLokiDir(root);
		const head = git(root, "rev-parse", "HEAD");

		const outcome = await commitLoki(root, { env: ENV });
		expect(outcome).toEqual({ reason: "git commit failed: policy: commits need a ticket" });
		expect(git(root, "rev-parse", "HEAD")).toBe(head);
		expect(existsSync(join(root, ".loki", "loki.py"))).toBe(true);
		expect(git(root, "diff", "--cached", "--name-only")).toBe("a.txt");
		expect(git(root, "status", "--porcelain", "--", ".loki")).toBe("?? .loki/");
	});

	test("a failing or hanging signer never prompts: the commit fails or times out and .loki stays uncommitted", async () => {
		const root = repository(work);
		const signer = join(work, "signer.sh");
		writeFileSync(signer, "#!/bin/sh\necho 'gpg: signing failed: No pinentry' >&2\nexit 2\n");
		chmodSync(signer, 0o755);
		git(root, "config", "commit.gpgsign", "true");
		git(root, "config", "gpg.program", signer);
		fakeLokiDir(root);
		const failed = await commitLoki(root, { env: ENV });
		expect("reason" in failed && failed.reason).toMatch(/^git commit failed: .*(gpg|sign)/i);
		expect(git(root, "status", "--porcelain", "--", ".loki")).toBe("?? .loki/");

		writeFileSync(signer, "#!/bin/sh\nsleep 30\n");
		const hung = await commitLoki(root, { env: ENV, timeoutMs: 1_000 });
		expect(hung).toEqual({ reason: "git commit failed: timed out after 1 s (a hook or a signing prompt?)" });
		expect(git(root, "status", "--porcelain", "--", ".loki")).toBe("?? .loki/");
	});
});

function executable(path: string, script: string): string {
	mkdirSync(join(path, ".."), { recursive: true });
	writeFileSync(path, script);
	chmodSync(path, 0o755);
	return path;
}

describe("Loki reports", () => {
	test("a JSON report keeps blocking, advisory and not-checked apart", () => {
		const report = parseLokiReport(
			JSON.stringify({
				loki: "0.1.3",
				command: "hook",
				status: "blocked",
				blocking: [{ text: "app.py:3: python-ast: stub body in todo", path: "app.py", line: 3 }],
				advisory: [{ text: "app.py:9: loki/slop-complexity: f cyclomatic complexity 9 -> 11 (> 10)" }, { line: 1 }],
				not_checked: ["NOT CHECKED python: missing ruff"],
			}),
		);
		expect(report).toEqual({
			blocking: ["app.py:3: python-ast: stub body in todo"],
			advisory: ["app.py:9: loki/slop-complexity: f cyclomatic complexity 9 -> 11 (> 10)"],
			notChecked: ["NOT CHECKED python: missing ruff"],
		});
		expect(
			parseLokiReport(JSON.stringify({ status: "error", blocking: [], error: "invalid hook input: x" })),
		).toEqual({ blocking: [], advisory: [], notChecked: [], error: "invalid hook input: x" });
		// Not a report: a harness envelope, other JSON, text, nothing.
		expect(parseLokiReport('{"hookSpecificOutput": {}}')).toBeUndefined();
		expect(parseLokiReport("[]")).toBeUndefined();
		expect(parseLokiReport("loki: post-write check failed")).toBeUndefined();
		expect(parseLokiReport("")).toBeUndefined();
	});

	test("an older engine's text is sorted into the same tiers", () => {
		// 0.1.3: one stream, advisory lines only recognizable by their rule.
		expect(
			classifyLokiText(
				[
					"loki: post-write check failed; files are already changed.",
					"app.py:3: python-ast: stub body in todo",
					"app.py:9: loki/slop-complexity: f cyclomatic complexity 9 -> 11 (> 10); split it into smaller functions",
					"NOT CHECKED python: missing ruff",
					"",
				].join("\n"),
				true,
			),
		).toEqual({
			blocking: ["app.py:3: python-ast: stub body in todo"],
			advisory: [
				"app.py:9: loki/slop-complexity: f cyclomatic complexity 9 -> 11 (> 10); split it into smaller functions",
			],
			notChecked: ["NOT CHECKED python: missing ruff"],
		});
		// The labelled text of a newer engine: everything after the label is advisory.
		expect(
			classifyLokiText(
				"loki: post-write check failed; files are already changed.\na.ts:1: loki/xss: x\nloki: advisory (not blocking):\na.ts:2: something new\nNOT CHECKED typescript: missing oxlint",
				true,
			),
		).toEqual({
			blocking: ["a.ts:1: loki/xss: x"],
			advisory: ["a.ts:2: something new"],
			notChecked: ["NOT CHECKED typescript: missing oxlint"],
		});
		// A check that passed has nothing blocking, whatever it printed.
		expect(classifyLokiText("copy.py:1: loki/slop-duplication: lines 1-10 duplicate lib.py:1-10", false)).toEqual({
			blocking: [],
			advisory: ["copy.py:1: loki/slop-duplication: lines 1-10 duplicate lib.py:1-10"],
			notChecked: [],
		});
		expect(classifyLokiText("loki: invalid hook input: bad path", true).error).toBe(
			"loki: invalid hook input: bad path",
		);
	});
});

describe("what the model is told after a cell", () => {
	/**
	 * A stand-in engine: `sh <engine> --root <root> hook --file ... [--format json]` prints the report in `report.json`
	 * (or the text in `report.txt` on stderr), exits with `status`, and records its arguments and LOKI_PYTHON.
	 */
	function standIn(root: string, options: { json?: boolean; projectPython?: string } = {}) {
		const state = join(work, "state");
		mkdirSync(state, { recursive: true });
		const script = executable(
			join(work, "engine.sh"),
			[
				`echo "$@" >> '${state}/args'`,
				`echo "python=$LOKI_PYTHON" >> '${state}/args'`,
				`[ -f '${state}/report.json' ] && cat '${state}/report.json'`,
				`[ -f '${state}/report.txt' ] && cat '${state}/report.txt' >&2`,
				`exit "$(cat '${state}/status')"`,
				"",
			].join("\n"),
		);
		const guard = new LokiGuard({
			root,
			engine: script,
			python: "sh",
			mode: "on",
			timeoutMs: 5_000,
			previewHost: "ultron",
			postWrite: true,
			jsonReports: options.json ?? true,
			...(options.projectPython === undefined ? {} : { projectPython: options.projectPython }),
			env: ENV,
		});
		const records: GuardRecord[] = [];
		const hooks = new FileHooks({ cwd: root, onRecord: (record) => records.push(record) });
		hooks.add(guard);
		let cell = 0;
		return {
			hooks,
			records,
			args: () => readFileSync(join(state, "args"), "utf8"),
			/** Change `file` as a cell would, with the engine answering `report`; returns what the model is told. */
			cell: async (
				report: {
					blocking?: string[];
					advisory?: string[];
					not_checked?: string[];
					text?: string;
					status?: number;
				},
				file = "app.py",
			): Promise<string | undefined> => {
				rmSync(join(state, "report.json"), { force: true });
				rmSync(join(state, "report.txt"), { force: true });
				const blocking = report.blocking ?? [];
				if (report.text === undefined)
					writeFileSync(
						join(state, "report.json"),
						JSON.stringify({
							status: blocking.length > 0 ? "blocked" : "passed",
							blocking: blocking.map((text) => ({ text })),
							advisory: (report.advisory ?? []).map((text) => ({ text })),
							not_checked: report.not_checked ?? [],
						}),
					);
				else writeFileSync(join(state, "report.txt"), report.text);
				writeFileSync(join(state, "status"), String(report.status ?? (blocking.length > 0 ? 2 : 0)));
				await hooks.start();
				hooks.cellStarted();
				cell += 1;
				writeFileSync(join(root, file), `VALUE = ${cell}\n`);
				hooks.cellEnded("main");
				await hooks.settled();
				return hooks.takePending("main");
			},
		};
	}

	const STUB = "app.py:12: python-ast: stub body in todo";
	const HOTSPOT =
		"app.py:20: loki/slop-complexity: f cyclomatic complexity 9 -> 11 (> 10); split it into smaller functions";
	const NO_RUFF = "NOT CHECKED python: missing ruff";

	test("new findings say fix these, advisory lines are FYI, and a missing analyzer is one short note", async () => {
		const session = standIn(repository(work));
		const told = await session.cell({ blocking: [STUB], advisory: [HOTSPOT], not_checked: [NO_RUFF] });
		expect(told).toBe(
			[
				"[Loki] 1 new finding from this cell's changes; fix these:",
				STUB,
				"FYI (advisory, not blocking; no change required):",
				HOTSPOT,
				"not checked in this session (said once): python: missing ruff",
			].join("\n"),
		);
		expect(told).not.toContain("post-write check failed");
		expect(session.records.at(-1)).toMatchObject({ phase: "after_cell", outcome: "findings" });
		expect(session.args()).toContain("hook --file app.py --format json");
	});

	test("what was said is not said again: open findings shrink to a count, the rest to nothing", async () => {
		const session = standIn(repository(work));
		await session.cell({ blocking: [STUB], advisory: [HOTSPOT], not_checked: [NO_RUFF] });
		// The same finding two lines further down after another edit of the file: still the same finding.
		const again = await session.cell({
			blocking: [STUB.replace(":12:", ":14:")],
			advisory: [HOTSPOT.replace(":20:", ":22:")],
			not_checked: [NO_RUFF],
		});
		expect(again).toBe("[Loki] 1 finding reported earlier is still open in app.py.");
		expect(session.records.at(-1)).toMatchObject({ outcome: "findings" });
		// A second finding next to the open one: only the new one is spelled out.
		const second = "app.py:30: python-ast: stub body in later";
		expect(await session.cell({ blocking: [STUB, second], not_checked: [NO_RUFF] })).toBe(
			`[Loki] 1 new finding from this cell's changes; fix these:\n${second}\n1 finding reported earlier is still open in app.py.`,
		);
		// Fixed: nothing to say, and the record is clean.
		expect(await session.cell({ not_checked: [NO_RUFF] })).toBeUndefined();
		expect(session.records.at(-1)).toMatchObject({ outcome: "clean" });
		expect(session.records.at(-1)!.detail).toBeUndefined();
		// Introduced again later, it is new again.
		expect(await session.cell({ blocking: [STUB] })).toContain("1 new finding from this cell's changes; fix these:");
		expect(session.hooks.stats()[0]).toMatchObject({ afterChecks: 5, afterFindings: 4 });
	});

	test("advisory lines alone never say fix, and a missing check alone is only the note", async () => {
		const session = standIn(repository(work));
		const advisory = await session.cell({ advisory: [HOTSPOT] });
		expect(advisory).toBe(`[Loki] FYI (advisory, not blocking; no change required):\n${HOTSPOT}`);
		expect(advisory).not.toMatch(/fix th/);
		expect(session.records.at(-1)).toMatchObject({ outcome: "advisory" });
		const note = await session.cell({ not_checked: [NO_RUFF] }, "other.py");
		expect(note).toBe("[Loki] not checked in this session (said once): python: missing ruff");
		expect(session.records.at(-1)).toMatchObject({ outcome: "clean" });
		// A different missing check later is said once too, the first one never again.
		expect(
			await session.cell(
				{
					not_checked: [
						NO_RUFF,
						"NOT CHECKED python imports: no project interpreter found (set LOKI_PYTHON or pass --python)",
					],
				},
				"other.py",
			),
		).toBe(
			"[Loki] not checked in this session (said once): python imports: no project interpreter found (set LOKI_PYTHON or pass --python)",
		);
		expect(await session.cell({ not_checked: [NO_RUFF] }, "other.py")).toBeUndefined();
		expect(session.hooks.stats()[0]).toMatchObject({ afterChecks: 4, afterFindings: 0 });
	});

	test("an older engine's text gets the same presentation, without --format", async () => {
		const session = standIn(repository(work), { json: false });
		const told = await session.cell({
			status: 2,
			text: `loki: post-write check failed; files are already changed.\n${STUB}\n${HOTSPOT}\n${NO_RUFF}\n`,
		});
		expect(told).toBe(
			[
				"[Loki] 1 new finding from this cell's changes; fix these:",
				STUB,
				"FYI (advisory, not blocking; no change required):",
				HOTSPOT,
				"not checked in this session (said once): python: missing ruff",
			].join("\n"),
		);
		expect(session.args()).not.toContain("--format");
		expect(await session.cell({ status: 0, text: `${NO_RUFF}\n` })).toBeUndefined();
	});

	test("an engine that cannot check says so instead of reporting findings", async () => {
		const session = standIn(repository(work));
		expect(await session.cell({ status: 2, text: "loki: invalid hook input: git: cannot resolve HEAD\n" })).toBe(
			"[Loki] Loki could not check the files this cell changed: loki: invalid hook input: git: cannot resolve HEAD",
		);
		expect(session.records.at(-1)).toMatchObject({ outcome: "unchecked" });
	});

	test("the project's interpreter reaches the engine as LOKI_PYTHON", async () => {
		const session = standIn(repository(work), { projectPython: "/opt/project/bin/python" });
		await session.cell({});
		expect(session.args()).toContain("python=/opt/project/bin/python\n");
		const plain = standIn(repository(work, "plain"));
		await plain.cell({});
		expect(plain.args().trim().split("\n").at(-1)).toBe(`python=${ENV.LOKI_PYTHON ?? ""}`);
	});
});

describe("the project's interpreter", () => {
	const fakePython = (dir: string, name: string, kind: "environment" | "system"): string =>
		executable(join(dir, name), `#!/bin/sh\necho '${join(dir, name)}'\necho ${kind === "environment" ? 1 : 0}\n`);
	const env = (path: string, extra: NodeJS.ProcessEnv = {}): NodeJS.ProcessEnv => ({ PATH: path, ...extra });

	test("ULTRON_LOKI_PROJECT_PYTHON wins; an inherited LOKI_PYTHON is left to the engine", async () => {
		const bin = join(work, "bin");
		fakePython(bin, "python", "environment");
		expect(await findProjectPython(work, env(bin, { ULTRON_LOKI_PROJECT_PYTHON: " /named/python " }))).toBe(
			"/named/python",
		);
		expect(await findProjectPython(work, env(bin, { LOKI_PYTHON: "/set/by/user" }))).toBeUndefined();
	});

	test("the python bash() would run is used when it belongs to an environment", async () => {
		const bin = join(work, "bin");
		const python = fakePython(bin, "python", "environment");
		expect(await findProjectPython(work, env(bin))).toBe(python);
		// Only python3 on PATH.
		const bin3 = join(work, "bin3");
		const python3 = fakePython(bin3, "python3", "environment");
		expect(await findProjectPython(work, env(bin3))).toBe(python3);
	});

	test("a bare system interpreter is not the project's; a .venv in the session directory is", async () => {
		const bin = join(work, "bin");
		fakePython(bin, "python", "system");
		fakePython(bin, "python3", "environment");
		const project = join(work, "project");
		mkdirSync(project);
		expect(await findProjectPython(project, env(bin))).toBeUndefined();
		const venv = executable(join(project, "venv", "bin", "python3"), "#!/bin/sh\n");
		expect(await findProjectPython(project, env(bin))).toBe(venv);
		const dotVenv = executable(join(project, ".venv", "bin", "python"), "#!/bin/sh\n");
		expect(await findProjectPython(project, env(bin))).toBe(dotVenv);
		// No python at all.
		expect(await findProjectPython(work, env(join(work, "empty")))).toBeUndefined();
	});
});

const python = await findPython(ENV);
const engine = getBundledLokiPath();
const realLoki = "command" in python && engine !== undefined;

describe.skipIf(!realLoki)("with the real bundled Loki engine", () => {
	test("setup creates only .loki/, commits it, and adds a short policy note", async () => {
		const root = repository(work);
		writeFileSync(join(root, "a.txt"), "staged before setup\n");
		git(root, "add", "a.txt");
		const setup = await setupLoki({ cwd: root, env: ENV, settings: {}, bundledEngine: engine, home: work });
		const notice = await setup.notice;
		const sha = git(root, "rev-parse", "--short", "HEAD");
		expect(notice).toBe(`Loki set up and committed (${sha}): .loki/ holds the guardrail engine and policy.`);
		expect(readdirSync(join(root, ".loki")).sort()).toEqual(["loki.json", "loki.py"]);
		for (const other of [".claude", ".codex", ".factory", ".pi", ".omp", ".github", ".ruff.toml", ".oxlintrc.json"])
			expect(existsSync(join(root, other)), other).toBe(false);
		expect(readFileSync(join(root, ".loki", "loki.py"))).toEqual(readFileSync(engine!));
		expect(git(root, "diff", "--cached", "--name-only")).toBe("a.txt");
		expect(setup.context).toMatch(/^LOKI guardrails: edit\(\) and write\(\) are checked/);
		expect(setup.context!.length).toBeLessThan(600);
		expect(setup.guard?.options.engine).toBe(join(root, ".loki", "loki.py"));

		// A second session uses the committed .loki/ and changes nothing.
		const again = await setupLoki({ cwd: root, env: ENV, settings: {}, bundledEngine: engine, home: work });
		expect(await again.notice).toBeUndefined();
		expect(git(root, "rev-parse", "--short", "HEAD")).toBe(sha);
	});

	test("a hardcoded secret is blocked before the write, a clean edit passes, and bash writes are caught after", async () => {
		const root = repository(work);
		writeFileSync(join(root, "settings.py"), "DEBUG = False\n");
		git(root, "add", "settings.py");
		git(root, "commit", "-qm", "settings");
		const setup = await setupLoki({ cwd: root, env: ENV, settings: {}, bundledEngine: engine, home: work });
		await setup.notice;
		const hooks = new FileHooks({ cwd: root });
		hooks.add(setup.guard!);
		const path = join(root, "settings.py");
		const [blocked] = await hooks.beforeWrite([{ path, content: `DEBUG = False\nAPI_KEY = "${FAKE_KEY}"\n` }], {
			lane: "main",
		});
		expect(blocked).toMatchObject({ blocked: true });
		expect(blocked!.reason).toContain("loki/secret");
		expect(blocked!.reason).not.toContain(FAKE_KEY);
		const [clean] = await hooks.beforeWrite([{ path, content: 'DEBUG = False\nAPI_KEY = os.environ["API_KEY"]\n' }], {
			lane: "main",
		});
		expect(clean).toMatchObject({ blocked: false });
		// Outside the repository Loki has nothing to say.
		const [outside] = await hooks.beforeWrite([{ path: join(work, "scratch.py"), content: `K = "${FAKE_KEY}"\n` }], {
			lane: "main",
		});
		expect(outside).toEqual({ blocked: false, notes: [] });

		// A bash write is only seen after the cell.
		await hooks.start();
		hooks.cellStarted();
		writeFileSync(path, `DEBUG = False\nAPI_KEY = "${FAKE_KEY}"\n`);
		hooks.cellEnded("main");
		await hooks.settled();
		const findings = hooks.takePending("main");
		expect(findings).toContain("[Loki] 1 new finding from this cell's changes; fix these:");
		expect(findings).toContain("loki/secret");
		expect(hooks.stats()[0]).toMatchObject({ name: "Loki", checks: 3, blocked: 1, afterChecks: 1, afterFindings: 1 });
	});

	test("in the REPL, edit() introducing a secret raises ValueError and writes nothing; a clean edit lands", async () => {
		const root = repository(work);
		writeFileSync(join(root, "settings.py"), "DEBUG = False\nAPI_KEY = None\n");
		git(root, "add", "settings.py");
		git(root, "commit", "-qm", "settings");
		const setup = await setupLoki({ cwd: root, env: ENV, settings: {}, bundledEngine: engine, home: work });
		await setup.notice;
		const hooks = new FileHooks({ cwd: root });
		hooks.add(setup.guard!);
		const tool = createUltronRlmTool(
			root,
			async (type) => {
				throw new Error(`unexpected host request ${type}`);
			},
			async () => "main",
			{ fileHooks: hooks },
		);
		const run = async (code: string): Promise<string> => {
			const invocation = {
				invocationId: code,
				operationId: code,
				turnId: "loki-turn",
				getMemo: async () => undefined,
				setMemo: async () => undefined,
			};
			try {
				const result = (await tool.execute(
					"loki",
					{ code },
					() => {},
					{ env: new NodeExecutionEnv({ cwd: root }) },
					invocation,
					BACKGROUND_CONTEXT,
				)) as { content: Array<{ text: string }> };
				return result.content.map((part) => part.text).join("");
			} catch (error) {
				return `ERROR ${error instanceof Error ? error.message : String(error)}`;
			}
		};
		try {
			const key = FAKE_KEY.split("-");
			const blocked = await run(
				`key = "-".join(${JSON.stringify(key)})\nawait edit("settings.py", "API_KEY = None", f'API_KEY = "{key}"')`,
			);
			expect(blocked).toContain("ValueError: settings.py was not written: [Loki] settings.py:2: loki/secret");
			expect(readFileSync(join(root, "settings.py"), "utf8")).toBe("DEBUG = False\nAPI_KEY = None\n");
			const clean = await run(`await edit("settings.py", "API_KEY = None", 'API_KEY = os.environ.get("API_KEY")')`);
			expect(clean).toContain("Edited settings.py");
			expect(readFileSync(join(root, "settings.py"), "utf8")).toContain('os.environ.get("API_KEY")');
		} finally {
			await tool.close();
		}
	});

	test("advise mode reports what it would block and writes anyway", async () => {
		const root = repository(work);
		const setup = await setupLoki({
			cwd: root,
			env: { ...ENV, ULTRON_LOKI: "advise" },
			settings: {},
			bundledEngine: engine,
			home: work,
		});
		const hooks = new FileHooks({ cwd: root });
		hooks.add(setup.guard!);
		const [result] = await hooks.beforeWrite([{ path: join(root, "k.py"), content: `K = "${FAKE_KEY}"\n` }], {
			lane: "main",
		});
		expect(result!.blocked).toBe(false);
		expect(result!.notes.join("\n")).toContain("would block this write (advise-only mode)");
	});

	test("an uncommitted .loki/ pauses post-write checks with one note; auto-install off uses the bundled engine", async () => {
		const root = repository(work);
		const setup = await setupLoki({
			cwd: root,
			env: { ...ENV, ULTRON_LOKI_AUTOCOMMIT: "off" },
			settings: {},
			bundledEngine: engine,
			home: work,
		});
		expect(await setup.notice).toMatch(/^Loki set up in \.loki\/ \(not committed: auto-commit is off/);
		const hooks = new FileHooks({ cwd: root });
		hooks.add(setup.guard!);
		await hooks.start();
		for (let cell = 0; cell < 2; cell++) {
			hooks.cellStarted();
			writeFileSync(join(root, `f${cell}.py`), "X = 1\n");
			hooks.cellEnded("main");
			await hooks.settled();
		}
		expect(hooks.takePending("main")).toContain("post-write checks are paused: .loki/ has uncommitted changes");

		const other = repository(work, "other");
		const off = await setupLoki({
			cwd: other,
			env: { ...ENV, ULTRON_LOKI_AUTOINIT: "off" },
			settings: {},
			bundledEngine: engine,
			home: work,
		});
		expect(await off.notice).toBeUndefined();
		expect(existsSync(join(other, ".loki"))).toBe(false);
		expect(off.guard?.options.engine).toBe(engine);
	});

	test("outside Git only before-write checks run", async () => {
		const plain = join(work, "plain");
		mkdirSync(plain);
		const setup = await setupLoki({ cwd: plain, env: ENV, settings: {}, bundledEngine: engine, home: work });
		expect(await setup.notice).toBeUndefined();
		expect(existsSync(join(plain, ".loki"))).toBe(false);
		expect(setup.guard?.watchesCells()).toBe(false);
		const hooks = new FileHooks({ cwd: plain });
		hooks.add(setup.guard!);
		expect(hooks.watchesCells).toBe(false);
		const [blocked] = await hooks.beforeWrite([{ path: join(plain, "k.py"), content: `K = "${FAKE_KEY}"\n` }], {
			lane: "main",
		});
		expect(blocked!.blocked).toBe(true);
	});

	/** A Django-like module: an abstract method, a placeholder and third-party imports, all committed. */
	const WIDGETS = [
		"import datetime",
		"",
		"import numpy",
		"from docutils import nodes",
		"",
		"",
		"class MultiWidget:",
		"    def decompress(self, value):",
		'        raise NotImplementedError("Subclasses must implement this method.")',
		"",
		"",
		"def unfinished():",
		"    pass",
		"",
		"",
		"class Other:",
		"    def __init__(self, x):",
		...Array.from({ length: 13 }, (_, i) => `        if x == ${i}:\n            x += ${i}`),
		"        self.x = x",
		"",
		"",
		"class SelectDateWidget:",
		"    def __init__(self, x):",
		"        self.x = x",
		"",
		"    def value_from_datadict(self, y, m, d):",
		"        try:",
		"            date_value = datetime.date(int(y), int(m), int(d))",
		"        except ValueError:",
		'            return "%s-%s-%s" % (y or 0, m or 0, d or 0)',
		"        return date_value.isoformat()",
		"",
	].join("\n");

	/** A session in a repository without `.loki/` (the bundled engine, the default policy), as in the pilot. */
	async function pilotSession(extraEnv: NodeJS.ProcessEnv = {}) {
		const root = repository(work);
		mkdirSync(join(root, "django", "forms"), { recursive: true });
		const file = join(root, "django", "forms", "widgets.py");
		writeFileSync(file, WIDGETS);
		git(root, "add", "django");
		git(root, "commit", "-qm", "widgets");
		const records: Record<string, unknown>[] = [];
		const setup = await setupLoki({
			cwd: root,
			env: { ...ENV, ULTRON_LOKI_AUTOINIT: "off", ...extraEnv },
			settings: {},
			bundledEngine: engine,
			home: work,
			record: (record) => records.push(record),
		});
		expect(existsSync(join(root, ".loki"))).toBe(false);
		const outcomes: GuardRecord[] = [];
		const hooks = new FileHooks({ cwd: root, onRecord: (record) => outcomes.push(record) });
		hooks.add(setup.guard!);
		await hooks.start();
		const cell = async (content: string): Promise<string | undefined> => {
			hooks.cellStarted();
			writeFileSync(file, content);
			hooks.cellEnded("main");
			await hooks.settled();
			return hooks.takePending("main");
		};
		return { root, setup, records, outcomes, cell };
	}

	const FIX = WIDGETS.replace(
		"        except ValueError:\n",
		'        except OverflowError:\n            return ""\n        except ValueError:\n',
	);

	test("a small edit in a file full of existing debt tells the model nothing about that debt", async () => {
		const session = await pilotSession();
		expect(session.setup.guard!.options.jsonReports).toBe(true);
		expect(session.records.at(-1)).toMatchObject({ phase: "setup", engine: "bundled", reports: "json" });
		const first = await session.cell(FIX);
		// At most the once-per-session note that Ruff did not run here (not installed, or no committed .ruff.toml).
		if (first !== undefined)
			expect(first).toMatch(/^\[Loki\] not checked in this session \(said once\): python[^\n]*$/);
		expect(session.outcomes.at(-1)).toMatchObject({ phase: "after_cell", outcome: "clean" });
		// The same again says nothing at all.
		expect(await session.cell(`${FIX}\nVALUE = 1\n`)).toBeUndefined();

		// A placeholder the cell itself adds is the only thing to fix; the existing one is not mentioned.
		const added = await session.cell(`${FIX}\n\ndef todo():\n    pass\n`);
		expect(added).toBe(
			`[Loki] 1 new finding from this cell's changes; fix these:\ndjango/forms/widgets.py:${FIX.split("\n").length + 2}: python-ast: stub body in todo`,
		);
		expect(session.outcomes.at(-1)).toMatchObject({ outcome: "findings" });
		for (const text of [first ?? "", added!])
			for (const noise of ["decompress", "unfinished", "unresolved import", "post-write check failed", "__init__"])
				expect(text).not.toContain(noise);
	});

	test("a function the cell makes complex is an FYI, never something to fix", async () => {
		const session = await pilotSession();
		await session.cell(FIX);
		const grown = FIX.replace("        if x == 0:\n", "        if x == 0 or x == 99:\n");
		const told = await session.cell(grown);
		expect(told).toMatch(
			/^\[Loki\] FYI \(advisory, not blocking; no change required\):\ndjango\/forms\/widgets\.py:17: loki\/slop-complexity: __init__ cyclomatic complexity 14 -> 15 \(> 10\)/,
		);
		expect(told).not.toMatch(/fix th/);
		expect(told!.split("\n")).toHaveLength(2);
		expect(session.outcomes.at(-1)).toMatchObject({ outcome: "advisory" });
	});

	test("a new import is checked with the project's interpreter, and not checked without one", async () => {
		const missing = executable(join(work, "project-python"), "#!/bin/sh\necho '[\"invented_package\"]'\n");
		const withPython = await pilotSession({ ULTRON_LOKI_PROJECT_PYTHON: missing });
		expect(withPython.setup.guard!.options.projectPython).toBe(missing);
		expect(withPython.records.at(-1)).toMatchObject({ projectPython: missing });
		const told = await withPython.cell(`import invented_package\n${WIDGETS}`);
		expect(told).toContain(
			"1 new finding from this cell's changes; fix these:\ndjango/forms/widgets.py:1: python-ast: unresolved import invented_package",
		);
		expect(told).not.toContain("numpy");
		rmSync(withPython.root, { recursive: true, force: true });

		// No interpreter named or found: the import is a note, once, not a finding.
		const bare = { ...ENV };
		delete bare.VIRTUAL_ENV;
		delete bare.CONDA_PREFIX;
		delete bare.LOKI_PYTHON;
		const guard = new LokiGuard({
			root: repository(work, "bare"),
			engine: engine!,
			python: (python as { command: string }).command,
			mode: "on",
			timeoutMs: 5_000,
			previewHost: "ultron",
			postWrite: true,
			jsonReports: true,
			env: bare,
		});
		const root = guard.options.root;
		writeFileSync(join(root, "app.py"), "import invented_package\n");
		const controller = new AbortController();
		const report = await guard.afterCellChanges(
			{ files: [join(root, "app.py")], checked: [], deleted: [], complete: true },
			{ lane: "main", cwd: root, signal: controller.signal },
		);
		expect(report?.outcome).toBe("clean");
		expect(report?.message).toContain("python imports: no project interpreter found");
		expect(report?.message).not.toContain("unresolved import");
	});

	test("ULTRON_LOKI=off does nothing", async () => {
		const root = repository(work);
		const setup = await setupLoki({
			cwd: root,
			env: { ...ENV, ULTRON_LOKI: "off" },
			settings: {},
			bundledEngine: engine,
		});
		expect(setup.guard).toBeUndefined();
		expect(existsSync(join(root, ".loki"))).toBe(false);
	});
});
