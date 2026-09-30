/**
 * Ultron's built-in Loki guardrails (https://github.com/alfredosdpiii/loki, MIT): deterministic checks on every file
 * the agent writes. This module is the only Loki-specific code; it plugs into the generic file hooks (file-hooks.ts).
 *
 * At session start, in a Git repository without `.loki/`, it creates only `.loki/` (the bundled engine and the
 * default policy, via `loki init --minimal`) and commits exactly that directory, so Loki reads a committed policy.
 * Then it checks writes:
 * - before `edit()`/`write()` change a file: `protect --file <path> --preview ultron` on the proposed content; a
 *   finding blocks the write (or, with ULTRON_LOKI=advise, is only reported);
 * - after each cell, in the background: `hook --file ...` on the files the cell changed (the post-write analyzers,
 *   and the only check for writes made through `bash` or `Path.write_text`).
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
import type { BeforeWriteVerdict, CellChanges, FileHookContext, FileWriteGuard, GuardRecord } from "./file-hooks.ts";
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

	/** Drop NOT CHECKED lines already shown this session. */
	#fresh(text: string): string {
		return text
			.split("\n")
			.filter((line) => {
				if (!line.startsWith("NOT CHECKED")) return true;
				if (this.#shownNotices.has(line)) return false;
				this.#shownNotices.add(line);
				return true;
			})
			.join("\n")
			.trim();
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
				...(this.options.env === undefined ? {} : { env: this.options.env }),
			},
		);
		if (result.error) throw new Error(result.error);
		const text = result.stderr.trim();
		if (result.status === 0) {
			const fresh = this.#fresh(text);
			return fresh ? { message: fresh } : {};
		}
		const reason = text || `the checker failed (exit status ${result.status})`;
		if (this.options.mode === "advise") return { message: `would block this write (advise-only mode):\n${reason}` };
		return { block: true, reason };
	}

	async afterCellChanges(changes: CellChanges, context: FileHookContext): Promise<string | undefined> {
		const root = this.#root(context);
		const targets = [...new Set([...changes.files, ...changes.checked])]
			.map((path) => this.#inside(path, root))
			.filter((path): path is string => path !== undefined);
		if (targets.length === 0) return undefined;
		const checked = targets.slice(0, MAX_HOOK_FILES);
		const result = await runProcess(
			this.options.python,
			this.#args(root, "hook", ...checked.flatMap((path) => ["--file", path])),
			{
				cwd: root,
				timeoutMs: this.afterTimeoutMs,
				signal: context.signal,
				...(this.options.env === undefined ? {} : { env: this.options.env }),
			},
		);
		if (result.error) throw new Error(result.error);
		const skipped =
			targets.length > checked.length
				? `\n(${targets.length - checked.length} more changed files were not checked)`
				: "";
		const text = result.stderr.trim();
		if (result.status === 0) {
			const fresh = this.#fresh(text);
			return fresh ? `${fresh}${skipped}` : skipped.trim() || undefined;
		}
		// Loki compares against the committed policy: while .loki/ itself has uncommitted changes (a new install whose
		// commit was skipped or refused) every post-write check reports that and stops. Say so once.
		const findings = text
			.split("\n")
			.filter((line) => line.trim() && !line.startsWith("loki: post-write check failed"));
		if (findings.length > 0 && findings.every((line) => PROTECTED_LOKI.test(line.trim()))) {
			if (this.#pausedShown) return undefined;
			this.#pausedShown = true;
			return "post-write checks are paused: .loki/ has uncommitted changes, and Loki checks against the committed policy. Before-write checks of edit() and write() still run. Commit .loki/ to resume them (git add .loki && git commit -m 'Add Loki guardrails' -- .loki).";
		}
		const label =
			changes.files.length > 0
				? `findings in files this cell changed (${changes.files.length} outside edit()/write()); fix them:`
				: "findings after this cell's writes; fix them:";
		return `${label}\n${this.#fresh(text) || text}${skipped}`;
	}
}

/**
 * Loki's policy note for the system prompt (`context --host ultron`), and which preview host the engine knows: an
 * older repository engine rejects `--host ultron` and gets `pi`, whose write envelope is the same.
 */
async function readContext(
	python: string,
	engine: string,
	root: string,
): Promise<{ previewHost: "ultron" | "pi"; context?: string }> {
	const run = (args: string[]) =>
		runProcess(python, [engine, "--root", root, ...args], { cwd: root, timeoutMs: DEFAULT_LOKI_TIMEOUT_MS });
	let previewHost: "ultron" | "pi" = "ultron";
	let result = await run(["context", "--host", "ultron"]);
	if (result.status === 2 && /invalid choice|unrecognized arguments/.test(result.stderr)) {
		previewHost = "pi";
		result = await run(["context"]);
	}
	if (result.status !== 0) return { previewHost };
	try {
		const parsed = JSON.parse(result.stdout) as { hookSpecificOutput?: { additionalContext?: unknown } };
		const text = parsed.hookSpecificOutput?.additionalContext;
		return typeof text === "string" && text.trim() ? { previewHost, context: text.trim() } : { previewHost };
	} catch {
		return { previewHost };
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
	const { previewHost, context } = await readContext(python.command, engine, root);
	const guard = new LokiGuard({
		root,
		engine,
		python: python.command,
		mode,
		timeoutMs: lokiTimeoutMs(options.env),
		previewHost,
		postWrite: repository !== undefined,
	});
	options.record?.({
		phase: "setup",
		outcome: "on",
		mode,
		engine: engine === repositoryEngine ? "repository" : "bundled",
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
