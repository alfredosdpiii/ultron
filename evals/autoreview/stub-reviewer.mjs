#!/usr/bin/env node
/**
 * A stand-in reviewer for testing the benchmark offline: takes the reviewer contract's flags, reads the diff with
 * git, and prints one review as JSON. It calls no model and knows nothing about bugs.
 *
 *   node evals/autoreview/stub-reviewer.mjs --repo-dir <dir> --base <sha> --head <sha> [--model m] [--verify-model m]
 *                                           [--budget n] [--json] [--dry-run] [--stub-mode hash|flag|approve|fail]
 *
 * `--stub-mode`:
 * - `hash` (default): a hash of base, head and model decides. About half of the reviews report a confirmed major
 *   finding on the first changed line of the diff's first file and request changes; a quarter report a minor finding
 *   there and comment; the rest approve with at most a nit. The same inputs always give the same review.
 * - `flag`: always the major finding; `approve`: never a finding; `fail`: print no JSON and exit 1.
 */

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { parsePatch } from "./lib.mjs";

const VALUE_FLAGS = ["repo-dir", "base", "head", "model", "verify-model", "budget", "stub-mode"];
const SWITCH_FLAGS = ["json", "dry-run"];

function parseArgs(argv) {
	const flags = {};
	for (let index = 0; index < argv.length; index++) {
		const name = argv[index].replace(/^--/, "");
		if (argv[index].startsWith("--") && VALUE_FLAGS.includes(name)) flags[name] = argv[++index];
		else if (argv[index].startsWith("--") && SWITCH_FLAGS.includes(name)) flags[name] = true;
		else {
			console.error(`stub-reviewer: unknown argument ${argv[index]}`);
			process.exit(2);
		}
	}
	return flags;
}

const flags = parseArgs(process.argv.slice(2));
for (const name of ["repo-dir", "base", "head"]) {
	if (!flags[name]) {
		console.error(`stub-reviewer: --${name} is required`);
		process.exit(2);
	}
}
const mode = flags["stub-mode"] ?? "hash";
if (mode === "fail") {
	console.error("stub-reviewer: failing as asked");
	process.exit(1);
}

const diff = execFileSync("git", ["diff", "--no-color", "--no-ext-diff", "--no-renames", "-U0", flags.base, flags.head], {
	cwd: flags["repo-dir"],
	encoding: "utf8",
	maxBuffer: 64 * 1024 * 1024,
});
const files = parsePatch(diff).filter((file) => file.status !== "deleted" && file.hunks.length);
const model = flags.model ?? "stub/default";
const bytes = createHash("sha256").update(`${flags.base}:${flags.head}:${model}`).digest();
const roll = mode === "flag" ? 0 : mode === "approve" ? 255 : bytes[0];
const target = files[0];
const line = target ? Math.max(1, target.hunks[0].newStart) : 1;
const findings = [];
if (target && roll < 128) {
	findings.push({
		file: target.path,
		line,
		endLine: line + Math.max(0, target.hunks[0].newLines - 1),
		severity: "major",
		category: "correctness",
		claim: "Stub: the first changed hunk is flagged.",
		why: "Deterministic output for testing the harness; no analysis happened.",
		verification: "confirmed",
		confidence: 0.9,
	});
} else if (target && roll < 192) {
	findings.push({
		file: target.path,
		line,
		severity: "minor",
		category: "maintainability",
		claim: "Stub: a minor remark on the first changed hunk.",
		why: "Deterministic output for testing the harness; no analysis happened.",
		verification: "confirmed",
		confidence: 0.6,
	});
} else if (target && mode === "hash" && roll < 224) {
	findings.push({
		file: target.path,
		line,
		severity: "nit",
		category: "style",
		claim: "Stub: a nit.",
		why: "Deterministic output for testing the harness; no analysis happened.",
		verification: "uncertain",
		confidence: 0.3,
	});
}
const serious = findings.some((finding) => finding.severity === "major");
const findMs = 200 + bytes[1] * 4;
const verifyMs = 100 + bytes[2] * 2;
const scopeMs = 20 + (bytes[3] % 40);
const inputTokens = 4000 + bytes[4] * 50;
const outputTokens = 300 + bytes[5] * 5;
process.stdout.write(
	`${JSON.stringify({
		verdict: serious ? "request_changes" : findings.some((finding) => finding.severity === "minor") ? "comment" : "approve",
		complete: true,
		findings,
		dropped: { rejected: bytes[6] % 3, duplicates: bytes[7] % 2 },
		timing: { totalMs: scopeMs + findMs + verifyMs, scopeMs, findMs, verifyMs },
		usage: { inputTokens, outputTokens, costUsd: Number(((inputTokens * 2 + outputTokens * 10) / 1e6).toFixed(6)), frames: 2 + (bytes[8] % 4) },
		model,
		verifyModel: flags["verify-model"] ?? model,
		notChecked: [],
	})}\n`,
);
