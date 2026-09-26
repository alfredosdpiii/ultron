#!/usr/bin/env node
/**
 * Rebuild the research-corpus generation cache from the committed fixture, so a later
 * `node scripts/generate-research-corpus.mjs` run reuses every frozen report instead of regenerating it.
 *
 *   node scripts/rebuild-research-cache.mjs [--out-dir evals/quality/fixtures]
 */
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const index = process.argv.indexOf("--out-dir");
const outDir = resolve(root, index === -1 ? "evals/quality/fixtures" : process.argv[index + 1]);
const fixture = join(outDir, "incident-root-causes.jsonl.gz");
const metaPath = join(outDir, "incident-root-causes.meta.json");
const cachePath = join(outDir, ".incident-root-causes.cache.jsonl");
if (existsSync(cachePath)) throw new Error(`${cachePath} exists; remove it first to rebuild`);
const meta = JSON.parse(readFileSync(metaPath, "utf8"));
const reports = gunzipSync(readFileSync(fixture)).toString("utf8").split("\n").filter(Boolean).map((line) => JSON.parse(line));
const lines = reports.map((report) => {
	const label = meta.labels[report.id];
	if (!label) throw new Error(`no label for ${report.id}`);
	return JSON.stringify({ id: report.id, text: report.text, validatorRootCause: label.validator_root_cause, attempts: label.attempts });
});
writeFileSync(cachePath, `${lines.join("\n")}\n`);
// Carry the recorded generation cost forward so the final stats cover every call made for this corpus.
writeFileSync(`${cachePath}.stats.json`, `${JSON.stringify({ ...meta.stats, cachedReused: 0 })}\n`);
console.log(`Rebuilt ${cachePath} with ${lines.length} reports`);
