#!/usr/bin/env node
// Runs `npm audit --omit=dev` and fails on any moderate-or-worse finding except the accepted ones below.
// An accepted advisory only passes while it reaches the tree through the listed packages: the same advisory
// arriving through any other dependency fails the audit.
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

export const ACCEPTED = [
	{
		advisory: "GHSA-86w9-cpqp-85rv",
		package: "node-forge",
		// Only the private example extension pi-extension-gondolin depends on it; it is not in the published
		// npm-shrinkwrap.json. Gondolin uses node-forge to make and check its own MITM CA certificates, and no
		// node-forge release fixes the advisory yet. Drop this entry once one does.
		through: ["@earendil-works/gondolin"],
	},
];

const LEVELS = ["info", "low", "moderate", "high", "critical"];

/** Returns the findings at or above `level` that the accepted list does not cover, as `name: reason` lines. */
export function unaccepted(report, level = "moderate", accepted = ACCEPTED) {
	const vulns = report.vulnerabilities ?? {};
	const ok = new Set();
	// A package is accepted when each advisory on it is accepted for it and it affects only the allowed packages;
	// a package that is vulnerable only through accepted packages (a string `via`) is accepted in turn.
	let changed = true;
	while (changed) {
		changed = false;
		for (const [name, vuln] of Object.entries(vulns)) {
			if (ok.has(name)) continue;
			const pass = vuln.via.every((via) => {
				if (typeof via === "string") return ok.has(via);
				const rule = accepted.find((r) => r.package === name && String(via.url ?? "").endsWith(r.advisory));
				return rule !== undefined && (vuln.effects ?? []).every((effect) => rule.through.includes(effect));
			});
			if (pass) {
				ok.add(name);
				changed = true;
			}
		}
	}
	const min = LEVELS.indexOf(level);
	return Object.entries(vulns)
		.filter(([name, vuln]) => !ok.has(name) && LEVELS.indexOf(vuln.severity) >= min)
		.map(([name, vuln]) => {
			const via = vuln.via.map((v) => (typeof v === "string" ? v : (v.url ?? v.title))).join(", ");
			return `${name} (${vuln.severity}): ${via}`;
		});
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	const run = spawnSync("npm", ["audit", "--omit=dev", "--json"], { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 });
	let report;
	try {
		report = JSON.parse(run.stdout);
	} catch {
		process.stderr.write(`npm audit did not return JSON (exit ${run.status}):\n${run.stderr}\n`);
		process.exit(2);
	}
	if (report.error) {
		process.stderr.write(`npm audit failed: ${JSON.stringify(report.error)}\n`);
		process.exit(2);
	}
	const bad = unaccepted(report);
	if (bad.length > 0) {
		process.stderr.write(`Production vulnerabilities:\n${bad.map((line) => `  ${line}`).join("\n")}\n`);
		process.exit(1);
	}
	const total = Object.keys(report.vulnerabilities ?? {}).length;
	process.stdout.write(`npm audit: no unaccepted moderate-or-worse findings (${total} accepted or below the bar).\n`);
}
