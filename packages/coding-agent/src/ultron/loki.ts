/**
 * Ultron's built-in Loki guardrails (https://github.com/alfredosdpiii/loki, MIT): deterministic checks on every file
 * the agent writes. This module is the only Loki-specific code; it plugs into the generic file hooks (file-hooks.ts).
 *
 * At session start, in a Git repository without `.loki/`, it creates only `.loki/` (the bundled engine and the
 * default policy, via `loki init --minimal`) and commits exactly that directory, so Loki reads a committed policy.
 * Then it checks writes:
 * - before `edit()`/`write()` change a file: `protect --file <path> --preview ultron` on the proposed content; a
 *   finding blocks the write (or, with ULTRON_LOKI=advise, is only reported);
 * - after each cell, in the background: `hook --file ... --format json` on the files the cell changed (the
 *   post-write analyzers, and the only check for writes made through `bash` or `Path.write_text`). The model is told
 *   three different things, never mixed: new findings to fix, advisory lines as FYI, and (once per session) what
 *   could not be checked.
 * `context --host ultron` adds a short policy note to the system prompt.
 *
 * A repository's own `.loki/loki.py` (committed by its owners) is used when present and never modified; otherwise
 * the engine shipped with Ultron runs with the default policy.
 */
import { spawn } from "node:child_process";
import { constants as fsConstants } from "node:fs";
import { access, appendFile, mkdir, stat } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import type { LokiSettings } from "../core/settings-manager.ts";
import type {
	AfterCellReport,
	BeforeWriteVerdict,
	CellChanges,
	FileHookContext,
	FileWriteGuard,
	GuardRecord,
} from "./file-hooks.ts";
import { isUltronWorktreePath } from "./rlm/worktrees.ts";

export type LokiMode = "on" | "advise" | "off";

const OFF_VALUES = ["off", "0", "false", "no", "none"];
/** Loki needs Python 3.11 or newer. */
export const LOKI_MIN_PYTHON: readonly [number, number] = [3, 11];
export const DEFAULT_LOKI_TIMEOUT_MS = 5_000;
/** At most this many files go to one post-cell `hook` run; the rest are named in a note. */
const MAX_HOOK_FILES = 40;
const COMMIT_MESSAGE = "Add Loki guardrails";
const COMMIT_TIMEOUT_MS = 30_000;
const SETUP_TIMEOUT_MS = 30_000;

function isOff(value: string): boolean {
	return OFF_VALUES.includes(value.trim().toLowerCase());
}

/** ULTRON_LOKI (off | advise | on), else the `loki.mode` setting, else on. */
export function lokiMode(env: NodeJS.ProcessEnv, settings: LokiSettings = {}): LokiMode {
	const raw = env.ULTRON_LOKI?.trim().toLowerCase();
	if (raw) return isOff(raw) ? "off" : raw === "advise" ? "advise" : "on";
	return settings.mode ?? "on";
}

/** ULTRON_LOKI_AUTOINIT, else the `loki.autoInit` setting, else on. */
export function lokiAutoInit(env: NodeJS.ProcessEnv, settings: LokiSettings = {}): boolean {
	const raw = env.ULTRON_LOKI_AUTOINIT?.trim();
	return raw ? !isOff(raw) : settings.autoInit !== false;
}

/** ULTRON_LOKI_AUTOCOMMIT, else the `loki.autoCommit` setting, else on. */
export function lokiAutoCommit(env: NodeJS.ProcessEnv, settings: LokiSettings = {}): boolean {
	const raw = env.ULTRON_LOKI_AUTOCOMMIT?.trim();
	return raw ? !isOff(raw) : settings.autoCommit !== false;
}

/** ULTRON_LOKI_TIMEOUT_MS: how long a before-write check may take before the write proceeds unchecked. */
export function lokiTimeoutMs(env: NodeJS.ProcessEnv): number {
	const value = Number(env.ULTRON_LOKI_TIMEOUT_MS);
	return Number.isSafeInteger(value) && value > 0 ? value : DEFAULT_LOKI_TIMEOUT_MS;
}

// ---------------------------------------------------------------------------------------------------------------
// Processes

export interface RunResult {
	readonly status: number | null;
	readonly stdout: string;
	readonly stderr: string;
	/** Set when the process could not start, timed out or was aborted. */
	readonly error?: string;
}

/** Run a command without a shell or a terminal (stdin is the given input, or closed). */
export function runProcess(
	command: string,
	args: readonly string[],
	options: { cwd: string; input?: string; timeoutMs: number; signal?: AbortSignal; env?: NodeJS.ProcessEnv },
): Promise<RunResult> {
	return new Promise((done) => {
		let stdout = "";
		let stderr = "";
		let settled = false;
		let failure: string | undefined;
		// Its own process group, so a timeout also stops what it started (a hook, a signer).
		const group = process.platform !== "win32";
		const child = spawn(command, [...args], {
			cwd: options.cwd,
			env: options.env ?? process.env,
			stdio: [options.input === undefined ? "ignore" : "pipe", "pipe", "pipe"],
			windowsHide: true,
			detached: group,
		});
		const finish = (status: number | null) => {
			if (settled) return;
			settled = true;
			clearTimeout(timer);
			options.signal?.removeEventListener("abort", onAbort);
			done({ status, stdout, stderr, ...(failure === undefined ? {} : { error: failure }) });
		};
		const signal = (name: NodeJS.Signals) => {
			try {
				if (group && child.pid !== undefined) process.kill(-child.pid, name);
				else child.kill(name);
			} catch {
				// Already gone.
			}
		};
		const kill = (reason: string) => {
			if (failure !== undefined) return;
			failure = reason;
			// SIGTERM first: git removes its index.lock on SIGTERM, never on SIGKILL.
			signal("SIGTERM");
			setTimeout(() => {
				if (!settled) signal("SIGKILL");
			}, 2_000).unref?.();
		};
		const onAbort = () => kill("aborted");
		const timer = setTimeout(
			() => kill(`timed out after ${Math.round(options.timeoutMs / 100) / 10} s`),
			options.timeoutMs,
		);
		timer.unref?.();
		options.signal?.addEventListener("abort", onAbort, { once: true });
		if (options.signal?.aborted) onAbort();
		child.stdout?.setEncoding("utf8").on("data", (chunk: string) => {
			if (stdout.length < 1_000_000) stdout += chunk;
		});
		child.stderr?.setEncoding("utf8").on("data", (chunk: string) => {
			if (stderr.length < 1_000_000) stderr += chunk;
		});
		child.on("error", (error) => {
			failure ??= error.message;
			finish(null);
		});
		child.on("close", (code) => finish(code));
		if (options.input !== undefined) {
			child.stdin?.on("error", () => {});
			child.stdin?.end(options.input);
		}
	});
}

function git(root: string, args: readonly string[], timeoutMs = 10_000, env?: NodeJS.ProcessEnv): Promise<RunResult> {
	return runProcess("git", args, { cwd: root, timeoutMs, env: { ...(env ?? process.env), GIT_OPTIONAL_LOCKS: "0" } });
}

function firstLine(text: string): string {
	return (
		text
			.split("\n")
			.map((line) => line.trim())
			.find(Boolean) ?? ""
	);
}

// ---------------------------------------------------------------------------------------------------------------
// Python

export type PythonProbe = { readonly command: string; readonly version: string } | { readonly reason: string };

/** python3 (or ULTRON_LOKI_PYTHON) when it is 3.11 or newer. */
export async function findPython(env: NodeJS.ProcessEnv = process.env): Promise<PythonProbe> {
	const command = env.ULTRON_LOKI_PYTHON?.trim() || "python3";
	const result = await runProcess(command, ["-c", "import sys; print('%d.%d' % sys.version_info[:2])"], {
		cwd: homedir(),
		timeoutMs: 10_000,
	});
	const version = result.stdout.trim();
	if (result.error || result.status !== 0 || !/^\d+\.\d+$/.test(version))
		return { reason: `${command} was not found` };
	const [major, minor] = version.split(".").map(Number) as [number, number];
	if (major < LOKI_MIN_PYTHON[0] || (major === LOKI_MIN_PYTHON[0] && minor < LOKI_MIN_PYTHON[1]))
		return { reason: `${command} is ${version}; Loki needs ${LOKI_MIN_PYTHON.join(".")} or newer` };
	return { command, version };
}

/** Prints the interpreter's path, then 1 when it belongs to a virtual or conda environment (any Python 2.7+). */
const ENVIRONMENT_PROBE = [
	"import os, sys",
	"print(sys.executable)",
	"base = getattr(sys, 'base_prefix', sys.prefix)",
	"conda = os.path.isdir(os.path.join(sys.prefix, 'conda-meta'))",
	"print(int(base != sys.prefix or hasattr(sys, 'real_prefix') or conda))",
].join("\n");

/**
 * The interpreter the project runs on, which Loki asks whether a newly imported module exists (LOKI_PYTHON). Not the
 * interpreter Loki runs on (ULTRON_LOKI_PYTHON), whose packages are not the project's. In order:
 * - ULTRON_LOKI_PROJECT_PYTHON;
 * - the `python` (else `python3`) that `bash()` runs in the session directory, when it belongs to a virtual or conda
 *   environment. A bare system interpreter says nothing about the project's dependencies and is not used;
 * - `.venv` or `venv` in the session directory.
 * Otherwise undefined: Loki looks in the repository root itself and reports new imports as not checked when it
 * finds no interpreter. An inherited LOKI_PYTHON is left to the engine.
 */
export async function findProjectPython(
	cwd: string,
	env: NodeJS.ProcessEnv = process.env,
): Promise<string | undefined> {
	const named = env.ULTRON_LOKI_PROJECT_PYTHON?.trim();
	if (named) return named;
	if (env.LOKI_PYTHON?.trim()) return undefined;
	for (const command of ["python", "python3"]) {
		const result = await runProcess(command, ["-c", ENVIRONMENT_PROBE], { cwd, timeoutMs: 5_000, env });
		if (result.error || result.status !== 0) continue;
		const [executable, environment] = result.stdout.trim().split(/\r?\n/);
		// The first one found is what `python` means here, whichever kind it is.
		if (executable && isAbsolute(executable) && environment === "1") return executable;
		break;
	}
	for (const directory of [".venv", "venv"]) {
		for (const relativePath of ["bin/python", "bin/python3", "Scripts/python.exe"]) {
			const candidate = join(cwd, directory, relativePath);
			if (
				await stat(candidate).then(
					(info) => info.isFile(),
					() => false,
				)
			)
				return candidate;
		}
	}
	return undefined;
}

// ---------------------------------------------------------------------------------------------------------------
// What a check reported

/** One `hook` run's findings by tier. Only `blocking` fails the check. */
export interface LokiReport {
	readonly blocking: readonly string[];
	readonly advisory: readonly string[];
	/** `NOT CHECKED ...` notes: an analyzer or an interpreter is missing. */
	readonly notChecked: readonly string[];
	/** The engine could not run the check (invalid input or policy). */
	readonly error?: string;
}

function texts(value: unknown): string[] {
	if (!Array.isArray(value)) return [];
	return value
		.map((item: unknown) =>
			typeof item === "string"
				? item
				: typeof item === "object" && item !== null && "text" in item && typeof item.text === "string"
					? item.text
					: "",
		)
		.filter((text) => text.trim() !== "");
}

/** The report an engine prints on stdout for `--format json`; undefined when stdout is not one. */
export function parseLokiReport(stdout: string): LokiReport | undefined {
	let parsed: unknown;
	try {
		parsed = JSON.parse(stdout);
	} catch {
		return undefined;
	}
	if (typeof parsed !== "object" || parsed === null || !("status" in parsed) || !("blocking" in parsed))
		return undefined;
	const error = "error" in parsed && typeof parsed.error === "string" ? parsed.error : undefined;
	return {
		blocking: texts(parsed.blocking),
		advisory: texts("advisory" in parsed ? parsed.advisory : []),
		notChecked: texts("not_checked" in parsed ? parsed.not_checked : []),
		...(error === undefined ? {} : { error }),
	};
}

const ADVISORY_LABEL = "loki: advisory (not blocking):";
const ADVISORY_LINE = /: loki\/slop-[\w-]+: |\[Warning\/|^oxlint advisory \(not blocking\)/;
const ENGINE_ERROR = /^loki: (invalid hook input|guard error)/;

/**
 * The same tiers from the text an engine without `--format json` prints on stderr (a repository's older
 * `.loki/loki.py`): `NOT CHECKED` lines, structural-sloppiness and analyzer-warning lines as advisory, and when the
 * check failed everything else as blocking.
 */
export function classifyLokiText(stderr: string, failed: boolean): LokiReport {
	const blocking: string[] = [];
	const advisory: string[] = [];
	const notChecked: string[] = [];
	let labelled = false;
	for (const raw of stderr.split("\n")) {
		const line = raw.trim();
		if (!line || line.startsWith("loki: post-write check failed")) continue;
		if (failed && ENGINE_ERROR.test(line)) return { blocking: [], advisory: [], notChecked: [], error: line };
		if (line === ADVISORY_LABEL) labelled = true;
		else if (line.startsWith("NOT CHECKED")) notChecked.push(line);
		else if (!failed || labelled || ADVISORY_LINE.test(line)) advisory.push(line);
		else blocking.push(line);
	}
	return { blocking, advisory, notChecked };
}

const FINDING_PATH = /^([^:\s][^:\n]*?)(?::\d+)?: /;

/** A finding's file and its identity without the line number, which moves as the file is edited. */
function findingKey(line: string): { path: string; key: string } {
	const match = FINDING_PATH.exec(line);
	return match ? { path: match[1]!, key: `${match[1]}: ${line.slice(match[0].length)}` } : { path: "", key: line };
}

function count(n: number, noun: string): string {
	return `${n} ${noun}${n === 1 ? "" : "s"}`;
}

// ---------------------------------------------------------------------------------------------------------------
// Where `.loki/` may be created and committed

/** The Git work tree containing `cwd`, or undefined outside one. */
export async function gitRoot(cwd: string): Promise<string | undefined> {
	const result = await git(cwd, ["rev-parse", "--show-toplevel"]);
	const root = result.stdout.trim();
	return result.status === 0 && root ? resolve(root) : undefined;
}

function expandHome(path: string, home: string): string {
	return path === "~" ? home : path.startsWith("~/") ? join(home, path.slice(2)) : path;
}

export interface EligibilityOptions {
	readonly env: NodeJS.ProcessEnv;
	readonly settings: LokiSettings;
	/** The user's home directory (tests). */
	readonly home?: string;
}

/**
 * Why `.loki/` must not be created for `cwd`, or `{ root }` when it may be. Skipped: auto-install off, CI, outside
 * Git, a repository rooted at the home directory or `/`, anything under node_modules, a listed repository, a
 * read-only checkout.
 */
export async function autoInitEligibility(
	cwd: string,
	options: EligibilityOptions,
): Promise<{ readonly root: string } | { readonly reason: string }> {
	if (!lokiAutoInit(options.env, options.settings))
		return {
			reason: options.env.ULTRON_LOKI_AUTOINIT?.trim()
				? "auto-install is off (ULTRON_LOKI_AUTOINIT)"
				: "auto-install is off (loki.autoInit setting)",
		};
	if (options.env.CI?.trim()) return { reason: "CI is set" };
	const root = await gitRoot(cwd);
	if (root === undefined) return { reason: "not a Git repository" };
	// A sub-agent's worktree has its parent's checkout (`.loki/` included, if the parent has it); its branch must
	// hold only the sub-agent's own work.
	if (isUltronWorktreePath(root, options.env)) return { reason: "this is an Ultron sub-agent worktree" };
	const home = resolve(options.home ?? homedir());
	if (root === home) return { reason: "the repository root is the home directory" };
	if (root === resolve("/") || dirname(root) === root) return { reason: "the repository root is the filesystem root" };
	if (root.split(sep).includes("node_modules")) return { reason: "the repository is inside node_modules" };
	const ignored = (options.settings.ignoreRepos ?? []).map((path) => resolve(expandHome(path, home)));
	if (ignored.includes(root)) return { reason: "the repository is listed in loki.ignoreRepos" };
	try {
		await access(root, fsConstants.W_OK);
	} catch {
		return { reason: "the checkout is read-only" };
	}
	return { root };
}

/** Git operations whose state files mean a commit now would be part of them. */
const IN_PROGRESS: readonly [string, string][] = [
	["MERGE_HEAD", "a merge is in progress"],
	["rebase-merge", "a rebase is in progress"],
	["rebase-apply", "a rebase or am is in progress"],
	["CHERRY_PICK_HEAD", "a cherry-pick is in progress"],
	["REVERT_HEAD", "a revert is in progress"],
	["BISECT_LOG", "a bisect is in progress"],
];

/** Why `.loki/` must not be committed now, or undefined when it may be. */
export async function autoCommitSkipReason(root: string, options: EligibilityOptions): Promise<string | undefined> {
	if (!lokiAutoCommit(options.env, options.settings))
		return options.env.ULTRON_LOKI_AUTOCOMMIT?.trim()
			? "auto-commit is off (ULTRON_LOKI_AUTOCOMMIT)"
			: "auto-commit is off (loki.autoCommit setting)";
	if (options.env.CI?.trim()) return "CI is set";
	if (isUltronWorktreePath(root, options.env)) return "this is an Ultron sub-agent worktree";
	for (const [name, reason] of IN_PROGRESS) {
		const path = await git(root, ["rev-parse", "--git-path", name]);
		if (path.status !== 0) return "Git state could not be read";
		const resolved = resolve(root, path.stdout.trim());
		if (
			await stat(resolved).then(
				() => true,
				() => false,
			)
		)
			return reason;
	}
	if ((await git(root, ["symbolic-ref", "-q", "HEAD"])).status !== 0) return "HEAD is detached";
	if ((await git(root, ["check-ignore", "-q", "--", ".loki/loki.py"])).status === 0) return ".loki is gitignored";
	return undefined;
}

export type CommitOutcome = { readonly sha: string } | { readonly reason: string };

/**
 * Commit `.loki/` and nothing else: `git add .loki`, then a pathspec-limited `git commit -- .loki`, which leaves every
 * other staged and unstaged change exactly as it was. Hooks and signing run as configured (never `--no-verify`);
 * nothing can prompt (no terminal, no stdin) and the commit is bounded in time. When it fails, `.loki/` is unstaged
 * again and the reason is returned.
 */
export async function commitLoki(
	root: string,
	options: { env?: NodeJS.ProcessEnv; timeoutMs?: number } = {},
): Promise<CommitOutcome> {
	const env = {
		...(options.env ?? process.env),
		GIT_TERMINAL_PROMPT: "0",
		GIT_EDITOR: "true",
		GIT_SEQUENCE_EDITOR: "true",
		GIT_ASKPASS: "true",
	};
	const timeoutMs = options.timeoutMs ?? COMMIT_TIMEOUT_MS;
	const added = await git(root, ["add", "--", ".loki"], timeoutMs, env);
	if (added.status !== 0) return { reason: `git add failed: ${firstLine(added.stderr) || added.error || "unknown"}` };
	const committed = await runProcess("git", ["commit", "-q", "-m", COMMIT_MESSAGE, "--", ".loki"], {
		cwd: root,
		timeoutMs,
		env,
	});
	if (committed.status !== 0) {
		await git(root, ["rm", "--cached", "-r", "-q", "--ignore-unmatch", "--", ".loki"], 10_000, env);
		const detail = committed.error
			? `${committed.error}${committed.error.startsWith("timed out") ? " (a hook or a signing prompt?)" : ""}`
			: firstLine(committed.stderr) || firstLine(committed.stdout) || `exit status ${committed.status}`;
		return { reason: `git commit failed: ${detail}` };
	}
	const head = await git(root, ["rev-parse", "--short", "HEAD"]);
	return { sha: head.stdout.trim() };
}

// ---------------------------------------------------------------------------------------------------------------
// The guard

export interface LokiGuardOptions {
	/** The repository (or working directory) Loki checks. */
	readonly root: string;
	/** loki.py to run: the repository's `.loki/loki.py`, or the bundled engine. */
	readonly engine: string;
	readonly python: string;
	readonly mode: Exclude<LokiMode, "off">;
	readonly timeoutMs: number;
	/** `ultron`, or `pi` for an older repository engine (same write envelope). */
	readonly previewHost: "ultron" | "pi";
	/** Post-write checks compare against Git; outside a repository only before-write checks run. */
	readonly postWrite: boolean;
	/** The engine knows `hook --format json` (false for an older repository engine: its text is classified). */
	readonly jsonReports?: boolean;
	/** The project's interpreter (findProjectPython), passed to the engine as LOKI_PYTHON. */
	readonly projectPython?: string;
	/**
	 * The bundled engine, for a check the repository's older `.loki/loki.py` does not have (`shell-writes`); the
	 * repository's policy still decides, as `--root` points at it.
	 */
	readonly fallbackEngine?: string;
	readonly env?: NodeJS.ProcessEnv;
}

const PROTECTED_LOKI = /^\.loki\/\S*: protected content changed$/;

export class LokiGuard implements FileWriteGuard {
	readonly name = "Loki";
	readonly timeoutMs: number;
	readonly afterTimeoutMs = 120_000;
	readonly options: LokiGuardOptions;
	/** NOT CHECKED lines (a missing analyzer) are shown once per session, not after every write. */
	readonly #shownNotices = new Set<string>();
	/** Findings already shown and not fixed since, per file, so a later cell is not told again in full. */
	readonly #open = { blocking: new Map<string, Set<string>>(), advisory: new Map<string, Set<string>>() };
	#pausedShown = false;

	constructor(options: LokiGuardOptions) {
		this.options = options;
		this.timeoutMs = options.timeoutMs;
	}

	watchesCells(): boolean {
		return this.options.postWrite;
	}

	#args(root: string, ...args: string[]): string[] {
		return [this.options.engine, "--root", root, ...args];
	}

	/** The engine's environment: the session's, plus the project's interpreter when Ultron found one. */
	#env(): { env?: NodeJS.ProcessEnv } {
		const { env, projectPython } = this.options;
		if (projectPython === undefined) return env === undefined ? {} : { env };
		return { env: { ...(env ?? process.env), LOKI_PYTHON: projectPython } };
	}

	/**
	 * The repository Loki checks for a write: a worktree subagent's own worktree (the same repository, its policy
	 * committed there too), else the session's.
	 */
	#root(context: FileHookContext): string {
		return context.root ?? this.options.root;
	}

	/** The repository-relative path, or undefined outside the repository (Loki guards only the repository). */
	#inside(path: string, root: string = this.options.root): string | undefined {
		const rel = relative(root, path);
		if (!rel || rel.startsWith("..") || isAbsolute(rel)) return undefined;
		return rel.split(sep).join("/");
	}

	async beforeWrite(write: { path: string; content: string }, context: FileHookContext): Promise<BeforeWriteVerdict> {
		const root = this.#root(context);
		const rel = this.#inside(write.path, root);
		if (rel === undefined) return {};
		const result = await runProcess(
			this.options.python,
			this.#args(root, "protect", "--file", rel, "--preview", this.options.previewHost),
			{
				cwd: root,
				timeoutMs: this.timeoutMs + 1_000,
				signal: context.signal,
				input: JSON.stringify({
					cwd: context.cwd,
					tool_name: "write",
					tool_input: { path: write.path, content: write.content },
				}),
				...this.#env(),
			},
		);
		if (result.error) throw new Error(result.error);
		const text = result.stderr.trim();
		if (result.status === 0) {
			// Nothing blocks: advisory lines pass through, a missing check is the once-per-session note.
			const report = classifyLokiText(text, false);
			const notice = this.#notice(report.notChecked);
			const lines = notice === undefined ? report.advisory : [...report.advisory, notice];
			return lines.length > 0 ? { message: lines.join("\n") } : {};
		}
		const reason = text || `the checker failed (exit status ${result.status})`;
		if (this.options.mode === "advise") return { message: `would block this write (advise-only mode):\n${reason}` };
		return { block: true, reason };
	}

	/** The lines of one tier not yet shown for their file, and how many shown earlier are still there. */
	#unseen(tier: "blocking" | "advisory", lines: readonly string[], checked: readonly string[]) {
		const memory = this.#open[tier];
		const now = new Map<string, Set<string>>();
		const fresh: string[] = [];
		const stale = new Set<string>();
		for (const line of lines) {
			const { path, key } = findingKey(line);
			if (path !== "" && memory.get(path)?.has(key)) stale.add(path);
			else fresh.push(line);
			// A line that names no file cannot be followed from cell to cell.
			if (path !== "") now.set(path, (now.get(path) ?? new Set()).add(key));
		}
		// A finding that is gone from a file checked now was fixed; if it comes back it is new again.
		for (const path of checked) memory.delete(path);
		for (const [path, keys] of now) memory.set(path, keys);
		return { fresh, earlier: lines.length - fresh.length, paths: [...stale] };
	}

	/** NOT CHECKED notes not yet shown this session, as one short line. */
	#notice(lines: readonly string[]): string | undefined {
		const fresh = lines.filter((line) => !this.#shownNotices.has(line));
		for (const line of fresh) this.#shownNotices.add(line);
		if (fresh.length === 0) return undefined;
		const reasons = fresh.map((line) => line.replace(/^NOT CHECKED\s*/, ""));
		return `not checked in this session (said once): ${reasons.join("; ")}`;
	}

	/** A shell command that may write files: Loki refuses one that writes source or configuration past `beforeWrite`. */
	async beforeShell(command: string, context: FileHookContext): Promise<BeforeWriteVerdict> {
		const root = this.#root(context);
		const run = (engine: string) =>
			runProcess(this.options.python, [engine, "--root", root, "shell-writes", "--harness", "ultron"], {
				cwd: root,
				timeoutMs: this.timeoutMs + 1_000,
				signal: context.signal,
				input: JSON.stringify({ command, cwd: context.cwd }),
				...this.#env(),
			});
		const unknown = (text: string) => /invalid choice: 'shell-writes'/.test(text);
		let result = await run(this.options.engine);
		// A repository engine from before `shell-writes` does not know the command: ask the bundled one, else let it run.
		if (!result.error && result.status !== 0 && unknown(result.stderr) && this.options.fallbackEngine !== undefined)
			result = await run(this.options.fallbackEngine);
		if (result.error) throw new Error(result.error);
		if (result.status === 0) return {};
		const reason = result.stderr.trim() || `the checker failed (exit status ${result.status})`;
		if (unknown(reason)) return {};
		if (this.options.mode === "advise")
			return { message: `would refuse this command (advise-only mode):\n${reason}` };
		return { block: true, reason };
	}

	async afterCellChanges(changes: CellChanges, context: FileHookContext): Promise<AfterCellReport | undefined> {
		const root = this.#root(context);
		const targets = [...new Set([...changes.files, ...changes.checked])]
			.map((path) => this.#inside(path, root))
			.filter((path): path is string => path !== undefined);
		if (targets.length === 0) return undefined;
		const checked = targets.slice(0, MAX_HOOK_FILES);
		const json = this.options.jsonReports === true;
		const result = await runProcess(
			this.options.python,
			this.#args(
				root,
				"hook",
				...checked.flatMap((path) => ["--file", path]),
				...(json ? ["--format", "json"] : []),
			),
			{ cwd: root, timeoutMs: this.afterTimeoutMs, signal: context.signal, ...this.#env() },
		);
		if (result.error) throw new Error(result.error);
		const failed = result.status !== 0;
		const report = (json ? parseLokiReport(result.stdout) : undefined) ?? classifyLokiText(result.stderr, failed);
		if (report.error !== undefined) throw new Error(report.error);
		if (failed && report.blocking.length === 0)
			throw new Error(firstLine(result.stderr) || `the checker failed (exit status ${result.status})`);
		// Loki compares against the committed policy: while .loki/ itself has uncommitted changes (a new install whose
		// commit was skipped or refused) every post-write check reports that and stops. Say so once.
		if (report.blocking.length > 0 && report.blocking.every((line) => PROTECTED_LOKI.test(line.trim()))) {
			if (this.#pausedShown) return undefined;
			this.#pausedShown = true;
			return {
				outcome: "clean",
				message:
					"post-write checks are paused: .loki/ has uncommitted changes, and Loki checks against the committed policy. Before-write checks of edit() and write() still run. Commit .loki/ to resume them (git add .loki && git commit -m 'Add Loki guardrails' -- .loki).",
			};
		}
		const blocking = this.#unseen("blocking", report.blocking, checked);
		const advisory = this.#unseen("advisory", report.advisory, checked);
		const sections: string[] = [];
		if (blocking.fresh.length > 0)
			sections.push(
				`${count(blocking.fresh.length, "new finding")} from this cell's changes; fix these:\n${blocking.fresh.join("\n")}`,
			);
		if (blocking.earlier > 0)
			sections.push(
				`${count(blocking.earlier, "finding")} reported earlier ${blocking.earlier === 1 ? "is" : "are"} still open in ${blocking.paths.join(", ")}.`,
			);
		if (advisory.fresh.length > 0)
			sections.push(`FYI (advisory, not blocking; no change required):\n${advisory.fresh.join("\n")}`);
		const notice = this.#notice(report.notChecked);
		if (notice !== undefined) sections.push(notice);
		if (targets.length > checked.length)
			sections.push(`(${targets.length - checked.length} more changed files were not checked)`);
		const outcome = report.blocking.length > 0 ? "findings" : advisory.fresh.length > 0 ? "advisory" : "clean";
		return sections.length === 0 ? undefined : { outcome, message: sections.join("\n") };
	}
}

/**
 * Loki's policy note for the system prompt (`context --host ultron`), which preview host the engine knows (an older
 * repository engine rejects `--host ultron` and gets `pi`, whose write envelope is the same) and whether it names
 * `json` among its report formats.
 */
async function readContext(
	python: string,
	engine: string,
	root: string,
): Promise<{ previewHost: "ultron" | "pi"; jsonReports: boolean; context?: string }> {
	const run = (args: string[]) =>
		runProcess(python, [engine, "--root", root, ...args], { cwd: root, timeoutMs: DEFAULT_LOKI_TIMEOUT_MS });
	let previewHost: "ultron" | "pi" = "ultron";
	let result = await run(["context", "--host", "ultron"]);
	if (result.status === 2 && /invalid choice|unrecognized arguments/.test(result.stderr)) {
		previewHost = "pi";
		result = await run(["context"]);
	}
	if (result.status !== 0) return { previewHost, jsonReports: false };
	try {
		const parsed = JSON.parse(result.stdout) as {
			hookSpecificOutput?: { additionalContext?: unknown };
			loki?: { formats?: unknown };
		};
		const text = parsed.hookSpecificOutput?.additionalContext;
		const formats = parsed.loki?.formats;
		const jsonReports = Array.isArray(formats) && formats.includes("json");
		return typeof text === "string" && text.trim()
			? { previewHost, jsonReports, context: text.trim() }
			: { previewHost, jsonReports };
	} catch {
		return { previewHost, jsonReports: false };
	}
}

// ---------------------------------------------------------------------------------------------------------------
// Session setup

export interface LokiSetupOptions {
	readonly cwd: string;
	readonly env: NodeJS.ProcessEnv;
	readonly settings: LokiSettings;
	/** The engine shipped with Ultron (getBundledLokiPath), if this installation has one. */
	readonly bundledEngine: string | undefined;
	readonly home?: string;
	/** Records for ULTRON_LOKI_LOG. */
	readonly record?: (record: Record<string, unknown>) => void;
}

export interface LokiSetup {
	readonly mode: LokiMode;
	readonly guard?: LokiGuard;
	/** Short policy note for the system prompt. */
	readonly context?: string;
	/** The one-time transcript note (setup and commit outcome), once known; undefined when there is nothing to say. */
	readonly notice: Promise<string | undefined>;
}

/**
 * Prepare Loki for a session in `cwd`: find Python, auto-install and auto-commit `.loki/` where allowed, pick the
 * engine and read the policy note. Never throws; problems become the notice.
 */
export async function setupLoki(options: LokiSetupOptions): Promise<LokiSetup> {
	const mode = lokiMode(options.env, options.settings);
	if (mode === "off") return { mode, notice: Promise.resolve(undefined) };
	const started = Date.now();
	const python = await findPython(options.env);
	if ("reason" in python) {
		options.record?.({ phase: "setup", outcome: "off", reason: python.reason, ms: Date.now() - started });
		return { mode, notice: Promise.resolve(`Loki guardrails are off: ${python.reason}.`) };
	}
	const repository = await gitRoot(options.cwd);
	const root = repository ?? resolve(options.cwd);
	const repositoryEngine = join(root, ".loki", "loki.py");
	const exists = (path: string) =>
		stat(path).then(
			(info) => info.isFile(),
			() => false,
		);
	let notice: Promise<string | undefined> = Promise.resolve(undefined);
	if (!(await exists(repositoryEngine))) {
		const eligible = await autoInitEligibility(options.cwd, options);
		if ("root" in eligible && options.bundledEngine !== undefined) {
			const installed = await runProcess(
				python.command,
				[options.bundledEngine, "init", "--minimal", "--dir", eligible.root],
				{ cwd: eligible.root, timeoutMs: SETUP_TIMEOUT_MS },
			);
			if (installed.status !== 0) {
				const reason = firstLine(installed.stderr) || installed.error || `exit status ${installed.status}`;
				notice = Promise.resolve(`Loki could not create .loki/: ${reason}. It runs with its default policy.`);
			} else {
				const skip = await autoCommitSkipReason(eligible.root, options);
				notice =
					skip !== undefined
						? Promise.resolve(
								`Loki set up in .loki/ (not committed: ${skip}). Commit it to enable post-write checks: git add .loki && git commit -m "Add Loki guardrails" -- .loki`,
							)
						: commitLoki(eligible.root, { env: options.env }).then((outcome) => {
								options.record?.({ phase: "commit", ...outcome });
								return "sha" in outcome
									? `Loki set up and committed (${outcome.sha}): .loki/ holds the guardrail engine and policy.`
									: `Loki set up in .loki/ but not committed (${outcome.reason}). Before-write checks run with the default policy; commit .loki/ to enable post-write checks.`;
							});
			}
		}
	}
	const engine = (await exists(repositoryEngine)) ? repositoryEngine : options.bundledEngine;
	if (engine === undefined) {
		return {
			mode,
			notice: Promise.resolve("Loki guardrails are off: this installation has no bundled Loki engine."),
		};
	}
	// Outside Git only before-write checks run (post-write checks compare against a base revision); that is
	// documented rather than announced in every such session.
	const [{ previewHost, jsonReports, context }, projectPython] = await Promise.all([
		readContext(python.command, engine, root),
		findProjectPython(resolve(options.cwd), options.env),
	]);
	const guard = new LokiGuard({
		root,
		engine,
		python: python.command,
		mode,
		timeoutMs: lokiTimeoutMs(options.env),
		previewHost,
		postWrite: repository !== undefined,
		jsonReports,
		...(engine === repositoryEngine && options.bundledEngine !== undefined
			? { fallbackEngine: options.bundledEngine }
			: {}),
		...(projectPython === undefined ? {} : { projectPython }),
	});
	options.record?.({
		phase: "setup",
		outcome: "on",
		mode,
		engine: engine === repositoryEngine ? "repository" : "bundled",
		reports: jsonReports ? "json" : "text",
		projectPython: projectPython ?? null,
		ms: Date.now() - started,
	});
	return { mode, guard, ...(context === undefined ? {} : { context }), notice };
}

/** An appender for ULTRON_LOKI_LOG (JSON lines); undefined when unset. Errors are ignored. */
export function lokiLog(
	path: string | undefined,
): ((record: Record<string, unknown> | GuardRecord) => void) | undefined {
	if (!path?.trim()) return undefined;
	const target = resolve(path.trim());
	let ready: Promise<unknown> | undefined;
	return (record) => {
		ready ??= mkdir(dirname(target), { recursive: true }).catch(() => {});
		void ready.then(() => appendFile(target, `${JSON.stringify({ at: Date.now(), ...record })}\n`).catch(() => {}));
	};
}
