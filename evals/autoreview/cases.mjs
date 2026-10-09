/**
 * Case repositories for the autoreview benchmark: a local git repository per case with two commits, base and head,
 * so a reviewer needs only `--repo-dir --base --head`.
 *
 * The upstream tree comes from a bare mirror per task (`<home>/mirrors/<owner>__<name>/<base commit>.git`) that holds
 * the task's base commit with a short history, fetched from the public upstream with plain `git fetch`, no credentials.
 * A case repository borrows the mirror's objects (`objects/info/alternates`) and holds two commits of its own with
 * fixed author, dates and messages, so the same inputs always give the same commit ids and nothing of the upstream
 * history (which holds the fix) is reachable from its branch.
 */

import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { basename, join } from "node:path";
import { parsePatch, patchStats } from "./lib.mjs";

/** History fetched with a base commit: what the search for extra hunks can look at. */
export const HISTORY_DEPTH = 30;
/** The search for extra hunks looks at this many non-merge commits, newest first, ending at the base commit. */
export const NOISE_LOOKBACK = 20;
/** Extra hunks bundled into a buggy diff, at most. */
export const DEFAULT_NOISE_HUNKS = 2;
/** A file's change is used as extra hunks only when it changes at most this many lines. */
export const NOISE_MAX_LINES = 30;
/** Bump when the way a case is built changes: built cases with another version are rebuilt. */
export const BUILD_VERSION = 1;

const IDENTITY = {
	GIT_AUTHOR_NAME: "autoreview-bench",
	GIT_AUTHOR_EMAIL: "autoreview-bench@example.invalid",
	GIT_AUTHOR_DATE: "2000-01-01T00:00:00Z",
	GIT_COMMITTER_NAME: "autoreview-bench",
	GIT_COMMITTER_EMAIL: "autoreview-bench@example.invalid",
	GIT_COMMITTER_DATE: "2000-01-01T00:00:00Z",
};

/** Run git without the user's or the system's configuration. Returns stdout; throws with git's stderr. */
export function git(args, { cwd, env = {}, input } = {}) {
	try {
		return execFileSync("git", args, {
			cwd,
			input,
			env: { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1", GIT_TERMINAL_PROMPT: "0", ...env },
			encoding: "utf8",
			maxBuffer: 256 * 1024 * 1024,
			stdio: ["pipe", "pipe", "pipe"],
		});
	} catch (error) {
		const detail = String(error.stderr ?? error.message).trim();
		throw new Error(`git ${args.slice(0, 3).join(" ")} failed: ${detail.split("\n").slice(-3).join(" ")}`);
	}
}

function gitOk(args, options) {
	try {
		git(args, options);
		return true;
	} catch {
		return false;
	}
}

/** One mirror per (repository, base commit): a fetch for one task can then never shorten the history of another. */
export function mirrorDir(home, repo, sha) {
	return join(home, "mirrors", repo.replace("/", "__"), `${sha}.git`);
}

export function upstreamUrl(repo) {
	return `https://github.com/${repo}.git`;
}

/** Whether the mirror was fetched completely: the ref written after the fetch names the commit. */
export function hasCommit(mirror, sha) {
	return existsSync(mirror) && gitOk(["rev-parse", "-q", "--verify", `refs/bench/${sha}^{commit}`], { cwd: mirror });
}

/**
 * The mirror holding `sha` and `HISTORY_DEPTH` generations of its history. Fetches from the public upstream when
 * it is missing (`fetched: true`), with plain `git fetch` and no credentials.
 */
export function ensureCommit({ home, repo, sha, url = upstreamUrl(repo), depth = HISTORY_DEPTH }) {
	const mirror = mirrorDir(home, repo, sha);
	if (hasCommit(mirror, sha)) return { mirror, fetched: false };
	rmSync(mirror, { recursive: true, force: true });
	mkdirSync(mirror, { recursive: true });
	git(["init", "-q", "--bare", mirror]);
	git(["fetch", "-q", "--no-tags", `--depth=${depth}`, url, sha], { cwd: mirror });
	git(["update-ref", `refs/bench/${sha}`, sha], { cwd: mirror });
	return { mirror, fetched: true };
}

/**
 * Apply extra hunks to the index `env` names, in reverse: changes that upstream made shortly before the base
 * commit, in files the gold patch does not touch, are taken out of the case's base so that they show up in the
 * reviewed diff next to the reversed fix. The reviewed head stays the real upstream tree.
 *
 * Deterministic: the non-merge commits from the base commit backwards, newest first (a commit at the edge of the
 * fetched history, whose parent is not there, is skipped); in each, the
 * modified text files in path order; a file is used when its change has at most `maxLines` changed lines, fits in
 * the hunks still wanted, and reverses cleanly against the index. Returns what was used: `[{ commit, file, hunks,
 * changed }]`.
 */
export function bundleNoise({ source, repoDir, env, baseCommit, goldFiles, maxHunks, lookback = NOISE_LOOKBACK, maxLines = NOISE_MAX_LINES }) {
	const used = [];
	if (maxHunks <= 0) return used;
	const taken = new Set(goldFiles);
	let hunksLeft = maxHunks;
	const commits = git(["rev-list", "--no-merges", "-n", String(lookback), baseCommit], { cwd: source })
		.split("\n")
		.filter(Boolean);
	for (const commit of commits) {
		if (hunksLeft <= 0) break;
		if (!gitOk(["cat-file", "-e", `${commit}^^{commit}`], { cwd: source })) continue;
		const diff = git(["diff-tree", "-p", "--no-renames", "--no-color", "--no-ext-diff", "-U3", `${commit}^`, commit], { cwd: source });
		const files = parsePatch(diff).sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
		for (const file of files) {
			if (hunksLeft <= 0) break;
			if (file.status !== "modified" || file.binary || taken.has(file.path) || !file.hunks.length) continue;
			const stats = patchStats([file]);
			if (file.hunks.length > hunksLeft || stats.changed > maxLines) continue;
			if (!gitOk(["apply", "--cached", "-R", "--check", "-"], { cwd: repoDir, env, input: file.raw })) continue;
			git(["apply", "--cached", "-R", "-"], { cwd: repoDir, env, input: file.raw });
			taken.add(file.path);
			hunksLeft -= file.hunks.length;
			used.push({ commit, file: file.path, hunks: file.hunks.length, changed: stats.changed });
		}
	}
	return used;
}

function markerPath(home, id) {
	return join(home, "built", `${id}.json`);
}

/** The record of a built case (`{ repoDir, base, head, noise }`), or null when it is not built with these settings. */
export function builtCase({ home, spec, noiseHunks = DEFAULT_NOISE_HUNKS }) {
	const path = markerPath(home, spec.id);
	if (!existsSync(path)) return null;
	try {
		const marker = JSON.parse(readFileSync(path, "utf8"));
		const repoDir = join(home, "cases", spec.id);
		const wanted = spec.kind === "buggy" ? noiseHunks : 0;
		if (marker.version !== BUILD_VERSION || marker.baseCommit !== spec.baseCommit || marker.noiseHunks !== wanted) return null;
		if (!gitOk(["cat-file", "-e", `${marker.head}^{commit}`], { cwd: repoDir })) return null;
		return { repoDir, base: marker.base, head: marker.head, noise: marker.noise };
	} catch {
		return null;
	}
}

/**
 * Build one case repository at `<home>/cases/<case id>` from `source`, a git repository that holds the task's base
 * commit (the mirror; in tests, any local repository).
 *
 * - buggy: base is the upstream tree with the gold patch applied (and the extra hunks taken out), head is the
 *   upstream tree at the task's base commit, the state with the bug. The diff base..head introduces the bug.
 * - clean: base is the upstream tree at the task's base commit, head has the gold patch applied.
 *
 * The work tree is checked out at head. Returns `{ repoDir, base, head, noise }`.
 */
export function buildCase({ home, spec, source, noiseHunks = DEFAULT_NOISE_HUNKS }) {
	const repoDir = join(home, "cases", spec.id);
	rmSync(repoDir, { recursive: true, force: true });
	rmSync(markerPath(home, spec.id), { force: true });
	mkdirSync(repoDir, { recursive: true });
	git(["init", "-q", "-b", "main", repoDir]);
	const sourceObjects = join(git(["rev-parse", "--absolute-git-dir"], { cwd: source }).trim(), "objects");
	writeFileSync(join(repoDir, ".git", "objects", "info", "alternates"), `${sourceObjects}\n`);
	const env = { ...IDENTITY, GIT_INDEX_FILE: join(repoDir, ".git", "bench-index") };
	const upstreamTree = git(["rev-parse", `${spec.baseCommit}^{tree}`], { cwd: repoDir }).trim();
	git(["read-tree", upstreamTree], { cwd: repoDir, env });
	git(["apply", "--cached", "-"], { cwd: repoDir, env, input: spec.instance.patch });
	const fixedTree = git(["write-tree"], { cwd: repoDir, env }).trim();
	if (fixedTree === upstreamTree) throw new Error(`${spec.id}: the gold patch changes nothing`);
	const goldFiles = parsePatch(spec.instance.patch).flatMap((file) => [file.oldPath, file.newPath]);
	let noise = [];
	let baseTree = upstreamTree;
	let headTree = fixedTree;
	if (spec.kind === "buggy") {
		noise = bundleNoise({ source, repoDir, env, baseCommit: spec.baseCommit, goldFiles, maxHunks: noiseHunks });
		baseTree = git(["write-tree"], { cwd: repoDir, env }).trim();
		headTree = upstreamTree;
	}
	rmSync(env.GIT_INDEX_FILE, { force: true });
	const changed = git(["diff-tree", "-r", "--name-only", "--no-renames", baseTree, headTree], { cwd: repoDir })
		.split("\n")
		.filter(Boolean);
	const base = git(["commit-tree", baseTree, "-m", "Base"], { cwd: repoDir, env: IDENTITY }).trim();
	const subject = `Update ${basename(changed[0])}${changed.length > 1 ? ` and ${changed.length - 1} more file${changed.length > 2 ? "s" : ""}` : ""}`;
	const head = git(["commit-tree", headTree, "-p", base, "-m", subject], { cwd: repoDir, env: IDENTITY }).trim();
	git(["update-ref", "refs/heads/main", head], { cwd: repoDir });
	git(["checkout", "-q", "-f", "main"], { cwd: repoDir });
	mkdirSync(join(home, "built"), { recursive: true });
	const marker = { version: BUILD_VERSION, baseCommit: spec.baseCommit, noiseHunks: spec.kind === "buggy" ? noiseHunks : 0, base, head, noise };
	writeFileSync(markerPath(home, spec.id), `${JSON.stringify(marker, null, 2)}\n`);
	return { repoDir, base, head, noise };
}
