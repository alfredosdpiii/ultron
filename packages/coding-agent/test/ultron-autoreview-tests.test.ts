/**
 * Test execution in `ultron autoreview`'s deep pass (`rlm/autoreview_tests.py`): runner detection, request
 * validation, run limits, the base-versus-head comparison, mutation checks, and the sandbox: how its command line
 * and environment are built, and a real self-check of the mechanism this machine has. No model calls; the project
 * "tests" are run by a scripted executor, except in the self-check, which really enters the sandbox.
 */
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, test } from "vitest";

const PYTHON =
	process.env.ULTRON_PYTHON ??
	(process.platform === "linux" && existsSync("/usr/bin/python3") ? "/usr/bin/python3" : "python3");
const here = dirname(fileURLToPath(import.meta.url));
const RLM_DIR = resolve(here, "../src/ultron/rlm");

const dirs: string[] = [];
afterEach(() => {
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function tempDir(prefix: string): string {
	const dir = mkdtempSync(join(tmpdir(), prefix));
	dirs.push(dir);
	return dir;
}

const PRELUDE = `
import sys, json, asyncio, os, glob, tempfile
sys.path.insert(0, ${JSON.stringify(RLM_DIR)})
import autoreview_api as a
import autoreview_deep as deep
import autoreview_tests as t
import review_api as r
import review_prompts as p
from infer_api import MapResults, FrameError

def emit(value):
    print(json.dumps(value, default=str))
`;

function py<T = unknown>(code: string, env: Record<string, string> = {}): T {
	const output = execFileSync(PYTHON, ["-c", `${PRELUDE}\n${code}`], {
		cwd: RLM_DIR,
		encoding: "utf8",
		env: { ...process.env, PYTHONDONTWRITEBYTECODE: "1", ...env },
	});
	return JSON.parse(output.trim().split("\n").at(-1)!) as T;
}

function git(cwd: string, ...args: string[]): string {
	return execFileSync(
		"git",
		["-c", "user.name=Review Test", "-c", "user.email=review@test", "-c", "commit.gpgsign=false", ...args],
		{ cwd, encoding: "utf8", env: { ...process.env, GIT_CONFIG_NOSYSTEM: "1" } },
	).trim();
}

/** base: KINDS has a and b, and the parametrized test covers both. head: KINDS gains c; the test file is as before. */
function fixture(): { dir: string; base: string; head: string } {
	const dir = tempDir("ultron-autoreview-tests-");
	git(dir, "init", "-q", "-b", "main");
	mkdirSync(join(dir, "src"));
	mkdirSync(join(dir, "tests"));
	writeFileSync(join(dir, "pytest.ini"), "[pytest]\n");
	writeFileSync(join(dir, "src/app.py"), 'KINDS = ["a", "b"]\n\n\ndef show(kind):\n    return kind.upper()\n');
	writeFileSync(
		join(dir, "tests/test_app.py"),
		'from app import KINDS, show\n\n\ndef test_kinds():\n    assert KINDS == ["a", "b"]\n\n\ndef test_show():\n    assert show("a")\n',
	);
	git(dir, "add", ".");
	git(dir, "commit", "-qm", "base");
	const base = git(dir, "rev-parse", "HEAD");
	writeFileSync(join(dir, "src/app.py"), 'KINDS = ["a", "b", "c"]\n\n\ndef show(kind):\n    return kind.upper()\n');
	git(dir, "commit", "-qam", "add c");
	return { dir, base, head: git(dir, "rev-parse", "HEAD") };
}

/**
 * Python: a session over the fixture whose executor is scripted. It "runs pytest" by reading the exported tree:
 * test_kinds fails when KINDS has c (the head commit), test_show fails when show() no longer upper-cases.
 */
const session = (repo: { dir: string; base: string; head: string }, options = "") => `
ROOT, BASE, HEAD = ${JSON.stringify(repo.dir)}, ${JSON.stringify(repo.base)}, ${JSON.stringify(repo.head)}
tracked = r.Git(ROOT).out("ls-tree", "-r", "--name-only", HEAD).split()
read = lambda path: open(os.path.join(ROOT, path)).read() if os.path.exists(os.path.join(ROOT, path)) else None
executed = []
exports = []
real_export = t.export_commit
def export(root, rev):
    path = real_export(root, rev)
    exports.append(path)
    return path
def executor(argv, cwd, env, timeout):
    source = open(os.path.join(cwd, "src/app.py")).read()
    executed.append({"argv": argv, "cwd": cwd, "env": env, "timeout": timeout, "source": source})
    kinds = "FAILED tests/test_app.py::test_kinds - AssertionError: assert ['a', 'b', 'c'] == ['a', 'b']" if '"c"' in source else "PASSED tests/test_app.py::test_kinds"
    show = "PASSED tests/test_app.py::test_show" if "upper()" in source else "FAILED tests/test_app.py::test_show - AssertionError"
    return (1 if "FAILED" in kinds + show else 0), kinds + "\\n" + show + "\\n"
sandbox = t.Sandbox("bwrap")
s = t.TestSession(ROOT, HEAD, BASE, tracked, read, sandbox, executor=executor, export=export ${options})
`;

describe("test runner detection", () => {
	test("pytest, vitest, jest, a package script, go, cargo and make are recognized from tracked files", () => {
		const out = py<Array<[string | null, string[]]>>(`
def detect(files):
    runner = t.detect_runner(list(files), lambda path: files.get(path))
    return [runner.name if runner else None, runner.command(["tests/test_a.py"], "name") if runner else []]
emit([
    detect({"pytest.ini": "", "tests/test_a.py": ""}),
    detect({"pyproject.toml": "[tool.pytest.ini_options]", "a.py": ""}),
    detect({"setup.cfg": "[tool:pytest]"}),
    detect({"src/x.py": "", "tests/test_x.py": ""}),
    detect({"package.json": json.dumps({"devDependencies": {"vitest": "1"}})}),
    detect({"package.json": json.dumps({"scripts": {"test": "jest --ci"}})}),
    detect({"package.json": json.dumps({"scripts": {"test": "node test.js"}}), "pnpm-lock.yaml": ""}),
    detect({"package.json": json.dumps({"scripts": {"test": "echo Error: no test specified && exit 1"}})}),
    detect({"go.mod": "module x"}),
    detect({"Cargo.toml": ""}),
    detect({"Makefile": "build:\\n\\tcc x.c\\ntest:\\n\\t./run\\n"}),
    detect({"Makefile": "build:\\n\\tcc x.c\\n", "README.md": ""}),
])`);
		expect(out.map((item) => item[0])).toEqual([
			"pytest",
			"pytest",
			"pytest",
			"pytest",
			"vitest",
			"jest",
			"pnpm",
			null,
			"go",
			"cargo",
			"make",
			null,
		]);
		expect(out[0]![1]).toEqual([
			"python3",
			"-m",
			"pytest",
			"-q",
			"-rA",
			"--no-header",
			"-p",
			"no:cacheprovider",
			"--tb=short",
			"-k",
			"name",
			"tests/test_a.py",
		]);
		expect(out[4]![1]).toEqual(["npx", "--no-install", "vitest", "run", "-t", "name", "tests/test_a.py"]);
		expect(out[8]![1]).toEqual(["go", "test", "-run", "name", "./tests"]);
		expect(out[9]![1]).toEqual(["cargo", "test", "--offline", "name"]);
	});

	test("an outcome is passed, failed, a timeout, or unavailable when the tests could not run at all", () => {
		const out = py<string[]>(`
emit([t.parse_outcome("pytest", code, output)[0] for code, output in [
    (0, "PASSED tests/a.py::t"),
    (1, "FAILED tests/a.py::t - assert 1 == 2"),
    (1, "ImportError while importing test module 'tests/a.py'\\nModuleNotFoundError: No module named 'requests'"),
    (1, "/usr/bin/python3: No module named pytest"),
    (5, "no tests ran"),
    (127, "npx: not found"),
    (124, ""),
    (1, "FAILED tests/a.py::t - ModuleNotFoundError: No module named 'x'"),
]] + [t.parse_outcome("vitest", 1, "Error: Cannot find module 'vitest'")[0], t.parse_outcome("go", 1, "--- FAIL: TestX (0.00s)")[0]])`);
		expect(out).toEqual([
			"passed",
			"failed",
			"unavailable",
			"unavailable",
			"unavailable",
			"unavailable",
			"timeout",
			"failed",
			"unavailable",
			"failed",
		]);
	});
});

describe("the sandbox", () => {
	test("the environment is built from scratch: no credential of the caller can be in it", () => {
		const out = py<{ env: Record<string, string>; launch: Record<string, string> }>(
			'emit({"env": t.sandbox_env("/opt/venv/bin"), "launch": t._launch_env()})',
			{
				GH_TOKEN: "ghp_shouldneverappear",
				AWS_SECRET_ACCESS_KEY: "x",
				SSH_AUTH_SOCK: "/run/ssh",
				ANTHROPIC_API_KEY: "sk-x",
				OPENAI_API_KEY: "sk-y",
			},
		);
		expect(Object.keys(out.env).sort()).toEqual([
			"CI",
			"HOME",
			"LANG",
			"NO_COLOR",
			"PATH",
			"PYTHONDONTWRITEBYTECODE",
			"PYTHONHASHSEED",
			"TERM",
			"TMPDIR",
		]);
		expect(out.env.HOME).toBe("/tmp/home");
		expect(out.env.PATH).toBe("/opt/venv/bin:/usr/local/bin:/usr/bin:/bin");
		// Even the sandbox launcher gets only a PATH.
		expect(Object.keys(out.launch)).toEqual(["PATH"]);
		expect(JSON.stringify(out)).not.toMatch(/ghp_|sk-|AWS|SSH|TOKEN|KEY/);
	});

	test("bubblewrap: every namespace unshared, an empty root, the export as the only writable bind, no home", () => {
		const home = homedir();
		const argv = py<string[]>(
			'emit(t.Sandbox("bwrap").wrap(["python3", "-m", "pytest"], "/tmp/export-1", t.sandbox_env(), ["/opt/venv"]))',
		);
		expect(argv.slice(0, 7)).toEqual([
			"bwrap",
			"--unshare-all",
			"--die-with-parent",
			"--new-session",
			"--cap-drop",
			"ALL",
			"--clearenv",
		]);
		expect(argv.join(" ")).toContain("--bind /tmp/export-1 /tmp/export-1 --chdir /tmp/export-1 -- python3 -m pytest");
		// Exactly one writable bind; everything else is read-only, a tmpfs or a symlink.
		expect(argv.filter((part) => part === "--bind")).toHaveLength(1);
		expect(argv.join(" ")).toContain("--ro-bind /opt/venv /opt/venv");
		expect(argv.join(" ")).toContain("--tmpfs /tmp --dir /tmp/home");
		expect(argv.join(" ")).toContain("--setenv HOME /tmp/home");
		// Nothing of the user's home, the Docker socket, /run or /var is bound.
		const bound = argv.filter((_, index) => ["--ro-bind", "--ro-bind-try", "--bind"].includes(argv[index - 1] ?? ""));
		expect(bound.some((path) => path === home || path.startsWith(`${home}/`))).toBe(false);
		expect(bound.some((path) => /^\/(home|root|run|var|mnt|media)(\/|$)/.test(path))).toBe(false);
		expect(argv.join(" ")).not.toMatch(/docker\.sock|--share-net/);
	});

	test("unshare and docker: no network, home hidden, only the export mounted", () => {
		const out = py<{ unshare: string[]; docker: string[] }>(`
emit({"unshare": t.Sandbox("unshare", home="/home/someone").wrap(["make", "test"], "/tmp/export-1", t.sandbox_env()),
      "docker": t.Sandbox("docker", "python:3.12").wrap(["make", "test"], "/tmp/export-1", t.sandbox_env(), ["/opt/venv"])})`);
		expect(out.unshare.slice(0, 10)).toEqual([
			"unshare",
			"--user",
			"--map-root-user",
			"--mount",
			"--net",
			"--pid",
			"--fork",
			"--ipc",
			"--uts",
			"--kill-child",
		]);
		expect(out.unshare.slice(10, 12)).toEqual(["env", "-i"]);
		const script = out.unshare[out.unshare.indexOf("-c") + 1]!;
		expect(script).toContain("mount -t tmpfs tmpfs /tmp");
		const after = out.unshare.slice(out.unshare.indexOf("sandbox") + 1);
		expect(after).toEqual([
			"/tmp/export-1",
			"/home",
			"/root",
			"/run",
			"/media",
			"/srv",
			"/var/run",
			"/var/lib/docker",
			"/home/someone",
			"--",
			"make",
			"test",
		]);
		const docker = out.docker.join(" ");
		expect(docker).toContain("docker run --rm --network none --cap-drop ALL --security-opt no-new-privileges");
		expect(out.docker.filter((part) => part === "-v")).toHaveLength(2);
		expect(docker).toContain("-v /tmp/export-1:/tmp/export-1:rw");
		expect(docker).toContain("-v /opt/venv:/opt/venv:ro");
		expect(docker).not.toContain("docker.sock");
		expect(docker).toMatch(/python:3\.12 make test$/);
	});

	test("the strongest working mechanism is chosen; with none, there is no sandbox and tests are never run", () => {
		const out = py<Array<string | null>>(`
ok = lambda argv, cwd, env, timeout: (0, "")
broken = lambda argv, cwd, env, timeout: (1, "bwrap: setting up uid map: Permission denied")
only = lambda name: (lambda argv, cwd, env, timeout: (0 if argv[0] == name else 1, ""))
have = lambda *names: (lambda name: "/usr/bin/" + name if name in names else None)
pick = lambda **options: (lambda sandbox: sandbox.mechanism if sandbox else None)(t.detect_sandbox(**options))
emit([
    pick(which=have("bwrap", "unshare", "docker"), executor=ok, image="img"),
    pick(which=have("unshare", "docker"), executor=ok, image="img"),
    pick(which=have("bwrap", "unshare"), executor=only("unshare")),
    pick(which=have("docker"), executor=ok, image="img"),
    pick(which=have("docker"), executor=ok),
    pick(which=have("bwrap", "unshare"), executor=broken),
    pick(which=have(), executor=ok),
])`);
		expect(out).toEqual(["bwrap", "unshare", "unshare", "docker", null, null, null]);
	});

	test("self-check on this machine: inside the sandbox there is no network, no real home, no caller credential", (context) => {
		const out = py<{
			mechanism: string | null;
			checks: Record<string, Record<string, unknown>>;
			leftovers: string[];
		}>(
			`
home = os.path.expanduser("~")
checks = {}
for name in ("bwrap", "unshare"):
    sandbox = t.detect_sandbox(only=name)
    if sandbox is not None:
        checks[name] = t.self_check(sandbox)
best = t.detect_sandbox()
emit({"mechanism": best.mechanism if best else None, "checks": checks,
      "leftovers": glob.glob(os.path.join(home, ".ultron-autoreview-canary-*"))})`,
			{ GH_TOKEN: "ghp_shouldneverappear", AWS_SECRET_ACCESS_KEY: "x" },
		);
		if (out.mechanism === null) {
			context.skip("no sandbox mechanism (bubblewrap or unprivileged user namespaces) is available on this machine");
			return;
		}
		expect(Object.keys(out.checks)).toContain(out.mechanism);
		for (const check of Object.values(out.checks))
			expect(check).toEqual({
				network: "unreachable",
				canary: "unreadable",
				token: "absent",
				credentialVariables: [],
				home: "/tmp/home",
				workdir: "writable",
				system: "read-only",
				dockerSocket: "hidden",
				ok: true,
			});
		// The canary file is removed from the real home afterwards.
		expect(out.leftovers).toEqual([]);
	}, 120_000);
});

describe("a review's test session", () => {
	test("requests are validated: existing tracked test paths, a plain selector, one-line replacements", () => {
		const repo = fixture();
		const out = py<{ bad: string[]; ran: number }>(`${session(repo)}
bad = []
def refuse(call):
    try:
        call()
        bad.append("RAN")
    except (t.TestsRejected, deep.Rejected) as error:
        bad.append(str(error))
refuse(lambda: s.paths(["../outside"]))
refuse(lambda: s.paths(["/etc/passwd"]))
refuse(lambda: s.paths(["tests/missing.py"]))
refuse(lambda: s.paths(["-rf"]))
refuse(lambda: s.paths([]))
refuse(lambda: s.paths(["tests"] * 20))
refuse(lambda: s.run(["tests/test_app.py"], "x; rm -rf /"))
refuse(lambda: s.run(["tests/test_app.py"], "$(id)"))
refuse(lambda: s.mutation("src/app.py", 1, "a\\nb", ["tests/test_app.py"]))
refuse(lambda: s.mutation("src/app.py", 99, "x", ["tests/test_app.py"]))
refuse(lambda: s.mutation("src/nope.py", 1, "x", ["tests/test_app.py"]))
refuse(lambda: s.mutation("src/app.py", 1, "x", ["../tests"]))
refuse(lambda: deep.serve_request(deep.Repo(ROOT, HEAD), {"run_tests": {"paths": ["tests/test_app.py"]}}))
emit({"bad": bad, "ran": len(executed), "ok": s.paths(["tests", "tests/test_app.py"])})
s.close()`);
		expect(out.bad).not.toContain("RAN");
		expect(out.bad).toHaveLength(13);
		expect(out.bad[0]).toContain("is not a path inside the repository");
		expect(out.bad[2]).toBe("tests/missing.py is not a tracked file or directory at the reviewed commit");
		expect(out.bad[6]).toBe("select must be a short test name expression");
		expect(out.bad[8]).toContain("replacement must be one line");
		expect(out.bad[9]).toBe("src/app.py has no line 99");
		expect(out.bad[12]).toBe("tests are not run in this review");
		// Nothing was executed for any refused request.
		expect(out.ran).toBe(0);
	});

	test("every execution is sandboxed, in an export of the commit, time-limited, and counted against the limit", () => {
		const repo = fixture();
		const out = py<{
			first: Record<string, unknown>;
			call: { argv: string[]; cwd: string; env: Record<string, string>; timeout: number };
			limit: string;
			records: number;
			inRepo: boolean;
			removed: boolean;
			summary: string;
		}>(`${session(repo, ", runs=2, timeout_s=45, env_dir=ROOT")}
first = s.run(["tests/test_app.py"])
s.run(["tests/test_app.py"], "test_show")
try:
    s.run(["tests/test_app.py"])
    limit = "RAN"
except t.TestsRejected as error:
    limit = str(error)
call = executed[0]
workdir = call["cwd"]
s.close()
emit({"first": first, "call": {k: call[k] for k in ("argv", "cwd", "env", "timeout")}, "limit": limit,
      "records": len(s.records), "inRepo": workdir.startswith(ROOT), "removed": not os.path.exists(workdir),
      "summary": t.summarize(first)})`);
		expect(out.first).toMatchObject({
			n: 1,
			kind: "run",
			rev: "head",
			status: "failed",
			passed: 1,
			failed: 1,
			command: "python3 -m pytest -q -rA --no-header -p no:cacheprovider --tb=short tests/test_app.py",
		});
		// The command is the sandbox's, run in a temporary export (not the repository), with the time limit.
		expect(out.call.argv[0]).toBe("bwrap");
		expect(out.call.argv).toContain("--unshare-all");
		expect(out.call.argv.join(" ")).toContain(`--bind ${out.call.cwd} ${out.call.cwd}`);
		expect(out.call.argv.join(" ")).toContain(`--ro-bind ${repo.dir} ${repo.dir}`);
		expect(out.inRepo).toBe(false);
		expect(out.call.timeout).toBe(45);
		expect(Object.keys(out.call.env)).toEqual(["PATH"]);
		expect(out.limit).toBe("the limit of 2 test executions per review is reached");
		expect(out.records).toBe(2);
		// The export is removed when the session closes.
		expect(out.removed).toBe(true);
		expect(out.summary).toContain("-> failed (1 passed, 1 failed)");
		expect(out.summary).toContain("failed: tests/test_app.py::test_kinds - AssertionError");
	});

	test("base versus head: a test that fails now and passed before is a regression; an old failure is not", () => {
		const repo = fixture();
		const out = py<{
			regressions: string[];
			revs: string[];
			old: string[];
			oldRuns: number;
			clean: Record<string, unknown>;
		}>(`${session(repo)}
compared = s.compare(["tests/test_app.py"])
# A test that already failed at the base commit: not this change's.
def always_failing(argv, cwd, env, timeout):
    return 1, "FAILED tests/test_app.py::test_kinds - AssertionError\\nPASSED tests/test_app.py::test_show"
old = t.TestSession(ROOT, HEAD, BASE, tracked, read, sandbox, executor=always_failing, export=export)
before = old.compare(["tests/test_app.py"])
# Everything passes at head: the base commit is not run at all.
passing = t.TestSession(ROOT, HEAD, BASE, tracked, read, sandbox, executor=lambda *args: (0, "PASSED tests/test_app.py::test_kinds"), export=export)
clean = passing.compare(["tests/test_app.py"])
emit({"regressions": [item["id"] for item in compared["regressions"]], "revs": [record["rev"] for record in s.records],
      "old": [item["id"] for item in before["regressions"]], "oldRuns": len(old.records),
      "clean": {"runs": len(passing.records), "base": clean["base"], "regressions": clean["regressions"]}})
for item in (s, old, passing):
    item.close()`);
		expect(out.regressions).toEqual(["tests/test_app.py::test_kinds"]);
		expect(out.revs).toEqual(["head", "base"]);
		expect(out.old).toEqual([]);
		expect(out.oldRuns).toBe(2);
		expect(out.clean).toEqual({ runs: 1, base: null, regressions: [] });
	});

	test("a mutation is applied for the run and reverted afterwards, also when the run fails to start", () => {
		const repo = fixture();
		const out = py<{
			caught: Record<string, unknown>;
			survived: Record<string, unknown>;
			during: string;
			after: string;
			afterError: string;
			error: string;
			summary: string;
			repoUntouched: boolean;
		}>(`${session(repo)}
path = lambda: os.path.join(exports[0], "src/app.py")
caught = s.mutation("src/app.py", 5, "    return kind", ["tests/test_app.py"])
during = executed[-1]["source"]
after = open(path()).read()
# A change the tests do not notice: both outcomes equal the unmutated run.
survived = s.mutation("src/app.py", 2, "# nothing", ["tests/test_app.py"])
def exploding(argv, cwd, env, timeout):
    raise OSError("the sandbox could not start")
s._executor = exploding
try:
    s.mutation("src/app.py", 5, "    return None", ["tests/test_app.py"])
    error = "no error"
except OSError as failure:
    error = str(failure)
after_error = open(path()).read()
emit({"caught": caught, "survived": survived, "during": during, "after": after, "afterError": after_error,
      "error": error, "summary": t.summarize(caught),
      "repoUntouched": open(os.path.join(ROOT, "src/app.py")).read() == after})
s.close()`);
		const original = 'KINDS = ["a", "b", "c"]\n\n\ndef show(kind):\n    return kind.upper()\n';
		expect(out.during).toBe('KINDS = ["a", "b", "c"]\n\n\ndef show(kind):\n    return kind\n');
		expect(out.after).toBe(original);
		expect(out.afterError).toBe(original);
		expect(out.error).toBe("the sandbox could not start");
		expect(out.repoUntouched).toBe(true);
		expect(out.caught).toMatchObject({
			kind: "mutation",
			caught: true,
			mutation: { path: "src/app.py", line: 5, was: "    return kind.upper()", replacement: "    return kind" },
		});
		expect(out.summary).toContain(
			"with src/app.py:5 changed from `    return kind.upper()` to `    return kind`: the tests caught it",
		);
		expect(out.survived.kind).toBe("mutation");
	});
});

describe("a real sandboxed run", () => {
	test("a make test target runs inside the sandbox on an export of each commit; the repository is not touched", (context) => {
		const dir = tempDir("ultron-autoreview-real-");
		git(dir, "init", "-q", "-b", "main");
		// The test passes when value.txt holds 1; it also tries to leave a file behind and to read the real home.
		writeFileSync(
			join(dir, "Makefile"),
			`test:\n\t@touch ran-here.txt\n\t@echo "home=$$HOME"\n\t@ls -A ${homedir()} 2>/dev/null | head -1 | sed "s/^/seen=/"\n\t@test "$$(cat value.txt)" = "1"\n`,
		);
		writeFileSync(join(dir, "value.txt"), "1\n");
		git(dir, "add", ".");
		git(dir, "commit", "-qm", "base");
		const base = git(dir, "rev-parse", "HEAD");
		writeFileSync(join(dir, "value.txt"), "2\n");
		git(dir, "commit", "-qam", "break it");
		const head = git(dir, "rev-parse", "HEAD");
		const out = py<{
			sandbox: string | null;
			make: boolean;
			compared?: { head: Record<string, unknown>; base: Record<string, unknown>; regressions: unknown[] };
			leftInRepo: boolean;
			exportsGone: boolean;
		}>(`
import shutil
ROOT = ${JSON.stringify(dir)}
sandbox = t.detect_sandbox()
out = {"sandbox": sandbox.mechanism if sandbox else None, "make": shutil.which("make") is not None}
if sandbox and out["make"]:
    tracked = r.Git(ROOT).out("ls-tree", "-r", "--name-only", ${JSON.stringify(head)}).split()
    read = lambda path: open(os.path.join(ROOT, path)).read()
    s = t.TestSession(ROOT, ${JSON.stringify(head)}, ${JSON.stringify(base)}, tracked, read, sandbox, timeout_s=60)
    out["compared"] = s.compare(["value.txt"])
    exported = list(s._dirs.values())
    s.close()
    out["leftInRepo"] = os.path.exists(os.path.join(ROOT, "ran-here.txt"))
    out["exportsGone"] = not any(os.path.exists(path) for path in exported)
emit(out)`);
		if (out.sandbox === null || !out.make) {
			context.skip(
				out.sandbox === null ? "no sandbox mechanism is available on this machine" : "make is not installed",
			);
			return;
		}
		const compared = out.compared!;
		// Failed at the head commit, passed at the base commit: a regression, observed by really running both.
		expect(compared.head).toMatchObject({ status: "failed", rev: "head", command: "make test" });
		expect(compared.base).toMatchObject({ status: "passed", rev: "base" });
		expect(compared.regressions).toHaveLength(1);
		// Inside: the sandbox's own empty HOME, and nothing of the real one.
		expect(String(compared.base.output)).toContain("home=/tmp/home");
		expect(String(compared.base.output)).not.toContain("seen=");
		// The test wrote into its export, not into the repository; the exports are removed.
		expect(out.leftInRepo).toBe(false);
		expect(out.exportsGone).toBe(true);
	}, 120_000);
});

/** Python: run the pipeline over the fixture with a scripted investigator and, for tests, the scripted executor. */
const pipeline = (repo: { dir: string; base: string; head: string }) => `${session(repo)}
s.close()
LENS = {p.deep_task(name, flag): name for name in p.DEEP_LENSES for flag in (False, True)}
class Rlm:
    def __init__(self, investigator):
        self.calls = []
        self.investigator = investigator
    async def map(self, tasks, items=None, **options):
        out = MapResults()
        task, item = tasks[0], items[0]
        text = item if isinstance(item, str) else "\\n".join(item)
        if task in LENS:
            self.calls.append({"lens": LENS[task], "task": task, "text": text})
            out.append(self.investigator(LENS[task], text, text.count("Results of your requests, round") + 1))
        elif task == p.AUTOREVIEW_VERIFIER_TASK:
            self.calls.append({"lens": "verify", "task": task, "text": text})
            out.append({"verdict": "confirmed", "evidence": "\`return kind.upper()\`", "corrected_line": None, "severity": "minor", "scenario_holds": "unknown"})
        else:
            out.append([])
        out.spent = {"calls": 1, "tokens": 100}
        out.usage = {}
        return out
SPEC = {"repoDir": ROOT, "base": BASE, "head": HEAD, "mode": "both"}
quiet = lambda lens, text, round: {"findings": [], "requests": [], "done": True}
`;

describe("tests in the deep pass", () => {
	test("the map's tests are run at once; a test that fails now and passed at the base is a major finding by itself", () => {
		const repo = fixture();
		const out = py<{
			result: {
				verdict?: string;
				complete: boolean;
				findings: Array<Record<string, unknown>>;
				assurance: string[];
				notChecked: string[];
				tests: { enabled: boolean; mechanism: string; note: string | null; runs: Array<Record<string, unknown>> };
			};
			brief: string;
			task: string;
			round2: string;
			verifierSaw: string;
			leftovers: boolean[];
			executions: number;
		}>(`${pipeline(repo)}
deep.testing.detect_sandbox = lambda **options: sandbox
deep.testing.run_process = executor
deep.testing.export_commit = export
def investigator(lens, text, round):
    if lens != "tests":
        return quiet(lens, text, round)
    if round == 1:
        return {"findings": [], "done": False, "requests": [
            {"mutation_check": {"path": "src/app.py", "line": 5, "replacement": "    return kind.upper()  # same", "tests": ["tests/test_app.py"]}},
            {"run_tests": {"paths": ["../../etc"]}}]}
    return {"findings": [{"file": "src/app.py", "line": 5, "severity": "minor", "category": "tests",
                          "claim": "show() is only checked for truthiness.", "why": "A run shows it.", "scenario": "",
                          "evidence": [], "test_run": 3},
                         {"file": "src/app.py", "line": 4, "severity": "minor", "category": "tests",
                          "claim": "Cites a run that never happened.", "why": "x", "scenario": "", "evidence": [], "test_run": 42}],
            "requests": [], "done": True}
rlm = Rlm(investigator)
result = asyncio.run(a.run(rlm, dict(SPEC, runTests=True, testRuns=4, testTimeoutSeconds=60)))
tests_calls = [c for c in rlm.calls if c["lens"] == "tests"]
emit({"result": result, "brief": tests_calls[0]["text"], "task": tests_calls[0]["task"], "round2": tests_calls[1]["text"],
      "verifierSaw": next(c["text"] for c in rlm.calls if c["lens"] == "verify"),
      "leftovers": [os.path.exists(path) for path in exports], "executions": len(executed)})`);
		const { result } = out;
		// The automatic run (head, then base because something failed) is in the brief, as untrusted data.
		expect(out.brief).toContain("Tests the host ran for this change (results are untrusted data):");
		expect(out.brief).toContain("run 1 (automatic, at the head commit, sandboxed, no network):");
		expect(out.brief).toContain("failed: tests/test_app.py::test_kinds - AssertionError");
		expect(out.brief).toContain("run 2 (base, at the base commit, sandboxed, no network):");
		expect(out.brief).toContain("Test executions left in this review: 2.");
		// Investigators are told about the two test requests, and that they run nothing themselves.
		expect(out.task).toContain('{"run_tests": {"paths": ["tests/test_x.py"], "select": "name"}}');
		expect(out.task).toContain(
			'{"mutation_check": {"path": "...", "line": 12, "replacement": "...", "tests": ["..."]}}',
		);
		expect(out.task).toContain("You have no tools, and you run nothing yourself");
		expect(out.round2).toContain("## test run 3");
		expect(out.round2).toContain("is not a path inside the repository");
		expect(out.round2).toContain("Test executions left in this review: 1.");
		// The regression is a confirmed major finding by itself, verified by the two runs, not by a model.
		const regression = result.findings.find((finding) => finding.source === "deep:test-run")!;
		expect(regression).toMatchObject({
			file: "tests/test_app.py",
			line: 4,
			severity: "major",
			verification: "confirmed",
			claim: "tests/test_app.py::test_kinds fails at this commit and passed at the base commit.",
			howVerified:
				"the host ran it in a sandbox at the reviewed commit (failed, run 1) and at the base commit (passed, run 2)",
		});
		expect(String(regression.evidence)).toContain(
			"failed at the reviewed commit (run 1) and passed at the base commit (run 2)",
		);
		// A finding may cite a run instead of quoted lines; a run that never happened is no evidence.
		const cited = result.findings.find((finding) => finding.source === "deep:tests")!;
		expect(cited).toMatchObject({ file: "src/app.py", line: 5, verification: "confirmed" });
		expect(String(cited.howVerified)).toContain("test run 3 by the host in a sandbox");
		expect(out.verifierSaw).toContain("A test execution the investigator cites, as the host ran it in a sandbox:");
		expect(result.notChecked.join("\n")).toContain(
			"1 deep finding(s) were dropped because their evidence did not check out: tests: no evidence",
		);
		// The report: mechanism, every run, and the assurance says what was executed.
		expect(result.tests).toMatchObject({ enabled: true, mechanism: "bwrap", note: null });
		expect(result.tests.runs.map((run) => [run.n, run.kind, run.rev, run.status])).toEqual([
			[1, "automatic", "head", "failed"],
			[2, "base", "base", "passed"],
			[3, "mutation", "head", "failed"],
		]);
		expect(result.assurance[0]).toContain("3 test runs in a bwrap sandbox (2 failed, 1 passed).");
		expect(out.executions).toBe(3);
		// Every export is gone.
		expect(out.leftovers).toEqual([false, false]);
	});

	test("no sandbox: tests are never run, and the review says so; missing dependencies are a stated limit, not a failure", () => {
		const repo = fixture();
		const out = py<
			Record<
				string,
				{
					note: string | null;
					runs: number;
					notChecked: string[];
					assurance: string;
					findings: number;
					executed: number;
				}
			>
		>(`${pipeline(repo)}
def summary(detect, run, spec):
    executed.clear()
    deep.testing.detect_sandbox = detect
    deep.testing.run_process = run
    deep.testing.export_commit = export
    result = asyncio.run(a.run(Rlm(quiet), dict(SPEC, **spec)))
    return {"note": result["tests"]["note"], "runs": len(result["tests"]["runs"]), "notChecked": result["notChecked"],
            "assurance": " ".join(result["assurance"]), "findings": len(result["findings"]), "executed": len(executed)}
def missing(argv, cwd, env, timeout):
    executed.append(argv)
    return 1, "/usr/bin/python3: No module named pytest"
def crash(argv, cwd, env, timeout):
    raise OSError("bwrap vanished")
out = {
    "noSandbox": summary(lambda **options: None, executor, {"runTests": True}),
    "missingDeps": summary(lambda **options: sandbox, missing, {"runTests": True}),
    "notAllowed": summary(lambda **options: sandbox, executor, {"runTests": False}),
    "unset": summary(lambda **options: sandbox, executor, {}),
}
before = len(exports)
deep.testing.run_process = crash
result = asyncio.run(a.run(Rlm(quiet), dict(SPEC, runTests=True)))
out["crash"] = {"note": None, "runs": 0, "notChecked": result["notChecked"], "assurance": " ".join(result["assurance"]),
                "findings": len(result["findings"]), "executed": sum(os.path.exists(path) for path in exports[before:])}
emit(out)`);
		expect(out.noSandbox).toMatchObject({ note: "tests not run: no sandbox available", runs: 0, executed: 0 });
		expect(out.noSandbox!.notChecked).toContain("Tests not run: no sandbox available.");
		expect(out.noSandbox!.assurance).toContain("nothing executed.");
		// Dependencies missing in the network-less sandbox: said as a limit, never reported as a failing test.
		expect(out.missingDeps).toMatchObject({ runs: 1, findings: 0 });
		expect(out.missingDeps!.note).toContain("tests could not run: missing dependencies");
		expect(out.missingDeps!.assurance).toContain("1 test run in a bwrap sandbox (1 unavailable).");
		// Not eligible, or not asked for: the deep pass stays read-only.
		expect(out.notAllowed).toMatchObject({ note: null, runs: 0, executed: 0 });
		expect(out.unset).toMatchObject({ note: null, runs: 0, executed: 0 });
		// A sandbox that breaks mid-review: the fast review stands, and the exports are still removed.
		expect(out.crash!.notChecked.join("\n")).toContain(
			"The deep pass failed (OSError: bwrap vanished); this is the fast review only.",
		);
		expect(out.crash!.executed).toBe(0);
	});
});
