import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { checkLock, computeLock, INSTRUMENT_PATHS, LOCK_PATH } from "./acceptance-lock.mjs";
import { applyEdits, classifyRun } from "./acceptance-mutation.mjs";
import { computeRows, INSTRUMENT_CHANGED, renderMarkdown, summarize } from "./acceptance-report.mjs";

const root = "/repo";
const FILE = "packages/coding-agent/test/ultron-x.test.ts";
const OTHER = "packages/coding-agent/test/ultron-y.test.ts";
const okLock = { ok: true, lockMissing: false, manifestChanged: false, instrumentChanged: [], evidenceChanged: [] };

function row(id, tests, status_when_green = "passed", file = FILE) {
	return { id, behavior: id, evidence: tests.length ? [{ file, tests }] : [], status_when_green, notes: `${id} note` };
}

function results(assertions, extra = {}) {
	return {
		success: !assertions.some((a) => a.status === "failed"),
		testResults: [
			{
				name: `${root}/${FILE}`,
				status: assertions.some((a) => a.status === "failed") ? "failed" : "passed",
				message: "",
				assertionResults: assertions.map(([title, status]) => ({ title, fullName: `suite ${title}`, status })),
				...extra,
			},
		],
	};
}

const green = results([
	["guards identity", "passed"],
	["rejects forgery", "passed"],
	["skipped case", "skipped"],
]);

function judge(rows, run, lock = okLock) {
	return Object.fromEntries(computeRows({ manifest: { rows }, run, lock, root }).map((r) => [r.id, r]));
}

test("passes only when every listed test passed and the row is declared passable", () => {
	const out = judge(
		[row("A01", ["guards identity", "rejects forgery"]), row("A02", ["guards identity"], "unverified")],
		{ results: green, infraError: null },
	);
	assert.equal(out.A01.status, "passed");
	assert.deepEqual(out.A01.evidence, { listed: 2, passed: 2, failed: 0, missing: 0, skipped: 0, infrastructure: 0 });
	assert.equal(out.A02.status, "unverified", "partial evidence is never a pass");
});

test("a failed evidence test fails the row, even for a declared-blocked row", () => {
	const run = { results: results([["guards identity", "failed"]]), infraError: null };
	const out = judge([row("A01", ["guards identity"]), row("A02", ["guards identity"], "blocked")], run);
	assert.equal(out.A01.status, "failed");
	assert.equal(out.A02.status, "failed");
	assert.match(out.A01.reasons[0], /guards identity/);
});

test("missing, skipped, and absent evidence are unverified", () => {
	const out = judge(
		[row("A01", ["guards identity", "does not exist"]), row("A02", ["skipped case"]), row("A03", [])],
		{ results: green, infraError: null },
	);
	assert.equal(out.A01.status, "unverified");
	assert.equal(out.A01.evidence.missing, 1);
	assert.equal(out.A02.status, "unverified");
	assert.equal(out.A02.evidence.skipped, 1);
	assert.equal(out.A03.status, "unverified");
	assert.ok(out.A03.reasons.includes("no evidence"));
});

test("an evidence file absent from the runner result is unverified, not passed", () => {
	const out = judge([row("A01", ["guards identity"], "passed", OTHER)], { results: green, infraError: null });
	assert.equal(out.A01.status, "unverified");
	assert.equal(out.A01.evidence.infrastructure, 1);
});

test("runner crash, timeout, and load failures are infrastructure outcomes, never passes", () => {
	const crashed = judge([row("A01", ["guards identity"])], { results: null, infraError: "test runner timed out" });
	assert.equal(crashed.A01.status, "unverified");
	assert.match(crashed.A01.reasons[0], /infrastructure outcome: test runner timed out/);

	const loadFailure = results([], { status: "failed", message: "SyntaxError: bad" });
	const notLoaded = judge([row("A01", ["guards identity"])], { results: loadFailure, infraError: null });
	assert.equal(notLoaded.A01.status, "unverified");
	assert.match(notLoaded.A01.reasons[0], /SyntaxError/);

	const unclean = judge([row("A01", ["guards identity"])], {
		results: { ...green, success: false },
		infraError: null,
	});
	assert.equal(unclean.A01.status, "unverified");
});

test("declared blocked rows stay blocked when their evidence is green", () => {
	const out = judge([row("A01", ["guards identity"], "blocked"), row("A02", [], "blocked")], {
		results: green,
		infraError: null,
	});
	assert.equal(out.A01.status, "blocked");
	assert.equal(out.A02.status, "blocked");
});

test("a lock mismatch makes the affected rows unverified", () => {
	const run = { results: green, infraError: null };
	const rows = [row("A01", ["guards identity"]), row("A02", ["guards identity"], "passed", OTHER), row("A03", [])];
	const evidence = judge(rows, run, { ...okLock, ok: false, evidenceChanged: [FILE] });
	assert.equal(evidence.A01.status, "unverified");
	assert.equal(evidence.A01.reasons[0], INSTRUMENT_CHANGED);
	assert.notEqual(evidence.A02.reasons[0], INSTRUMENT_CHANGED, "rows on unchanged files are unaffected");

	const manifest = judge(rows, run, { ...okLock, ok: false, manifestChanged: true });
	for (const id of ["A01", "A02", "A03"]) assert.equal(manifest[id].reasons[0], INSTRUMENT_CHANGED);
	const missing = judge(rows, run, { ...okLock, ok: false, lockMissing: true, manifestChanged: true });
	assert.equal(missing.A01.status, "unverified");
});

test("summary and markdown cover every row", () => {
	const rows = computeRows({
		manifest: { rows: [row("A01", ["guards identity"]), row("A02", [], "blocked")] },
		run: { results: green, infraError: null },
		lock: okLock,
		root,
	});
	const summary = summarize(rows);
	assert.deepEqual(summary, { total: 2, passed: 1, failed: 0, unverified: 0, blocked: 1 });
	const md = renderMarkdown({
		generated_at: "now",
		lock: { description: "instrument lock matches" },
		runner: { infraError: null, exitCode: 0 },
		summary,
		rows,
	});
	assert.match(md, /\| A01 \| passed \| 1\/1 \|/);
	assert.match(md, /\| A02 \| blocked \| 0\/0 \|/);
});

test("lock detects manifest and evidence edits", (t) => {
	const dir = mkdtempSync(join(tmpdir(), "ultron-acceptance-lock-"));
	t.after(() => rmSync(dir, { recursive: true, force: true }));
	mkdirSync(join(dir, "acceptance"));
	mkdirSync(join(dir, "scripts"));
	mkdirSync(join(dir, "packages/coding-agent/test"), { recursive: true });
	for (const path of INSTRUMENT_PATHS) writeFileSync(join(dir, path), "{}\n");
	writeFileSync(join(dir, FILE), "test('a', () => {})\n");
	writeFileSync(join(dir, "acceptance/manifest.json"), JSON.stringify({ rows: [row("A01", ["a"])] }));
	writeFileSync(join(dir, LOCK_PATH), JSON.stringify(computeLock(dir)));
	assert.equal(checkLock(dir).ok, true);
	writeFileSync(join(dir, FILE), "test.skip('a', () => {})\n");
	assert.deepEqual(checkLock(dir).evidenceChanged, [FILE]);

	const script = fileURLToPath(new URL("./acceptance-lock.mjs", import.meta.url));
	assert.equal(spawnSync(process.execPath, [script, "--check", "--root", dir]).status, 1);
	assert.equal(spawnSync(process.execPath, [script, "--write", "--root", dir]).status, 0);
	assert.equal(spawnSync(process.execPath, [script, "--check", "--root", dir]).status, 0);
	assert.ok(JSON.parse(readFileSync(join(dir, LOCK_PATH), "utf8")).evidence[FILE]);
});

test("mutation edits must match exactly once and runner crashes are not kills", () => {
	assert.deepEqual(applyEdits("a === true", [{ find: "=== true", replace: "" }]), { text: "a " });
	assert.match(applyEdits("x x", [{ find: "x", replace: "y" }]).error, /not unique/);
	assert.match(applyEdits("x", [{ find: "z", replace: "y" }]).error, /not present/);

	assert.equal(classifyRun({ results: results([["t", "failed"]]), infraError: null }).outcome, "killed");
	assert.equal(classifyRun({ results: green, infraError: null }).outcome, "survived");
	assert.equal(classifyRun({ results: null, infraError: "timed out" }).outcome, "error");
	const loadFailure = results([], { status: "failed", message: "SyntaxError" });
	assert.equal(classifyRun({ results: loadFailure, infraError: null }).outcome, "error");
});

test("a surviving mutation of a row's guarantee keeps the row unverified", () => {
	const mutation = {
		mutations_sha256: "m1",
		results: [
			{ id: "weaken-a", row: "A01", outcome: "survived" },
			{ id: "weaken-b", row: "A02", outcome: "killed" },
		],
	};
	const manifest = { rows: [row("A01", ["guards identity"]), row("A02", ["guards identity"])] };
	const run = { results: green, infraError: null };
	const judged = (sha) =>
		Object.fromEntries(
			computeRows({ manifest, run, lock: okLock, root, mutation, mutationsSha: sha }).map((r) => [r.id, r]),
		);
	const fresh = judged("m1");
	assert.equal(fresh.A01.status, "unverified");
	assert.match(fresh.A01.reasons[0], /weaken-a/);
	assert.equal(fresh.A02.status, "passed");
	assert.equal(judged("stale").A01.status, "passed", "stale mutation results do not apply");
});

test("a fully killed mutation slice with an intact lock passes the instrument row", () => {
	const mutation = { mutations_sha256: "m1", summary: { total: 2, killed: 2 }, results: [{ outcome: "killed" }, { outcome: "killed" }] };
	const instrument = { ...row("A26", []), mutation_slice: true };
	const judged = (candidate) =>
		computeRows({ manifest: { rows: [instrument] }, run: { results: green, infraError: null }, lock: okLock, root, mutation: candidate, mutationsSha: "m1" })[0];
	assert.equal(judged(mutation).status, "passed");
	assert.equal(judged({ ...mutation, results: [{ outcome: "killed" }, { outcome: "survived" }] }).status, "unverified");
	assert.equal(judged({ ...mutation, mutations_sha256: "stale" }).status, "unverified");
});

test("recorded live evidence stands in only for skipped live tests", () => {
	const liveRow = row("A39", ["skipped case"]);
	const judged = (verdict) =>
		computeRows({
			manifest: { rows: [liveRow] },
			run: { results: green, infraError: null },
			lock: okLock,
			root,
			live: new Map([["A39", verdict]]),
		})[0];
	assert.equal(judged({ ok: true, detail: "model qualified 5/5" }).status, "passed");
	assert.equal(judged({ ok: false, detail: "not all qualified" }).status, "unverified");
	const missing = computeRows({
		manifest: { rows: [row("A39", ["not a real test"])] },
		run: { results: green, infraError: null },
		lock: okLock,
		root,
		live: new Map([["A39", { ok: true, detail: "x" }]]),
	})[0];
	assert.equal(missing.status, "unverified", "missing evidence is never excused by live results");
});
