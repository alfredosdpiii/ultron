/**
 * The autoreview loop: per account, discover pull requests that ask for a review (notifications, plus a search
 * fallback every few cycles so nothing is missed). Each one found is decided and acknowledged at once, outside
 * the review limit, so the author hears back within seconds; the reviews themselves then run up to `concurrency`
 * at a time. A pull request is never handled twice at the same time by one account, rate limits pause the account
 * they hit, and a failed review is retried on a later cycle (three attempts per commit).
 */
import { appendFileSync, mkdirSync, readdirSync, rmSync, statSync } from "node:fs";
import { join } from "node:path";
import { maskCellOutput } from "../rlm/output-secrets.ts";
import { type Account, accountKey } from "./accounts.ts";
import { RETENTION_DAYS } from "./config.ts";
import { GitHub, type PullRef, pullKey, RateLimitError } from "./github.ts";
import { type Candidate, type Outcome, type Prepared, preparePull, type ReviewerDeps, runReview } from "./reviewer.ts";
import type { AccountState, AutoreviewState } from "./state.ts";

/** The search fallback runs on the first cycle and then every this many cycles. */
export const SEARCH_EVERY = 5;
const FIRST_POLL_WINDOW_MS = 24 * 60 * 60 * 1000;
const SEARCH_WINDOW_MS = 2 * 24 * 60 * 60 * 1000;
const SINCE_OVERLAP_MS = 2 * 60 * 1000;
const RETRY_DELAY_MS = 60_000;

export interface Discovered {
	readonly ref: PullRef;
	readonly reasons: string[];
	/** The notification's update time (epoch ms), when a notification led here. */
	notifiedAt?: number;
}

/** Remove files in `dirs` last modified more than `days` ago. Never throws. */
export function pruneOld(dirs: readonly string[], now: number, days = RETENTION_DAYS): number {
	let removed = 0;
	for (const dir of dirs) {
		try {
			for (const entry of readdirSync(dir, { withFileTypes: true })) {
				const path = join(dir, entry.name);
				if (!entry.isFile() || now - statSync(path).mtimeMs <= days * 24 * 60 * 60 * 1000) continue;
				rmSync(path, { force: true });
				removed += 1;
			}
		} catch {
			// A directory that does not exist yet has nothing to prune.
		}
	}
	return removed;
}

/**
 * Open pull requests on which this account's last review requested changes: it is blocking them, so they are
 * looked at again on the search cycles even when no notification arrives for a push.
 */
export function blockedPulls(state: AutoreviewState, account: Account): PullRef[] {
	const out: PullRef[] = [];
	const prefix = `${account.login}@${account.host}/`;
	for (const [key, pull] of Object.entries(state.pulls)) {
		if (!key.startsWith(prefix) || pull.lastVerdict !== "request_changes" || pull.closed) continue;
		if (pull.lastReviewDryRun) continue;
		const match = /^([^/]+)\/([^#]+)#(\d+)$/.exec(key.slice(prefix.length));
		if (match) out.push({ host: account.host, owner: match[1]!, repo: match[2]!, number: Number(match[3]) });
	}
	return out;
}

/** Pull requests that may need a review by this account, and the account's next notification cursor. */
export async function discover(
	github: GitHub,
	state: AccountState,
	options: { now: number; search: boolean; blocked?: readonly PullRef[] },
): Promise<{ pulls: Discovered[]; state: AccountState }> {
	const found = new Map<string, Discovered>();
	const add = (ref: PullRef, reason: string, updatedAt?: string) => {
		const key = pullKey(ref);
		const entry = found.get(key) ?? { ref, reasons: [] };
		if (!entry.reasons.includes(reason)) entry.reasons.push(reason);
		const at = updatedAt ? Date.parse(updatedAt) : Number.NaN;
		if (Number.isFinite(at) && (entry.notifiedAt === undefined || at > entry.notifiedAt)) entry.notifiedAt = at;
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
	for (const pull of page.pulls)
		add({ host: pull.host, owner: pull.owner, repo: pull.repo, number: pull.number }, pull.reason, pull.updatedAt);
	if (options.search) {
		const login = github.account.login;
		const day = new Date(options.now - SEARCH_WINDOW_MS).toISOString().slice(0, 10);
		for (const ref of await github.search(`is:open is:pr review-requested:${login} archived:false`))
			add(ref, "review_requested");
		for (const ref of await github.search(`is:pr mentions:${login} updated:>=${day} archived:false`))
			add(ref, "search_mention");
		for (const ref of options.blocked ?? []) add(ref, "requested_changes");
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

type Waiting = Candidate & { notBefore: number };

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

const DAY_MS = 24 * 60 * 60 * 1000;

export class Daemon {
	readonly #deps: DaemonDeps;
	/** Found by discovery, not yet decided. */
	readonly #candidates = new Map<string, Waiting>();
	/** Being decided and acknowledged; not limited by the review concurrency. */
	readonly #preparing = new Map<string, Promise<void>>();
	/** Due and acknowledged, waiting for a review slot. */
	readonly #queue = new Map<string, Prepared>();
	readonly #running = new Map<string, Promise<void>>();
	readonly #paused = new Map<string, number>();
	#cycles = 0;
	#pollSeconds: number;
	#prunedAt = Number.NEGATIVE_INFINITY;

	constructor(deps: DaemonDeps) {
		this.#deps = deps;
		this.#pollSeconds = deps.config.pollSeconds;
	}

	#now(): number {
		return (this.#deps.now ?? Date.now)();
	}

	get queued(): number {
		return this.#candidates.size + this.#queue.size;
	}

	get running(): number {
		return this.#running.size;
	}

	#key(account: Account, ref: PullRef): string {
		return `${accountKey(account)} ${pullKey(ref)}`;
	}

	#enqueue(candidate: Candidate, notBefore = 0): void {
		const key = this.#key(candidate.account, candidate.ref);
		const waiting = this.#candidates.get(key);
		if (waiting) {
			// Keep the earliest pickup; merge the reasons.
			this.#candidates.set(key, {
				...waiting,
				reasons: [...new Set([...waiting.reasons, ...candidate.reasons])],
				notBefore: Math.min(waiting.notBefore, notBefore),
			});
			return;
		}
		this.#candidates.set(key, { ...candidate, notBefore });
	}

	/** Remove dry-run files and logs past retention, at most once a day. */
	prune(): void {
		const now = this.#now();
		if (now - this.#prunedAt < DAY_MS) return;
		this.#prunedAt = now;
		const removed = pruneOld([this.#deps.paths.dryRun, this.#deps.paths.logs], now);
		if (removed > 0) this.#deps.log(`removed ${removed} dry-run and log files older than ${RETENTION_DAYS} days`);
	}

	/** Poll every account once and note what was found. */
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
			const all = this.#deps.store.read();
			const saved = all.accounts[key] ?? {};
			try {
				const { pulls, state } = await discover(github, saved, {
					now,
					search,
					...(search ? { blocked: blockedPulls(all, account) } : {}),
				});
				delete state.pausedUntil;
				await this.#deps.store.update((current) => {
					current.accounts[key] = state;
				});
				if (state.pollInterval !== undefined)
					this.#pollSeconds = Math.max(this.#deps.config.pollSeconds, state.pollInterval);
				for (const pull of pulls)
					this.#enqueue({
						account,
						ref: pull.ref,
						reasons: pull.reasons,
						pickedAt: now,
						...(pull.notifiedAt === undefined ? {} : { notifiedAt: pull.notifiedAt }),
					});
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

	/** A failure of one pull request: wait out a rate limit, otherwise try again on a later cycle. */
	async #retry(candidate: Candidate, error: unknown, what: string): Promise<void> {
		const message = error instanceof Error ? error.message : String(error);
		if (error instanceof RateLimitError) {
			await this.#failed(candidate.account, error);
			this.#enqueue(candidate, error.resumeAt);
		} else {
			this.#deps.log(`${pullKey(candidate.ref)} as ${candidate.account.login}: ${what} failed: ${message}`);
			// preparePull gives up after three failed reviews of one commit.
			this.#enqueue(candidate, this.#now() + RETRY_DELAY_MS);
		}
		this.#deps.onOutcome?.(candidate, { kind: "failed", error: message });
	}

	/**
	 * Decide and acknowledge every candidate that is due, all at once: acknowledgements do not wait for a review
	 * slot. What is due for a review joins the queue.
	 */
	prepare(): void {
		const now = this.#now();
		for (const [key, candidate] of this.#candidates) {
			if (candidate.notBefore > now || this.#preparing.has(key) || this.#queue.has(key) || this.#running.has(key))
				continue;
			if ((this.#paused.get(accountKey(candidate.account)) ?? 0) > now) continue;
			this.#candidates.delete(key);
			const task = (async () => {
				try {
					const prepared = await preparePull(this.#deps, candidate);
					if ("kind" in prepared) {
						if (prepared.kind === "skipped")
							this.#deps.log(
								`${pullKey(candidate.ref)} as ${candidate.account.login}: skipped (${prepared.reason})`,
							);
						this.#deps.onOutcome?.(candidate, prepared);
					} else this.#queue.set(key, prepared);
				} catch (error) {
					await this.#retry(candidate, error, "the decision");
				}
			})().finally(() => {
				this.#preparing.delete(key);
				this.pump();
			});
			this.#preparing.set(key, task);
		}
	}

	/** Start queued reviews, up to the concurrency limit. */
	pump(): void {
		const now = this.#now();
		for (const [key, prepared] of this.#queue) {
			if (this.#running.size >= this.#deps.config.concurrency) break;
			if (this.#running.has(key)) continue;
			if ((this.#paused.get(accountKey(prepared.candidate.account)) ?? 0) > now) continue;
			this.#queue.delete(key);
			const task = this.#review(prepared).finally(() => {
				this.#running.delete(key);
			});
			this.#running.set(key, task);
		}
	}

	async #review(prepared: Prepared): Promise<void> {
		const { candidate } = prepared;
		try {
			const outcome = await runReview(this.#deps, prepared);
			if (outcome.kind === "requeue") this.#enqueue({ ...candidate, pickedAt: this.#now() });
			this.#deps.onOutcome?.(candidate, outcome);
		} catch (error) {
			await this.#retry(candidate, error, "the review");
		}
	}

	async #record(): Promise<void> {
		await this.#deps.store
			.update((all) => {
				all.queue = [
					...[...this.#running.keys()].map((key) => `${key} (reviewing)`),
					...[...this.#queue.keys()].map((key) => `${key} (acknowledged, waiting)`),
					...[...this.#candidates.keys(), ...this.#preparing.keys()],
				];
			})
			.catch(() => {});
	}

	#due(): boolean {
		const now = this.#now();
		return [...this.#candidates.values()].some(
			(candidate) => candidate.notBefore <= now && (this.#paused.get(accountKey(candidate.account)) ?? 0) <= now,
		);
	}

	/** One cycle: poll, acknowledge, then review everything that is due and wait for it. */
	async once(): Promise<void> {
		this.prune();
		await this.poll();
		for (;;) {
			this.prepare();
			this.pump();
			await this.#record();
			const active = [...this.#preparing.values(), ...this.#running.values()];
			if (active.length === 0) {
				if (this.#queue.size === 0 || !this.#startable()) break;
				continue;
			}
			await Promise.race(active);
		}
		await this.#record();
	}

	/** Whether a queued review could start now (its account is not paused). */
	#startable(): boolean {
		const now = this.#now();
		return (
			this.#due() ||
			[...this.#queue.values()].some(
				(prepared) => (this.#paused.get(accountKey(prepared.candidate.account)) ?? 0) <= now,
			)
		);
	}

	/** Poll and review until `signal` aborts; running reviews finish first. */
	async run(signal: AbortSignal): Promise<void> {
		while (!signal.aborted) {
			this.prune();
			await this.poll();
			this.prepare();
			this.pump();
			await this.#record();
			// Reviews that finish free a slot: fill it without waiting for the next poll.
			const deadline = this.#now() + this.#pollSeconds * 1000;
			while (!signal.aborted && this.#now() < deadline) {
				const wait = (this.#deps.sleep ?? defaultSleep)(
					Math.min(5_000, Math.max(50, deadline - this.#now())),
					signal,
				);
				await Promise.race([wait, ...this.#preparing.values(), ...this.#running.values()]);
				this.prepare();
				this.pump();
			}
		}
		await Promise.allSettled([...this.#preparing.values(), ...this.#running.values()]);
		await this.#record();
	}
}
