#!/usr/bin/env node
/**
 * Refresh the Loki engine bundled with ultron-agent (packages/coding-agent/src/ultron/loki-engine), pinned by sha256.
 *
 *   npm run loki:update -- <path-to-loki-checkout>    # a clean checkout; its commit is recorded
 *   npm run loki:update -- <version>                  # a loki-guardrails release from PyPI (sdist, digest verified)
 *   npm run loki:update -- --check                    # verify the bundled files against VERSION.json
 *
 * Loki (https://github.com/alfredosdpiii/loki, MIT) is one dependency-free Python file. Ultron ships the engine
 * (loki.py), the default policy template `loki init --minimal` copies (templates/loki.json) and the license.
 * VERSION.json records the version, where it came from and every file's sha256; `--check` (run by the script tests)
 * fails when a file no longer matches its pin.
 */
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
export const ENGINE_DIR = join(root, "packages", "coding-agent", "src", "ultron", "loki-engine");
/** Files copied from Loki, by their path in the Loki repository (and in the bundle). */
export const ENGINE_FILES = ["loki.py", "templates/loki.json", "LICENSE"];
const PYPI_PROJECT = "loki-guardrails";

export function sha256(data) {
	return createHash("sha256").update(data).digest("hex");
}

/** `__version__ = "x.y.z"` from loki.py. */
export function engineVersion(source) {
	const match = /^__version__ = "([^"]+)"$/m.exec(source);
	if (!match) throw new Error("loki.py has no __version__");
	return match[1];
}

function git(directory, args) {
	return execFileSync("git", ["-C", directory, ...args], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
}

/** Files and provenance from a Loki checkout; refuses uncommitted engine files, since the pin names a commit. */
export function fromCheckout(directory) {
	const files = {};
	for (const file of ENGINE_FILES) {
		const path = join(directory, file);
		if (!existsSync(path)) throw new Error(`${file} is missing from ${directory}; is it a Loki checkout?`);
		files[file] = readFileSync(path);
	}
	const commit = git(directory, ["rev-parse", "HEAD"]);
	const dirty = git(directory, ["status", "--porcelain", "--", ...ENGINE_FILES]);
	if (dirty) throw new Error(`Commit the Loki engine files first; uncommitted changes:\n${dirty}`);
	let repository;
	try {
		repository = git(directory, ["remote", "get-url", "origin"]);
	} catch {
		repository = undefined;
	}
	// Record only a public URL, never a local path (an SSH GitHub remote is recorded as its https form).
	const github = repository && /^git@github\.com:(.+?)(?:\.git)?$/.exec(repository);
	if (github) repository = `https://github.com/${github[1]}`;
	const source = { kind: "git", commit, ...(repository && /^https:\/\//.test(repository) ? { repository } : {}) };
	return { files, source };
}

/** Files and provenance from a PyPI release: the sdist is downloaded, its digest checked, and unpacked. */
export async function fromRelease(version, fetchImpl = fetch) {
	const metadataResponse = await fetchImpl(`https://pypi.org/pypi/${PYPI_PROJECT}/${encodeURIComponent(version)}/json`);
	if (!metadataResponse.ok) throw new Error(`PyPI has no ${PYPI_PROJECT} ${version} (HTTP ${metadataResponse.status})`);
	const metadata = await metadataResponse.json();
	const sdist = (metadata.urls ?? []).find((item) => item.packagetype === "sdist");
	if (!sdist) throw new Error(`${PYPI_PROJECT} ${version} has no source distribution`);
	const archiveResponse = await fetchImpl(sdist.url);
	if (!archiveResponse.ok) throw new Error(`Download failed: HTTP ${archiveResponse.status}`);
	const archive = Buffer.from(await archiveResponse.arrayBuffer());
	if (sha256(archive) !== sdist.digests?.sha256) throw new Error("The sdist does not match PyPI's sha256 digest");
	const work = mkdtempSync(join(tmpdir(), "loki-sdist-"));
	try {
		const archivePath = join(work, "sdist.tar.gz");
		writeFileSync(archivePath, archive);
		execFileSync("tar", ["-xzf", archivePath, "-C", work], { stdio: "ignore" });
		const top = execFileSync("tar", ["-tzf", archivePath], { encoding: "utf8" }).split("\n")[0].split("/")[0];
		const files = {};
		for (const file of ENGINE_FILES) files[file] = readFileSync(join(work, top, file));
		return { files, source: { kind: "pypi", project: PYPI_PROJECT, url: sdist.url, sdistSha256: sdist.digests.sha256 } };
	} finally {
		rmSync(work, { recursive: true, force: true });
	}
}

/** Write the engine files and VERSION.json into `target`. */
export function writeEngine(target, { files, source }) {
	const version = engineVersion(files["loki.py"].toString("utf8"));
	for (const [file, data] of Object.entries(files)) {
		mkdirSync(dirname(join(target, file)), { recursive: true });
		writeFileSync(join(target, file), data);
	}
	const pin = {
		version,
		source,
		files: Object.fromEntries(ENGINE_FILES.map((file) => [file, sha256(files[file])])),
	};
	writeFileSync(join(target, "VERSION.json"), `${JSON.stringify(pin, null, "\t")}\n`);
	return pin;
}

/** Problems with the bundled engine: missing files or files that do not match their pinned sha256. */
export function checkEngine(target = ENGINE_DIR) {
	const pinPath = join(target, "VERSION.json");
	if (!existsSync(pinPath)) return ["VERSION.json is missing"];
	const pin = JSON.parse(readFileSync(pinPath, "utf8"));
	const problems = [];
	for (const file of ENGINE_FILES) {
		const path = join(target, file);
		if (!existsSync(path)) problems.push(`${file} is missing`);
		else if (sha256(readFileSync(path)) !== pin.files?.[file]) problems.push(`${file} does not match its pinned sha256`);
	}
	if (existsSync(join(target, "loki.py"))) {
		const version = engineVersion(readFileSync(join(target, "loki.py"), "utf8"));
		if (version !== pin.version) problems.push(`loki.py is ${version}, VERSION.json says ${pin.version}`);
	}
	return problems;
}

async function main(args) {
	const [argument] = args;
	if (args.length !== 1 || !argument || argument === "--help" || argument === "-h") {
		console.error("usage: npm run loki:update -- <loki-checkout-path | version | --check>");
		return 2;
	}
	if (argument === "--check") {
		const problems = checkEngine();
		for (const problem of problems) console.error(`loki-engine: ${problem}`);
		if (problems.length === 0) console.log("loki-engine: every file matches VERSION.json");
		return problems.length === 0 ? 0 : 1;
	}
	const looksLikePath = existsSync(argument) && statSync(argument).isDirectory();
	const update = looksLikePath ? fromCheckout(resolve(argument)) : await fromRelease(argument);
	const pin = writeEngine(ENGINE_DIR, update);
	console.log(
		`Bundled Loki ${pin.version} (${pin.source.kind === "git" ? `commit ${pin.source.commit.slice(0, 12)}` : pin.source.url}); loki.py sha256 ${pin.files["loki.py"]}`,
	);
	console.log("Review the diff, run npm run test:scripts and the Loki tests, then commit.");
	return 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
	main(process.argv.slice(2)).then(
		(code) => process.exit(code),
		(error) => {
			console.error(error instanceof Error ? error.message : String(error));
			process.exit(1);
		},
	);
}
