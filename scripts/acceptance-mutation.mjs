#!/usr/bin/env node
/**
 * A26 regression-mutation slice.
 *
 * Applies each mutation in acceptance/mutations.json (a find/replace that weakens one critical
 * guarantee) to the source file, runs the listed evidence tests, and restores the original bytes,
 * even when the run fails or is interrupted. A mutation is killed only when evidence tests fail;
 * a runner crash or a suite that fails to load is an infrastructure outcome, not a kill.
 * Survivors are findings: they are written to acceptance/mutation.json and make the exit code non-zero.
 *
 * Usage: node scripts/acceptance-mutation.mjs [--root <dir>] [--only <id,...>] [--timeout-ms <n>]
 */
import { existsSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join, relative, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { defaultRoot, MANIFEST_PATH, MUTATIONS_PATH, readJson, sha256File } from "./acceptance-lock.mjs";
import { MUTATION_RESULT, runVitest } from "./acceptance-report.mjs";

const INFLIGHT = "acceptance/.mutation-inflight.json";
const DEFAULT_TIMEOUT_MS = 5 * 60 * 1000;

/** Apply edits in order; each `find` must occur exactly once in the current text. */
export function applyEdits(source, edits) {
	let text = source;
	for (const { find, replace } of edits) {
		const first = text.indexOf(find);
		if (first < 0) return { error: `find string not present: ${find}` };
		if (text.indexOf(find, first + find.length) >= 0) return { error: `find string is not unique: ${find}` };
		text = text.slice(0, first) + replace + text.slice(first + find.length);
	}
	return { text };
}

/** Classify a vitest run of the evidence tests against a mutant. */
export function classifyRun(run) {
	if (run.infraError) return { outcome: "error", detail: run.infraError };
	const entries = run.results?.testResults ?? [];
	const failed = entries.flatMap((entry) =>
		(entry.assertionResults ?? []).filter((item) => item.status === "failed").map((item) => item.fullName),
	);
	if (failed.length) return { outcome: "killed", failing_tests: failed.slice(0, 20) };
	const loadFailure = entries.find((entry) => entry.status === "failed");
	if (loadFailure) {
		return {
			outcome: "error",
			detail: `suite failed outside its tests: ${(loadFailure.message ?? "").split("\n")[0]}`,
		};
	}
	if (run.results?.success === false) return { outcome: "error", detail: "runner reported errors outside tests" };
	return { outcome: "survived" };
}

function restoreInflight(root) {
	const path = join(root, INFLIGHT);
	if (!existsSync(path)) return;
	const record = readJson(path);
	writeFileSync(join(root, record.file), Buffer.from(record.original, "base64"));
	rmSync(path);
	console.error(`Restored ${record.file} from an interrupted mutation run`);
}

function argValue(argv, name) {
	const index = argv.indexOf(name);
	return index >= 0 ? argv[index + 1] : undefined;
}

function main(argv) {
	const root = resolve(argValue(argv, "--root") ?? defaultRoot);
	const timeoutMs = Number(argValue(argv, "--timeout-ms") ?? DEFAULT_TIMEOUT_MS);
	const only = argValue(argv, "--only")?.split(",");
	restoreInflight(root);

	const manifest = readJson(join(root, MANIFEST_PATH));
	const vitestCwd = join(root, manifest.vitest_cwd ?? "packages/coding-agent");
	const config = readJson(join(root, MUTATIONS_PATH));
	const mutations = config.mutations.filter((mutation) => !only || only.includes(mutation.id));
	const toRunner = (files) => files.map((file) => relative(vitestCwd, join(root, file)));

	const onSignal = (signal) => {
		restoreInflight(root);
		process.exit(signal === "SIGINT" ? 130 : 143);
	};
	process.on("SIGINT", onSignal);
	process.on("SIGTERM", onSignal);

	// Baseline: a mutation can only be killed by tests that pass on the unmutated source.
	const baseline = new Map();
	for (const file of [...new Set(mutations.flatMap((mutation) => mutation.tests))]) {
		const verdict = classifyRun(runVitest({ cwd: vitestCwd, files: toRunner([file]), timeoutMs }));
		baseline.set(file, verdict);
		console.log(`baseline ${file}: ${verdict.outcome === "survived" ? "green" : `not green (${verdict.outcome})`}`);
	}

	const results = [];
	for (const mutation of mutations) {
		const base = { id: mutation.id, row: mutation.row, file: mutation.file, guarantee: mutation.guarantee };
		const notGreen = mutation.tests.filter((file) => baseline.get(file)?.outcome !== "survived");
		if (notGreen.length) {
			results.push({ ...base, outcome: "error", detail: `baseline not green: ${notGreen.join(", ")}` });
			continue;
		}
		const path = join(root, mutation.file);
		const original = readFileSync(path);
		const originalHash = sha256File(path);
		const mutated = applyEdits(original.toString("utf8"), mutation.edits);
		if (mutated.error) {
			results.push({ ...base, outcome: "invalid", detail: mutated.error });
			continue;
		}
		let verdict;
		writeFileSync(
			join(root, INFLIGHT),
			JSON.stringify({ file: mutation.file, original: original.toString("base64") }),
		);
		try {
			writeFileSync(path, mutated.text);
			verdict = classifyRun(runVitest({ cwd: vitestCwd, files: toRunner(mutation.tests), timeoutMs }));
		} finally {
			writeFileSync(path, original);
			rmSync(join(root, INFLIGHT), { force: true });
		}
		if (sha256File(path) !== originalHash) throw new Error(`Failed to restore ${mutation.file}`);
		results.push({ ...base, tests: mutation.tests, ...verdict });
		console.log(`${mutation.id} (${mutation.row}): ${verdict.outcome}`);
	}

	const summary = { total: results.length, killed: 0, survived: 0, error: 0, invalid: 0 };
	for (const result of results) summary[result.outcome] += 1;
	const report = {
		generated_at: new Date().toISOString(),
		mutations_sha256: sha256File(join(root, MUTATIONS_PATH)),
		summary,
		findings: results.filter((result) => result.outcome !== "killed").map((result) => result.id),
		results,
	};
	writeFileSync(join(root, MUTATION_RESULT), `${JSON.stringify(report, null, "\t")}\n`);
	console.log(
		`Mutation slice: ${summary.killed}/${summary.total} killed, ${summary.survived} survived, ${summary.error} error, ${summary.invalid} invalid. Wrote ${MUTATION_RESULT}`,
	);
	return summary.killed === summary.total ? 0 : 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
	process.exitCode = main(process.argv.slice(2));
}
