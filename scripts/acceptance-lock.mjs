#!/usr/bin/env node
/**
 * A26 acceptance instrument lock.
 *
 * The lock records sha256 hashes of the reviewed acceptance manifest, the mutation slice, the
 * instrument scripts, and every evidence test file. Changing any of them without a recorded
 * review (and `npm run acceptance:lock`) is detected by `--check`, and the acceptance report
 * then refuses to count the affected rows as passed.
 *
 * Usage: node scripts/acceptance-lock.mjs --write|--check [--root <dir>]
 */
import { createHash } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

export const MANIFEST_PATH = "acceptance/manifest.json";
export const MUTATIONS_PATH = "acceptance/mutations.json";
export const LOCK_PATH = "acceptance/instrument.lock";
/** Files that define how rows are judged. A change to any of them affects every row. */
export const INSTRUMENT_PATHS = [
	MUTATIONS_PATH,
	"scripts/acceptance-lock.mjs",
	"scripts/acceptance-report.mjs",
	"scripts/acceptance-mutation.mjs",
];

export const defaultRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");

export function sha256File(path) {
	if (!existsSync(path)) return null;
	return createHash("sha256").update(readFileSync(path)).digest("hex");
}

export function readJson(path) {
	return JSON.parse(readFileSync(path, "utf8"));
}

/** Every evidence test file named by the manifest and the mutation slice, sorted and unique. */
export function evidenceFiles(manifest, mutations) {
	const files = new Set();
	for (const row of manifest.rows ?? []) for (const evidence of row.evidence ?? []) files.add(evidence.file);
	for (const mutation of mutations?.mutations ?? []) for (const file of mutation.tests ?? []) files.add(file);
	return [...files].sort();
}

export function computeLock(root) {
	const manifestPath = join(root, MANIFEST_PATH);
	const manifest = readJson(manifestPath);
	const mutationsPath = join(root, MUTATIONS_PATH);
	const mutations = existsSync(mutationsPath) ? readJson(mutationsPath) : { mutations: [] };
	const instrument = {};
	for (const path of INSTRUMENT_PATHS) instrument[path] = sha256File(join(root, path));
	const evidence = {};
	for (const path of evidenceFiles(manifest, mutations)) evidence[path] = sha256File(join(root, path));
	return { version: 1, algorithm: "sha256", manifest: sha256File(manifestPath), instrument, evidence };
}

/**
 * Compare the current files against the recorded lock.
 * Returns which parts changed so the report can mark exactly the affected rows.
 */
export function checkLock(root) {
	const lockPath = join(root, LOCK_PATH);
	const current = computeLock(root);
	if (!existsSync(lockPath)) {
		return { ok: false, lockMissing: true, manifestChanged: true, instrumentChanged: [], evidenceChanged: [] };
	}
	let recorded;
	try {
		recorded = readJson(lockPath);
	} catch {
		return { ok: false, lockMissing: true, manifestChanged: true, instrumentChanged: [], evidenceChanged: [] };
	}
	const manifestChanged = recorded.manifest !== current.manifest;
	const differ = (a = {}, b = {}) =>
		[...new Set([...Object.keys(a), ...Object.keys(b)])].filter((path) => a[path] !== b[path]).sort();
	const instrumentChanged = differ(recorded.instrument, current.instrument);
	const evidenceChanged = differ(recorded.evidence, current.evidence);
	return {
		ok: !manifestChanged && instrumentChanged.length === 0 && evidenceChanged.length === 0,
		lockMissing: false,
		manifestChanged,
		instrumentChanged,
		evidenceChanged,
	};
}

export function describeLockCheck(check) {
	if (check.ok) return "instrument lock matches";
	if (check.lockMissing) return "instrument lock is missing or unreadable";
	const parts = [];
	if (check.manifestChanged) parts.push("manifest changed");
	if (check.instrumentChanged.length) parts.push(`instrument changed: ${check.instrumentChanged.join(", ")}`);
	if (check.evidenceChanged.length) parts.push(`evidence changed: ${check.evidenceChanged.join(", ")}`);
	return parts.join("; ");
}

function main(argv) {
	const rootIndex = argv.indexOf("--root");
	const root = rootIndex >= 0 ? resolve(argv[rootIndex + 1]) : defaultRoot;
	if (argv.includes("--write")) {
		const lock = computeLock(root);
		const missing = Object.entries({ ...lock.instrument, ...lock.evidence }).filter(([, hash]) => hash === null);
		if (missing.length) {
			console.error(`Cannot lock missing files: ${missing.map(([path]) => path).join(", ")}`);
			return 1;
		}
		writeFileSync(join(root, LOCK_PATH), `${JSON.stringify(lock, null, "\t")}\n`);
		console.log(`Wrote ${LOCK_PATH} (${Object.keys(lock.evidence).length} evidence files)`);
		return 0;
	}
	if (argv.includes("--check")) {
		const check = checkLock(root);
		console.log(describeLockCheck(check));
		return check.ok ? 0 : 1;
	}
	console.error("Usage: node scripts/acceptance-lock.mjs --write|--check [--root <dir>]");
	return 2;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
	process.exitCode = main(process.argv.slice(2));
}
