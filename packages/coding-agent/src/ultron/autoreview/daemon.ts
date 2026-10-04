/**
 * The autoreview loop: per account, discover pull requests that ask for a review (notifications, plus a search
 * fallback every few cycles so nothing is missed), queue them, and review up to `concurrency` at once. A pull
 * request is never reviewed twice at the same time by one account, rate limits pause the account they hit, and
 * a failed review is retried on a later cycle (three attempts per commit).
 */
import { appendFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { maskCellOutput } from "../rlm/output-secrets.ts";
import { type Account, accountKey } from "./accounts.ts";
import { GitHub, type PullRef, pullKey, RateLimitError } from "./github.ts";
import { type Candidate, type Outcome, type ReviewerDeps, reviewPull } from "./reviewer.ts";
import type { AccountState } from "./state.ts";

/** The search fallback runs on the first cycle and then every this many cycles. */
export const SEARCH_EVERY = 5;
const FIRST_POLL_WINDOW_MS = 24 * 60 * 60 * 1000;
const SEARCH_WINDOW_MS = 2 * 24 * 60 * 60 * 1000;
const SINCE_OVERLAP_MS = 2 * 60 * 1000;
const RETRY_DELAY_MS = 60_000;

export interface Discovered {
	readonly ref: PullRef;
	readonly reasons: string[];
}

/** Pull requests that may need a review by this account, and the account's next notification cursor. */
export async function discover(
	github: GitHub,
	state: AccountState,
	options: { now: number; search: boolean },
): Promise<{ pulls: Discovered[]; state: AccountState }> {
	const found = new Map<string, Discovered>();
	const add = (ref: PullRef, reason: string) => {
		const key = pullKey(ref);
		const entry = found.get(key) ?? { ref, reasons: [] };
		if (!entry.reasons.includes(reason)) entry.reasons.push(reason);
		found.set(key, entry);
	};
	const last = state.lastPollAt === undefined ? Number.NaN : Date.parse(state.lastPollAt);
	const since = new Date(
		Number.isFinite(last) ? last - SINCE_OVERLAP_MS : options.now - FIRST_POLL_WINDOW_MS,
	).toISOString();
	const page = await github.notifications({
		since,
		...(state.lastModified === undefined ? {} : { lastModified: state.lastModified }),
	});
	for (const pull of page.pulls) add(pull, pull.reason);
	if (options.search) {
		const login = github.account.login;
		const day = new Date(options.now - SEARCH_WINDOW_MS).toISOString().slice(0, 10);
		for (const ref of await github.search(`is:open is:pr review-requested:${login} archived:false`))
			add(ref, "review_requested");
		for (const ref of await github.search(`is:pr mentions:${login} updated:>=${day} archived:false`))
			add(ref, "search_mention");
		// Pull requests this account reviewed that moved on since.
		for (const ref of await github.search(`is:open is:pr reviewed-by:${login} updated:>=${day} archived:false`))
			add(ref, "search_reviewed");
	}
	const next: AccountState = { ...state, lastPollAt: new Date(options.now).toISOString() };
	if (page.lastModified !== undefined) next.lastModified = page.lastModified;
	if (page.pollInterval !== undefined) next.pollInterval = page.pollInterval;
	delete next.lastError;
	return { pulls: [...found.values()], state: next };
}

/** Lines go to stderr and to `<logs>/autoreview-<date>.log`, with known secrets masked. */
export function createLogger(
	logsDir: string | undefined,
	now: () => number = Date.now,
	write: (text: string) => void = (text) => void process.stderr.write(text),
): (line: string) => void {
	return (line) => {
		const stamp = new Date(now()).toISOString();
		const text = `${stamp} ${maskCellOutput(line)}\n`;
		write(text);
		if (logsDir === undefined) return;
		try {
			mkdirSync(logsDir, { recursive: true, mode: 0o700 });
			appendFileSync(join(logsDir, `autoreview-${stamp.slice(0, 10)}.log`), text, { mode: 0o600 });
		} catch {
			// Logging never stops a review.
		}
	};
}

type Queued = Candidate & { notBefore: number };

export interface DaemonDeps extends ReviewerDeps {
	readonly accounts: readonly Account[];
	readonly sleep?: (ms: number, signal?: AbortSignal) => Promise<void>;
	/** Told every finished review (tests, `once --json`). */
	readonly onOutcome?: (candidate: Candidate, outcome: Outcome | { kind: "failed"; error: string }) => void;
}

const defaultSleep = (ms: number, signal?: AbortSignal) =>
	new Promise<void>((resolve) => {
		const timer = setTimeout(resolve, ms);
		signal?.addEventListener(
			"abort",
			() => {
				clearTimeout(timer);
				resolve();
			},
			{ once: true },
		);
	});

export class Daemon {
	readonly #deps: DaemonDeps;
	readonly #queue = new Map<string, Queued>();
	readonly #running = new Map<string, Promise<void>>();
	readonly #paused = new Map<string, number>();
	#cycles = 0;
	#pollSeconds: number;

	constructor(deps: DaemonDeps) {
		this.#deps = deps;
		this.#pollSeconds = deps.config.pollSeconds;
	}

	#now(): number {
		return (this.#deps.now ?? Date.now)();
	}

	get queued(): number {
		return this.#queue.size;
	}

	get running(): number {
		return this.#running.size;
	}

	#key(account: Account, ref: PullRef): string {
		return `${accountKey(account)} ${pullKey(ref)}`;
	}

	#enqueue(candidate: Candidate, notBefore = 0): void {
		const key = this.#key(candidate.account, candidate.ref);
		const queued = this.#queue.get(key);
		if (queued) {
			// Keep the earliest pickup time; merge the reasons.
			this.#queue.set(key, {
				...queued,
				reasons: [...new Set([...queued.reasons, ...candidate.reasons])],
				notBefore: Math.min(queued.notBefore, notBefore),
			});
			return;
		}
		this.#queue.set(key, { ...candidate, notBefore });
	}

	/** Poll every account once and queue what was found. */
	async poll(): Promise<void> {
		const search = this.#cycles % SEARCH_EVERY === 0;
		this.#cycles += 1;
		for (const account of this.#deps.accounts) {
			const key = accountKey(account);
			const now = this.#now();
			if ((this.#paused.get(key) ?? 0) > now) continue;
			const github = new GitHub(this.#deps.runner, account, () => this.#deps.tokens.env(account), {
				now: () => this.#now(),
			});
			const saved = this.#deps.store.read().accounts[key] ?? {};
			try {
				const { pulls, state } = await discover(github, saved, { now, search });
				delete state.pausedUntil;
				await this.#deps.store.update((all) => {
					all.accounts[key] = state;
				});
				if (state.pollInterval !== undefined)
					this.#pollSeconds = Math.max(this.#deps.config.pollSeconds, state.pollInterval);
				for (const pull of pulls) this.#enqueue({ account, ref: pull.ref, reasons: pull.reasons, pickedAt: now });
			} catch (error) {
				await this.#failed(account, error);
			}
		}
	}

	async #failed(account: Account, error: unknown): Promise<void> {
		const key = accountKey(account);
		const message = error instanceof Error ? error.message : String(error);
		if (error instanceof RateLimitError) {
			this.#paused.set(key, error.resumeAt);
			this.#deps.log(`${key}: ${message}; paused until ${new Date(error.resumeAt).toISOString()}`);
		} else this.#deps.log(`${key}: ${message}`);
		await this.#deps.store
			.update((all) => {
				const state = all.accounts[key] ?? {};
				all.accounts[key] = state;
				state.lastError = message.slice(0, 300);
				if (error instanceof RateLimitError) state.pausedUntil = new Date(error.resumeAt).toISOString();
			})
			.catch(() => {});
	}

	/** Start queued reviews, up to the concurrency limit. */
	pump(): void {
		const now = this.#now();
		for (const [key, candidate] of this.#queue) {
			if (this.#running.size >= this.#deps.config.concurrency) break;
			if (this.#running.has(key) || candidate.notBefore > now) continue;
			if ((this.#paused.get(accountKey(candidate.account)) ?? 0) > now) continue;
			this.#queue.delete(key);
			const task = this.#review(candidate).finally(() => {
				this.#running.delete(key);
			});
			this.#running.set(key, task);
		}
	}

	async #review(candidate: Queued): Promise<void> {
		const name = `${pullKey(candidate.ref)} as ${candidate.account.login}`;
		try {
			const outcome = await reviewPull(this.#deps, candidate);
			if (outcome.kind === "skipped") this.#deps.log(`${name}: skipped (${outcome.reason})`);
			if (outcome.kind === "requeue") this.#enqueue({ ...candidate, pickedAt: this.#now() });
			this.#deps.onOutcome?.(candidate, outcome);
		} catch (error) {
			const message = error instanceof Error ? error.message : String(error);
			if (error instanceof RateLimitError) {
				await this.#failed(candidate.account, error);
				this.#enqueue(candidate, error.resumeAt);
			} else {
				this.#deps.log(`${name}: review failed: ${message}`);
				// Tried again on a later cycle; reviewPull gives up after three attempts on one commit.
				this.#enqueue(candidate, this.#now() + RETRY_DELAY_MS);
			}
			this.#deps.onOutcome?.(candidate, { kind: "failed", error: message });
		}
	}

	async #record(): Promise<void> {
		await this.#deps.store
			.update((all) => {
				all.queue = [...this.#queue.keys(), ...[...this.#running.keys()].map((key) => `${key} (running)`)];
			})
			.catch(() => {});
	}

	/** One cycle: poll, then review everything that is due and wait for it. */
	async once(): Promise<void> {
		await this.poll();
		for (;;) {
			this.pump();
			await this.#record();
			if (this.#running.size === 0) break;
			await Promise.race(this.#running.values());
		}
		await this.#record();
	}

	/** Poll and review until `signal` aborts; running reviews finish first. */
	async run(signal: AbortSignal): Promise<void> {
		while (!signal.aborted) {
			await this.poll();
			this.pump();
			await this.#record();
			// Reviews that finish free a slot: fill it without waiting for the next poll.
			const deadline = this.#now() + this.#pollSeconds * 1000;
			while (!signal.aborted && this.#now() < deadline) {
				const wait = (this.#deps.sleep ?? defaultSleep)(
					Math.min(5_000, Math.max(50, deadline - this.#now())),
					signal,
				);
				await Promise.race([wait, ...this.#running.values()]);
				this.pump();
			}
		}
		await Promise.allSettled(this.#running.values());
		await this.#record();
	}
}
