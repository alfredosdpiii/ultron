/**
 * Ultron's built-in Loki integration (loki.ts):
 * - where `.loki/` may be created (auto-install eligibility) and committed (auto-commit skip reasons);
 * - the auto-commit commits `.loki/` only, keeps every other staged and unstaged change as it was, and on a rejecting
 *   hook or a failing or hanging signer leaves `.loki/` uncommitted and unstaged with the reason;
 * - with the real bundled engine (python3 >= 3.11; skipped otherwise): setup creates only `.loki/` and commits it,
 *   an edit introducing a hardcoded credential is blocked before the write, a clean edit passes, advise mode only
 *   reports, and a secret written through bash is reported after the cell.
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
import { FileHooks } from "../src/ultron/file-hooks.ts";
import {
	autoCommitSkipReason,
	autoInitEligibility,
	commitLoki,
	findPython,
	lokiAutoCommit,
	lokiAutoInit,
	lokiMode,
	lokiTimeoutMs,
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
		expect(findings).toContain("[Loki] findings in files this cell changed");
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
