#!/usr/bin/env node
/**
 * Scan for credentials and home-directory paths before a release or in CI.
 *
 *   node scripts/secret-scan.mjs                     # tracked files of the repository (npm run scan:secrets)
 *   node scripts/secret-scan.mjs --diff origin/main  # lines added since a ref (any `git diff` range)
 *   node scripts/secret-scan.mjs --dir <path>        # a directory (skips .git and node_modules)
 *   node scripts/secret-scan.mjs --tarball <x.tgz>   # an npm tarball, unpacked (node_modules included)
 *
 * The rules are packages/coding-agent/src/ultron/rlm/secret-patterns.json, the same ones that mask secrets in
 * RLM cell output. Findings print as `path:line:col kind preview`, with the value redacted, and the scan exits 1;
 * a clean scan exits 0 and a usage error 2.
 *
 * `.secret-scan-allow` at the repository root excuses paths: one glob per line (`**`, `*`, `?`), optionally
 * followed by the rule ids it excuses (comma-separated; all rules when omitted). Use it only for files that must
 * contain fake credentials or example paths, such as test fixtures. `--allow <file>` reads another list and
 * `--no-allow` ignores it.
 */
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { loadSecretPatterns, SecretDetector } from "../packages/coding-agent/src/ultron/secrets.ts";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
export const PATTERNS_PATH = join(root, "packages", "coding-agent", "src", "ultron", "rlm", "secret-patterns.json");
export const DEFAULT_ALLOWLIST = join(root, ".secret-scan-allow");
/** Larger files are skipped (and counted); a release bundle is well below this. */
const MAX_FILE_BYTES = 64 * 1024 * 1024;

let detector;
export function secretDetector() {
	detector ??= new SecretDetector(loadSecretPatterns(PATTERNS_PATH));
	return detector;
}

/** A path glob as a regular expression over `/`-separated relative paths. */
export function globToRegExp(glob) {
	let out = "";
	for (let i = 0; i < glob.length; i++) {
		const char = glob[i];
		if (char === "*" && glob[i + 1] === "*") {
			const slash = glob[i + 2] === "/";
			out += slash ? "(?:.*/)?" : ".*";
			i += slash ? 2 : 1;
		} else if (char === "*") out += "[^/]*";
		else if (char === "?") out += "[^/]";
		else out += char.replace(/[.+^${}()|[\]\\]/g, "\\$&");
	}
	return new RegExp(`^${out}$`);
}

/** Parse an allowlist: `glob [rule,rule]` per line, `#` comments. */
export function parseAllowlist(text) {
	const entries = [];
	for (const raw of text.split("\n")) {
		const line = raw.replace(/#.*$/, "").trim();
		if (!line) continue;
		const [glob, kinds] = line.split(/\s+/, 2);
		entries.push({ glob, regex: globToRegExp(glob), kinds: kinds ? new Set(kinds.split(",").filter(Boolean)) : undefined });
	}
	return entries;
}

export function loadAllowlist(path) {
	return path && existsSync(path) ? parseAllowlist(readFileSync(path, "utf8")) : [];
}

function allowed(allowlist, path, kind) {
	return allowlist.some((entry) => entry.regex.test(path) && (!entry.kinds || entry.kinds.has(kind)));
}

/** Findings in one file's text; `lineOffset` maps line 1 of `text` to a later line (diff hunks). */
export function scanText(path, text, { allowlist = [], lineOffset = 0, lineMap } = {}) {
	const findings = [];
	let line = 1;
	let lineStart = 0;
	let cursor = 0;
	for (const finding of secretDetector().scan(text, "scan")) {
		if (allowed(allowlist, path, finding.kind)) continue;
		// Advance the line count incrementally; findings come in order.
		for (; cursor < finding.start; cursor++)
			if (text.charCodeAt(cursor) === 10) {
				line++;
				lineStart = cursor + 1;
			}
		const at = lineMap ? lineMap[line - 1] : line + lineOffset;
		findings.push({ path, line: at, column: finding.start - lineStart + 1, kind: finding.kind, preview: finding.preview });
	}
	return findings;
}

function readText(file) {
	const size = statSync(file).size;
	if (size > MAX_FILE_BYTES) return undefined;
	const data = readFileSync(file);
	// Binary files (a NUL byte near the start) are not scanned.
	if (data.subarray(0, 8000).includes(0)) return undefined;
	return data.toString("utf8");
}

function toPosix(path) {
	return sep === "/" ? path : path.split(sep).join("/");
}

/** Scan files under `dir`; paths are reported relative to it. */
export function scanDirectory(dir, { allowlist = [], includeNodeModules = false, prefix = "" } = {}) {
	const findings = [];
	let files = 0;
	let skipped = 0;
	const walk = (current) => {
		const entries = readdirSync(current, { withFileTypes: true }).sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
		for (const entry of entries) {
			if (entry.name === ".git" || (!includeNodeModules && entry.name === "node_modules")) continue;
			const full = join(current, entry.name);
			if (entry.isDirectory()) walk(full);
			else if (entry.isFile()) {
				const text = readText(full);
				if (text === undefined) {
					skipped++;
					continue;
				}
				files++;
				findings.push(...scanText(prefix + toPosix(relative(dir, full)), text, { allowlist }));
			}
		}
	};
	walk(dir);
	return { findings, files, skipped };
}

function git(args, cwd) {
	return execFileSync("git", args, { cwd, encoding: "utf8", maxBuffer: 1024 * 1024 * 1024 });
}

/** Scan the tracked files of the repository containing `cwd`. */
export function scanTree({ cwd = root, allowlist = [] } = {}) {
	const top = git(["rev-parse", "--show-toplevel"], cwd).trim();
	const findings = [];
	let files = 0;
	let skipped = 0;
	for (const path of git(["ls-files", "-z"], top).split("\0").filter(Boolean)) {
		const full = join(top, path);
		if (!existsSync(full) || !statSync(full).isFile()) continue;
		const text = readText(full);
		if (text === undefined) {
			skipped++;
			continue;
		}
		files++;
		findings.push(...scanText(path, text, { allowlist }));
	}
	return { findings, files, skipped };
}

/**
 * Scan the lines a `git diff` adds. Consecutive added lines are scanned together, so a multi-line key block is
 * seen whole; findings carry the line numbers of the new file.
 */
export function scanDiffText(diff, { allowlist = [] } = {}) {
	const findings = [];
	let path;
	let next = 0;
	let block = [];
	let blockLines = [];
	let files = 0;
	const flush = () => {
		if (path && block.length > 0) findings.push(...scanText(path, block.join("\n"), { allowlist, lineMap: blockLines }));
		block = [];
		blockLines = [];
	};
	for (const line of diff.split("\n")) {
		if (line.startsWith("diff --git ")) {
			flush();
			path = undefined;
		} else if (line.startsWith("+++ ")) {
			flush();
			const target = line.slice(4).trim();
			path = target === "/dev/null" ? undefined : target.replace(/^b\//, "");
			if (path) files++;
		} else if (line.startsWith("@@")) {
			flush();
			const match = /\+(\d+)/.exec(line);
			next = match ? Number(match[1]) : 0;
		} else if (line.startsWith("+")) {
			block.push(line.slice(1));
			blockLines.push(next++);
		} else {
			flush();
			if (line.startsWith(" ")) next++;
		}
	}
	flush();
	return { findings, files, skipped: 0 };
}

export function scanDiff(range, { cwd = root, allowlist = [] } = {}) {
	const diff = git(["diff", "--no-color", "--no-ext-diff", "--unified=0", range], cwd);
	return scanDiffText(diff, { allowlist });
}

/** Unpack an npm tarball and scan everything in it, node_modules included. */
export function scanTarball(tarball, { allowlist = [] } = {}) {
	const work = mkdtempSync(join(tmpdir(), "ultron-secret-scan-"));
	try {
		execFileSync("tar", ["-xzf", resolve(tarball), "-C", work]);
		return scanDirectory(work, { allowlist, includeNodeModules: true });
	} finally {
		rmSync(work, { recursive: true, force: true });
	}
}

export function formatFindings(findings) {
	return findings.map((f) => `${f.path}:${f.line}:${f.column}  ${f.kind}  ${f.preview}`).join("\n");
}

function usage() {
	return "usage: secret-scan.mjs [--tree | --diff <range> | --dir <path> | --tarball <file.tgz>] [--allow <file> | --no-allow] [--json]";
}

/** Run the CLI; returns the exit code. */
export function main(argv, { log = console.log, error = console.error, cwd = process.cwd() } = {}) {
	let mode = "tree";
	let target;
	let allowPath = DEFAULT_ALLOWLIST;
	let json = false;
	for (let i = 0; i < argv.length; i++) {
		const arg = argv[i];
		if (arg === "--tree") mode = "tree";
		else if (arg === "--diff" || arg === "--dir" || arg === "--tarball") {
			mode = arg.slice(2);
			target = argv[++i];
			if (!target) {
				error(usage());
				return 2;
			}
		} else if (arg === "--allow") allowPath = argv[++i];
		else if (arg === "--no-allow") allowPath = undefined;
		else if (arg === "--json") json = true;
		else if (arg === "--help" || arg === "-h") {
			log(usage());
			return 0;
		} else {
			error(`unknown argument ${arg}\n${usage()}`);
			return 2;
		}
	}
	const allowlist = loadAllowlist(allowPath);
	const result =
		mode === "diff"
			? scanDiff(target, { cwd, allowlist })
			: mode === "dir"
				? scanDirectory(resolve(cwd, target), { allowlist })
				: mode === "tarball"
					? scanTarball(resolve(cwd, target), { allowlist })
					: scanTree({ cwd, allowlist });
	if (json) log(JSON.stringify(result, null, 2));
	else if (result.findings.length > 0) {
		error(formatFindings(result.findings));
		error(
			`secret-scan: ${result.findings.length} finding(s) in ${mode}${target ? ` ${target}` : ""}. Remove the value (read it from the environment instead), or, for a deliberate fake in a test fixture, add the path to .secret-scan-allow.`,
		);
	} else log(`secret-scan: clean (${result.files} files${result.skipped ? `, ${result.skipped} binary or oversized skipped` : ""})`);
	return result.findings.length > 0 ? 1 : 0;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) process.exitCode = main(process.argv.slice(2));
