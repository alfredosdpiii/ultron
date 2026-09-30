/**
 * Git worktrees for sub-agents: `rlm.spawn(brief, name=..., worktree=True)` gives a child a private checkout of its
 * parent's repository, and `rlm.merge(children)` brings each child's work back into the parent's working tree.
 *
 * Creating (createChildWorktree). The parent's working tree is pictured as a commit without touching its index or
 * files: a copy of its index gets `git add -A` (tracked changes, deletions and untracked files Git does not ignore)
 * and `git write-tree`/`git commit-tree` turn it into a snapshot commit on top of HEAD (HEAD itself when nothing is
 * dirty). The child's branch `ultron/<session>/<name>` starts there, checked out in
 * `<git common dir>/ultron-worktrees/<id>`: inside `.git`, so the parent's tools, `git status` and file listings
 * never see it. Git ignores the parent's gitignored environment (node_modules, .venv, .env), so a setup step links
 * the heavy dependency directories into the worktree and copies small env files (see WorktreeSetup).
 *
 * Finishing (commitChildWorktree). Whatever the child changed is committed on its branch (hooks and signing off;
 * the message is its verdict's summary), so nothing lives only in a directory.
 *
 * Merging (mergeChildWorktree). A three-way merge of trees, never of the parent's index: base = the child's snapshot
 * commit, ours = a fresh snapshot of the parent's working tree, theirs = the child's commit, merged by
 * `git merge-tree --write-tree`. A clean result is written into the parent's working tree as uncommitted changes
 * (through a temporary index, so smudge filters and file modes apply and the real index is untouched), after
 * checking that none of the files it writes changed since the snapshot. A conflict writes nothing (unless the
 * caller asked for conflict markers) and is reported with its files and hunks. Nothing is ever committed on the
 * parent's branch.
 *
 * Crashes. Each worktree has a record next to it (`<id>.json`) naming the process that owns it; a later session
 * prunes the worktrees of owners that are gone, first committing any uncommitted work on the branch, and keeps
 * branches that hold work. Only Ultron's own directory and branches are touched.
 *
 * Limits: submodules are not initialized in a worktree (their directories are empty); Git LFS files are checked
 * out only when git-lfs is installed; a repository without commits cannot branch.
 */
import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import {
	copyFile,
	lstat,
	mkdir,
	readdir,
	readFile,
	readlink,
	realpath,
	rm,
	rmdir,
	stat,
	symlink,
	unlink,
	writeFile,
} from "node:fs/promises";
import { devNull, hostname, tmpdir } from "node:os";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";

/** Directory under the Git common dir that holds Ultron's worktrees and their records. */
export const WORKTREES_DIR = "ultron-worktrees";
/** Branches of sub-agent worktrees: `ultron/<session>/<name>`. */
export const BRANCH_PREFIX = "ultron";
/** Keep worktrees and branches after merges, on session end and at startup pruning (debugging). */
export const KEEP_WORKTREES_ENV = "ULTRON_KEEP_WORKTREES";

/** Gitignored directories linked into a worktree by default (dependency trees, virtual environments). */
export const DEFAULT_LINK_NAMES: readonly string[] = ["node_modules", ".venv", "venv"];
/** Gitignored files copied by default: `.env` and `.env.*`, when small. */
const DEFAULT_COPY_PATTERN = /^\.env(?:\..+)?$/;
const MAX_COPY_BYTES = 256 * 1024;
/** Default link/copy candidates are looked for this many directories deep at most. */
const MAX_SETUP_DEPTH = 3;
const MAX_SETUP_ENTRIES = 64;
const DEFAULT_SETUP_TIMEOUT_MS = 10 * 60 * 1000;
const GIT_TIMEOUT_MS = 120_000;
const MAX_HUNKS_PER_FILE = 5;
const MAX_CONFLICT_FILES = 20;
const HUNK_TEXT_CHARS = 400;

export class WorktreeError extends Error {
	/** True when a shared working directory is a fine fallback (`worktree="auto"`): no Git, no repository, no commits. */
	readonly fallback: boolean;
	constructor(message: string, fallback = false) {
		super(message);
		this.name = "WorktreeError";
		this.fallback = fallback;
	}
}

export function keepWorktrees(env: NodeJS.ProcessEnv = process.env): boolean {
	const value = env[KEEP_WORKTREES_ENV]?.trim().toLowerCase();
	return value !== undefined && value !== "" && !["0", "false", "no", "off"].includes(value);
}

/** How a worktree is prepared for its child after checkout. */
export interface WorktreeSetup {
	/**
	 * Gitignored paths (relative to the repository root) to link from the parent's checkout. Default: every
	 * gitignored `node_modules`, `.venv` and `venv` directory present in the parent (a few levels deep). `[]` links
	 * nothing. A `node_modules` directory is linked entry by entry, so workspace packages resolve to the worktree's
	 * own sources; another directory becomes a directory of links to its entries; a file is one symlink.
	 */
	readonly link?: readonly string[];
	/** Gitignored files to copy. Default: small `.env` / `.env.*` files. `[]` copies nothing. */
	readonly copy?: readonly string[];
	/** A shell command run in the child's working directory before it starts (for example `npm ci`). */
	readonly command?: string;
	readonly timeoutMs?: number;
}

/** A sub-agent worktree, as recorded next to it (`<common dir>/ultron-worktrees/<id>.json`). */
export interface WorktreeRecord {
	readonly version: 1;
	readonly id: string;
	readonly sessionId: string;
	taskId?: string;
	readonly name: string;
	/** The process that owns it; a later session prunes it once that process is gone. */
	readonly owner: { readonly pid: number; readonly host: string };
	/** Top of the working tree it branched from, and where `rlm.merge` writes its changes by default. */
	readonly repo: string;
	readonly commonDir: string;
	/** The worktree's root, and the child's working directory in it (the parent's subdirectory, when it had one). */
	readonly path: string;
	readonly cwd: string;
	readonly branch: string;
	/** The parent's HEAD, and the snapshot commit the branch starts at (HEAD plus uncommitted changes). */
	readonly head: string;
	readonly base: string;
	/** The parent had uncommitted changes, so `base` is a snapshot commit on top of `head`. */
	readonly dirty: boolean;
	/** Setup: linked and copied paths (relative to the worktree root), and the command still to run. */
	readonly linked: string[];
	readonly copied: string[];
	readonly command?: string;
	readonly commandTimeoutMs?: number;
	readonly createdAt: number;
	state: "creating" | "active" | "ended" | "merged" | "conflict" | "failed";
	commit?: string | null;
}

/** What a finished child's result says about its worktree. */
export interface WorktreeInfo {
	readonly task?: string;
	readonly branch: string;
	readonly path: string;
	readonly repo: string;
	readonly base: string;
	/** The branch's final commit, or null when the child changed nothing. */
	readonly commit: string | null;
	readonly changed_files: string[];
	readonly diffstat: string;
	/** The directory is gone (an empty child's worktree is removed at once). */
	readonly removed?: boolean;
	readonly error?: string;
}

// ---------------------------------------------------------------------------------------------------------------
// Git

type GitResult = { code: number; stdout: string; stderr: string };

interface GitOptions {
	env?: NodeJS.ProcessEnv;
	input?: string;
	signal?: AbortSignal;
	timeoutMs?: number;
}

/** Runs `git` without a shell or terminal. Throws WorktreeError when Git cannot start. */
export function git(cwd: string, args: readonly string[], options: GitOptions = {}): Promise<GitResult> {
	return new Promise((done, fail) => {
		let stdout = "";
		let stderr = "";
		let settled = false;
		const child = spawn("git", [...args], {
			cwd,
			env: { ...process.env, GIT_TERMINAL_PROMPT: "0", ...options.env },
			stdio: [options.input === undefined ? "ignore" : "pipe", "pipe", "pipe"],
			windowsHide: true,
		});
		const timer = setTimeout(() => child.kill("SIGTERM"), options.timeoutMs ?? GIT_TIMEOUT_MS);
		timer.unref?.();
		const abort = () => child.kill("SIGTERM");
		options.signal?.addEventListener("abort", abort, { once: true });
		const finish = (result: GitResult | Error) => {
			if (settled) return;
			settled = true;
			clearTimeout(timer);
			options.signal?.removeEventListener("abort", abort);
			if (result instanceof Error) fail(result);
			else done(result);
		};
		child.stdout?.setEncoding("utf8").on("data", (chunk: string) => {
			stdout += chunk;
		});
		child.stderr?.setEncoding("utf8").on("data", (chunk: string) => {
			if (stderr.length < 100_000) stderr += chunk;
		});
		child.on("error", (error: NodeJS.ErrnoException) =>
			finish(
				error.code === "ENOENT"
					? new WorktreeError("worktree=True needs Git, and `git` is not installed or not on PATH", true)
					: error,
			),
		);
		child.on("close", (code) => finish({ code: code ?? 1, stdout, stderr }));
		if (options.input !== undefined) {
			child.stdin?.on("error", () => {});
			child.stdin?.end(options.input);
		}
	});
}

/** `git` that must succeed; its stderr names what failed. */
async function gitOk(cwd: string, args: readonly string[], options: GitOptions = {}): Promise<string> {
	const result = await git(cwd, args, options);
	if (result.code !== 0)
		throw new WorktreeError(
			`git ${args.filter((arg) => !arg.startsWith("-c") && !arg.includes("=")).join(" ")} failed: ${firstLines(result.stderr || result.stdout) || `exit ${result.code}`}`,
		);
	return result.stdout;
}

function firstLines(text: string, count = 3): string {
	return text
		.split("\n")
		.map((line) => line.trim())
		.filter(Boolean)
		.slice(0, count)
		.join(" | ");
}

/** No user hooks, no signing prompts: these are Ultron's own bookkeeping commits and checkouts. */
const QUIET = ["-c", `core.hooksPath=${devNull}`, "-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"];

const identities = new Map<string, Promise<NodeJS.ProcessEnv>>();

/** Author/committer for Ultron's commits: the repository's configured identity, else "Ultron". */
function identityEnv(cwd: string): Promise<NodeJS.ProcessEnv> {
	let cached = identities.get(cwd);
	if (cached === undefined) {
		cached = (async () => {
			const [name, email] = await Promise.all([
				git(cwd, ["config", "user.name"]),
				git(cwd, ["config", "user.email"]),
			]);
			const env: NodeJS.ProcessEnv = {};
			if (!name.stdout.trim() && !process.env.GIT_AUTHOR_NAME) {
				env.GIT_AUTHOR_NAME = "Ultron";
				env.GIT_COMMITTER_NAME = "Ultron";
			}
			if (!email.stdout.trim() && !process.env.GIT_AUTHOR_EMAIL) {
				env.GIT_AUTHOR_EMAIL = "ultron@localhost";
				env.GIT_COMMITTER_EMAIL = "ultron@localhost";
			}
			return env;
		})();
		identities.set(cwd, cached);
	}
	return cached;
}

export interface RepoInfo {
	/** The working tree's top directory (real path). */
	readonly top: string;
	readonly commonDir: string;
	/** HEAD's commit, or undefined in a repository without commits. */
	readonly head: string | undefined;
}

/** The repository `cwd` is in. Throws WorktreeError (a fallback one) outside Git or a working tree. */
export async function repoInfo(cwd: string): Promise<RepoInfo> {
	const result = await git(cwd, [
		"rev-parse",
		"--path-format=absolute",
		"--show-toplevel",
		"--git-common-dir",
		"--is-bare-repository",
	]);
	if (result.code !== 0) {
		if (/not a git repository/i.test(result.stderr))
			throw new WorktreeError(`worktree=True needs a Git repository, and ${cwd} is not inside one`, true);
		throw new WorktreeError(`git could not read the repository at ${cwd}: ${firstLines(result.stderr)}`, true);
	}
	const [top, commonDir, bare] = result.stdout.trim().split("\n");
	if (bare === "true" || !top) throw new WorktreeError(`${cwd} is not inside a Git working tree`, true);
	const head = await git(cwd, ["rev-parse", "--verify", "-q", "HEAD^{commit}"]);
	return {
		top: resolve(top),
		commonDir: resolve(commonDir!),
		head: head.code === 0 ? head.stdout.trim() : undefined,
	};
}

/**
 * A commit of `top`'s working tree as it is now (tracked changes, deletions, untracked files Git does not ignore),
 * made from a copy of its index, so the working tree and the real index stay untouched. Returns HEAD itself when
 * nothing differs from it.
 */
export async function snapshotWorkingTree(
	top: string,
	head: string,
	message: string,
	signal?: AbortSignal,
): Promise<{ commit: string; dirty: boolean }> {
	const indexPath = (await gitOk(top, ["rev-parse", "--path-format=absolute", "--git-path", "index"])).trim();
	const temporary = join(tmpdir(), `ultron-index-${process.pid}-${randomBytes(6).toString("hex")}`);
	const env = { GIT_INDEX_FILE: temporary, GIT_OPTIONAL_LOCKS: "0" };
	try {
		const copied = await copyFile(indexPath, temporary).then(
			() => true,
			() => false,
		);
		if (!copied) await gitOk(top, ["read-tree", head], { env, signal });
		await gitOk(top, [...QUIET, "add", "-A", "--", "."], { env, signal });
		const tree = (await gitOk(top, ["write-tree"], { env, signal })).trim();
		const headTree = (await gitOk(top, ["rev-parse", `${head}^{tree}`], { signal })).trim();
		if (tree === headTree) return { commit: head, dirty: false };
		const commit = (
			await gitOk(top, [...QUIET, "commit-tree", tree, "-p", head, "-m", message], {
				env: await identityEnv(top),
				signal,
			})
		).trim();
		return { commit, dirty: true };
	} finally {
		await rm(temporary, { force: true }).catch(() => {});
		await rm(`${temporary}.lock`, { force: true }).catch(() => {});
	}
}

// ---------------------------------------------------------------------------------------------------------------
// Records

function recordsDir(commonDir: string): string {
	return join(commonDir, WORKTREES_DIR);
}

function recordPath(record: Pick<WorktreeRecord, "commonDir" | "id">): string {
	return join(recordsDir(record.commonDir), `${record.id}.json`);
}

export async function saveRecord(record: WorktreeRecord): Promise<void> {
	await mkdir(recordsDir(record.commonDir), { recursive: true });
	await writeFile(recordPath(record), `${JSON.stringify(record, null, "\t")}\n`, { mode: 0o600 });
}

async function deleteRecord(record: Pick<WorktreeRecord, "commonDir" | "id">): Promise<void> {
	await rm(recordPath(record), { force: true });
}

/** Every recorded worktree of the repository `cwd` is in (any session). */
export async function listRecords(cwd: string): Promise<WorktreeRecord[]> {
	let commonDir: string;
	try {
		commonDir = (await repoInfo(cwd)).commonDir;
	} catch {
		return [];
	}
	return readRecords(commonDir);
}

async function readRecords(commonDir: string): Promise<WorktreeRecord[]> {
	const dir = recordsDir(commonDir);
	const names = await readdir(dir).catch(() => [] as string[]);
	const records: WorktreeRecord[] = [];
	for (const name of names.filter((item) => item.endsWith(".json")).sort()) {
		try {
			const parsed = JSON.parse(await readFile(join(dir, name), "utf8")) as WorktreeRecord;
			if (parsed?.version === 1 && typeof parsed.id === "string" && typeof parsed.path === "string")
				records.push(parsed);
		} catch {
			// A torn record is left for its directory scan below.
		}
	}
	return records;
}

/** Whether a record's owning process still runs (on this machine). */
export function ownerAlive(owner: WorktreeRecord["owner"]): boolean {
	if (owner.host !== hostname()) return true;
	if (owner.pid === process.pid) return true;
	try {
		process.kill(owner.pid, 0);
		return true;
	} catch (error) {
		return (error as NodeJS.ErrnoException).code === "EPERM";
	}
}

/** True when `path` lies in one of Ultron's sub-agent worktrees (Loki must not auto-install or commit there). */
export function isUltronWorktreePath(path: string, env: NodeJS.ProcessEnv = process.env): boolean {
	if (env.ULTRON_WORKTREE?.trim()) return true;
	return resolve(path).split(sep).includes(WORKTREES_DIR);
}

// ---------------------------------------------------------------------------------------------------------------
// Create

function slug(name: string): string {
	const text = name
		.toLowerCase()
		.replace(/[^a-z0-9._-]+/g, "-")
		.replace(/\.{2,}/g, ".")
		.replace(/-{2,}/g, "-")
		.replace(/^[-.]+|[-.]+$/g, "")
		.slice(0, 40)
		.replace(/[-.]+$/g, "");
	return text || "child";
}

async function branchExists(top: string, branch: string): Promise<boolean> {
	return (await git(top, ["show-ref", "--verify", "--quiet", `refs/heads/${branch}`])).code === 0;
}

/** A new branch name for a child: `ultron/<session>/<name>`, suffixed when that name is taken (never reused). */
async function uniqueBranch(top: string, sessionId: string, name: string): Promise<string> {
	const stem = `${BRANCH_PREFIX}/${slug(sessionId).slice(0, 8)}/${slug(name)}`;
	for (let attempt = 1; attempt < 1000; attempt += 1) {
		const candidate = attempt === 1 ? stem : `${stem}-${attempt}`;
		const valid = await git(top, ["check-ref-format", "--branch", candidate]);
		if (valid.code !== 0) throw new WorktreeError(`cannot make a branch name from ${JSON.stringify(name)}`);
		if (!(await branchExists(top, candidate))) return candidate;
	}
	throw new WorktreeError(`no free branch name for ${name}`);
}

function relativeInside(root: string, path: string): string | undefined {
	const rel = relative(root, path);
	if (rel === "") return "";
	if (rel.startsWith("..") || isAbsolute(rel)) return undefined;
	return rel.split(sep).join("/");
}

function safeRelative(path: string, what: string): string {
	const normalized = path.replace(/\\/g, "/").replace(/\/+$/g, "");
	if (!normalized || isAbsolute(normalized) || normalized.split("/").some((part) => part === ".." || part === ""))
		throw new WorktreeError(
			`worktree setup ${what} path ${JSON.stringify(path)} must be relative to the repository root`,
		);
	return normalized;
}

/** Gitignored entries of the parent's checkout: directories end with "/". */
async function ignoredEntries(top: string): Promise<string[]> {
	const result = await git(
		top,
		["ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--directory", "--no-empty-directory"],
		{ env: { GIT_OPTIONAL_LOCKS: "0" } },
	);
	if (result.code !== 0) return [];
	return result.stdout.split("\0").filter(Boolean);
}

/**
 * Link a `node_modules` directory entry by entry: each package links to the parent's copy, except links that point
 * back into the repository (workspace packages), which point to the same place in the worktree.
 */
async function linkNodeModules(source: string, target: string, repo: string, worktree: string): Promise<void> {
	await mkdir(target, { recursive: true });
	for (const entry of await readdir(source)) {
		const from = join(source, entry);
		const to = join(target, entry);
		const info = await lstat(from).catch(() => undefined);
		if (info === undefined) continue;
		if (entry.startsWith("@") && info.isDirectory()) {
			await linkNodeModules(from, to, repo, worktree);
			continue;
		}
		let destination = from;
		if (info.isSymbolicLink()) {
			const resolved = await realpath(from).catch(() => undefined);
			const inside = resolved === undefined ? undefined : relativeInside(repo, resolved);
			if (inside !== undefined && !inside.split("/").includes("node_modules")) destination = join(worktree, inside);
		}
		await symlink(destination, to).catch(() => {});
	}
}

async function applySetup(
	top: string,
	worktree: string,
	setup: WorktreeSetup,
): Promise<{ linked: string[]; copied: string[] }> {
	const needsScan = setup.link === undefined || setup.copy === undefined;
	const ignored = needsScan ? await ignoredEntries(top) : [];
	const depth = (path: string) => path.replace(/\/$/, "").split("/").length;
	const links =
		setup.link !== undefined
			? setup.link.map((path) => safeRelative(path, "link"))
			: ignored
					.filter((entry) => entry.endsWith("/") && depth(entry) <= MAX_SETUP_DEPTH)
					.map((entry) => entry.slice(0, -1))
					.filter((path) => DEFAULT_LINK_NAMES.includes(path.split("/").pop()!))
					.slice(0, MAX_SETUP_ENTRIES);
	const inLinked = (path: string) => links.some((link) => path === link || path.startsWith(`${link}/`));
	const copies =
		setup.copy !== undefined
			? setup.copy.map((path) => safeRelative(path, "copy"))
			: ignored
					.filter(
						(entry) =>
							!entry.endsWith("/") &&
							depth(entry) <= MAX_SETUP_DEPTH &&
							DEFAULT_COPY_PATTERN.test(entry.split("/").pop()!) &&
							!inLinked(entry),
					)
					.slice(0, MAX_SETUP_ENTRIES);
	const linked: string[] = [];
	for (const path of links) {
		const from = join(top, path);
		const to = join(worktree, path);
		const source = await stat(from).catch(() => undefined);
		if (source === undefined) {
			if (setup.link !== undefined) throw new WorktreeError(`worktree setup link ${path} does not exist in ${top}`);
			continue;
		}
		// A path the checkout already has (tracked content) is not replaced.
		if (await lstat(to).catch(() => undefined)) continue;
		await mkdir(dirname(to), { recursive: true });
		if (path.split("/").pop() === "node_modules" && source.isDirectory())
			await linkNodeModules(from, to, top, worktree);
		else if (source.isDirectory()) {
			// A real directory of links, not one link: an ignore pattern with a trailing slash (`.venv/`) matches only
			// directories, so a single link would show up as an untracked file.
			await mkdir(to, { recursive: true });
			for (const entry of await readdir(from)) await symlink(join(from, entry), join(to, entry)).catch(() => {});
		} else await symlink(from, to);
		linked.push(path);
	}
	const copied: string[] = [];
	for (const path of copies) {
		const from = join(top, path);
		const to = join(worktree, path);
		const source = await stat(from).catch(() => undefined);
		if (source === undefined || !source.isFile()) {
			if (setup.copy !== undefined) throw new WorktreeError(`worktree setup copy ${path} is not a file in ${top}`);
			continue;
		}
		if (setup.copy === undefined && source.size > MAX_COPY_BYTES) continue;
		if (await lstat(to).catch(() => undefined)) continue;
		await mkdir(dirname(to), { recursive: true });
		await copyFile(from, to);
		copied.push(path);
	}
	return { linked, copied };
}

export interface CreateWorktreeOptions {
	/** The parent's working directory: the worktree branches from its repository's working tree. */
	readonly cwd: string;
	readonly sessionId: string;
	readonly name: string;
	readonly setup?: WorktreeSetup;
	readonly signal?: AbortSignal;
}

/** Worktree creation per repository, one at a time: branch names are picked and taken without a race. */
const creating = new Map<string, Promise<unknown>>();

/** Create a child's worktree (see the module comment). Throws WorktreeError with Git's reason on failure. */
export async function createChildWorktree(options: CreateWorktreeOptions): Promise<WorktreeRecord> {
	const info = await repoInfo(options.cwd);
	const previous = creating.get(info.commonDir) ?? Promise.resolve();
	const next = previous.catch(() => {}).then(() => createIn(info, options));
	creating.set(info.commonDir, next);
	try {
		return await next;
	} finally {
		if (creating.get(info.commonDir) === next) creating.delete(info.commonDir);
	}
}

async function createIn(info: RepoInfo, options: CreateWorktreeOptions): Promise<WorktreeRecord> {
	if (info.head === undefined)
		throw new WorktreeError(
			`worktree=True needs a commit to branch from, and the repository at ${info.top} has none yet`,
			true,
		);
	const snapshot = await snapshotWorkingTree(
		info.top,
		info.head,
		`ultron: snapshot of the working tree for sub-agent ${options.name}`,
		options.signal,
	);
	const branch = await uniqueBranch(info.top, options.sessionId, options.name);
	const id = `${slug(options.sessionId).slice(0, 8)}-${slug(options.name).slice(0, 24)}-${randomBytes(3).toString("hex")}`;
	const path = join(recordsDir(info.commonDir), id);
	const where = relativeInside(info.top, await realpath(options.cwd).catch(() => resolve(options.cwd))) ?? "";
	const record: WorktreeRecord = {
		version: 1,
		id,
		sessionId: options.sessionId,
		name: options.name,
		owner: { pid: process.pid, host: hostname() },
		repo: info.top,
		commonDir: info.commonDir,
		path,
		cwd: where === "" ? path : join(path, where),
		branch,
		head: info.head,
		base: snapshot.commit,
		dirty: snapshot.dirty,
		linked: [],
		copied: [],
		...(options.setup?.command?.trim() ? { command: options.setup.command.trim() } : {}),
		...(options.setup?.timeoutMs === undefined ? {} : { commandTimeoutMs: options.setup.timeoutMs }),
		createdAt: Date.now(),
		state: "creating",
	};
	// Recorded first: a crash in the middle leaves something the next session can prune.
	await saveRecord(record);
	try {
		await gitOk(info.top, [...QUIET, "worktree", "add", "--quiet", "-b", branch, path, snapshot.commit], {
			signal: options.signal,
		});
		await mkdir(record.cwd, { recursive: true });
		const setup = await applySetup(info.top, path, options.setup ?? {});
		record.linked.push(...setup.linked);
		record.copied.push(...setup.copied);
		record.state = "active";
		await saveRecord(record);
		return record;
	} catch (error) {
		await removeChildWorktree(record, { deleteBranch: true, force: true }).catch(() => {});
		throw error instanceof WorktreeError
			? error
			: new WorktreeError(`could not create a worktree: ${error instanceof Error ? error.message : String(error)}`);
	}
}

/** Run the setup command (`worktree_setup={"command": ...}`) in the child's working directory. */
export async function runSetupCommand(record: WorktreeRecord, signal?: AbortSignal): Promise<void> {
	if (!record.command) return;
	const shell = process.platform === "win32" ? "cmd.exe" : "/bin/sh";
	const args = process.platform === "win32" ? ["/d", "/s", "/c", record.command] : ["-c", record.command];
	const result = await new Promise<{ code: number | null; output: string; timedOut: boolean }>((done) => {
		let output = "";
		let timedOut = false;
		const child = spawn(shell, args, {
			cwd: record.cwd,
			env: { ...process.env, ...worktreeEnv(record) },
			stdio: ["ignore", "pipe", "pipe"],
			detached: process.platform !== "win32",
			windowsHide: true,
		});
		const kill = () => {
			try {
				if (process.platform !== "win32" && child.pid !== undefined) process.kill(-child.pid, "SIGTERM");
				else child.kill("SIGTERM");
			} catch {
				// Already gone.
			}
		};
		const timer = setTimeout(() => {
			timedOut = true;
			kill();
		}, record.commandTimeoutMs ?? DEFAULT_SETUP_TIMEOUT_MS);
		timer.unref?.();
		signal?.addEventListener("abort", kill, { once: true });
		const collect = (chunk: Buffer) => {
			output = `${output}${chunk.toString("utf8")}`.slice(-4000);
		};
		child.stdout.on("data", collect);
		child.stderr.on("data", collect);
		child.on("error", (error) => {
			clearTimeout(timer);
			done({ code: null, output: error.message, timedOut });
		});
		child.on("close", (code) => {
			clearTimeout(timer);
			signal?.removeEventListener("abort", kill);
			done({ code, output, timedOut });
		});
	});
	signal?.throwIfAborted();
	if (result.code !== 0)
		throw new WorktreeError(
			`worktree setup command ${JSON.stringify(record.command)} ${result.timedOut ? "timed out" : `failed (exit ${result.code ?? "?"})`}: ${result.output.trim().slice(-1500)}`,
		);
}

/** Environment of every process working in a child's worktree (the kernel, bash, Claude Code children). */
export function worktreeEnv(record: Pick<WorktreeRecord, "path" | "repo" | "branch" | "cwd">): Record<string, string> {
	return {
		ULTRON_WORKTREE: record.path,
		ULTRON_PARENT_REPO: record.repo,
		ULTRON_WORKTREE_BRANCH: record.branch,
		PWD: record.cwd,
	};
}

/** The note added to a worktree child's brief. */
export function worktreeBriefNote(record: Pick<WorktreeRecord, "cwd" | "branch" | "repo">): string {
	return `[Worktree] You work in your own Git worktree ${record.cwd} (branch ${record.branch}): a private copy of your parent's checkout, uncommitted changes included. Use paths relative to it; read/edit/write map absolute paths under ${record.repo} into it, but shell commands do not, so never cd there. Your changes are committed on your branch when you finish and your parent merges them: do not commit, switch branches or push yourself.`;
}

// ---------------------------------------------------------------------------------------------------------------
// Commit

/** Paths (relative to the root) that differ from HEAD in a working tree, untracked ones included; undefined on error. */
export async function listDirty(root: string): Promise<string[] | undefined> {
	const status = await git(
		root,
		["status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames", "--ignore-submodules=all"],
		{ env: { GIT_OPTIONAL_LOCKS: "0" } },
	);
	if (status.code !== 0) return undefined;
	return status.stdout
		.split("\0")
		.filter((item) => item.length > 3)
		.map((item) => item.slice(3));
}

/** Paths changed in a worktree relative to HEAD, minus its setup links and copies. */
async function dirtyPaths(record: WorktreeRecord): Promise<string[]> {
	const setup = [...record.linked, ...record.copied];
	return ((await listDirty(record.path)) ?? []).filter(
		(path) => !setup.some((entry) => path === entry || path.startsWith(`${entry}/`)),
	);
}

export interface CommitOutcome {
	readonly commit: string | null;
	readonly changedFiles: string[];
	readonly diffstat: string;
}

/**
 * Commit what the child changed on its branch. `paths` (relative to the worktree root) limits the commit to them;
 * by default every change Git sees, minus the setup links and copies. Returns the branch's final commit (null when
 * it differs from the base in nothing), the files that differ from the base and a one-line diffstat.
 */
export async function commitChildWorktree(
	record: WorktreeRecord,
	options: { message: string; paths?: readonly string[] },
): Promise<CommitOutcome> {
	const setup = [...record.linked, ...record.copied];
	const wanted = (options.paths ?? (await dirtyPaths(record))).filter(
		(path) => !setup.some((entry) => path === entry || path.startsWith(`${entry}/`)),
	);
	if (wanted.length > 0) {
		// Ignored paths are refused by `git add`; they are never part of the child's work.
		await gitOk(
			record.path,
			[...QUIET, "add", "-A", "--ignore-errors", "--pathspec-from-file=-", "--pathspec-file-nul"],
			{
				input: `${wanted.join("\0")}\0`,
				env: { GIT_LITERAL_PATHSPECS: "1" },
			},
		).catch(async () => {
			// One bad path (ignored, vanished) fails the batch: add the rest one by one.
			for (const path of wanted)
				await git(record.path, [...QUIET, "add", "-A", "--", path], { env: { GIT_LITERAL_PATHSPECS: "1" } });
		});
		const staged = await git(record.path, ["diff", "--cached", "--quiet"]);
		if (staged.code === 1)
			await gitOk(record.path, [...QUIET, "commit", "-q", "--no-verify", "-m", options.message], {
				env: await identityEnv(record.path),
			});
	}
	const head = (await gitOk(record.path, ["rev-parse", "HEAD"])).trim();
	const same = await git(record.path, ["diff", "--quiet", record.base, head]);
	if (head === record.base || same.code === 0) return { commit: null, changedFiles: [], diffstat: "" };
	const names = await gitOk(record.path, ["diff", "--name-only", "-z", "--no-renames", record.base, head]);
	const stat = await gitOk(record.path, ["diff", "--shortstat", record.base, head]);
	return { commit: head, changedFiles: names.split("\0").filter(Boolean), diffstat: stat.trim() };
}

/** A commit message from a child's summary, with its name and task (no trailers). */
export function commitMessage(summary: string, name: string, taskId: string | undefined, status: string): string {
	const text = summary.trim() || `Sub-agent ${name} (${status})`;
	const [first = "", ...rest] = text.split("\n");
	const subject = first.length > 72 ? `${first.slice(0, 71)}…` : first;
	const body = [first.length > 72 ? first : "", ...rest].join("\n").trim();
	return [
		subject,
		...(body ? ["", body] : []),
		"",
		`Ultron sub-agent "${name}"${taskId ? ` (task ${taskId})` : ""}, ${status}.`,
	].join("\n");
}

// ---------------------------------------------------------------------------------------------------------------
// Merge

export interface ConflictHunk {
	/** First line of the conflict in the merged file (1-based). */
	readonly line: number;
	readonly ours: string;
	readonly theirs: string;
}

export interface ConflictFile {
	readonly path: string;
	readonly kind: string;
	readonly hunks: ConflictHunk[];
}

export type MergeOutcome =
	| { readonly status: "merged"; readonly files: string[]; readonly diffstat: string }
	| { readonly status: "empty" }
	| {
			readonly status: "conflict";
			readonly files: string[];
			readonly conflicts: ConflictFile[];
			/** Conflict markers were written into the working tree (`on_conflict="markers"`). */
			readonly markers: boolean;
			readonly message: string;
	  };

type TreeChange = { srcMode: string; dstMode: string; srcOid: string; dstOid: string; status: string; path: string };

async function treeChanges(top: string, from: string, to: string): Promise<TreeChange[]> {
	const raw = await gitOk(top, ["diff-tree", "-r", "-z", "--no-renames", from, to]);
	const parts = raw.split("\0");
	const changes: TreeChange[] = [];
	for (let index = 0; index + 1 < parts.length; index += 2) {
		const meta = parts[index]!;
		const path = parts[index + 1]!;
		if (!meta.startsWith(":")) break;
		const [srcMode, dstMode, srcOid, dstOid, status] = meta.slice(1).split(" ");
		changes.push({ srcMode: srcMode!, dstMode: dstMode!, srcOid: srcOid!, dstOid: dstOid!, status: status!, path });
	}
	return changes;
}

const ZERO_MODE = "000000";

/** Paths whose working-tree content no longer matches `changes`' source side (something wrote there meanwhile). */
async function drifted(top: string, changes: readonly TreeChange[]): Promise<string[]> {
	const moved: string[] = [];
	const files: TreeChange[] = [];
	for (const change of changes) {
		if (change.srcMode === "160000" || change.dstMode === "160000") continue;
		const info = await lstat(join(top, change.path)).catch(() => undefined);
		if (change.srcMode === ZERO_MODE) {
			if (info !== undefined) moved.push(change.path);
			continue;
		}
		if (info === undefined) {
			moved.push(change.path);
			continue;
		}
		if (change.srcMode === "120000") {
			if (!info.isSymbolicLink()) moved.push(change.path);
			else {
				const target = await readlink(join(top, change.path));
				const oid = (await gitOk(top, ["hash-object", "--stdin"], { input: target })).trim();
				if (oid !== change.srcOid) moved.push(change.path);
			}
			continue;
		}
		files.push(change);
	}
	if (files.length > 0) {
		const oids = (
			await gitOk(top, ["hash-object", "--stdin-paths"], {
				input: `${files.map((change) => change.path).join("\n")}\n`,
			})
		)
			.trim()
			.split("\n");
		files.forEach((change, index) => {
			if (oids[index] !== change.srcOid) moved.push(change.path);
		});
	}
	return moved;
}

/** Write `to`'s side of `changes` into the working tree through a temporary index (the real index is untouched). */
async function writeChanges(top: string, tree: string, changes: readonly TreeChange[]): Promise<void> {
	const temporary = join(tmpdir(), `ultron-merge-index-${process.pid}-${randomBytes(6).toString("hex")}`);
	const env = { GIT_INDEX_FILE: temporary };
	try {
		const written = changes.filter((change) => change.dstMode !== ZERO_MODE && change.dstMode !== "160000");
		if (written.length > 0) {
			await gitOk(top, ["read-tree", tree], { env });
			// A path that is a directory now but a file in the result (or the reverse) is cleared first.
			for (const change of written) {
				const target = join(top, change.path);
				const info = await lstat(target).catch(() => undefined);
				if (info?.isDirectory()) await rm(target, { recursive: true, force: true });
			}
			await gitOk(top, [...QUIET, "checkout-index", "-f", "-z", "--stdin"], {
				env,
				input: `${written.map((change) => change.path).join("\0")}\0`,
			});
		}
		for (const change of changes.filter((item) => item.dstMode === ZERO_MODE)) {
			const target = join(top, change.path);
			await rm(target, { force: true });
			// Directories the deletion emptied go too, as a checkout would remove them.
			for (let dir = dirname(target); dir !== top && dir.startsWith(top); dir = dirname(dir))
				if (
					!(await rmdir(dir).then(
						() => true,
						() => false,
					))
				)
					break;
		}
	} finally {
		await rm(temporary, { force: true }).catch(() => {});
		await rm(`${temporary}.lock`, { force: true }).catch(() => {});
	}
}

function clip(text: string): string {
	return text.length > HUNK_TEXT_CHARS ? `${text.slice(0, HUNK_TEXT_CHARS - 1)}…` : text;
}

/** The conflict regions of a merged file with markers. */
export function conflictHunks(text: string): ConflictHunk[] {
	const hunks: ConflictHunk[] = [];
	const lines = text.split("\n");
	for (let index = 0; index < lines.length && hunks.length < MAX_HUNKS_PER_FILE; index += 1) {
		if (!lines[index]!.startsWith("<<<<<<< ")) continue;
		const start = index;
		const ours: string[] = [];
		const theirs: string[] = [];
		let side: "ours" | "base" | "theirs" = "ours";
		for (index += 1; index < lines.length; index += 1) {
			const line = lines[index]!;
			if (line.startsWith("||||||| ")) side = "base";
			else if (line === "=======") side = "theirs";
			else if (line.startsWith(">>>>>>> ")) break;
			else if (side === "ours") ours.push(line);
			else if (side === "theirs") theirs.push(line);
		}
		hunks.push({ line: start + 1, ours: clip(ours.join("\n")), theirs: clip(theirs.join("\n")) });
	}
	return hunks;
}

/** Parse `git merge-tree --write-tree -z --name-only` output: the tree, conflicted paths and messages. */
function parseMergeTree(stdout: string): {
	tree: string;
	paths: string[];
	messages: Array<{ paths: string[]; kind: string; text: string }>;
} {
	const parts = stdout.split("\0");
	const tree = parts[0]!.trim();
	let index = 1;
	const paths: string[] = [];
	while (index < parts.length && parts[index] !== "") paths.push(parts[index++]!);
	index += 1;
	const messages: Array<{ paths: string[]; kind: string; text: string }> = [];
	while (index < parts.length) {
		const count = Number(parts[index]);
		if (!Number.isSafeInteger(count) || count < 0) break;
		const own = parts.slice(index + 1, index + 1 + count);
		const kind = parts[index + 1 + count] ?? "";
		const text = (parts[index + 2 + count] ?? "").trim();
		messages.push({ paths: own, kind, text });
		index += 3 + count;
	}
	return { tree, paths: [...new Set(paths)], messages };
}

export interface MergeSource {
	readonly name: string;
	readonly branch: string;
	readonly base: string;
	readonly commit: string | null;
}

/**
 * Merge a child's branch into the working tree at `targetCwd` as uncommitted changes (see the module comment).
 * `onConflict: "markers"` writes the conflicted result with conflict markers instead of leaving the tree as it was.
 */
export async function mergeChildWorktree(
	targetCwd: string,
	source: MergeSource,
	options: { onConflict?: "stop" | "markers"; signal?: AbortSignal } = {},
): Promise<MergeOutcome> {
	if (source.commit === null) return { status: "empty" };
	const target = await repoInfo(targetCwd);
	if (target.head === undefined)
		throw new WorktreeError(`the repository at ${target.top} has no commits to merge into`);
	const exists = await git(target.top, ["cat-file", "-e", `${source.commit}^{commit}`]);
	if (exists.code !== 0)
		throw new WorktreeError(
			`${source.name}'s commit ${source.commit.slice(0, 12)} is not in the repository at ${target.top}`,
		);
	const ours = await snapshotWorkingTree(
		target.top,
		target.head,
		`ultron: working tree before merging sub-agent ${source.name}`,
		options.signal,
	);
	const merged = await git(target.top, [
		"merge-tree",
		"--write-tree",
		"-z",
		"--name-only",
		`--merge-base=${source.base}`,
		ours.commit,
		source.commit,
	]);
	if (merged.code !== 0 && merged.code !== 1)
		throw new WorktreeError(
			/unknown option|usage: git merge-tree/i.test(merged.stderr)
				? "rlm.merge needs Git 2.40 or newer (git merge-tree --write-tree --merge-base); update Git"
				: `git merge-tree failed: ${firstLines(merged.stderr) || `exit ${merged.code}`}`,
		);
	const parsed = parseMergeTree(merged.stdout);
	const changes = await treeChanges(target.top, ours.commit, parsed.tree);
	if (changes.length === 0 && merged.code === 0) return { status: "empty" };
	if (merged.code === 1) {
		const conflicts: ConflictFile[] = [];
		for (const path of parsed.paths.slice(0, MAX_CONFLICT_FILES)) {
			const blob = await git(target.top, ["cat-file", "blob", `${parsed.tree}:${path}`]);
			const kind =
				parsed.messages.find((message) => message.paths.includes(path) && message.kind.startsWith("CONFLICT"))
					?.kind ?? "CONFLICT";
			conflicts.push({ path, kind, hunks: blob.code === 0 ? conflictHunks(blob.stdout) : [] });
		}
		const message = parsed.messages
			.filter((item) => item.kind.startsWith("CONFLICT"))
			.map((item) => item.text)
			.join("\n");
		if (options.onConflict !== "markers")
			return { status: "conflict", files: parsed.paths, conflicts, markers: false, message };
		const moved = await drifted(target.top, changes);
		if (moved.length > 0)
			return {
				status: "conflict",
				files: moved,
				conflicts: moved.map((path) => ({ path, kind: "CONFLICT (changed during the merge)", hunks: [] })),
				markers: false,
				message: `files changed in the working tree while merging: ${moved.join(", ")}`,
			};
		await writeChanges(target.top, parsed.tree, changes);
		// Readable labels instead of commit ids on the markers.
		for (const path of parsed.paths) {
			const file = join(target.top, path);
			const text = await readFile(file, "utf8").catch(() => undefined);
			if (text === undefined) continue;
			await writeFile(
				file,
				text
					.split(`<<<<<<< ${ours.commit}`)
					.join("<<<<<<< working tree")
					.split(`>>>>>>> ${source.commit}`)
					.join(`>>>>>>> ${source.branch}`),
			);
		}
		return { status: "conflict", files: parsed.paths, conflicts, markers: true, message };
	}
	const moved = await drifted(target.top, changes);
	if (moved.length > 0)
		return {
			status: "conflict",
			files: moved,
			conflicts: moved.map((path) => ({ path, kind: "CONFLICT (changed during the merge)", hunks: [] })),
			markers: false,
			message: `files changed in the working tree while merging: ${moved.join(", ")}`,
		};
	await writeChanges(target.top, parsed.tree, changes);
	const stat = await git(target.top, ["diff", "--shortstat", ours.commit, parsed.tree]);
	return { status: "merged", files: changes.map((change) => change.path), diffstat: stat.stdout.trim() };
}

// ---------------------------------------------------------------------------------------------------------------
// Remove and prune

/** Remove a worktree's directory (its setup links first, never what they point to) and, optionally, its branch. */
export async function removeChildWorktree(
	record: WorktreeRecord,
	options: { deleteBranch: boolean; force?: boolean },
): Promise<void> {
	for (const path of [...record.linked, ...record.copied]) {
		const target = join(record.path, path);
		const info = await lstat(target).catch(() => undefined);
		if (info === undefined) continue;
		if (info.isSymbolicLink() || info.isFile()) await unlink(target).catch(() => {});
		else if (info.isDirectory()) {
			// A node_modules linked entry by entry: its links and scope folders only.
			await unlinkTree(target);
		}
	}
	const cwd = (await stat(record.repo).catch(() => undefined)) ? record.repo : record.commonDir;
	const gitDirArgs = cwd === record.commonDir ? [`--git-dir=${record.commonDir}`] : [];
	await git(cwd, [...gitDirArgs, ...QUIET, "worktree", "remove", "--force", "--force", record.path]);
	await rm(record.path, { recursive: true, force: true }).catch(() => {});
	await git(cwd, [...gitDirArgs, "worktree", "prune"]);
	if (options.deleteBranch) await git(cwd, [...gitDirArgs, "branch", "-D", "--", record.branch]);
	await deleteRecord(record);
}

async function unlinkTree(dir: string): Promise<void> {
	for (const entry of await readdir(dir).catch(() => [] as string[])) {
		const path = join(dir, entry);
		const info = await lstat(path).catch(() => undefined);
		if (info?.isDirectory()) await unlinkTree(path);
		else await unlink(path).catch(() => {});
	}
	await rmdir(dir).catch(() => {});
}

/** Whether a branch holds work beyond the base it started at. */
async function branchHasWork(record: WorktreeRecord): Promise<boolean> {
	const cwd = (await stat(record.repo).catch(() => undefined)) ? record.repo : record.commonDir;
	const tip = await git(cwd, ["rev-parse", "--verify", "-q", `refs/heads/${record.branch}`]);
	if (tip.code !== 0) return false;
	const same = await git(cwd, ["diff", "--quiet", record.base, tip.stdout.trim()]);
	return same.code !== 0;
}

export interface PruneReport {
	/** Worktree directories removed. */
	readonly removed: string[];
	/** Branches kept because they hold work (merge or delete them yourself). */
	readonly keptBranches: string[];
}

/**
 * Remove the worktrees of owners that are gone (a crashed or ended session): uncommitted work is committed on the
 * branch first, the directory is removed, and the branch is deleted unless it holds work. Then `git worktree prune`.
 * `select` limits it further (for example to one session's records); `force` also takes records whose owner lives.
 */
export async function pruneWorktrees(
	cwd: string,
	options: { select?: (record: WorktreeRecord) => boolean; force?: boolean; deleteBranches?: boolean } = {},
): Promise<PruneReport> {
	let info: RepoInfo;
	try {
		info = await repoInfo(cwd);
	} catch {
		return { removed: [], keptBranches: [] };
	}
	const removed: string[] = [];
	const keptBranches: string[] = [];
	const records = await readRecords(info.commonDir);
	for (const record of records) {
		if (options.select && !options.select(record)) continue;
		if (!options.force && ownerAlive(record.owner)) continue;
		if (await stat(record.path).catch(() => undefined)) {
			// Work the owner never committed (it stopped mid-run) is kept on the branch.
			if (record.state === "active" || record.state === "creating")
				await commitChildWorktree(record, {
					message: commitMessage(
						`Uncommitted work of sub-agent ${record.name}, saved when its worktree was pruned`,
						record.name,
						record.taskId,
						"interrupted",
					),
				}).catch(() => undefined);
		}
		const keep = !options.deleteBranches && (await branchHasWork(record));
		await removeChildWorktree(record, { deleteBranch: !keep, force: true }).catch(() => {});
		removed.push(record.path);
		if (keep) keptBranches.push(record.branch);
	}
	// Directories of ours with no record (a torn record) are leftovers too.
	if (!options.select) {
		const known = new Set(records.map((record) => record.id));
		for (const entry of await readdir(recordsDir(info.commonDir)).catch(() => [] as string[])) {
			if (entry.endsWith(".json") || known.has(entry)) continue;
			const path = join(recordsDir(info.commonDir), entry);
			await git(info.top, [...QUIET, "worktree", "remove", "--force", "--force", path]);
			await rm(path, { recursive: true, force: true }).catch(() => {});
			removed.push(path);
		}
	}
	await git(info.top, ["worktree", "prune"]);
	return { removed, keptBranches };
}
