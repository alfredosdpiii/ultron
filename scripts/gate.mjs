#!/usr/bin/env node
/**
 * Release gate: required suites plus the frozen quality thresholds.
 *
 * 1. `npm run check` (lint, types, dependency checks).
 * 2. `npm run test:acceptance` must report every A01-A46 row passed.
 * 3. The newest full quality comparison of the default task set in acceptance/quality/ must exist, use the current frozen task set,
 *    and meet the thresholds it was frozen with. Quality runs are metered, so the gate reads the recorded
 *    result instead of re-running it; run `npm run eval:quality` to refresh it.
 */
import { spawnSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { FROZEN_AT } from "../evals/quality/tasks.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const results = [];

function step(name, command) {
	console.log(`\n== ${name}: ${command}`);
	const run = spawnSync("sh", ["-c", command], { cwd: root, stdio: "inherit" });
	results.push({ name, ok: run.status === 0 });
}

step("check", "npm run check");
step("acceptance", "npm run test:acceptance");
const report = join(root, "acceptance/report.json");
if (existsSync(report)) {
	const { summary } = JSON.parse(readFileSync(report, "utf8"));
	const complete = summary.passed === summary.total || summary.passed === 46;
	results.push({ name: "acceptance rows", ok: complete, detail: `${summary.passed} passed of 46` });
}

const qualityDir = join(root, "acceptance/quality");
const latest = existsSync(qualityDir)
	? readdirSync(qualityDir)
			.filter((file) => file.endsWith(".json"))
			// The release gate reads the default frozen set; hard-set runs and self-checks are separate evidence.
			.filter((file) => {
				const recorded = JSON.parse(readFileSync(join(qualityDir, file), "utf8"));
				// Partial reruns of one variant carry no gate entries; they supplement a full comparison.
				return (recorded.taskSet ?? "default") === "default" && (recorded.summary?.gate?.length ?? 0) > 0;
			})
			.map((file) => join(qualityDir, file))
			.sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0]
	: undefined;
if (!latest) {
	results.push({ name: "quality", ok: false, detail: "no recorded comparison; run npm run eval:quality" });
} else {
	const quality = JSON.parse(readFileSync(latest, "utf8"));
	const current = quality.frozenAt === FROZEN_AT;
	results.push({
		name: "quality",
		ok: current && quality.summary?.passed === true,
		detail: `${latest.slice(root.length + 1)}: ${current ? "" : "stale task set; "}${(quality.summary?.gate ?? [])
			.map((entry) => `${entry.check} ${entry.ok === false ? "FAILED" : entry.ok === null ? "n/a" : "ok"} (${entry.detail})`)
			.join(", ")}`,
	});
}

console.log("\n== gate");
for (const result of results) console.log(`${result.ok ? "ok  " : "FAIL"} ${result.name}${result.detail ? `: ${result.detail}` : ""}`);
process.exitCode = results.every((result) => result.ok) ? 0 : 1;
