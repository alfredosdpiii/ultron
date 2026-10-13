#!/usr/bin/env node
// Rust parity gate: every row of docs/rust-rewrite/ledger.tsv needs a named Rust test that ran and passed.
// Rows without a test, and tests that fail, are skipped or are missing, keep the gate red.
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const [header, ...lines] = readFileSync(join(root, "docs/rust-rewrite/ledger.tsv"), "utf8").trimEnd().split("\n");
if (header !== "id\tarea\tbehavior\trust_test") throw new Error(`unexpected ledger header: ${header}`);
const rows = lines.map((line) => {
	const [id, area, behavior, test] = line.split("\t");
	return { id, area, behavior, test };
});

const run = spawnSync(process.env.CARGO ?? "cargo", ["test", "--workspace", "--locked", "--no-fail-fast"], { cwd: root, encoding: "utf8" });
const results = new Map();
for (const match of `${run.stdout}`.matchAll(/^test (\S+) \.\.\. (\w+)/gm)) {
	const name = match[1].split("::").pop();
	results.set(name, results.has(name) ? "duplicate" : match[2]);
}

const missing = rows.filter((row) => !row.test || results.get(row.test) !== "ok");
const verified = rows.length - missing.length;
for (const row of missing) {
	const why = row.test ? `${row.test}: ${results.get(row.test) ?? "not found"}` : "no Rust test";
	console.log(`MISSING ${row.id} (${why}) ${row.behavior}`);
}
console.log(`\nverified ${verified}/${rows.length}; cargo test exit ${run.status}`);
if (run.status !== 0) console.log(run.stderr.split("\n").slice(-20).join("\n"));
process.exit(missing.length === 0 && run.status === 0 ? 0 : 1);
