#!/usr/bin/env node
/**
 * Ultron acceptance report (`npm run test:acceptance`), over every row in acceptance/manifest.json.
 *
 * Runs the evidence tests named in acceptance/manifest.json and judges every row:
 * - passed: every listed evidence test exists and passed, the instrument lock matches, and the
 *   reviewed manifest declares the row passable (`status_when_green: "passed"`).
 * - failed: a listed evidence test failed.
 * - unverified: evidence is missing, skipped, not found, the runner crashed or timed out
 *   (an infrastructure outcome, never a pass), the instrument changed without review, or the
 *   manifest declares a gap.
 * - blocked: declared by the manifest.
 * Unavailable is never passed. Exits non-zero when any row failed.
 *
 * Usage: node scripts/acceptance-report.mjs [--root <dir>] [--results <vitest.json>] [--timeout-ms <n>]
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import {
	checkLock,
	defaultRoot,
	describeLockCheck,
	MANIFEST_PATH,
	MUTATIONS_PATH,
	readJson,
	sha256File,
} from "./acceptance-lock.mjs";

export const REPORT_JSON = "acceptance/report.json";
export const REPORT_MD = "acceptance/report.md";
export const MUTATION_RESULT = "acceptance/mutation.json";
export const DEFAULT_TIMEOUT_MS = 15 * 60 * 1000;
export const INSTRUMENT_CHANGED = "instrument changed without review";

/**
 * Run vitest with the JSON reporter. Any crash, signal, timeout, or unreadable output is
 * returned as `infraError`; the caller must never treat it as a pass.
 */
export function runVitest({ cwd, files, timeoutMs = DEFAULT_TIMEOUT_MS }) {
	const dir = mkdtempSync(join(tmpdir(), "ultron-acceptance-"));
	const outputFile = join(dir, "vitest.json");
	try {
		const run = spawnSync("npx", ["vitest", "--run", "--reporter=json", `--outputFile=${outputFile}`, ...files], {
			cwd,
			encoding: "utf8",
			timeout: timeoutMs,
			killSignal: "SIGKILL",
			maxBuffer: 64 * 1024 * 1024,
			env: { ...process.env, CI: "1" },
		});
		const tail = (text) => (text ?? "").trim().split("\n").slice(-20).join("\n");
		if (run.error) {
			const timedOut = run.error.code === "ETIMEDOUT";
			return {
				results: null,
				infraError: timedOut
					? `test runner timed out after ${timeoutMs} ms`
					: `test runner failed: ${run.error.message}`,
				exitCode: run.status,
			};
		}
		if (run.signal) return { results: null, infraError: `test runner killed by ${run.signal}`, exitCode: null };
		if (!existsSync(outputFile)) {
			return {
				results: null,
				infraError: `test runner produced no JSON result (exit ${run.status}): ${tail(run.stderr) || tail(run.stdout)}`,
				exitCode: run.status,
			};
		}
		try {
			return { results: JSON.parse(readFileSync(outputFile, "utf8")), infraError: null, exitCode: run.status };
		} catch (error) {
			return { results: null, infraError: `unreadable test runner JSON: ${error.message}`, exitCode: run.status };
		}
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
}

/** Index vitest results by evidence file path (relative to the repository root). */
export function indexResults(results, root, files) {
	const byFile = new Map();
	for (const file of files) {
		const absolute = resolve(root, file);
		const entry = results?.testResults?.find((item) => resolve(item.name) === absolute);
		if (!entry) {
			byFile.set(file, { infraError: "no runner result for evidence file", assertions: [] });
			continue;
		}
		const assertions = entry.assertionResults ?? [];
		const loadFailed = entry.status === "failed" && !assertions.some((item) => item.status === "failed");
		byFile.set(file, {
			infraError: loadFailed
				? `evidence file failed outside its tests: ${(entry.message ?? "").split("\n")[0] || "unknown error"}`
				: null,
			assertions,
		});
	}
	return byFile;
}

function evaluateEvidence(row, byFile) {
	const checks = [];
	for (const evidence of row.evidence ?? []) {
		const file = byFile.get(evidence.file);
		for (const name of evidence.tests ?? []) {
			const matches = (file?.assertions ?? []).filter(
				(item) => (item.fullName ?? "").includes(name) || (item.title ?? "").includes(name),
			);
			let outcome;
			if (matches.some((item) => item.status === "failed")) outcome = "failed";
			else if (file?.infraError) outcome = "infrastructure";
			else if (matches.length === 0) outcome = "missing";
			else if (matches.every((item) => item.status === "passed")) outcome = "passed";
			else outcome = "skipped";
			checks.push({ file: evidence.file, test: name, outcome, matched: matches.length, infra: file?.infraError });
		}
	}
	return checks;
}

function mutationSummary(mutation, mutationsSha) {
	if (!mutation) return "mutation slice has not been run (acceptance/mutation.json missing)";
	const { summary = {} } = mutation;
	const survived = (mutation.results ?? []).filter((item) => item.outcome !== "killed").map((item) => item.id);
	const stale = mutation.mutations_sha256 !== mutationsSha ? " (stale: mutations changed since the run)" : "";
	return `mutation slice ${summary.killed ?? 0}/${summary.total ?? 0} killed${
		survived.length ? `; not killed: ${survived.join(", ")}` : ""
	}${stale}`;
}

/**
 * Pure row judgement. `run` is `{ results, infraError }` from runVitest (or a fixture),
 * `lock` is the result of checkLock.
 */
export function computeRows({ manifest, run, lock, root, mutation = null, mutationsSha = null, live = new Map() }) {
	const files = [...new Set((manifest.rows ?? []).flatMap((row) => (row.evidence ?? []).map((e) => e.file)))];
	const byFile = run.infraError
		? new Map(files.map((file) => [file, { infraError: run.infraError, assertions: [] }]))
		: indexResults(run.results, root, files);
	const runnerUnclean =
		!run.infraError &&
		run.results?.success === false &&
		!(run.results.testResults ?? []).some((entry) =>
			(entry.assertionResults ?? []).some((a) => a.status === "failed"),
		);
	const everyRowAffected = !lock.ok && (lock.lockMissing || lock.manifestChanged || lock.instrumentChanged.length > 0);
	const changedEvidence = new Set(lock.evidenceChanged ?? []);
	// A surviving mutation shows the row's evidence would still pass with that guarantee removed.
	const survivors = new Map();
	if (mutation && mutation.mutations_sha256 === mutationsSha) {
		for (const result of mutation.results ?? []) {
			if (result.outcome === "killed") continue;
			survivors.set(result.row, [...(survivors.get(result.row) ?? []), `${result.id} (${result.outcome})`]);
		}
	}

	return (manifest.rows ?? []).map((row) => {
		const checks = evaluateEvidence(row, byFile);
		const count = (outcome) => checks.filter((check) => check.outcome === outcome).length;
		const evidence = {
			listed: checks.length,
			passed: count("passed"),
			failed: count("failed"),
			missing: count("missing"),
			skipped: count("skipped"),
			infrastructure: count("infrastructure"),
		};
		const reasons = [];
		const failedNames = checks.filter((check) => check.outcome === "failed").map((check) => check.test);
		const lockAffected = everyRowAffected || (row.evidence ?? []).some((e) => changedEvidence.has(e.file));
		let status;
		if (lockAffected) {
			status = "unverified";
			reasons.push(INSTRUMENT_CHANGED);
			if (failedNames.length) reasons.push(`failed: ${failedNames.join("; ")}`);
		} else if (failedNames.length) {
			status = "failed";
			reasons.push(`failed: ${failedNames.join("; ")}`);
		} else if (evidence.infrastructure || runnerUnclean) {
			status = "unverified";
			const infra = [...new Set(checks.map((check) => check.infra).filter(Boolean))];
			if (runnerUnclean) infra.push("test runner reported errors outside tests");
			reasons.push(`infrastructure outcome: ${infra.join("; ")}`);
		} else if (row.status_when_green === "blocked") {
			status = "blocked";
		} else if (evidence.missing || evidence.skipped) {
			// A metered live row skips by default; its recorded live results can stand in for the skipped tests.
			const recorded = live.get(row.id);
			if (!evidence.missing && recorded?.ok && row.status_when_green === "passed") {
				status = "passed";
				reasons.push(`recorded live evidence: ${recorded.detail}`);
			} else {
				status = "unverified";
				const names = checks.filter((c) => c.outcome === "missing" || c.outcome === "skipped");
				reasons.push(`evidence not found or skipped: ${names.map((c) => c.test).join("; ")}`);
				if (recorded && !recorded.ok) reasons.push(`live evidence insufficient: ${recorded.detail}`);
			}
		} else if (checks.length === 0) {
			// The instrument's own row: its evidence is the intact lock plus a fully killed mutation slice.
			const sliceComplete =
				row.mutation_slice &&
				mutation &&
				mutation.mutations_sha256 === mutationsSha &&
				(mutation.summary?.total ?? 0) > 0 &&
				(mutation.results ?? []).every((result) => result.outcome === "killed");
			if (sliceComplete && row.status_when_green === "passed") {
				status = "passed";
			} else {
				status = "unverified";
				reasons.push("no evidence");
			}
		} else if (row.status_when_green !== "passed") {
			status = "unverified";
			reasons.push("evidence green but incomplete for this row");
		} else if (survivors.has(row.id)) {
			status = "unverified";
			reasons.push(`evidence green but mutation not killed: ${survivors.get(row.id).join(", ")}`);
		} else {
			status = "passed";
		}
		if (row.mutation_slice) reasons.push(mutationSummary(mutation, mutationsSha));
		if (row.notes && status !== "passed") reasons.push(row.notes);
		return { id: row.id, behavior: row.behavior, status, declared: row.status_when_green, evidence, reasons, checks };
	});
}

/** The release gate's rule: every manifest row passed, and there is at least one row (never a fixed count). */
export function allRowsPassed(summary) {
	return summary.total > 0 && summary.passed === summary.total;
}

/** "A01-A55" for the rows in the report, so the label follows the manifest. */
export function rowRange(rows) {
	return rows.length === 0 ? "no rows" : `${rows[0].id}-${rows.at(-1).id}`;
}

const cell = (text) => String(text).replace(/\|/g, "\\|").replace(/\n/g, " ");

export function renderMarkdown(report) {
	const { summary } = report;
	const lines = [
		`# Ultron ${rowRange(report.rows)} acceptance report`,
		"",
		`Generated ${report.generated_at} by \`npm run test:acceptance\`. Unavailable is never passed; partial evidence is never a pass.`,
		"",
		`- Instrument lock: ${report.lock.description}`,
		`- Test runner: ${report.runner.infraError ? `infrastructure failure: ${report.runner.infraError}` : `exit ${report.runner.exitCode}`}`,
		`- Rows: ${summary.passed} passed, ${summary.failed} failed, ${summary.unverified} unverified, ${summary.blocked} blocked (of ${summary.total})`,
		"",
		"| Row | Status | Evidence (passed/listed) | Reasons |",
		"|---|---|---|---|",
	];
	for (const row of report.rows) {
		const { evidence } = row;
		const extra = ["failed", "missing", "skipped", "infrastructure"]
			.filter((key) => evidence[key])
			.map((key) => `${evidence[key]} ${key}`);
		const counts = `${evidence.passed}/${evidence.listed}${extra.length ? ` (${extra.join(", ")})` : ""}`;
		lines.push(`| ${row.id} | ${row.status} | ${counts} | ${cell(row.reasons.join(". ") || "-")} |`);
	}
	return `${lines.join("\n")}\n`;
}

export function summarize(rows) {
	const summary = { total: rows.length, passed: 0, failed: 0, unverified: 0, blocked: 0 };
	for (const row of rows) summary[row.status] += 1;
	return summary;
}

/**
 * Recorded live runs. `capabilities` files pass when every case qualified; `demonstration` files pass when
 * the run recorded passed: true. Every listed file must exist and pass.
 */
export function judgeLiveEvidence(root, spec) {
	const verdicts = (spec.files ?? []).map((file) => {
		const path = join(root, file);
		if (!existsSync(path)) return { file, ok: false, why: "missing" };
		const record = readJson(path);
		if (spec.kind === "capabilities") {
			const summary = record.summary ?? [];
			const ok = summary.length > 0 && summary.every((entry) => entry.outcome === "qualified");
			return { file, ok, why: ok ? `${record.model} qualified ${summary.length}/${summary.length}` : "not all qualified" };
		}
		return { file, ok: record.passed === true, why: record.passed === true ? `${record.model} passed` : "not passed" };
	});
	return {
		ok: verdicts.length > 0 && verdicts.every((verdict) => verdict.ok),
		detail: verdicts.map((verdict) => `${verdict.file}: ${verdict.why}`).join("; ") || "no live files listed",
	};
}

function argValue(argv, name) {
	const index = argv.indexOf(name);
	return index >= 0 ? argv[index + 1] : undefined;
}

function main(argv) {
	const root = resolve(argValue(argv, "--root") ?? defaultRoot);
	const manifest = readJson(join(root, MANIFEST_PATH));
	const lock = checkLock(root);
	console.log(`acceptance-lock --check: ${describeLockCheck(lock)}`);

	const vitestCwd = join(root, manifest.vitest_cwd ?? "packages/coding-agent");
	const files = [...new Set(manifest.rows.flatMap((row) => row.evidence.map((e) => e.file)))].sort();
	let run;
	const resultsPath = argValue(argv, "--results");
	if (resultsPath) {
		run = { results: readJson(resolve(resultsPath)), infraError: null, exitCode: null };
	} else {
		const timeoutMs = Number(argValue(argv, "--timeout-ms") ?? DEFAULT_TIMEOUT_MS);
		console.log(`Running ${files.length} evidence files with vitest...`);
		run = runVitest({ cwd: vitestCwd, files: files.map((file) => relative(vitestCwd, join(root, file))), timeoutMs });
	}
	const live = new Map(
		manifest.rows.filter((row) => row.live_evidence).map((row) => [row.id, judgeLiveEvidence(root, row.live_evidence)]),
	);
	const mutationPath = join(root, MUTATION_RESULT);
	const mutation = existsSync(mutationPath) ? readJson(mutationPath) : null;
	const rows = computeRows({
		manifest,
		run,
		lock,
		root,
		mutation,
		mutationsSha: sha256File(join(root, MUTATIONS_PATH)),
		live,
	});
	const report = {
		generated_at: new Date().toISOString(),
		lock: { ...lock, description: describeLockCheck(lock) },
		runner: { infraError: run.infraError, exitCode: run.exitCode },
		summary: summarize(rows),
		rows,
	};
	writeFileSync(join(root, REPORT_JSON), `${JSON.stringify(report, null, "\t")}\n`);
	writeFileSync(join(root, REPORT_MD), renderMarkdown(report));
	const { summary } = report;
	console.log(
		`${rowRange(report.rows)}: ${summary.passed} passed, ${summary.failed} failed, ${summary.unverified} unverified, ${summary.blocked} blocked. Wrote ${REPORT_MD}`,
	);
	return summary.failed > 0 ? 1 : 0;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
	process.exitCode = main(process.argv.slice(2));
}
