/**
 * GitHub, as `ultron autoreview` uses it: REST and GraphQL through `gh api`, authenticated per account by the
 * child's environment. Responses are requested with their headers (`gh api -i`), so conditional requests
 * (ETag, If-Modified-Since), `X-Poll-Interval` and rate limits (`Retry-After`, `X-RateLimit-Reset`) are honoured.
 */
import type { Account } from "./accounts.ts";
import type { Runner } from "./runner.ts";

/** GitHub asked us to wait: nothing is sent for this account before `resumeAt` (epoch ms). */
export class RateLimitError extends Error {
	readonly resumeAt: number;
	constructor(message: string, resumeAt: number) {
		super(message);
		this.name = "RateLimitError";
		this.resumeAt = resumeAt;
	}
}

export class GitHubError extends Error {
	readonly status: number;
	constructor(message: string, status: number) {
		super(message);
		this.name = "GitHubError";
		this.status = status;
	}
}

export interface ApiResponse {
	readonly status: number;
	readonly headers: Readonly<Record<string, string>>;
	readonly text: string;
	readonly body: unknown;
}

export interface PullRef {
	readonly host: string;
	readonly owner: string;
	readonly repo: string;
	readonly number: number;
}

export interface ReviewThread {
	readonly id: string;
	readonly resolved: boolean;
	readonly thumbsUp: number;
	readonly thumbsDown: number;
	readonly replies: number;
}

export function pullKey(ref: PullRef): string {
	return `${ref.host}/${ref.owner}/${ref.repo}#${ref.number}`;
}

export interface PullRequest {
	readonly number: number;
	readonly title: string;
	readonly body: string;
	readonly state: "open" | "closed";
	readonly merged: boolean;
	readonly draft: boolean;
	readonly author: string;
	readonly headSha: string;
	readonly baseSha: string;
	readonly baseRef: string;
	readonly url: string;
	readonly createdAt: string;
	readonly requestedReviewers: readonly string[];
	readonly requestedTeams: readonly string[];
}

export interface Review {
	readonly id: number;
	readonly user: string;
	readonly state: string;
	readonly commitId: string;
	readonly submittedAt: string;
	readonly body: string;
}

export interface Comment {
	readonly id: number;
	readonly user: string;
	readonly body: string;
	readonly createdAt: string;
	readonly updatedAt: string;
	/** Review comments only. */
	readonly path?: string;
	readonly line?: number;
	readonly reviewId?: number;
}

export interface ReviewCommentInput {
	readonly path: string;
	readonly line: number;
	readonly side: "RIGHT";
	readonly start_line?: number;
	readonly start_side?: "RIGHT";
	readonly body: string;
}

export interface ReviewInput {
	readonly commit_id: string;
	readonly event: "APPROVE" | "REQUEST_CHANGES" | "COMMENT";
	readonly body: string;
	readonly comments: readonly ReviewCommentInput[];
}

export interface NotificationPage {
	readonly notModified: boolean;
	readonly pulls: ReadonlyArray<PullRef & { reason: string; updatedAt: string }>;
	readonly lastModified?: string;
	readonly etag?: string;
	/** Seconds GitHub asks to wait between polls (`X-Poll-Interval`). */
	readonly pollInterval?: number;
}

export const NOTIFICATION_REASONS: readonly string[] = ["review_requested", "mention", "team_mention"];

type Json = Record<string, unknown>;

const text = (value: unknown): string => (typeof value === "string" ? value : "");
const login = (value: unknown): string => text((value as Json | null | undefined)?.login);

/** Split `gh api -i` output into the status, the headers and the body. */
export function parseApiOutput(stdout: string): { status: number; headers: Record<string, string>; text: string } {
	const match = /^HTTP\/[\d.]+ (\d{3})[^\n]*\r?\n/.exec(stdout);
	if (!match) return { status: 0, headers: {}, text: stdout };
	const rest = stdout.slice(match[0].length);
	// A blank line ends the headers (at once, when there are none).
	const end = /^\r?\n|\r?\n\r?\n/.exec(rest);
	const head = end ? rest.slice(0, end.index) : rest;
	const headers: Record<string, string> = {};
	for (const line of head.split(/\r?\n/)) {
		const colon = line.indexOf(":");
		if (colon > 0) headers[line.slice(0, colon).trim().toLowerCase()] = line.slice(colon + 1).trim();
	}
	return { status: Number(match[1]), headers, text: end ? rest.slice(end.index + end[0].length) : "" };
}

/** `https://api.github.com/repos/o/r/pulls/7` (or `.../issues/7`, or a web URL) as a PullRef. */
export function pullRefFromUrl(url: string, host: string): PullRef | undefined {
	const match = /\/([^/]+)\/([^/]+)\/(?:pulls|pull|issues)\/(\d+)(?:[/?#].*)?$/.exec(url);
	return match ? { host, owner: match[1]!, repo: match[2]!, number: Number(match[3]) } : undefined;
}

/** `owner/repo#N` or a pull request URL. */
export function parsePullTarget(target: string, defaultHost = "github.com"): PullRef | undefined {
	const short = /^([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+)#(\d+)$/.exec(target);
	if (short) return { host: defaultHost, owner: short[1]!, repo: short[2]!, number: Number(short[3]) };
	const url = /^https?:\/\/([^/]+)\/([^/]+)\/([^/]+)\/pull\/(\d+)(?:[/?#].*)?$/.exec(target);
	return url ? { host: url[1]!, owner: url[2]!, repo: url[3]!, number: Number(url[4]) } : undefined;
}

export interface GitHubOptions {
	readonly now?: () => number;
}

export class GitHub {
	readonly account: Account;
	readonly #runner: Runner;
	readonly #env: () => Promise<Record<string, string>>;
	readonly #now: () => number;

	constructor(
		runner: Runner,
		account: Account,
		env: () => Promise<Record<string, string>>,
		options: GitHubOptions = {},
	) {
		this.#runner = runner;
		this.account = account;
		this.#env = env;
		this.#now = options.now ?? Date.now;
	}

	/** One API request. Throws RateLimitError when GitHub says to wait, GitHubError for any other failure. */
	async request(
		method: string,
		path: string,
		options: { headers?: Record<string, string>; body?: unknown; allow?: readonly number[] } = {},
	): Promise<ApiResponse> {
		const argv = ["gh", "api", "-i", "--method", method, "--hostname", this.account.host, path];
		for (const [name, value] of Object.entries(options.headers ?? {})) argv.push("-H", `${name}: ${value}`);
		if (options.body !== undefined) argv.push("--input", "-");
		const result = await this.#runner(argv, {
			env: await this.#env(),
			...(options.body === undefined ? {} : { input: JSON.stringify(options.body) }),
		});
		const parsed = parseApiOutput(result.stdout);
		if (parsed.status === 0)
			throw new GitHubError(
				`gh api ${method} ${path} failed: ${(result.stderr || result.stdout).trim().slice(0, 300)}`,
				0,
			);
		let body: unknown;
		try {
			body = parsed.text.trim() === "" ? undefined : JSON.parse(parsed.text);
		} catch {
			body = undefined;
		}
		const response: ApiResponse = { ...parsed, body };
		if (parsed.status < 400 || options.allow?.includes(parsed.status)) return response;
		const message = text((body as Json | undefined)?.message) || `HTTP ${parsed.status}`;
		const resumeAt = this.#resumeAt(parsed.status, parsed.headers, message);
		if (resumeAt !== undefined) throw new RateLimitError(`GitHub rate limit: ${message}`, resumeAt);
		throw new GitHubError(`GitHub ${method} ${path}: ${parsed.status} ${message}`.slice(0, 400), parsed.status);
	}

	/** When a 403 or 429 is a rate limit: the time to resume at. */
	#resumeAt(status: number, headers: Record<string, string>, message: string): number | undefined {
		if (status !== 403 && status !== 429) return undefined;
		const retryAfter = Number(headers["retry-after"]);
		if (Number.isFinite(retryAfter) && retryAfter > 0) return this.#now() + retryAfter * 1000;
		if (headers["x-ratelimit-remaining"] === "0") {
			const reset = Number(headers["x-ratelimit-reset"]);
			if (Number.isFinite(reset) && reset > 0) return Math.max(this.#now() + 1_000, reset * 1000);
		}
		// A secondary limit without headers: GitHub asks for at least a minute.
		if (status === 429 || /rate limit|abuse/i.test(message)) return this.#now() + 60_000;
		return undefined;
	}

	/** Every page of a list endpoint (100 per page, at most `maxPages`). */
	async list(path: string, maxPages = 5): Promise<Json[]> {
		const out: Json[] = [];
		for (let page = 1; page <= maxPages; page += 1) {
			const separator = path.includes("?") ? "&" : "?";
			const response = await this.request("GET", `${path}${separator}per_page=100&page=${page}`);
			const items = Array.isArray(response.body) ? (response.body as Json[]) : [];
			out.push(...items);
			if (items.length < 100) break;
		}
		return out;
	}

	/** Notifications about pull requests this account was asked to review or was mentioned on. */
	async notifications(options: { since: string; lastModified?: string; etag?: string }): Promise<NotificationPage> {
		const headers: Record<string, string> = {};
		if (options.etag) headers["If-None-Match"] = options.etag;
		else if (options.lastModified) headers["If-Modified-Since"] = options.lastModified;
		const response = await this.request(
			"GET",
			`notifications?all=true&per_page=50&since=${encodeURIComponent(options.since)}`,
			{ headers, allow: [304] },
		);
		const interval = Number(response.headers["x-poll-interval"]);
		const meta = {
			...(response.headers["last-modified"] ? { lastModified: response.headers["last-modified"] } : {}),
			...(response.headers.etag ? { etag: response.headers.etag } : {}),
			...(Number.isFinite(interval) && interval > 0 ? { pollInterval: interval } : {}),
		};
		if (response.status === 304)
			return {
				notModified: true,
				pulls: [],
				...(options.lastModified ? { lastModified: options.lastModified } : {}),
				...(options.etag ? { etag: options.etag } : {}),
				...meta,
			};
		const pulls: Array<PullRef & { reason: string; updatedAt: string }> = [];
		for (const item of Array.isArray(response.body) ? (response.body as Json[]) : []) {
			const subject = item.subject as Json | undefined;
			const reason = text(item.reason);
			if (text(subject?.type) !== "PullRequest" || !NOTIFICATION_REASONS.includes(reason)) continue;
			const ref = pullRefFromUrl(text(subject?.url), this.account.host);
			if (ref) pulls.push({ ...ref, reason, updatedAt: text(item.updated_at) });
		}
		return { notModified: false, pulls, ...meta };
	}

	/** Pull requests matching a search query. */
	async search(query: string): Promise<PullRef[]> {
		const response = await this.request(
			"GET",
			`search/issues?per_page=50&sort=updated&order=desc&q=${encodeURIComponent(query)}`,
		);
		const items = (response.body as Json | undefined)?.items;
		const out: PullRef[] = [];
		for (const item of Array.isArray(items) ? (items as Json[]) : []) {
			const ref = pullRefFromUrl(`${text(item.repository_url)}/pulls/${String(item.number)}`, this.account.host);
			if (ref && item.pull_request !== undefined) out.push(ref);
		}
		return out;
	}

	async pull(ref: PullRef): Promise<PullRequest> {
		const response = await this.request("GET", `repos/${ref.owner}/${ref.repo}/pulls/${ref.number}`);
		const item = (response.body ?? {}) as Json;
		const head = (item.head ?? {}) as Json;
		const base = (item.base ?? {}) as Json;
		return {
			number: ref.number,
			title: text(item.title),
			body: text(item.body),
			state: item.state === "open" ? "open" : "closed",
			merged: item.merged === true || typeof item.merged_at === "string",
			draft: item.draft === true,
			author: login(item.user),
			headSha: text(head.sha),
			baseSha: text(base.sha),
			baseRef: text(base.ref),
			url: text(item.html_url),
			createdAt: text(item.created_at),
			requestedReviewers: (Array.isArray(item.requested_reviewers) ? item.requested_reviewers : []).map(login),
			requestedTeams: (Array.isArray(item.requested_teams) ? (item.requested_teams as Json[]) : []).map((team) =>
				text(team.slug),
			),
		};
	}

	/** The pull request's diff as GitHub computes it (merge base to head). */
	async pullDiff(ref: PullRef): Promise<string> {
		const response = await this.request("GET", `repos/${ref.owner}/${ref.repo}/pulls/${ref.number}`, {
			headers: { Accept: "application/vnd.github.v3.diff" },
		});
		return response.text;
	}

	async reviews(ref: PullRef): Promise<Review[]> {
		const items = await this.list(`repos/${ref.owner}/${ref.repo}/pulls/${ref.number}/reviews`);
		return items.map((item) => ({
			id: Number(item.id),
			user: login(item.user),
			state: text(item.state),
			commitId: text(item.commit_id),
			submittedAt: text(item.submitted_at),
			body: text(item.body),
		}));
	}

	#comment(item: Json): Comment {
		const line =
			typeof item.line === "number"
				? item.line
				: typeof item.original_line === "number"
					? item.original_line
					: undefined;
		return {
			id: Number(item.id),
			user: login(item.user),
			body: text(item.body),
			createdAt: text(item.created_at),
			updatedAt: text(item.updated_at) || text(item.created_at),
			...(typeof item.path === "string" ? { path: item.path } : {}),
			...(line === undefined ? {} : { line }),
			...(typeof item.pull_request_review_id === "number" ? { reviewId: item.pull_request_review_id } : {}),
		};
	}

	/** Comments on the pull request's conversation. */
	async issueComments(ref: PullRef): Promise<Comment[]> {
		return (await this.list(`repos/${ref.owner}/${ref.repo}/issues/${ref.number}/comments`)).map((item) =>
			this.#comment(item),
		);
	}

	/** Inline review comments on the pull request. */
	async reviewComments(ref: PullRef): Promise<Comment[]> {
		return (await this.list(`repos/${ref.owner}/${ref.repo}/pulls/${ref.number}/comments`)).map((item) =>
			this.#comment(item),
		);
	}

	/** When this account was last asked to review the pull request (ISO time), from the issue's events. */
	async lastReviewRequestAt(ref: PullRef): Promise<string | undefined> {
		const events = await this.list(`repos/${ref.owner}/${ref.repo}/issues/${ref.number}/events`);
		let latest: string | undefined;
		for (const event of events) {
			if (event.event !== "review_requested") continue;
			if (login(event.requested_reviewer).toLowerCase() !== this.account.login.toLowerCase()) continue;
			const at = text(event.created_at);
			if (latest === undefined || at > latest) latest = at;
		}
		return latest;
	}

	/**
	 * Whether this account is an active member of a team of the repository's organization; undefined when the
	 * membership cannot be read (the token lacks `read:org`, or the team is not visible).
	 */
	async inTeam(org: string, slug: string): Promise<boolean | undefined> {
		try {
			const response = await this.request("GET", `orgs/${org}/teams/${slug}/memberships/${this.account.login}`, {
				allow: [404],
			});
			if (response.status === 404) return false;
			return (response.body as Json | undefined)?.state === "active";
		} catch (error) {
			if (error instanceof RateLimitError) throw error;
			return undefined;
		}
	}

	/** Whether this account may push to the repository (false when that cannot be read). */
	async canPush(ref: PullRef): Promise<boolean> {
		try {
			const response = await this.request("GET", `repos/${ref.owner}/${ref.repo}`);
			const permissions = (response.body as Json | undefined)?.permissions as Json | undefined;
			return permissions?.push === true || permissions?.admin === true || permissions?.maintain === true;
		} catch (error) {
			if (error instanceof RateLimitError) throw error;
			return false;
		}
	}

	/** One line on the head commit's CI checks, or undefined when there are none or they cannot be read. */
	async checkSummary(ref: PullRef, sha: string): Promise<string | undefined> {
		let response: ApiResponse;
		try {
			response = await this.request("GET", `repos/${ref.owner}/${ref.repo}/commits/${sha}/check-runs?per_page=100`);
		} catch (error) {
			if (error instanceof RateLimitError) throw error;
			return undefined;
		}
		const runs = (response.body as Json | undefined)?.check_runs;
		if (!Array.isArray(runs) || runs.length === 0) return undefined;
		let passed = 0;
		let pending = 0;
		const failed: string[] = [];
		for (const run of runs as Json[]) {
			if (run.status !== "completed") pending += 1;
			else if (["success", "neutral", "skipped"].includes(text(run.conclusion))) passed += 1;
			else failed.push(text(run.name));
		}
		const parts = [`${passed} passed`];
		if (failed.length) parts.push(`${failed.length} failed (${failed.slice(0, 5).join(", ")})`);
		if (pending) parts.push(`${pending} pending`);
		return parts.join(", ");
	}

	/** Post a comment on the pull request's conversation; returns its id. */
	async postComment(ref: PullRef, body: string): Promise<number> {
		const response = await this.request("POST", `repos/${ref.owner}/${ref.repo}/issues/${ref.number}/comments`, {
			body: { body },
		});
		return Number((response.body as Json | undefined)?.id);
	}

	/** Post a review in one request; returns its id. */
	async postReview(ref: PullRef, review: ReviewInput): Promise<number> {
		const response = await this.request("POST", `repos/${ref.owner}/${ref.repo}/pulls/${ref.number}/reviews`, {
			body: review,
		});
		return Number((response.body as Json | undefined)?.id);
	}

	/** The inline comments GitHub kept of a posted review. */
	async commentsOfReview(ref: PullRef, reviewId: number): Promise<Comment[]> {
		return (await this.list(`repos/${ref.owner}/${ref.repo}/pulls/${ref.number}/reviews/${reviewId}/comments`)).map(
			(item) => this.#comment(item),
		);
	}

	/** Post one inline comment outside a review; returns its id. */
	async postReviewComment(ref: PullRef, commitId: string, comment: ReviewCommentInput): Promise<number> {
		const response = await this.request("POST", `repos/${ref.owner}/${ref.repo}/pulls/${ref.number}/comments`, {
			body: { commit_id: commitId, ...comment },
		});
		return Number((response.body as Json | undefined)?.id);
	}

	async graphql(query: string, variables: Record<string, unknown>): Promise<Json> {
		const response = await this.request("POST", "graphql", { body: { query, variables } });
		const body = (response.body ?? {}) as Json;
		if (Array.isArray(body.errors) && body.errors.length > 0)
			throw new GitHubError(`GitHub GraphQL: ${text((body.errors[0] as Json).message)}`.slice(0, 300), 200);
		return (body.data ?? {}) as Json;
	}

	/** Review thread by the database id of its first comment: id, resolved, the reactions on that comment, replies. */
	async reviewThreads(ref: PullRef): Promise<Map<number, ReviewThread>> {
		const out = new Map<number, ReviewThread>();
		let after: string | null = null;
		for (let page = 0; page < 10; page += 1) {
			const data = await this.graphql(
				"query($owner:String!,$repo:String!,$number:Int!,$after:String){repository(owner:$owner,name:$repo){pullRequest(number:$number){reviewThreads(first:100,after:$after){pageInfo{hasNextPage endCursor}nodes{id isResolved comments(first:1){totalCount nodes{databaseId reactionGroups{content reactors{totalCount}}}}}}}}}",
				{ owner: ref.owner, repo: ref.repo, number: ref.number, after },
			);
			const threads = (((data.repository as Json | undefined)?.pullRequest as Json | undefined)?.reviewThreads ??
				{}) as Json;
			for (const node of Array.isArray(threads.nodes) ? (threads.nodes as Json[]) : []) {
				const comments = (node.comments ?? {}) as Json;
				const first = (comments.nodes as Json[] | undefined)?.[0];
				if (typeof first?.databaseId !== "number" || typeof node.id !== "string") continue;
				const reactions = (content: string): number => {
					const groups = Array.isArray(first.reactionGroups) ? (first.reactionGroups as Json[]) : [];
					const group = groups.find((item) => item.content === content);
					const count = ((group?.reactors as Json | undefined)?.totalCount ?? 0) as number;
					return typeof count === "number" ? count : 0;
				};
				out.set(first.databaseId, {
					id: node.id,
					resolved: node.isResolved === true,
					thumbsUp: reactions("THUMBS_UP"),
					thumbsDown: reactions("THUMBS_DOWN"),
					replies: typeof comments.totalCount === "number" ? Math.max(0, comments.totalCount - 1) : 0,
				});
			}
			const pageInfo = (threads.pageInfo ?? {}) as Json;
			if (pageInfo.hasNextPage !== true || typeof pageInfo.endCursor !== "string") break;
			after = pageInfo.endCursor;
		}
		return out;
	}

	async resolveThread(threadId: string): Promise<void> {
		await this.graphql(
			"mutation($threadId:ID!){resolveReviewThread(input:{threadId:$threadId}){thread{id isResolved}}}",
			{ threadId },
		);
	}
}
