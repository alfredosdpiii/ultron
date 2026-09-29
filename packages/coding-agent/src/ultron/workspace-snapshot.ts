/**
 * Cheap before/after pictures of a working directory, for checking which files changed while some piece of work
 * ran (a subagent's run today; a single REPL cell later).
 *
 * A snapshot records one metadata signature per file, never file contents: size, modification and change times in
 * nanoseconds, mode and inode. Any write, rename-over, permission change or deletion changes the signature, so a
 * diff of two snapshots is the set of paths something touched in between. Rewriting a file with identical bytes
 * still counts as a change: the check answers "was it written", not "does it differ".
 *
 * Inside a Git working tree the file list comes from `git ls-files --cached --others --exclude-standard`, so
 * `.gitignore`d output (builds, caches, dependencies) is never listed and deleted tracked files still are. Outside
 * one, the directory is walked without following links, skipping the usual dependency and cache directories. Both
 * paths are bounded by `maxEntries`; a snapshot that hit the bound says so (`complete: false`) and so does every
 * diff that uses it.
 */
import { execFile } from "node:child_process";
import { lstat, readdir, readlink } from "node:fs/promises";
import { join, resolve, sep } from "node:path";

export type WorkspaceSnapshot = {
	/** Absolute directory the snapshot describes; paths in `entries` are relative to it, with `/` separators. */
	readonly root: string;
	/** How the file list was made. */
	readonly method: "git" | "walk";
	/** Relative path -> metadata signature. A path that does not exist is absent. */
	readonly entries: ReadonlyMap<string, string>;
	/** False when the entry bound was reached and some files were left out. */
	readonly complete: boolean;
	readonly takenAt: number;
	readonly durationMs: number;
};

export type WorkspaceDiff = {
	readonly added: string[];
	readonly modified: string[];
	readonly deleted: string[];
	/** Every path in `added`, `modified` or `deleted`, sorted. */
	readonly changed: string[];
	/** False when either snapshot was incomplete, so changes outside the listed files may be missing. */
	readonly complete: boolean;
};

export type SnapshotOptions = {
	/** Most files recorded (default 200,000). */
	maxEntries?: number;
	/** Directory and file names skipped while walking a directory that is not a Git working tree. */
	walkIgnore?: readonly string[];
	/** Relative paths (with `/` separators) to leave out in either mode, such as a harness's own state files. */
	exclude?: (path: string) => boolean;
	/** Set false to always walk, even inside a Git working tree. */
	git?: boolean;
	signal?: AbortSignal;
};

export const DEFAULT_MAX_SNAPSHOT_ENTRIES = 200_000;

/** Skipped by the non-Git walk: dependency trees, virtual environments and tool caches are not source. */
export const DEFAULT_WALK_IGNORE: readonly string[] = [
	".git",
	".hg",
	".svn",
	"node_modules",
	".venv",
	"venv",
	"__pycache__",
	".pytest_cache",
	".mypy_cache",
	".ruff_cache",
	".tox",
	".cache",
];

const STAT_BATCH = 256;

/** One file's metadata signature; undefined when it no longer exists. */
async function signature(path: string): Promise<string | undefined> {
	try {
		const stats = await lstat(path, { bigint: true });
		if (stats.isSymbolicLink()) return `l:${await readlink(path).catch(() => "")}`;
		if (stats.isDirectory()) return "d";
		return `f:${stats.size}:${stats.mtimeNs}:${stats.ctimeNs}:${stats.mode}:${stats.ino}`;
	} catch {
		return undefined;
	}
}

async function signatures(root: string, paths: readonly string[], signal?: AbortSignal): Promise<Map<string, string>> {
	const entries = new Map<string, string>();
	for (let start = 0; start < paths.length; start += STAT_BATCH) {
		signal?.throwIfAborted();
		const batch = paths.slice(start, start + STAT_BATCH);
		const found = await Promise.all(batch.map((path) => signature(join(root, path))));
		for (let index = 0; index < batch.length; index += 1) {
			const value = found[index];
			if (value !== undefined) entries.set(batch[index]!, value);
		}
	}
	return entries;
}

/** Tracked and untracked, not ignored, files under `root` (relative to it), or undefined outside a Git work tree. */
function gitFiles(root: string, signal?: AbortSignal): Promise<string[] | undefined> {
	return new Promise((done) => {
		execFile(
			"git",
			["-c", "core.quotepath=off", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
			{
				cwd: root,
				encoding: "utf8",
				maxBuffer: 1024 * 1024 * 1024,
				// Listing must not take the index lock that a concurrent `git add` or commit needs.
				env: { ...process.env, GIT_OPTIONAL_LOCKS: "0" },
				...(signal === undefined ? {} : { signal }),
				windowsHide: true,
			},
			(error, stdout) => {
				if (error) {
					done(undefined);
					return;
				}
				const files = new Set<string>();
				for (const item of stdout.split("\0")) {
					// A nested repository is listed as its directory ("sub/"); it is recorded as one entry.
					const path = item.endsWith("/") ? item.slice(0, -1) : item;
					if (path) files.add(path);
				}
				done([...files]);
			},
		);
	});
}

/** Files and links under `root`, without following links; stops at `limit` entries. */
async function walkFiles(
	root: string,
	ignore: ReadonlySet<string>,
	limit: number,
	signal?: AbortSignal,
): Promise<{ files: string[]; complete: boolean }> {
	const files: string[] = [];
	const pending = [""];
	while (pending.length > 0) {
		signal?.throwIfAborted();
		const directory = pending.pop()!;
		let items: import("node:fs").Dirent[];
		try {
			items = await readdir(directory === "" ? root : join(root, directory), { withFileTypes: true });
		} catch {
			continue;
		}
		for (const item of items) {
			if (ignore.has(item.name)) continue;
			const path = directory === "" ? item.name : `${directory}/${item.name}`;
			if (item.isDirectory()) {
				pending.push(path);
				continue;
			}
			if (files.length >= limit) return { files, complete: false };
			files.push(path);
		}
	}
	return { files, complete: true };
}

/** A metadata snapshot of the files under `cwd` (see the module comment). Never throws for an unreadable file. */
export async function snapshotWorkspace(cwd: string, options: SnapshotOptions = {}): Promise<WorkspaceSnapshot> {
	const started = Date.now();
	const root = resolve(cwd);
	const limit = options.maxEntries ?? DEFAULT_MAX_SNAPSHOT_ENTRIES;
	const listed = options.git === false ? undefined : await gitFiles(root, options.signal);
	let method: WorkspaceSnapshot["method"];
	let files: string[];
	let complete: boolean;
	if (listed !== undefined) {
		method = "git";
		complete = listed.length <= limit;
		files = listed.slice(0, limit);
	} else {
		method = "walk";
		({ files, complete } = await walkFiles(
			root,
			new Set(options.walkIgnore ?? DEFAULT_WALK_IGNORE),
			limit,
			options.signal,
		));
	}
	if (sep !== "/") files = files.map((path) => path.split(sep).join("/"));
	if (options.exclude) files = files.filter((path) => !options.exclude!(path));
	const entries = await signatures(root, files, options.signal);
	return { root, method, entries, complete, takenAt: started, durationMs: Date.now() - started };
}

/** What changed between two snapshots of the same directory. */
export function diffSnapshots(before: WorkspaceSnapshot, after: WorkspaceSnapshot): WorkspaceDiff {
	if (before.root !== after.root) throw new Error("Snapshots describe different directories");
	const added: string[] = [];
	const modified: string[] = [];
	const deleted: string[] = [];
	for (const [path, value] of after.entries) {
		const previous = before.entries.get(path);
		if (previous === undefined) added.push(path);
		else if (previous !== value) modified.push(path);
	}
	for (const path of before.entries.keys()) if (!after.entries.has(path)) deleted.push(path);
	added.sort();
	modified.sort();
	deleted.sort();
	return {
		added,
		modified,
		deleted,
		changed: [...added, ...modified, ...deleted].sort(),
		complete: before.complete && after.complete,
	};
}
