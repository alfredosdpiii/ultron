/**
 * Source for the verifier: a cached blob-less bare clone per repository (`<cache>/<host>/<owner>/<repo>.git`),
 * the pull request's head and base fetched into it, and a detached worktree at the head commit for each review,
 * removed afterwards. Private repositories authenticate through the child's environment (the account's token)
 * and `gh auth git-credential` as a per-command credential helper: nothing is written to any git config.
 */
import { randomBytes } from "node:crypto";
import { existsSync, mkdirSync, renameSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import type { PullRef } from "./github.ts";
import type { Runner, RunResult } from "./runner.ts";

const CLONE_TIMEOUT_MS = 5 * 60_000;
const FETCH_TIMEOUT_MS = 3 * 60_000;
const GIT_TIMEOUT_MS = 2 * 60_000;

/** The pull request's head is no longer the commit that was to be reviewed. */
export class StaleHeadError extends Error {}

export interface Checkout {
	/** A worktree at the head commit. */
	readonly workDir: string;
	/** The merge base of the base branch and the head: the pull request diff starts here. */
	readonly mergeBase: string;
	/** `git diff from to`, with the prefixes the diff parser expects. */
	diff(from: string, to: string): Promise<string>;
	/** Whether `ancestor` is in the clone and an ancestor of `descendant`. */
	isAncestor(ancestor: string, descendant: string): Promise<boolean>;
	cleanup(): Promise<void>;
}

const SAFE = /^[A-Za-z0-9._-]+$/;

/** Per-command git options: credentials from `gh`, no stored helper, no prompts. */
const CREDENTIALS = ["-c", "credential.helper=", "-c", "credential.helper=!gh auth git-credential"];

export class CheckoutManager {
	readonly #runner: Runner;
	readonly #cache: string;
	readonly #chains = new Map<string, Promise<unknown>>();

	constructor(runner: Runner, cacheDir: string) {
		this.#runner = runner;
		this.#cache = cacheDir;
	}

	repoDir(ref: PullRef): string {
		return join(this.#cache, ref.host, ref.owner, `${ref.repo}.git`);
	}

	/** Git operations on one cached clone run one at a time. */
	#serial<T>(key: string, work: () => Promise<T>): Promise<T> {
		const previous = this.#chains.get(key) ?? Promise.resolve();
		const next = previous.then(work, work);
		this.#chains.set(
			key,
			next.catch(() => {}),
		);
		return next;
	}

	async #git(args: readonly string[], env: Record<string, string>, timeoutMs = GIT_TIMEOUT_MS): Promise<RunResult> {
		return this.#runner(["git", ...CREDENTIALS, ...args], {
			env: { ...env, GIT_LFS_SKIP_SMUDGE: "1", GIT_OPTIONAL_LOCKS: "0" },
			timeoutMs,
		});
	}

	async #must(args: readonly string[], env: Record<string, string>, timeoutMs?: number): Promise<string> {
		const result = await this.#git(args, env, timeoutMs);
		if (result.code !== 0)
			throw new Error(
				`git ${args.find((arg) => !arg.startsWith("-") && arg !== this.#cache) ?? ""} failed: ${(result.stderr || result.stdout).trim().slice(0, 300)}`,
			);
		return result.stdout;
	}

	/** Clone or update the cached repository, fetch the pull request, and check out its head. */
	async prepare(
		ref: PullRef,
		pull: { headSha: string; baseRef: string; baseSha: string },
		env: Record<string, string>,
	): Promise<Checkout> {
		for (const part of [ref.host, ref.owner, ref.repo])
			if (!SAFE.test(part) || part === "." || part === "..") throw new Error(`unsafe repository name: ${part}`);
		const dir = this.repoDir(ref);
		const pullRefName = `refs/autoreview/pull/${ref.number}`;
		const baseRefName = `refs/autoreview/base/${ref.number}`;
		const workDir = join(
			this.#cache,
			"worktrees",
			`${ref.owner}-${ref.repo}-${ref.number}-${pull.headSha.slice(0, 7)}-${randomBytes(3).toString("hex")}`,
		);
		const mergeBase = await this.#serial(dir, async () => {
			if (!existsSync(join(dir, "HEAD"))) {
				mkdirSync(dirname(dir), { recursive: true, mode: 0o700 });
				const temporary = `${dir}.${process.pid}.${randomBytes(3).toString("hex")}.tmp`;
				try {
					await this.#must(
						[
							"clone",
							"--quiet",
							"--filter=blob:none",
							"--bare",
							`https://${ref.host}/${ref.owner}/${ref.repo}.git`,
							temporary,
						],
						env,
						CLONE_TIMEOUT_MS,
					);
					renameSync(temporary, dir);
				} finally {
					rmSync(temporary, { recursive: true, force: true });
				}
			}
			await this.#must(
				[
					"-C",
					dir,
					"fetch",
					"--quiet",
					"--no-tags",
					"--force",
					"origin",
					`+refs/pull/${ref.number}/head:${pullRefName}`,
					`+refs/heads/${pull.baseRef}:${baseRefName}`,
				],
				env,
				FETCH_TIMEOUT_MS,
			);
			const fetched = (await this.#must(["-C", dir, "rev-parse", pullRefName], env)).trim();
			if (fetched !== pull.headSha)
				throw new StaleHeadError(
					`the pull request head moved to ${fetched.slice(0, 7)} while it was being fetched`,
				);
			const base = await this.#git(["-C", dir, "merge-base", baseRefName, pull.headSha], env);
			mkdirSync(dirname(workDir), { recursive: true, mode: 0o700 });
			await this.#must(
				["-C", dir, "worktree", "add", "--quiet", "--detach", workDir, pull.headSha],
				env,
				FETCH_TIMEOUT_MS,
			);
			return base.code === 0 && base.stdout.trim() ? base.stdout.trim() : pull.baseSha;
		});
		return {
			workDir,
			mergeBase,
			diff: (from, to) =>
				this.#must(
					[
						"-C",
						dir,
						"-c",
						"diff.noprefix=false",
						"diff",
						"--no-color",
						"--no-ext-diff",
						"-M",
						"-U3",
						"--src-prefix=a/",
						"--dst-prefix=b/",
						from,
						to,
						"--",
					],
					env,
				),
			isAncestor: async (ancestor, descendant) =>
				/^[0-9a-f]{7,64}$/.test(ancestor) &&
				(await this.#git(["-C", dir, "merge-base", "--is-ancestor", ancestor, descendant], env)).code === 0,
			cleanup: () =>
				this.#serial(dir, async () => {
					await this.#git(["-C", dir, "worktree", "remove", "--force", workDir], env);
					rmSync(workDir, { recursive: true, force: true });
					await this.#git(["-C", dir, "worktree", "prune"], env);
				}),
		};
	}
}
