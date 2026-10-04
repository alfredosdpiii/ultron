import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { after, test } from "node:test";
import { fileURLToPath } from "node:url";
import { buildCase, builtCase, git, mirrorDir } from "../evals/autoreview/cases.mjs";
import {
	caseOrder,
	caseSpec,
	changeRanges,
	contractArgs,
	DEFAULT_SEED,
	ineligibleReason,
	judgeCandidates,
	judgePrompt,
	located,
	normalizePath,
	parseJudgeReply,
	parsePatch,
	parseReview,
	patchStats,
	percentile,
	renderMarkdown,
	resultStem,
	reviewerArgv,
	sampleCases,
	scoreCase,
	splitCommand,
	summarize,
	ultronCommand,
	withoutHome,
} from "../evals/autoreview/lib.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const RUN = resolve(HERE, "../evals/autoreview/run.mjs");
const STUB = resolve(HERE, "../evals/autoreview/stub-reviewer.mjs");

const temps = [];
function tempDir(prefix) {
	const dir = mkdtempSync(join(tmpdir(), prefix));
	temps.push(dir);
	return dir;
}
after(() => {
	for (const dir of temps) rmSync(dir, { recursive: true, force: true });
});

// ---------------------------------------------------------------------------------------------------------------
// Fixture: a tiny upstream repository with a bug at `baseCommit` and a gold patch that fixes it.

const COMMIT_ENV = {
	GIT_AUTHOR_NAME: "t",
	GIT_AUTHOR_EMAIL: "t@example.invalid",
	GIT_AUTHOR_DATE: "2001-01-01T00:00:00Z",
	GIT_COMMITTER_NAME: "t",
	GIT_COMMITTER_EMAIL: "t@example.invalid",
	GIT_COMMITTER_DATE: "2001-01-01T00:00:00Z",
};
const numbered = (name, count) => Array.from({ length: count }, (_, index) => `${name} line ${index + 1}`);
const text = (lines) => `${lines.join("\n")}\n`;

function write(dir, path, lines) {
	mkdirSync(dirname(join(dir, path)), { recursive: true });
	writeFileSync(join(dir, path), text(lines));
}

function commit(dir, message) {
	git(["add", "--all"], { cwd: dir });
	git(["commit", "-q", "-m", message], { cwd: dir, env: COMMIT_ENV });
	return git(["rev-parse", "HEAD"], { cwd: dir }).trim();
}

/**
 * History: an initial commit; a commit that changes docs/notes.txt (a candidate for extra hunks); a commit that
 * changes src/calc.py (the gold patch's file, never a candidate) and is the buggy base commit. The gold patch then
 * replaces line 5 of calc.py, adds two lines after its line 25, replaces line 3 of util.py, adds src/helper.py and
 * deletes src/old.py.
 */
function makeUpstream(dir, { repo = "fixture/one", id = "fixture__one-1" } = {}) {
	mkdirSync(dir, { recursive: true });
	git(["init", "-q", "-b", "main", dir]);
	const calc = numbered("calc", 30);
	const util = numbered("util", 8);
	const notes = numbered("notes", 10);
	write(dir, "src/calc.py", calc);
	write(dir, "src/util.py", util);
	write(dir, "src/old.py", numbered("old", 4));
	write(dir, "docs/notes.txt", notes);
	commit(dir, "initial");
	notes[6] = "notes line 7, reworded";
	write(dir, "docs/notes.txt", notes);
	commit(dir, "reword a note");
	calc[14] = "calc line 15, tuned";
	write(dir, "src/calc.py", calc);
	const baseCommit = commit(dir, "tune calc");
	calc[4] = "calc line 5, fixed";
	calc.splice(25, 0, "calc guard A", "calc guard B");
	util[2] = "util line 3, fixed";
	write(dir, "src/calc.py", calc);
	write(dir, "src/util.py", util);
	write(dir, "src/helper.py", numbered("helper", 3));
	rmSync(join(dir, "src/old.py"));
	const fixed = commit(dir, "fix");
	const patch = git(["diff", "--no-renames", baseCommit, fixed], { cwd: dir });
	git(["update-ref", `refs/bench/${baseCommit}`, baseCommit], { cwd: dir });
	return { instance_id: id, repo, base_commit: baseCommit, problem_statement: "calc returns the wrong value for line 5", patch };
}

const EXPECTED_BUGGY_TRUTH = [
	{ file: "src/calc.py", status: "modified", hunks: [[2, 8], [23, 28]], changed: [[5, 5], [25, 26]] },
	{ file: "src/helper.py", status: "deleted", hunks: [], changed: [] },
	{ file: "src/old.py", status: "added", hunks: [[1, 4]], changed: [[1, 4]] },
	{ file: "src/util.py", status: "modified", hunks: [[1, 6]], changed: [[3, 3]] },
];

// ---------------------------------------------------------------------------------------------------------------
// Patches and ground truth

test("parsePatch reads files, statuses and hunks; patchStats counts them", () => {
	const instance = makeUpstream(join(tempDir("ar-parse-"), "up"));
	const files = parsePatch(instance.patch);
	assert.deepEqual(
		files.map((file) => [file.path, file.status, file.hunks.length]),
		[
			["src/calc.py", "modified", 2],
			["src/helper.py", "added", 1],
			["src/old.py", "deleted", 1],
			["src/util.py", "modified", 1],
		],
	);
	assert.deepEqual(patchStats(files), { files: 4, hunks: 5, added: 7, removed: 6, changed: 13 });
	assert.ok(files[0].raw.startsWith("diff --git a/src/calc.py b/src/calc.py\n"));
	assert.ok(!files[0].raw.includes("src/helper.py"));
	assert.deepEqual(files[0].hunks[0], {
		oldStart: 2,
		oldLines: 7,
		newStart: 2,
		newLines: 7,
		lines: [" calc line 2", " calc line 3", " calc line 4", "-calc line 5", "+calc line 5, fixed", " calc line 6", " calc line 7", " calc line 8"],
	});
});

test("parsePatch handles omitted counts, no-newline markers and hunk bodies that look like headers", () => {
	const patch = [
		"diff --git a/a.txt b/a.txt",
		"--- a/a.txt",
		"+++ b/a.txt",
		"@@ -3 +3 @@ section",
		"-old",
		"\\ No newline at end of file",
		"+new",
		"@@ -9,0 +10,2 @@",
		"+--- not a header",
		"+diff --git a/x b/x",
		"",
	].join("\n");
	const [file] = parsePatch(patch);
	assert.equal(file.hunks.length, 2);
	assert.deepEqual(file.hunks[0].lines, ["-old", "+new"]);
	assert.deepEqual(file.hunks[1].lines, ["+--- not a header", "+diff --git a/x b/x"]);
	// New side: line 3 replaced; lines 10-11 added. Old side: line 3; the two lines around the insertion after 9.
	assert.deepEqual(changeRanges([file], "new")[0].changed, [[3, 3], [10, 11]]);
	assert.deepEqual(changeRanges([file], "old")[0].changed, [[3, 3], [9, 10]]);
	assert.deepEqual(changeRanges([file], "old")[0].hunks, [[3, 3], [9, 10]]);
});

test("ground truth of the reversed patch is in the buggy file's numbering: multi-hunk, multi-file, new and deleted files", () => {
	const instance = makeUpstream(join(tempDir("ar-truth-"), "up"));
	const buggy = caseSpec(instance, "buggy");
	assert.deepEqual(buggy.truth, EXPECTED_BUGGY_TRUTH);
	assert.equal(buggy.id, "buggy-fixture__one-1");
	const clean = caseSpec(instance, "clean");
	assert.deepEqual(clean.truth, [
		{ file: "src/calc.py", status: "modified", hunks: [[2, 8], [23, 30]], changed: [[5, 5], [26, 27]] },
		{ file: "src/helper.py", status: "added", hunks: [[1, 3]], changed: [[1, 3]] },
		{ file: "src/old.py", status: "deleted", hunks: [], changed: [] },
		{ file: "src/util.py", status: "modified", hunks: [[1, 6]], changed: [[3, 3]] },
	]);
});

test("ineligibleReason rejects large, wide, binary, renamed and empty patches", () => {
	const instance = makeUpstream(join(tempDir("ar-elig-"), "up"));
	assert.equal(ineligibleReason(instance), null);
	assert.equal(ineligibleReason(instance, { maxLines: 12 }), "more than 12 changed lines");
	assert.equal(ineligibleReason(instance, { maxFiles: 3 }), "more than 3 files");
	assert.equal(ineligibleReason({ patch: "" }), "no text hunks");
	assert.equal(ineligibleReason({ patch: "diff --git a/x.png b/x.png\nBinary files a/x.png and b/x.png differ\n" }), "no text hunks");
	assert.equal(
		ineligibleReason({ patch: `${instance.patch}diff --git a/x.png b/x.png\nBinary files a/x.png and b/x.png differ\n` }),
		"binary change",
	);
	assert.equal(
		ineligibleReason({ patch: "diff --git a/a b/b\nrename from a\nrename to b\n--- a/a\n+++ b/b\n@@ -1 +1 @@\n-x\n+y\n" }),
		"rename",
	);
});

// ---------------------------------------------------------------------------------------------------------------
// Case repositories

function diffOf(built, extra = []) {
	return parsePatch(git(["diff", "--no-renames", ...extra, built.base, built.head], { cwd: built.repoDir }));
}

test("buildCase: the buggy case's diff is the reversed gold patch and its head holds the buggy lines at the ground truth", () => {
	const root = tempDir("ar-build-");
	const upstream = join(root, "up");
	const instance = makeUpstream(upstream);
	const home = join(root, "home");
	const spec = caseSpec(instance, "buggy");
	const built = buildCase({ home, spec, source: upstream, noiseHunks: 0 });
	assert.equal(built.repoDir, join(home, "cases", spec.id));
	assert.deepEqual(built.noise, []);
	// Two commits, the work tree at head and clean.
	assert.equal(git(["rev-list", "--count", "main"], { cwd: built.repoDir }).trim(), "2");
	assert.equal(git(["rev-parse", "main"], { cwd: built.repoDir }).trim(), built.head);
	assert.equal(git(["rev-parse", "main~1"], { cwd: built.repoDir }).trim(), built.base);
	assert.equal(git(["status", "--porcelain"], { cwd: built.repoDir }).trim(), "");
	// What git says the diff changes, in head numbering, is the ground truth computed from the gold patch alone.
	assert.deepEqual(changeRanges(diffOf(built), "new"), EXPECTED_BUGGY_TRUTH);
	// The head is the buggy upstream state; the base has the fix.
	const head = readFileSync(join(built.repoDir, "src/calc.py"), "utf8").split("\n");
	assert.equal(head[4], "calc line 5");
	assert.equal(head[24], "calc line 25");
	assert.equal(head[25], "calc line 26");
	assert.ok(!existsSync(join(built.repoDir, "src/helper.py")));
	assert.ok(existsSync(join(built.repoDir, "src/old.py")));
	assert.match(git(["show", `${built.base}:src/calc.py`], { cwd: built.repoDir }), /calc line 5, fixed\n[^]*calc guard A\ncalc guard B\n/);
	// Nothing of the upstream history is on the branch, and the messages do not describe the defect.
	const log = git(["log", "--format=%s", "main"], { cwd: built.repoDir });
	assert.equal(log, "Update calc.py and 3 more files\nBase\n");
});

test("buildCase: the clean case's diff is the gold patch", () => {
	const root = tempDir("ar-clean-");
	const upstream = join(root, "up");
	const instance = makeUpstream(upstream);
	const spec = caseSpec(instance, "clean");
	const built = buildCase({ home: join(root, "home"), spec, source: upstream });
	assert.deepEqual(built.noise, []);
	assert.deepEqual(changeRanges(diffOf(built), "new"), spec.truth);
	assert.equal(git(["rev-parse", `${built.base}^{tree}`], { cwd: built.repoDir }), git(["rev-parse", `${instance.base_commit}^{tree}`], { cwd: upstream }));
	assert.match(readFileSync(join(built.repoDir, "src/calc.py"), "utf8"), /calc line 5, fixed/);
});

test("buildCase bundles an unrelated earlier change into the buggy diff without moving the ground truth", () => {
	const root = tempDir("ar-noise-");
	const upstream = join(root, "up");
	const instance = makeUpstream(upstream);
	const spec = caseSpec(instance, "buggy");
	const built = buildCase({ home: join(root, "home"), spec, source: upstream, noiseHunks: 2 });
	// The commit that touched calc.py (a gold file) is skipped, the root commit has no parent: only the note is used.
	assert.equal(built.noise.length, 1);
	assert.deepEqual({ ...built.noise[0], commit: "" }, { commit: "", file: "docs/notes.txt", hunks: 1, changed: 2 });
	const files = diffOf(built);
	assert.deepEqual(files.map((file) => file.path), ["docs/notes.txt", "src/calc.py", "src/helper.py", "src/old.py", "src/util.py"]);
	assert.deepEqual(files[0].hunks[0].lines.filter((line) => !line.startsWith(" ")), ["-notes line 7", "+notes line 7, reworded"]);
	assert.deepEqual(changeRanges(files.slice(1), "new"), EXPECTED_BUGGY_TRUTH);
	// The head is still exactly the upstream tree at the buggy commit.
	assert.equal(git(["rev-parse", `${built.head}^{tree}`], { cwd: built.repoDir }), git(["rev-parse", `${instance.base_commit}^{tree}`], { cwd: upstream }));
});

test("buildCase is deterministic and builtCase finds a finished build with the same settings only", () => {
	const root = tempDir("ar-determ-");
	const upstream = join(root, "up");
	const instance = makeUpstream(upstream);
	const spec = caseSpec(instance, "buggy");
	const home = join(root, "home");
	assert.equal(builtCase({ home, spec, noiseHunks: 2 }), null);
	const first = buildCase({ home, spec, source: upstream, noiseHunks: 2 });
	assert.deepEqual(builtCase({ home, spec, noiseHunks: 2 }), first);
	assert.equal(builtCase({ home, spec, noiseHunks: 0 }), null);
	const again = buildCase({ home: join(root, "home2"), spec, source: upstream, noiseHunks: 2 });
	assert.equal(again.base, first.base);
	assert.equal(again.head, first.head);
	assert.throws(() => buildCase({ home, spec: { ...spec, instance: { ...instance, patch: instance.patch.replace("calc line 4", "calc line X") } }, source: upstream }), /git apply/);
});

// ---------------------------------------------------------------------------------------------------------------
// Sample

function fakeInstances() {
	const patch = "diff --git a/f.py b/f.py\n--- a/f.py\n+++ b/f.py\n@@ -1 +1 @@\n-a\n+b\n";
	const out = [];
	for (const [repo, count] of [["big/repo", 30], ["mid/repo", 6], ["small/repo", 1]]) {
		for (let index = 0; index < count; index++) out.push({ instance_id: `${repo.replace("/", "__")}-${index}`, repo, base_commit: "0".repeat(40), patch });
	}
	out.push({ instance_id: "big__repo-binary", repo: "big/repo", base_commit: "0".repeat(40), patch: "" });
	return out;
}

test("sampleCases is seeded, prefix-stable, alternates kinds, and never uses a task twice", () => {
	const instances = fakeInstances();
	const sample = sampleCases(instances, { perKind: 5 });
	assert.equal(sample.length, 10);
	assert.deepEqual(sample, sampleCases([...instances].reverse(), { perKind: 5, seed: DEFAULT_SEED }));
	assert.deepEqual(sample.map((item) => item.kind), ["buggy", "clean", "buggy", "clean", "buggy", "clean", "buggy", "clean", "buggy", "clean"]);
	assert.equal(new Set(sample.map((item) => item.instanceId)).size, 10);
	assert.deepEqual(sampleCases(instances, { perKind: 2 }).map((item) => item.id), sample.slice(0, 4).map((item) => item.id));
	assert.notDeepEqual(sampleCases(instances, { perKind: 5, seed: "another" }).map((item) => item.id), sample.map((item) => item.id));
	assert.ok(!caseOrder(instances).some((instance) => instance.instance_id === "big__repo-binary"));
	assert.equal(caseOrder(instances).length, 37);
});

test("caseOrder goes round the repositories before taking a second task from any", () => {
	const order = caseOrder(fakeInstances());
	assert.deepEqual(new Set(order.slice(0, 3).map((instance) => instance.repo)), new Set(["big/repo", "mid/repo", "small/repo"]));
	// The one-task repository is used up after round one; rounds two to six alternate the other two.
	assert.deepEqual(new Set(order.slice(3, 5).map((instance) => instance.repo)), new Set(["big/repo", "mid/repo"]));
	assert.ok(order.slice(13).every((instance) => instance.repo === "big/repo"));
});

// ---------------------------------------------------------------------------------------------------------------
// Reviewer command

test("splitCommand and reviewerArgv build the reviewer's argv", () => {
	assert.deepEqual(splitCommand(`node "my script.mjs" --flag 'a b' c\\ d ""`), ["node", "my script.mjs", "--flag", "a b", "c d", ""]);
	assert.throws(() => splitCommand('node "x'), /unbalanced/);
	const values = { repo: "/r", base: "b1", head: "h1", model: "p/m", verifyModel: "p/v", budget: 5000 };
	assert.deepEqual(contractArgs(values), ["--repo-dir", "/r", "--base", "b1", "--head", "h1", "--model", "p/m", "--verify-model", "p/v", "--budget", "5000", "--json", "--dry-run"]);
	assert.deepEqual(reviewerArgv({ template: null, ultron: ["node", "cli.js"] }, { repo: "/r", base: "b1", head: "h1" }), [
		"node",
		"cli.js",
		"autoreview",
		"review",
		"--repo-dir",
		"/r",
		"--base",
		"b1",
		"--head",
		"h1",
		"--json",
		"--dry-run",
	]);
	assert.deepEqual(reviewerArgv({ template: "node stub.mjs --stub-mode flag" }, { repo: "/r", base: "b1", head: "h1" }).slice(0, 6), ["node", "stub.mjs", "--stub-mode", "flag", "--repo-dir", "/r"]);
	assert.deepEqual(reviewerArgv({ template: "adapter --dir {repo} --range {base}..{head} --m={model} {budget}" }, { repo: "/r", base: "b1", head: "h1", model: null }), [
		"adapter",
		"--dir",
		"/r",
		"--range",
		"b1..h1",
		"--m=",
		"",
	]);
	assert.deepEqual(ultronCommand(undefined), ["ultron"]);
	assert.deepEqual(ultronCommand("/x/dist/cli.js", "/usr/bin/node"), ["/usr/bin/node", "/x/dist/cli.js"]);
	assert.deepEqual(ultronCommand("/x/bin/ultron"), ["/x/bin/ultron"]);
});

// ---------------------------------------------------------------------------------------------------------------
// Scoring

const finding = (overrides) => ({
	file: "src/calc.py",
	line: 5,
	severity: "major",
	category: "correctness",
	claim: "c",
	why: "w",
	verification: "confirmed",
	confidence: 0.8,
	...overrides,
});
const reviewOf = (verdict, findings) => {
	const parsed = parseReview(JSON.stringify({ verdict, complete: true, findings, timing: { totalMs: 1000, scopeMs: 100, findMs: 600, verifyMs: 300 }, usage: { inputTokens: 1000, outputTokens: 100, costUsd: 0.01, frames: 3 } }));
	assert.ok(parsed.ok, parsed.error);
	return parsed.review;
};
const BUGGY = { kind: "buggy", truth: EXPECTED_BUGGY_TRUTH };
const CLEAN = { kind: "clean", truth: [] };

test("parseReview validates and normalises the reviewer's JSON", () => {
	assert.deepEqual(parseReview(""), { ok: false, error: "no JSON object on stdout" });
	assert.equal(parseReview("[1]").ok, false);
	assert.match(parseReview('{"verdict":"lgtm","findings":[]}').error, /unknown verdict/);
	assert.match(parseReview('{"verdict":"approve"}').error, /findings/);
	const noisy = parseReview(`warming up\n{"verdict":"comment","findings":[{"file":"a.py","line":"7","severity":"weird","verification":"maybe"}]}\n`);
	assert.ok(noisy.ok);
	assert.deepEqual(noisy.review.findings[0], { file: "a.py", line: 7, endLine: 7, severity: "nit", category: "", claim: "", why: "", verification: "uncertain", confidence: null });
	assert.equal(noisy.review.complete, true);
	assert.deepEqual(noisy.review.usage, { inputTokens: null, outputTokens: null, costUsd: null, frames: null });
	assert.equal(parseReview('{"verdict":"approve","complete":false,"findings":[]}').review.complete, false);
	assert.equal(reviewOf("approve", [finding({ line: 5, endLine: 3 })]).findings[0].endLine, 5);
});

test("located: file, hunk range with tolerance, changed lines, deleted files, path forms", () => {
	const at = (line, extra = {}) => reviewOf("comment", [finding({ line, ...extra })]).findings[0];
	assert.equal(located(at(2), EXPECTED_BUGGY_TRUTH), true);
	assert.equal(located(at(13), EXPECTED_BUGGY_TRUTH), true); // hunk 2..8, +5
	assert.equal(located(at(14), EXPECTED_BUGGY_TRUTH), false);
	assert.equal(located(at(17), EXPECTED_BUGGY_TRUTH), false);
	assert.equal(located(at(18), EXPECTED_BUGGY_TRUTH), true); // hunk 23..28, -5
	assert.equal(located(at(10, { endLine: 40 }), EXPECTED_BUGGY_TRUTH), true);
	assert.equal(located(at(8), EXPECTED_BUGGY_TRUTH, { ranges: "changed", tolerance: 2 }), false);
	assert.equal(located(at(7), EXPECTED_BUGGY_TRUTH, { ranges: "changed", tolerance: 2 }), true);
	assert.equal(located(at(5, { file: "src/other.py" }), EXPECTED_BUGGY_TRUTH), false);
	assert.equal(located(at(999, { file: "src/helper.py" }), EXPECTED_BUGGY_TRUTH), true); // deleted by the diff
	assert.equal(located(at(5, { file: "./src/calc.py" }), EXPECTED_BUGGY_TRUTH), true);
	assert.equal(located(at(5, { file: "b/src/calc.py" }), EXPECTED_BUGGY_TRUTH), true);
	assert.equal(located(at(5, { file: "/cases/x/src/calc.py" }), EXPECTED_BUGGY_TRUTH, { repoDir: "/cases/x" }), true);
	assert.equal(located(at(5, { file: "/cases/x/src/calc.py" }), EXPECTED_BUGGY_TRUTH), false);
	assert.equal(located(at(undefined), EXPECTED_BUGGY_TRUTH), false);
	assert.equal(normalizePath("./a/b.py"), "a/b.py");
});

test("scoreCase, buggy: caught needs a confirmed blocker or major finding in the changed region", () => {
	const caught = scoreCase(BUGGY, reviewOf("request_changes", [finding({})]));
	assert.deepEqual(
		{ caught: caught.caught, loose: caught.caughtLoose, tight: caught.caughtTight, verdict: caught.verdictCorrect, located: caught.locatedFindings },
		{ caught: true, loose: true, tight: true, verdict: true, located: [0] },
	);
	assert.deepEqual(caught.bySeverity, { blocker: 0, major: 1, minor: 0, nit: 0 });
	const minor = scoreCase(BUGGY, reviewOf("comment", [finding({ severity: "minor" })]));
	assert.deepEqual([minor.caught, minor.caughtLoose, minor.caughtTight, minor.verdictCorrect], [false, true, false, false]);
	const uncertain = scoreCase(BUGGY, reviewOf("request_changes", [finding({ verification: "uncertain", severity: "blocker" })]));
	assert.deepEqual([uncertain.caught, uncertain.caughtLoose, uncertain.locatedFindings], [false, false, [0]]);
	const elsewhere = scoreCase(BUGGY, reviewOf("request_changes", [finding({ line: 16 }), finding({ file: "docs/notes.txt", line: 7 })]));
	assert.deepEqual([elsewhere.caught, elsewhere.caughtLoose, elsewhere.verdictCorrect, elsewhere.findings], [false, false, true, 2]);
	const inHunkOnly = scoreCase(BUGGY, reviewOf("request_changes", [finding({ line: 11 })]));
	assert.deepEqual([inHunkOnly.caught, inHunkOnly.caughtTight], [true, false]);
	const nit = scoreCase(BUGGY, reviewOf("approve", [finding({ severity: "nit" })]));
	assert.deepEqual([nit.caught, nit.caughtLoose, nit.verdictCorrect], [false, false, false]);
	assert.equal(caught.judged, undefined);
});

test("scoreCase, buggy: judged recall follows the judge, wherever the finding points", () => {
	const review = reviewOf("request_changes", [finding({ file: "src/caller.py", line: 90 }), finding({ severity: "minor" }), finding({ verification: "uncertain" })]);
	assert.deepEqual(judgeCandidates(review).map((entry) => entry.index), [0, 1]);
	const yes = scoreCase(BUGGY, review, { judgements: [{ index: 0, match: true }, { index: 1, match: false }] });
	assert.deepEqual([yes.caught, yes.judged, yes.judgedLoose, yes.judgeErrors], [false, true, true, 0]);
	const minorOnly = scoreCase(BUGGY, review, { judgements: [{ index: 0, match: false }, { index: 1, match: true }] });
	assert.deepEqual([minorOnly.judged, minorOnly.judgedLoose], [false, true]);
	const failed = scoreCase(BUGGY, review, { judgements: [{ index: 0, match: null }, { index: 1, match: false }] });
	assert.deepEqual([failed.judged, failed.judgedLoose, failed.judgeErrors], [false, false, 1]);
	const many = reviewOf("request_changes", Array.from({ length: 12 }, (_, index) => finding({ severity: index === 11 ? "blocker" : "minor" })));
	assert.equal(judgeCandidates(many).length, 8);
	assert.equal(judgeCandidates(many)[0].index, 11);
});

test("scoreCase, clean: a confirmed blocker or major finding is a false alarm; minor and nit are only counted", () => {
	const quiet = scoreCase(CLEAN, reviewOf("approve", [finding({ severity: "minor" }), finding({ severity: "nit" })]));
	assert.deepEqual([quiet.falseAlarm, quiet.approved, quiet.verdictCorrect], [false, true, true]);
	assert.deepEqual(quiet.bySeverity, { blocker: 0, major: 0, minor: 1, nit: 1 });
	const alarm = scoreCase(CLEAN, reviewOf("request_changes", [finding({ severity: "blocker", file: "anything.py" })]));
	assert.deepEqual([alarm.falseAlarm, alarm.approved, alarm.verdictCorrect], [true, false, false]);
	const unconfirmed = scoreCase(CLEAN, reviewOf("comment", [finding({ verification: "uncertain" })]));
	assert.deepEqual([unconfirmed.falseAlarm, unconfirmed.approved, unconfirmed.verdictCorrect], [false, false, false]);
	assert.equal(unconfirmed.caught, undefined);
});

test("judgePrompt carries the defect, the fix and the finding; parseJudgeReply takes the strict JSON answer", () => {
	const spec = { instance: { problem_statement: "wrong value for line 5", patch: "diff --git a/x b/x\n" } };
	const prompt = judgePrompt(spec, reviewOf("comment", [finding({ claim: "returns stale value" })]).findings[0]);
	assert.match(prompt, /wrong value for line 5/);
	assert.match(prompt, /diff --git a\/x b\/x/);
	assert.match(prompt, /returns stale value/);
	assert.deepEqual(parseJudgeReply('Sure.\n{"match": true, "reason": "same bug"}'), { match: true, reason: "same bug" });
	assert.deepEqual(parseJudgeReply('{"match": false}'), { match: false, reason: "" });
	assert.equal(parseJudgeReply('{"match": "yes"}'), null);
	assert.equal(parseJudgeReply("no idea"), null);
});

// ---------------------------------------------------------------------------------------------------------------
// Aggregation and report

function record(arm, kind, name, wallMs, review, judgements) {
	const spec = kind === "buggy" ? BUGGY : CLEAN;
	return { arm, case: `${kind}-${name}`, kind, trial: 1, status: "ok", wallMs, review, score: scoreCase(spec, review, { judgements }) };
}

function sampleRecords() {
	return [
		record("m1", "buggy", "a", 10_000, reviewOf("request_changes", [finding({})]), [{ index: 0, match: true }]),
		record("m1", "buggy", "b", 20_000, reviewOf("comment", [finding({ severity: "minor" })]), [{ index: 0, match: false }]),
		record("m1", "buggy", "c", 30_000, reviewOf("approve", [])),
		record("m1", "clean", "d", 40_000, reviewOf("approve", [finding({ severity: "nit" })])),
		record("m1", "clean", "e", 50_000, reviewOf("request_changes", [finding({})])),
		{ arm: "m1", case: "clean-f", kind: "clean", trial: 1, status: "error", error: "no JSON object on stdout (exit 1)", wallMs: 500 },
		record("m2", "buggy", "a", 5000, reviewOf("approve", [])),
	];
}

test("percentile is nearest-rank", () => {
	assert.equal(percentile([], 50), null);
	assert.equal(percentile([3, 1, 2], 50), 2);
	assert.equal(percentile([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 90), 9);
	assert.equal(percentile([1, 2, 3, 4], 100), 4);
});

test("summarize: recall, false alarms, verdict accuracy, speed and cost per arm", () => {
	const summary = summarize(sampleRecords(), ["m1", "m2"]);
	const m1 = summary.m1;
	assert.deepEqual([m1.runs, m1.scored, m1.errors], [6, 5, 1]);
	assert.deepEqual([m1.buggy.runs, m1.buggy.caught, m1.buggy.caughtLoose, m1.buggy.caughtTight], [3, 1, 2, 1]);
	assert.equal(m1.buggy.recall, 1 / 3);
	assert.equal(m1.buggy.recallLoose, 2 / 3);
	// Judged recall is over the reviews that were judged: two of three.
	assert.deepEqual([m1.buggy.judgedRuns, m1.buggy.recallJudged], [2, 0.5]);
	assert.deepEqual([m1.clean.runs, m1.clean.falseAlarms, m1.clean.falseAlarmRate, m1.clean.approved], [2, 1, 0.5, 1]);
	assert.deepEqual(m1.clean.findings, { blocker: 0, major: 1, minor: 0, nit: 1 });
	assert.equal(m1.verdictAccuracy, 2 / 5);
	assert.equal(m1.findingsPerReview, 4 / 5);
	assert.deepEqual(m1.seconds, { p50: 30, p90: 50, max: 50 });
	assert.deepEqual(m1.reviewerTimingMs, { total: 1000, scope: 100, find: 600, verify: 300 });
	assert.deepEqual(m1.tokens, { input: 5000, output: 500, inputPerReview: 1000, outputPerReview: 100 });
	assert.ok(Math.abs(m1.costUsd.total - 0.05) < 1e-9);
	assert.equal(m1.framesPerReview, 3);
	assert.deepEqual([summary.m2.buggy.recall, summary.m2.clean.falseAlarmRate, summary.m2.buggy.recallJudged], [0, null, null]);
});

test("renderMarkdown: a summary row per arm, a row per case and per run; resultStem names the arms", () => {
	const records = sampleRecords();
	const result = {
		date: "2026-10-04",
		dataset: "SWE-bench/SWE-bench_Verified",
		trials: 1,
		reviewer: { command: "ultron autoreview review --repo-dir <repo> --base <base> --head <head> --json --dry-run", version: "ultron 0.87.26" },
		judge: { model: "p/judge" },
		arms: [{ name: "m1" }, { name: "m2" }],
		sample: {
			seed: DEFAULT_SEED,
			buggy: 3,
			clean: 3,
			cases: [{ id: "buggy-a", repo: "fixture/one", stats: { files: 1, hunks: 2, changed: 5 }, noise: [{ hunks: 2 }] }],
		},
		summary: summarize(records, ["m1", "m2"]),
		records,
	};
	const markdown = renderMarkdown(result);
	assert.match(markdown, /^# Autoreview benchmark: m1, m2\n/);
	assert.match(markdown, /\| m1 \| 1\/3 \(33%\) \| 2\/3 \(67%\) \| 1\/3 \(33%\) \| 50% \| 1\/2 \(50%\) \| 40% \| 0\.8 \| 30\.0 \| 50\.0 \| 50\.0 \| 1,000 \/ 100 \| \$0\.010 \| 1 \|/);
	assert.match(markdown, /\| m2 \| 0\/1 \(0%\) \| 0\/1 \(0%\) \| 0\/1 \(0%\) \| - \| - \| 0% \|/);
	assert.match(markdown, /\| buggy-a \| fixture\/one \| 1 \| 2 \| 5 \| 2 \|/);
	assert.match(markdown, /\| m1 \| buggy-a \| 1 \| ok \| request_changes \| yes \| yes \| yes \| yes \| - \| 0\/1\/0\/0 \| 10\.0 \|/);
	assert.match(markdown, /\| m1 \| clean-e \| 1 \| ok \| request_changes \(wrong\) \| - \| - \| - \| - \| yes \|/);
	assert.match(markdown, /\| m1 \| clean-f \| 1 \| error: no JSON object on stdout \(exit 1\) \|/);
	assert.match(markdown, /Judge: `p\/judge`/);
	assert.equal(resultStem({ date: "2026-10-04", perKind: 20, arms: ["cliproxyapi/gpt-6.1-sol", "default"] }), "2026-10-04-autoreview-bench-20x2-cliproxyapi_gpt-6.1-sol+default");
	assert.equal(withoutHome("/users/someone/x and /users/someone/y", "/users/someone"), "~/x and ~/y");
});

// ---------------------------------------------------------------------------------------------------------------
// The harness end to end, offline: fixture upstreams as mirrors, the stub reviewer, a fake judge

function harnessHome() {
	const root = tempDir("ar-e2e-");
	const home = join(root, "home");
	mkdirSync(home, { recursive: true });
	const rows = [];
	for (const [repo, id] of [["fixture/one", "fixture__one-1"], ["fixture/two", "fixture__two-1"]]) {
		// The mirror path needs the base commit, which is only known once the upstream exists: build, then move.
		const staging = join(root, `staging-${id}`);
		const instance = makeUpstream(staging, { repo, id });
		const mirror = mirrorDir(home, repo, instance.base_commit);
		mkdirSync(dirname(mirror), { recursive: true });
		git(["clone", "-q", "--bare", staging, mirror]);
		git(["update-ref", `refs/bench/${instance.base_commit}`, instance.base_commit], { cwd: mirror });
		rows.push(instance);
	}
	writeFileSync(join(home, "dataset.jsonl"), rows.map((row) => JSON.stringify(row)).join("\n"));
	return { root, home };
}

function runHarness(home, args, env = {}) {
	return spawnSync(process.execPath, [RUN, ...args], { encoding: "utf8", env: { ...process.env, ULTRON_AUTOREVIEW_HOME: home, ...env } });
}

test("run.mjs: an unknown flag exits 2 and --plan lists cases and arms without building anything", () => {
	const { home } = harnessHome();
	const unknown = runHarness(home, ["--plan", "--help"]);
	assert.equal(unknown.status, 2);
	assert.match(unknown.stderr, /Unknown arguments: --help/);
	assert.equal(runHarness(home, ["--cases", "0", "--plan"]).status, 2);
	const plan = runHarness(home, ["--plan", "--cases", "1", "--models", "p/a,p/b"]);
	assert.equal(plan.status, 0, plan.stderr);
	assert.match(plan.stdout, /2 cases, seed ultron-autoreview-1/);
	assert.match(plan.stdout, /buggy-fixture__(one|two)-1\tbuggy\tfixture\/(one|two)\t[0-9a-f]{12}\t4\t5\t13\tno/);
	assert.match(plan.stdout, /clean-fixture__(one|two)-1\tclean/);
	assert.match(plan.stdout, /Arms \(2\): p\/a, p\/b/);
	assert.match(plan.stdout, /Reviews to run: 4/);
	assert.ok(!existsSync(join(home, "cases")));
});

test("run.mjs: stub reviewer and fake judge end to end; private dirs are removed and no credential is kept", () => {
	const { root, home } = harnessHome();
	const profile = join(root, "profile");
	mkdirSync(profile);
	const secret = "sk-fixture-secret-0123456789abcdef";
	writeFileSync(join(profile, "auth.json"), JSON.stringify({ provider: { key: secret } }));
	writeFileSync(join(profile, "models.json"), JSON.stringify({ providers: {} }));
	const seen = join(root, "judge-env.jsonl");
	const fakeUltron = join(root, "fake-ultron.mjs");
	writeFileSync(
		fakeUltron,
		[
			'import { appendFileSync, existsSync, readFileSync } from "node:fs";',
			'import { join } from "node:path";',
			"const agentDir = process.env.ULTRON_CODING_AGENT_DIR;",
			'const key = JSON.parse(readFileSync(join(agentDir, "auth.json"), "utf8")).provider.key;',
			"appendFileSync(process.env.FAKE_JUDGE_SEEN, `${JSON.stringify({ agentDir, serverDir: process.env.ULTRON_SERVER_DIR, home: process.env.HOME, args: process.argv.slice(2, 5) })}\\n`);",
			"console.error(`debug: using key ${key}`);",
			'console.log(JSON.stringify({ match: process.argv.at(-1).includes("calc returns the wrong value"), reason: "fake" }));',
		].join("\n"),
	);
	const out = join(root, "out");
	const args = ["--reviewer-cmd", `node ${STUB} --stub-mode flag`, "--cases", "1", "--noise", "0", "--models", "stub/m", "--trials", "2", "--ultron", fakeUltron, "--judge-model", "p/judge", "--run-id", "e2e", "--out", out];
	const env = { ULTRON_AUTOREVIEW_PROFILE: profile, FAKE_JUDGE_SEEN: seen };
	const run = runHarness(home, args, env);
	assert.equal(run.status, 0, run.stderr);
	const files = readdirSync(out).sort();
	assert.equal(files.length, 2);
	assert.match(files[0], /^\d{4}-\d\d-\d\d-autoreview-bench-1x2-stub_m\.json$/);
	const raw = readFileSync(join(out, files[0]), "utf8");
	const result = JSON.parse(raw);
	// The stub flags the first changed hunk of the first file: inside the ground truth for the buggy case (caught),
	// and a false alarm for the clean one. The fake judge agrees whenever it is shown the problem statement.
	const summary = result.summary["stub/m"];
	assert.deepEqual([summary.runs, summary.errors], [4, 0]);
	assert.deepEqual([summary.buggy.runs, summary.buggy.caught, summary.buggy.caughtTight, summary.buggy.recallJudged], [2, 2, 2, 1]);
	assert.deepEqual([summary.clean.runs, summary.clean.falseAlarms, summary.clean.approved], [2, 2, 0]);
	assert.equal(summary.verdictAccuracy, 0.5);
	assert.equal(result.records.length, 4);
	assert.deepEqual(result.records[0].judge, [{ index: 0, match: true, reason: "fake" }]);
	assert.equal(result.sample.cases[0].truth.length, 4);
	assert.equal(result.reviewer.command, `node ${STUB} --stub-mode flag --repo-dir <repo> --base <base> --head <head> --model <model> --json --dry-run`.split(process.env.HOME).join("~"));
	assert.match(readFileSync(join(out, files[1]), "utf8"), /\| stub\/m \| 2\/2 \(100%\) \|/);
	// No local paths of the cache in the recorded result.
	assert.ok(!raw.includes(home));
	// The judge ran with private, short, now-deleted dirs holding a copy of the credentials.
	const calls = readFileSync(seen, "utf8").trim().split("\n").map((line) => JSON.parse(line));
	assert.equal(calls.length, 2);
	for (const call of calls) {
		assert.match(call.agentDir, /^\/tmp\/u-ar-[^/]+\/a$/);
		assert.match(call.serverDir, /^\/tmp\/u-ar-[^/]+\/s$/);
		assert.match(call.home, /^\/tmp\/u-ar-[^/]+\/h$/);
		assert.deepEqual(call.args, ["-p", "--model", "p/judge"]);
		assert.ok(!existsSync(dirname(call.agentDir)));
	}
	// Evidence is kept under the cache dir, scrubbed of the key, without auth.json.
	const evidence = [];
	const walk = (dir) => {
		for (const entry of readdirSync(dir, { withFileTypes: true })) {
			if (entry.isDirectory()) walk(join(dir, entry.name));
			else evidence.push(join(dir, entry.name));
		}
	};
	walk(join(home, "runs", "e2e"));
	assert.ok(evidence.some((file) => file.endsWith("judge-0.txt")));
	assert.ok(evidence.some((file) => file.endsWith("stdout.json")));
	assert.ok(!evidence.some((file) => file.endsWith("auth.json")));
	for (const file of evidence) assert.ok(!readFileSync(file, "utf8").includes(secret), file);
	assert.match(readFileSync(evidence.find((file) => file.endsWith("judge-0.txt")), "utf8"), /using key \[redacted\]/);
	// The same run id resumes: nothing is reviewed or judged again.
	const again = runHarness(home, args, env);
	assert.equal(again.status, 0, again.stderr);
	assert.equal(readFileSync(seen, "utf8").trim().split("\n").length, 2);
});

test("run.mjs: a reviewer that prints no JSON is an error row, not a score", () => {
	const { root, home } = harnessHome();
	const out = join(root, "out");
	const run = runHarness(home, ["--reviewer-cmd", `node ${STUB} --stub-mode fail`, "--cases", "1", "--only", "fixture__one-1,fixture__two-1", "--out", out]);
	assert.equal(run.status, 0, run.stderr);
	const result = JSON.parse(readFileSync(join(out, readdirSync(out).find((file) => file.endsWith(".json"))), "utf8"));
	assert.deepEqual([result.summary.custom.runs, result.summary.custom.errors, result.summary.custom.buggy.recall], [2, 2, null]);
	assert.match(result.records[0].error, /no JSON object on stdout \(exit 1\): stub-reviewer: failing as asked/);
});

test("run.mjs: Ultron's own reviewer runs with the contract's flags, and only when the CLI has the command", () => {
	const { root, home } = harnessHome();
	const out = join(root, "out");
	const seen = join(root, "seen.jsonl");
	// A CLI without the command: `autoreview --help` prints general help. No review may start.
	const old = join(root, "old-ultron.mjs");
	writeFileSync(old, `import { appendFileSync } from "node:fs";\nappendFileSync(${JSON.stringify(seen)}, JSON.stringify(process.argv.slice(2)) + "\\n");\nconsole.log("ultron - AI coding agent");\n`);
	const refused = runHarness(home, ["--cases", "1", "--ultron", old, "--out", out]);
	assert.equal(refused.status, 1);
	assert.match(refused.stderr, /does not have it/);
	assert.deepEqual(readFileSync(seen, "utf8").trim().split("\n"), ['["autoreview","--help"]']);
	assert.ok(!existsSync(out));
	// A CLI with it: here the stub behind an `autoreview review` front.
	const cli = join(root, "new-ultron.mjs");
	writeFileSync(
		cli,
		[
			'import { spawnSync } from "node:child_process";',
			'import { appendFileSync } from "node:fs";',
			"const args = process.argv.slice(2);",
			'if (args.includes("--help")) console.log("ultron autoreview review --repo-dir <dir> --base <sha> --head <sha>");',
			'else if (args[0] === "--version") console.log("9.9.9");',
			"else {",
			`	appendFileSync(${JSON.stringify(seen)}, JSON.stringify({ args, cwd: process.cwd(), agentDir: process.env.ULTRON_CODING_AGENT_DIR }) + "\\n");`,
			`	const run = spawnSync(process.execPath, [${JSON.stringify(STUB)}, "--stub-mode", "approve", ...args.slice(2)], { stdio: "inherit" });`,
			"	process.exit(run.status);",
			"}",
		].join("\n"),
	);
	const run = runHarness(home, ["--cases", "1", "--ultron", cli, "--models", "p/m", "--verify-model", "p/v", "--budget", "9000", "--out", out]);
	assert.equal(run.status, 0, run.stderr);
	const result = JSON.parse(readFileSync(join(out, readdirSync(out).find((file) => file.endsWith(".json"))), "utf8"));
	assert.equal(result.reviewer.version, "ultron 9.9.9");
	assert.deepEqual([result.summary["p/m"].buggy.recall, result.summary["p/m"].clean.falseAlarmRate, result.summary["p/m"].verdictAccuracy], [0, 0, 0.5]);
	const calls = readFileSync(seen, "utf8").trim().split("\n").slice(1).map((line) => JSON.parse(line));
	assert.equal(calls.length, 2);
	for (const call of calls) {
		assert.deepEqual(call.args.slice(0, 3), ["autoreview", "review", "--repo-dir"]);
		assert.equal(call.args[3], call.cwd);
		assert.deepEqual(call.args.slice(8), ["--model", "p/m", "--verify-model", "p/v", "--budget", "9000", "--json", "--dry-run"]);
		assert.match(call.agentDir, /^\/tmp\/u-ar-[^/]+\/a$/);
		assert.ok(!existsSync(call.agentDir));
	}
});
