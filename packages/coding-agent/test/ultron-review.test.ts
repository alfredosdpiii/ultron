/**
 * `/review`: the command expansion (TypeScript), the review logic in `rlm/review_api.py` (argument parsing, diff
 * scoping and chunking, dedupe, the verify filter, report rendering, posting), and an end-to-end review in a real
 * kernel whose frames are answered by a scripted provider: a fixture repository with one real bug and one planted
 * false finding, where the bug must be reported and the false finding rejected.
 */
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import type { Context } from "@ultron/agent-core";
import { afterEach, describe, expect, test } from "vitest";
import { createAgentController } from "../src/experimental/services/agent-controller-provider.ts";
import { expandReviewCommand, REVIEW_COMMAND, reviewPrompt } from "../src/ultron/review.ts";
import { createInferenceRuntime, createMemoryFrameStore } from "../src/ultron/rlm/inference.ts";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import { NativeUsageLedger } from "../src/ultron/usage.ts";
import { memoryDefinitionStore, memoryStore } from "./ultron-host-fixtures.ts";

const RLM_DIR = fileURLToPath(new URL("../src/ultron/rlm/", import.meta.url));
const RUNTIME = join(RLM_DIR, "runtime.py");

const dirs: string[] = [];
afterEach(() => {
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function tempDir(prefix: string): string {
	const dir = mkdtempSync(join(tmpdir(), prefix));
	dirs.push(dir);
	return dir;
}

/** A fake `rlm` for unit runs: finder and verifier replies come from Python callables; `calls` records frames. */
const PRELUDE = `
import sys, json, asyncio
sys.path.insert(0, ${JSON.stringify(RLM_DIR)})
import review_api as r
from infer_api import MapResults, Incomplete, FrameError

class FakeRlm:
    def __init__(self, finder=None, verifier=None):
        self.calls = []
        self.finder = finder or (lambda task, text: [])
        self.verifier = verifier or (lambda text: {"verdict": "uncertain", "evidence": "?"})
    async def map(self, tasks, items=None, **options):
        tasks = [tasks] * len(items) if isinstance(tasks, str) else list(tasks)
        out = MapResults()
        for task, item in zip(tasks, items):
            text = item if isinstance(item, str) else "\\n".join(item)
            self.calls.append({"task": task, "text": text, "options": {k: repr(v) for k, v in options.items()}})
            out.append(self.verifier(text) if task == r.VERIFIER_TASK else self.finder(task, text))
        out.spent = {"calls": len(items), "tokens": 100 * len(items)}
        out.budget = {}
        out.remaining = {}
        return out

def emit(value):
    print(json.dumps(value, default=str))
`;

function py<T = unknown>(code: string, cwd?: string): T {
	const output = execFileSync("python3", ["-c", `${PRELUDE}\n${code}`], {
		cwd: cwd ?? RLM_DIR,
		encoding: "utf8",
		env: { ...process.env, PYTHONDONTWRITEBYTECODE: "1" },
	});
	return JSON.parse(output.trim().split("\n").at(-1)!) as T;
}

function git(cwd: string, ...args: string[]): string {
	return execFileSync(
		"git",
		["-c", "user.name=Review Test", "-c", "user.email=review@test", "-c", "commit.gpgsign=false", ...args],
		{ cwd, encoding: "utf8", env: { ...process.env, GIT_CONFIG_NOSYSTEM: "1" } },
	);
}

const CALC = `def total(items):
    """Sum of item prices."""
    result = 0
    for i in range(len(items)):
        result += items[i]["price"]
    return result


def average(items, count):
    if count == 0:
        return 0
    return total(items) / count
`;

/**
 * main has calc.py and report.py (a caller of total()); feature commits an off-by-one in total() (line 4).
 * average() already guards count == 0, so a finding that it divides by zero is false.
 */
function fixtureRepo(): string {
	const dir = tempDir("ultron-review-repo-");
	git(dir, "init", "-q", "-b", "main");
	writeFileSync(join(dir, "calc.py"), CALC);
	writeFileSync(join(dir, "README.md"), "# calc\n");
	writeFileSync(
		join(dir, "report.py"),
		"from calc import total\n\n\ndef summary(items):\n    return f'total={total(items)}'\n",
	);
	git(dir, "add", ".");
	git(dir, "commit", "-qm", "calc");
	git(dir, "checkout", "-qb", "feature");
	writeFileSync(join(dir, "calc.py"), CALC.replace("range(len(items))", "range(len(items) - 1)"));
	git(dir, "commit", "-qam", "tweak total");
	return dir;
}

describe("/review command expansion", () => {
	test("/review becomes a one-cell prompt with the arguments as a Python string literal", () => {
		const prompt = expandReviewCommand("/review");
		expect(prompt).toContain('review = await review_api.run(rlm, "")');
		expect(prompt).toContain("print(review.report)");
		expect(prompt).not.toContain("post");
		const withArgs = expandReviewCommand('/review main --only sec,bugs "src/a b.py"');
		expect(withArgs.split("\n")[0]).toBe('/review main --only sec,bugs "src/a b.py"');
		expect(withArgs).toContain('review_api.run(rlm, "main --only sec,bugs \\"src/a b.py\\"")');
	});

	test("--post adds the explicit-confirmation rule; other text is untouched", () => {
		expect(reviewPrompt("12 --post")).toMatch(/only after I answer yes.*confirm=True/s);
		expect(expandReviewCommand("/reviewer x")).toBe("/reviewer x");
		expect(expandReviewCommand("please /review this")).toBe("please /review this");
		expect(REVIEW_COMMAND.name).toBe("review");
	});

	test("the worker's AgentController expands /review for every client", async () => {
		const seen: string[] = [];
		const lane = {
			prompt: async (message: string) => {
				seen.push(message);
				return { ok: true, value: { operationId: "op-1", status: "completed" } };
			},
			followUp: async (message: string) => {
				seen.push(message);
				return { ok: true, value: { entryId: "e-1" } };
			},
		};
		const controller = createAgentController(lane as never);
		await controller.prompt({ message: "/review --only bugs", images: null }, {} as never);
		await controller.followUp({ message: "plain text", images: null }, {} as never);
		expect(seen[0]).toContain('review_api.run(rlm, "--only bugs")');
		expect(seen[1]).toBe("plain text");
	});
});

describe("review_api: arguments", () => {
	test("options, aliases and budgets", () => {
		const parsed = py<Record<string, unknown>>(`
o = r.parse_args('--only sec,bugs,qa --budget 200k --model cliproxyapi/glm-5.3-flash --post --deep=2 main -- "a b.py"')
d = r.parse_args('')
emit({"only": o.only, "budget": o.budget_tokens, "model": o.model, "post": o.post, "deep": o.deep,
      "targets": o.targets, "paths": o.explicit_paths, "default_only": d.only, "default_budget": d.budget_tokens,
      "reviewers": [x.key for x in d.reviewers], "m": r.parse_args('--budget=1.5m').budget_tokens,
      "deep": r.parse_args('--deep').deep, "plan": r.parse_args('--dry-run').plan,
      "env": [(e.budget_tokens, e.model, e.only) for e in [r.parse_args('--model x/y', {"ULTRON_REVIEW_BUDGET": "50k",
              "ULTRON_REVIEW_MODEL": "a/b", "ULTRON_REVIEW_ONLY": "sec"})]][0]})`);
		expect(parsed).toEqual({
			only: ["security", "bugs", "tests"],
			budget: 200_000,
			model: "cliproxyapi/glm-5.3-flash",
			post: true,
			deep: 3,
			targets: ["main"],
			paths: ["a b.py"],
			default_only: null,
			default_budget: 300_000,
			reviewers: ["bugs", "security", "arch", "tests", "ai"],
			m: 1_500_000,
			plan: true,
			env: [50_000, "x/y", ["security"]],
		});
	});

	test("bad arguments are errors with a reason", () => {
		const errors = py<string[]>(`
out = []
for text in ['--only nope', '--budget 5', '--budget lots', '--model gpt', '--frobnicate', '--deep=99', '"unterminated', '--only']:
    try:
        r.parse_args(text)
        out.append('accepted ' + text)
    except r.ReviewError as error:
        out.append(str(error).splitlines()[0])
emit(out)`);
		expect(errors).toEqual([
			"unknown reviewer 'nope'; choose from bugs, security, arch, tests, ai",
			"--budget must be at least 10,000 tokens",
			"--budget takes a token count such as 200000, 200k or 1.5m, not 'lots'",
			"--model takes provider/model, not 'gpt'",
			"unknown option --frobnicate",
			"--deep=N takes a number from 1 to 10",
			"cannot parse the arguments: No closing quotation",
			"--only needs a value",
		]);
	});

	test("positional targets: PR numbers and URLs, paths, one base ref", () => {
		const targets = py<unknown[]>(`
exists = lambda t: t in ('src', 'docs/a.md')
is_ref = lambda t: t in ('main', 'v1.2')
def target(text):
    try:
        t = r.classify_targets(r.parse_args(text), exists=exists, is_ref=is_ref)
        return [t.pr, t.base, t.paths]
    except r.ReviewError as error:
        return str(error)
emit([target('42'), target('#7 src'), target('https://github.com/o/r/pull/19/files'), target('main src docs/a.md'),
      target('nope'), target('main v1.2'), target('3 main')])`);
		expect(targets).toEqual([
			[42, null, []],
			[7, null, ["src"]],
			[19, null, []],
			[null, "main", ["src", "docs/a.md"]],
			"'nope' is not a path in this repository, a git ref, or a PR number",
			"give at most one base ref (got 'main' and 'v1.2')",
			"give a PR number or a base ref, not both",
		]);
	});
});

const DIFF = [
	"diff --git c/src/app.py w/src/app.py",
	"index 1111111..2222222 100644",
	"--- c/src/app.py",
	"+++ w/src/app.py",
	"@@ -3,4 +3,5 @@ def main():",
	" a = 1",
	"--- not a header, a removed line starting with two dashes",
	"+b = 2",
	"+c = 3",
	" d = 4",
	" e = 5",
	"\\ No newline at end of file",
	"diff --git a/new.txt b/new.txt",
	"new file mode 100644",
	"--- /dev/null",
	"+++ b/new.txt",
	"@@ -0,0 +1,2 @@",
	"+hello",
	"+world",
	"diff --git a/old.py b/old.py",
	"deleted file mode 100644",
	"--- a/old.py",
	"+++ /dev/null",
	"@@ -1 +0,0 @@",
	"-gone",
	"diff --git a/x.py b/y.py",
	"similarity index 100%",
	"rename from x.py",
	"rename to y.py",
	"diff --git a/logo.png b/logo.png",
	"Binary files a/logo.png and b/logo.png differ",
	"diff --git a/package-lock.json b/package-lock.json",
	"--- a/package-lock.json",
	"+++ b/package-lock.json",
	"@@ -1 +1 @@",
	'-{"v": 1}',
	'+{"v": 2}',
	"",
].join("\n");

describe("review_api: diff scoping and chunking", () => {
	test("parse_diff reads statuses, renames, binaries and hunk line numbers, whatever the prefixes", () => {
		const files = py<unknown[]>(`
files = r.parse_diff(${JSON.stringify(DIFF)})
emit([[f.path, f.old_path, f.status, f.binary, f.added, f.removed,
       [[(l.kind, l.old, l.new) for l in h.lines] for h in f.hunks], r.skip_reason(f)] for f in files])`);
		expect(files).toEqual([
			[
				"src/app.py",
				"src/app.py",
				"modified",
				false,
				2,
				1,
				[
					[
						[" ", 3, 3],
						["-", 4, null],
						["+", null, 4],
						["+", null, 5],
						[" ", 5, 6],
						[" ", 6, 7],
					],
				],
				null,
			],
			[
				"new.txt",
				null,
				"added",
				false,
				2,
				0,
				[
					[
						["+", null, 1],
						["+", null, 2],
					],
				],
				null,
			],
			["old.py", "old.py", "deleted", false, 0, 1, [[["-", 1, null]]], "deleted"],
			["y.py", "x.py", "renamed", false, 0, 0, [], "no added lines (rename, mode change or removal only)"],
			["logo.png", "logo.png", "modified", true, 0, 0, [], "binary"],
			[
				"package-lock.json",
				"package-lock.json",
				"modified",
				false,
				1,
				1,
				[
					[
						["-", 1, null],
						["+", null, 1],
					],
				],
				"generated, lockfile or vendored",
			],
		]);
	});

	test("chunks carry gutter line numbers and repository context, split big files, and list skipped files", () => {
		const result = py<{ texts: string[]; skipped: unknown[]; paths: unknown[] }>(`
source = ["line %d" % n for n in range(1, 401)]
hunks = []
for start in (50, 150, 300):
    lines = [r.DiffLine(" ", start, start, source[start - 1])]
    lines += [r.DiffLine("+", None, start + 1, "added %d" % (start + 1))]
    lines += [r.DiffLine(" ", start + 1, start + 2, source[start + 1])]
    hunks.append(r.Hunk(start, 2, start, 3, "def f():", lines))
big = r.FileDiff("src/big.py", hunks=hunks)
lock = r.FileDiff("yarn.lock", hunks=[r.Hunk(1, 0, 1, 1, "", [r.DiffLine("+", None, 1, "x")])])
chunks, skipped = r.build_chunks([big, lock, r.FileDiff("gone.py", status="deleted")],
                                 lambda path: source if path == "src/big.py" else None, max_chars=350, context=3)
emit({"texts": [c.text for c in chunks], "skipped": skipped, "paths": [[c.path, c.part, c.parts, c.kind] for c in chunks]})`);
		expect(result.paths).toEqual([
			["src/big.py", 1, 2, "code"],
			["src/big.py", 2, 2, "code"],
		]);
		expect(result.skipped).toEqual([
			["yarn.lock", "generated, lockfile or vendored"],
			["gone.py", "deleted"],
		]);
		const first = result.texts[0]!;
		expect(first).toContain("File: src/big.py (modified, part 1 of 2)");
		expect(first).toContain("Other files changed in this review: yarn.lock, gone.py");
		expect(first).toContain("   47   line 47");
		expect(first).toContain("   51 + added 51");
		expect(first).toContain("   55   line 55");
		expect(first).not.toContain("   56   line 56");
		expect(result.texts.every((text) => text.length < 900)).toBe(true);
		expect(result.texts.join("\n")).toContain("  301 + added 301");
	});

	test("reviewer applicability: AI only on LLM code, docs only for security; the plan fits the budget", () => {
		const result = py<Record<string, unknown>>(`
def chunk(i, path, text):
    return r.Chunk(i, path, r.file_kind(path), "modified", [], text)
chunks = [chunk(1, "src/a.py", "x = 1"), chunk(2, "src/llm.py", "client.chat.completions.create(prompt)"),
          chunk(3, "README.md", "docs"), chunk(4, "tests/test_a.py", "assert x")]
reviewers = list(r.REVIEWERS.values())
plan = r.plan_find(chunks, reviewers, 10**9)
small = r.plan_find(chunks, reviewers, 9000)
emit({"frames": sorted([f[0].key, f[1].path] for f in plan["frames"]), "na": plan["not_applicable"],
      "small": len(small["frames"]), "dropped": len(small["dropped"]),
      "order": [f[0].key for f in plan["frames"]][:3]})`);
		expect(result.frames).toEqual([
			["ai", "src/llm.py"],
			["arch", "src/a.py"],
			["arch", "src/llm.py"],
			["arch", "tests/test_a.py"],
			["bugs", "src/a.py"],
			["bugs", "src/llm.py"],
			["bugs", "tests/test_a.py"],
			["security", "README.md"],
			["security", "src/a.py"],
			["security", "src/llm.py"],
			["security", "tests/test_a.py"],
			["tests", "src/a.py"],
			["tests", "src/llm.py"],
			["tests", "tests/test_a.py"],
		]);
		expect(result.na).toEqual({ bugs: 1, arch: 1, tests: 1, ai: 3 });
		expect(result.order).toEqual(["bugs", "bugs", "bugs"]);
		expect((result.small as number) + (result.dropped as number)).toBe(14);
		expect(result.dropped as number).toBeGreaterThan(0);
	});

	test("resolve_scope: the branch since its merge base with main, plus uncommitted and untracked files", () => {
		const dir = fixtureRepo();
		writeFileSync(join(dir, "README.md"), "# calc\n\nmore\n");
		writeFileSync(join(dir, "extra.py"), "print('new')\n");
		const scope = py<{ label: string; paths: string[] }>(
			`s = r.resolve_scope(r.parse_args(''), '.')\nemit({"label": s.label, "paths": [f.path for f in s.files]})`,
			dir,
		);
		expect(scope.label).toMatch(/^branch feature vs main \(merge base [0-9a-f]{8}\) plus uncommitted changes$/);
		expect(scope.paths).toEqual(["README.md", "calc.py", "extra.py"]);
		const limited = py<{ label: string; paths: string[] }>(
			`s = r.resolve_scope(r.parse_args('main calc.py'), '.')\nemit({"label": s.label, "paths": [f.path for f in s.files]})`,
			dir,
		);
		expect(limited.label).toMatch(
			/^changes since main \(merge base [0-9a-f]{8}\), including uncommitted in calc\.py$/,
		);
		expect(limited.paths).toEqual(["calc.py"]);
	});

	test("no changes, no repository and a missing gh end cleanly without a single frame", () => {
		const dir = tempDir("ultron-review-clean-");
		git(dir, "init", "-q", "-b", "main");
		writeFileSync(join(dir, "a.py"), "x = 1\n");
		git(dir, "add", ".");
		git(dir, "commit", "-qm", "a");
		const clean = py<{ report: string; calls: number }>(
			`f = FakeRlm()\nrv = asyncio.run(r.run(f, ''))\nemit({"report": rv.report, "calls": len(f.calls)})`,
			dir,
		);
		expect(clean).toEqual({
			report: "# Code review: uncommitted changes on main\n\nNo changes to review.\n",
			calls: 0,
		});
		const outside = tempDir("ultron-review-nogit-");
		const none = py<string>(`emit(asyncio.run(r.run(FakeRlm(), '')).report)`, outside);
		expect(none).toMatch(/^\/review: .* is not inside a git repository/);
		const noGh = py<string>(`emit(asyncio.run(r.run(FakeRlm(), '12', which=lambda name: None)).report)`, dir);
		expect(noGh).toBe("/review: reviewing a pull request needs the GitHub CLI (gh), which is not installed\n");
	});

	test("a PR is read with gh pr diff and gh pr view", () => {
		const dir = fixtureRepo();
		const diff = git(dir, "diff", "main", "--", "calc.py");
		const result = py<{ label: string; paths: string[]; argv: string[][] }>(
			`
argv = []
def runner(args, cwd, timeout):
    if args[0] == 'gh':
        argv.append(args)
        if args[2] == 'diff':
            return 0, ${JSON.stringify(diff)}, ''
        return 0, json.dumps({"number": 5, "title": "Tweak total", "url": "https://x/pull/5", "headRefOid": "0" * 40}), ''
    return r._run_process(args, cwd, timeout)
s = r.resolve_scope(r.parse_args('#5'), '.', runner=runner, which=lambda name: '/usr/bin/gh')
emit({"label": s.label, "paths": [f.path for f in s.files], "argv": argv, "notes": s.notes})`,
			dir,
		);
		expect(result.label).toBe("PR #5: Tweak total");
		expect(result.paths).toEqual(["calc.py"]);
		expect(result.argv.map((args) => args.slice(0, 3))).toEqual([
			["gh", "pr", "diff"],
			["gh", "pr", "view"],
		]);
	});
});

describe("review_api: findings, dedupe and the verify filter", () => {
	test("finder replies are bounded: the chunk's file, lines in range, known severities, clamped confidence", () => {
		const findings = py<Record<string, unknown>[]>(`
hunk = r.Hunk(10, 1, 10, 2, "", [r.DiffLine(" ", 10, 10, "a"), r.DiffLine("+", None, 11, "b")])
chunk = r.Chunk(3, "src/a.py", "code", "modified", [hunk], "text")
raw = [{"file": "other.py", "line": 11, "severity": "BLOCKER", "category": "SQL injection", "claim": "c" * 900,
        "why": "w", "suggested_fix": "f", "confidence": 7},
       {"file": "src/a.py", "line": 9999, "severity": "catastrophic", "category": "", "claim": "x", "why": "",
        "suggested_fix": "", "confidence": "high"},
       {"claim": ""}, "not a dict"]
emit(r.normalize_findings(raw, r.REVIEWERS["bugs"], chunk, 40))`);
		expect(findings).toHaveLength(2);
		expect(findings[0]).toMatchObject({
			file: "src/a.py",
			line: 11,
			severity: "blocker",
			category: "security",
			confidence: 1,
			reviewers: ["bugs"],
			chunk: 3,
		});
		expect((findings[0]!.claim as string).length).toBe(300);
		expect(findings[1]).toMatchObject({ line: 10, severity: "minor", category: "correctness", confidence: 0.5 });
	});

	test("dedupe merges the same file, nearby line and category (or the same claim), keeping the strongest", () => {
		const merged = py<Record<string, unknown>[]>(`
def f(line, severity, category, claim, reviewer, confidence=0.5, path="a.py"):
    return {"file": path, "line": line, "severity": severity, "category": category, "claim": claim, "why": "",
            "suggested_fix": "", "confidence": confidence, "reviewers": [reviewer]}
emit(r.dedupe([
    f(10, "minor", "correctness", "loop skips the last element", "bugs", 0.4),
    f(12, "major", "correctness", "off by one in the loop bound", "tests", 0.8),
    f(11, "nit", "maintainability", "rename this variable", "arch"),
    f(30, "minor", "security", "user input reaches the shell unescaped", "security"),
    f(31, "major", "correctness", "user input reaches the shell unescaped here", "bugs", 0.9),
    f(10, "minor", "correctness", "loop skips the last element", "bugs", path="b.py"),
]))`);
		expect(merged.map((item) => [item.file, item.line, item.severity, item.reviewers, item.id])).toEqual([
			["a.py", 31, "major", ["security", "bugs"], 1],
			["a.py", 12, "major", ["bugs", "tests"], 2],
			["b.py", 10, "minor", ["bugs"], 3],
			["a.py", 11, "nit", ["arch"], 4],
		]);
		expect(merged[1]).toMatchObject({ claim: "off by one in the loop bound", confidence: 0.8, duplicates: 2 });
	});

	test("paraphrases of one problem from different reviewers merge; a different problem on the line does not", () => {
		const merged = py<unknown[]>(`
def f(category, claim, reviewer):
    return {"file": "inventory.py", "line": 15, "severity": "major", "category": category, "claim": claim,
            "why": "", "suggested_fix": "", "confidence": 0.9, "reviewers": [reviewer]}
emit([[m["reviewers"], m.get("duplicates", 1)] for m in r.dedupe([
    f("correctness", "The added and have > 0 clause skips the underflow guard when the item has zero stock, so take drives the count negative and breaks its documented refuse to go below zero contract.", "bugs"),
    f("tests", "The rewritten guard skips the check entirely when have == 0, so take() stores a negative count instead of raising, breaking the documented refuse to go below zero contract.", "tests"),
    f("security", "take accepts a negative amount, which inverts the operation into a restock, unlike restock's non-negative check.", "security"),
])])`);
		expect(merged).toEqual([
			[["bugs", "tests"], 2],
			[["security"], 1],
		]);
	});

	test("rejected findings never survive; confirmations must quote the source; failures stay uncertain", () => {
		const result = py<{ confirmed: unknown[]; uncertain: unknown[]; rejected: unknown[] }>(`
source = r.source_window(["def f(xs):", "    for i in range(len(xs) - 1):", "        use(xs[i])"], 2)
def f(claim):
    return {"file": "a.py", "line": 2, "severity": "major", "category": "correctness", "claim": claim,
            "why": "", "suggested_fix": "", "confidence": 0.7, "reviewers": ["bugs"]}
findings = [f("real"), f("false"), f("unquoted"), f("lost"), f("failed"), f("moved")]
verdicts = [
    {"verdict": "confirmed", "evidence": "The loop \`for i in range(len(xs) - 1):\` stops one early."},
    {"verdict": "rejected", "evidence": "No such problem."},
    {"verdict": "confirmed", "evidence": "Trust me, it is broken."},
    Incomplete({"reason": "budget_exhausted"}),
    FrameError({"error": "HTTP 500"}),
    {"verdict": "confirmed", "evidence": "for i in range(len(xs) - 1): drops the last", "corrected_line": 3},
]
c, u, rej = r.apply_verdicts(findings, verdicts, [source] * 6, [3] * 6)
emit({"confirmed": [[x["claim"], x["line"]] for x in c], "uncertain": [[x["claim"], x["verification"]] for x in u],
      "rejected": [x["claim"] for x in rej]})`);
		expect(result.confirmed).toEqual([
			["real", 2],
			["moved", 3],
		]);
		expect(result.rejected).toEqual(["false"]);
		expect(result.uncertain).toEqual([
			["unquoted", "the verifier confirmed it without quoting the source"],
			["lost", "not verified (budget_exhausted)"],
			["failed", "not verified (HTTP 500)"],
		]);
	});

	test("the report groups confirmed findings by severity, lists uncertain ones apart and omits rejected ones", () => {
		const report = py<string>(`
def f(line, severity, claim, **extra):
    return dict({"file": "a.py", "line": line, "severity": severity, "category": "correctness", "claim": claim,
                 "why": "why " + claim, "suggested_fix": "fix " + claim, "confidence": 0.8, "reviewers": ["bugs"],
                 "evidence": "evidence " + claim}, **extra)
source = ["line %d" % n for n in range(1, 20)]
text = r.render_report(label="branch x vs main", files=[r.FileDiff("a.py")], reviewers=[r.REVIEWERS["bugs"]],
    confirmed=[f(3, "minor", "minor one"), f(2, "blocker", "blocking one")],
    uncertain=[f(9, "major", "maybe one", verification="needs the caller")],
    rejected=[f(5, "major", "SECRET-REJECTED-CLAIM")],
    stats={"raised": 6, "merged": 4, "frames": 7, "find_frames": 4, "verify_frames": 3, "tokens": 12345,
           "budget": 300000},
    not_checked=["Reviewers not selected: Security."], read_file=lambda path: source, path="/tmp/r.md")
emit(text)`);
		expect(report).not.toContain("SECRET-REJECTED-CLAIM");
		expect(report.indexOf("## Blockers (1)")).toBeLessThan(report.indexOf("## Minor (1)"));
		expect(report).toContain("**1. `a.py:2`** blocking one");
		expect(report).toContain("    2 | line 2");
		expect(report).toContain("- Evidence: evidence blocking one");
		expect(report).toContain("- Fix: fix blocking one");
		expect(report).toContain("## Uncertain, not confirmed (1)");
		expect(report).toContain("- `a.py:9` (major) maybe one Verifier: needs the caller");
		expect(report).toContain(
			"- Findings: 6 raised, 4 after merging duplicates, 2 confirmed, 1 rejected, 1 uncertain.",
		);
		expect(report).toContain(
			"- Cost: 7 model calls for 4 finder and 3 verifier frames (re-asks included), 12,345 tokens of a 300,000-token cap.",
		);
		expect(report).toContain("### What this review did not check");
		expect(report).toContain("- Reviewers not selected: Security.");
	});

	test("posting needs confirm=True and a PR, and uses gh pr comment with the saved report", () => {
		const result = py<string[]>(`
import tempfile, os
path = os.path.join(tempfile.mkdtemp(), "r.md")
open(path, "w").write("report")
argv = []
def runner(args, cwd, timeout):
    argv.append(args)
    return 0, "https://github.com/o/r/pull/5#issuecomment-1", ""
gh = lambda name: "/usr/bin/gh"
out = []
for review, kwargs in [(r.Review("x", path, pr=5), {}), (r.Review("x", path), {"confirm": True}),
                       (r.Review("x", path, pr=5, post_pending=True), {"confirm": True})]:
    try:
        out.append(asyncio.run(r.post(review, runner=runner, which=gh, **kwargs)))
    except r.ReviewError as error:
        out.append(str(error))
out.append(asyncio.run(r.post(path, pr=9, confirm=True, runner=runner, which=gh)))
out.append(" ".join(argv[0][:4]) + " " + argv[0][4])
emit(out)`);
		expect(result).toEqual([
			"posting needs confirm=True, and only after the user explicitly agreed to post",
			"this review has no pull request to post to",
			"https://github.com/o/r/pull/5#issuecomment-1",
			"https://github.com/o/r/pull/5#issuecomment-1",
			"gh pr comment 5 --body-file",
		]);
	});

	test("--deep sends uncertain findings to sub-agents and applies their verdicts", () => {
		const dir = fixtureRepo();
		const result = py<{ report: string; spawned: string[]; confirmed: number }>(
			`
class Handle:
    def __init__(self, i): self.rlm_child_id = "child-%d" % i
class DeepRlm(FakeRlm):
    spawned = []
    async def spawn(self, prompt, **kwargs):
        self.spawned.append(kwargs["name"])
        assert "Do not edit or create files" in prompt and '"line": 4' in prompt
        return Handle(len(self.spawned))
    async def collect(self, handles, timeout_ms=0):
        return [{"id": h.rlm_child_id, "result": {"status": "succeeded", "value":
                 'Checked callers. {"verdict": "confirmed", "evidence": "for i in range(len(items) - 1): skips one", "corrected_line": null}'}}
                for h in handles]
finder = lambda task, text: ([{"file": "calc.py", "line": 4, "severity": "major", "category": "bug",
    "claim": "The loop skips the last item.", "why": "", "suggested_fix": "", "confidence": 0.9}]
    if "Correctness" in task else [])
f = DeepRlm(finder, lambda text: {"verdict": "uncertain", "evidence": "need callers"})
rv = asyncio.run(r.run(f, '--deep --only bugs'))
emit({"report": rv.report, "spawned": f.spawned, "confirmed": len(rv.confirmed)})`,
			dir,
		);
		expect(result.spawned).toEqual(["review-check-1"]);
		expect(result.confirmed).toBe(1);
		expect(result.report).toContain("1 verification sub-agents");
	});
});

// --- End to end: a real kernel and host, with frames answered by a scripted provider ---------------------------

type Call = { lane: string; message: string };
const MODEL = {
	provider: "fake",
	id: "review-model",
	api: "openai-completions",
	contextWindow: 64_000,
	maxTokens: 4_096,
};

/** Harness lanes that run the inference hooks and answer each frame request from `script`. */
function scriptedFrames(script: (message: string) => string) {
	const handlers = new Map<string, Array<(event: unknown, context: Context) => unknown>>();
	const calls: Call[] = [];
	const run = async (name: string, event: Record<string, unknown>) => {
		let result: unknown;
		for (const handler of handlers.get(name) ?? []) result = (await handler(event, {} as Context)) ?? result;
		return result as Record<string, unknown> | undefined;
	};
	const lanes = new Map<string, object>();
	const harness = {
		hooks: {
			on(name: string, handler: (event: unknown, context: Context) => unknown) {
				const list = handlers.get(name) ?? [];
				list.push(handler);
				handlers.set(name, list);
				return () => list.splice(list.indexOf(handler), 1);
			},
		},
		lane: async (name: string) => {
			let lane = lanes.get(name);
			if (lane) return lane;
			const entries: Array<{ id: string; type: "message"; message: Record<string, unknown> }> = [];
			let counter = 0;
			lane = {
				getActiveTools: async () => ["rlm"],
				setActiveTools: async () => {},
				setModel: async () => {},
				getModel: async () => MODEL,
				steer: async () => ({ ok: true, value: {} }),
				abort: async () => ({ ok: true }),
				prompt: async (message: string) => {
					const fromTipId = entries.at(-1)?.id ?? null;
					await run("transform_context", { lane: name, messages: [], systemPrompt: "ROOT" });
					const payload = { model: MODEL.id, max_completion_tokens: MODEL.maxTokens, messages: [] };
					await run("before_payload", { lane: name, model: MODEL, payload });
					calls.push({ lane: name, message });
					const text = script(message);
					const usage = {
						input: Math.ceil(message.length / 4),
						output: Math.ceil(text.length / 4),
						cacheRead: 0,
						cacheWrite: 0,
						totalTokens: 0,
						cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
					};
					const assistant = { role: "assistant", content: [{ type: "text", text }], usage, stopReason: "stop" };
					await run("after_response", { lane: name, message: assistant });
					counter += 1;
					const id = `${name}#${counter}`;
					entries.push({ id, type: "message", message: assistant });
					return { ok: true, value: { status: "completed", tipId: id, fromTipId } };
				},
				findEntries: async (query?: { stopAtId?: string }) => {
					const start =
						query?.stopAtId === undefined ? 0 : entries.findIndex((entry) => entry.id === query.stopAtId) + 1;
					return entries.slice(start).reverse();
				},
			};
			lanes.set(name, lane);
			return lane;
		},
	};
	return { harness, calls };
}

/** The scripted model: finders report one real bug and one planted false finding; the verifier checks the source. */
function reviewScript(message: string): string {
	if (message.includes("You check one code review finding")) {
		const cited = /around line (\d+) \(> marks the cited line\):\n([\s\S]*?)\n--- end of view/.exec(message);
		const window = cited?.[2] ?? "";
		const citedLine = window.split("\n").find((line) => line.startsWith(">")) ?? "";
		if (message.includes("skips the last item") && citedLine.includes("range(len(items) - 1)"))
			return JSON.stringify({
				verdict: "confirmed",
				evidence:
					"`for i in range(len(items) - 1):` stops before the last index, so the last price is never added.",
				corrected_line: null,
			});
		if (message.includes("divides by zero") && window.includes("if count == 0:"))
			return JSON.stringify({
				verdict: "rejected",
				evidence: "`if count == 0:` returns before the division.",
				corrected_line: null,
			});
		return JSON.stringify({ verdict: "uncertain", evidence: "cannot tell", corrected_line: null });
	}
	if (message.includes("Your specialty: Correctness") && message.includes("File: calc.py")) {
		return JSON.stringify([
			{
				file: "calc.py",
				line: 4,
				severity: "major",
				category: "bug",
				claim: "total() skips the last item.",
				why: "range(len(items) - 1) stops one index early.",
				suggested_fix: "Loop over range(len(items)) or sum the prices directly.",
				confidence: 0.9,
			},
			{
				file: "calc.py",
				line: 12,
				severity: "blocker",
				category: "bug",
				claim: "average() divides by zero when count is 0.",
				why: "count may be zero.",
				suggested_fix: "Guard count == 0.",
				confidence: 0.6,
			},
		]);
	}
	return "[]";
}

describe("/review end to end in a kernel with a scripted provider", () => {
	test("the real bug is confirmed and reported; the planted false finding is rejected and never shown", async () => {
		const repo = fixtureRepo();
		const { harness, calls } = scriptedFrames(reviewScript);
		const usage = new NativeUsageLedger();
		const inference = createInferenceRuntime({
			contextDir: tempDir("ultron-review-ctx-"),
			traces: createMemoryFrameStore(),
			usage,
		});
		inference.install(harness as never);
		const host = new NativeRlmHost(harness as never, {} as never, {
			store: memoryStore(),
			definitionStore: memoryDefinitionStore(),
			usage,
			frames: inference.executor,
			modules: [inference.module],
		});
		const kernel = new RlmKernel({ cwd: repo, runtimePath: RUNTIME }, (type, payload) =>
			host.handle(type, payload as Record<string, unknown>, {} as Context),
		);
		try {
			const result = await kernel.execute(
				["import review_api", 'review = await review_api.run(rlm, "")', "print(review.report)", "review"].join(
					"\n",
				),
			);
			expect(result.status).toBe("ok");
			const report = result.stdout ?? "";
			expect(report).toContain("# Code review: branch feature vs main");
			expect(report).toContain("## Major (1)");
			expect(report).toContain("**1. `calc.py:4`** total() skips the last item.");
			expect(report).toContain("    4 |     for i in range(len(items) - 1):");
			expect(report).not.toContain("divides by zero");
			expect(report).toContain(
				"- Findings: 2 raised, 2 after merging duplicates, 1 confirmed, 1 rejected, 0 uncertain.",
			);
			expect(report).toMatch(
				/- Cost: 6 model calls for 4 finder and 2 verifier frames \(re-asks included\), [\d,]+ tokens of a 300,000-token cap\./,
			);
			expect(report).toContain("AI and LLM integration reviewer skipped 1 chunk(s) with no LLM-related code.");
			expect(result.result).toMatch(/^Review\(1 confirmed, 0 uncertain, 1 rejected; report: .*ultron-review/);
			// Finders ran one frame per (reviewer, chunk); README.md is prose, so only security read it.
			const finders = calls.filter((call) => !call.message.includes("You check one code review finding"));
			expect(finders).toHaveLength(4);
			expect(finders.filter((call) => call.message.includes("File: README.md"))).toHaveLength(0);
			// The verifier saw the cited source and the callers of the enclosing function, found with git grep.
			const verifier = calls.find((call) => call.message.includes("total() skips the last item"))!;
			expect(verifier.message).toContain(">    4 |     for i in range(len(items) - 1):");
			expect(verifier.message).toContain("report.py:5: return f'total={total(items)}'");
		} finally {
			await kernel.shutdown();
		}
	}, 60_000);
});
