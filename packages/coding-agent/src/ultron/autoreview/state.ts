/**
 * What `ultron autoreview` remembers, in `<agentDir>/autoreview/state.json`: per account, the notification
 * cursor; per account and pull request, the last reviewed and acknowledged commits, attempts per commit and the
 * findings it posted (for re-reviews); and the recent reviews with their timings. No token is ever stored.
 *
 * Writes are atomic (a temporary file renamed over the state) and serialized by a lock file, so a one-off
 * `ultron autoreview review` and the daemon can share the state. The daemon itself holds `daemon.lock`, so only
 * one runs per agent directory.
 */
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import lockfile from "proper-lockfile";

export interface PostedFinding {
	/** Stable within the pull request: `<short sha>-<n>`. */
	readonly id: string;
	readonly file: string;
	line: number;
	/** The finding's level (older state files hold the old four-name severity). */
	readonly severity: string;
	readonly claim: string;
	readonly claimHash: string;
	/** The commit the finding was posted on. */
	readonly sha: string;
	/** The inline comment, when the finding was posted inline. */
	commentId?: number;
	/** The review thread of that comment (the only thread this finding may resolve). */
	threadId?: string;
	status: "open" | "fixed" | "not_applicable";
}

export interface PullState {
	lastReviewedSha?: string;
	lastReviewedAt?: string;
	lastReviewId?: number;
	/** The last "review" was a dry run: nothing was posted. */
	lastReviewDryRun?: boolean;
	/** The verdict of the last review: after "request_changes", new commits are reviewed without a new request. */
	lastVerdict?: "approve" | "request_changes" | "comment";
	/** The pull request was seen closed or merged after the last review. */
	closed?: boolean;
	lastAckSha?: string;
	lastAckAt?: string;
	/** The acknowledgement line used last on this pull request (not repeated next time). */
	lastAckLine?: string;
	/** Failed attempts by head commit. */
	attempts: Record<string, number>;
	/** The commit a "could not review" comment was posted for. */
	gaveUpSha?: string;
	findings: PostedFinding[];
}

export interface AccountState {
	lastPollAt?: string;
	lastModified?: string;
	etag?: string;
	pollInterval?: number;
	/** Nothing is sent for this account before this time (a rate limit). */
	pausedUntil?: string;
	lastError?: string;
}

export interface ReviewRecord {
	readonly account: string;
	readonly pull: string;
	readonly sha: string;
	readonly at: string;
	readonly outcome: string;
	readonly verdict?: string;
	readonly findings?: number;
	readonly totalMs?: number;
	/** Where the time went, by stage (the pipeline's stages, then `post`), in milliseconds. */
	readonly stages?: Readonly<Record<string, number>>;
	readonly pickupToPostMs?: number;
	/** From the notification's update time to the acknowledgement comment. */
	readonly tagToAckMs?: number;
	/** From the acknowledgement comment to the posted review. */
	readonly ackToPostMs?: number;
	readonly costUsd?: number;
	readonly dryRun?: boolean;
}

export interface AutoreviewState {
	version: 1;
	accounts: Record<string, AccountState>;
	pulls: Record<string, PullState>;
	recent: ReviewRecord[];
	/** The daemon's queue as last written, for `ultron autoreview status`. */
	queue?: string[];
	daemon?: { pid: number; startedAt: string; engineStartMs?: number };
}

const RECENT_LIMIT = 50;
const MAX_ATTEMPT_SHAS = 8;

export function emptyState(): AutoreviewState {
	return { version: 1, accounts: {}, pulls: {}, recent: [] };
}

export function claimHash(claim: string): string {
	return createHash("sha256").update(claim.toLowerCase().replace(/\s+/g, " ").trim()).digest("hex").slice(0, 16);
}

export function emptyPull(): PullState {
	return { attempts: {}, findings: [] };
}

function normalize(value: unknown): AutoreviewState {
	const state = emptyState();
	if (typeof value !== "object" || value === null) return state;
	const raw = value as Partial<AutoreviewState>;
	if (typeof raw.accounts === "object" && raw.accounts !== null) state.accounts = raw.accounts;
	if (typeof raw.pulls === "object" && raw.pulls !== null) {
		for (const [key, pull] of Object.entries(raw.pulls)) {
			if (typeof pull !== "object" || pull === null) continue;
			state.pulls[key] = {
				...pull,
				attempts: typeof pull.attempts === "object" && pull.attempts !== null ? pull.attempts : {},
				findings: Array.isArray(pull.findings) ? pull.findings : [],
			};
		}
	}
	if (Array.isArray(raw.recent)) state.recent = raw.recent;
	if (Array.isArray(raw.queue)) state.queue = raw.queue;
	if (typeof raw.daemon === "object" && raw.daemon !== null) state.daemon = raw.daemon;
	return state;
}

export class StateStore {
	readonly path: string;

	constructor(path: string) {
		this.path = path;
	}

	read(): AutoreviewState {
		try {
			return normalize(JSON.parse(readFileSync(this.path, "utf8")));
		} catch {
			return emptyState();
		}
	}

	/** Read, change and write the state under the state lock. */
	async update<T>(change: (state: AutoreviewState) => T): Promise<T> {
		mkdirSync(dirname(this.path), { recursive: true, mode: 0o700 });
		const release = await lockfile.lock(this.path, {
			realpath: false,
			stale: 10_000,
			retries: { retries: 50, factor: 1, minTimeout: 20, maxTimeout: 100 },
		});
		try {
			const state = this.read();
			const result = change(state);
			if (state.recent.length > RECENT_LIMIT) state.recent = state.recent.slice(-RECENT_LIMIT);
			for (const pull of Object.values(state.pulls)) {
				const shas = Object.keys(pull.attempts);
				for (const sha of shas.slice(0, Math.max(0, shas.length - MAX_ATTEMPT_SHAS))) delete pull.attempts[sha];
			}
			const temporary = `${this.path}.${process.pid}.tmp`;
			writeFileSync(temporary, `${JSON.stringify(state, null, 1)}\n`, { mode: 0o600 });
			renameSync(temporary, this.path);
			return result;
		} finally {
			await release().catch(() => {});
		}
	}

	/** Change one pull request's entry (created when missing). */
	updatePull<T>(key: string, change: (pull: PullState, state: AutoreviewState) => T): Promise<T> {
		return this.update((state) => {
			const pull = state.pulls[key] ?? emptyPull();
			state.pulls[key] = pull;
			return change(pull, state);
		});
	}
}

/** Another `ultron autoreview run` already holds this agent directory. */
export class DaemonRunningError extends Error {}

/** Take the daemon lock of an autoreview directory; the returned function releases it. */
export async function acquireDaemonLock(dir: string): Promise<() => Promise<void>> {
	mkdirSync(dir, { recursive: true, mode: 0o700 });
	const target = join(dir, "daemon");
	if (!existsSync(target)) writeFileSync(target, "", { mode: 0o600 });
	try {
		return await lockfile.lock(target, { realpath: false, stale: 15_000, update: 5_000, retries: 0 });
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code === "ELOCKED")
			throw new DaemonRunningError(`another ultron autoreview is already running for ${dir}`);
		throw error;
	}
}
