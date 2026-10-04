/**
 * A fake GitHub for `ultron autoreview` tests: an in-memory model of accounts, pull requests, notifications and
 * search behind the injected Runner (`gh auth`, `gh api -i`, and the `git` commands of the checkout). Every call
 * is recorded with its argv, environment and stdin, so tests assert on what would have been sent.
 */
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { Runner, RunOptions, RunResult } from "../src/ultron/autoreview/runner.ts";
import type { EngineResult, EngineSpec, ReviewEngine } from "../src/ultron/autoreview/types.ts";

export interface FakeComment {
	id: number;
	user: string;
	body: string;
	created_at: string;
	updated_at?: string;
	path?: string;
	line?: number;
	pull_request_review_id?: number;
}

export interface FakeReview {
	id: number;
	user: string;
	state: string;
	commit_id: string;
	submitted_at: string;
	body: string;
}

export interface FakePull {
	owner: string;
	repo: string;
	number: number;
	title: string;
	body: string;
	state: "open" | "closed";
	merged: boolean;
	author: string;
	headSha: string;
	baseSha: string;
	baseRef: string;
	createdAt: string;
	requestedReviewers: string[];
	requestedTeams: string[];
	draft: boolean;
	reviews: FakeReview[];
	issueComments: FakeComment[];
	reviewComments: FakeComment[];
	events: Array<{ event: string; created_at: string; requested_reviewer?: { login: string } }>;
	checkRuns: Array<{ name: string; status: string; conclusion: string | null }>;
	/** The pull request diff (`git diff <merge base> <head>` and the API's diff media type). */
	diff: string;
	/** Diffs between other commit pairs, by `from..to`. */
	diffs: Record<string, string>;
	/** Commits that are ancestors of the head (for `git merge-base --is-ancestor`). */
	ancestors: string[];
	/** Review thread id by the id of its first comment, and which threads are resolved. */
	threads: Map<number, string>;
	resolved: Set<string>;
}

export interface RecordedCall {
	argv: string[];
	env: Record<string, string>;
	input?: string;
}

export const DIFF = `diff --git a/calc.py b/calc.py
index 1111111..2222222 100644
--- a/calc.py
+++ b/calc.py
@@ -1,6 +1,6 @@
 def total(items):
     """Sum of item prices."""
     result = 0
-    for i in range(len(items)):
+    for i in range(len(items) - 1):
         result += items[i]["price"]
     return result
`;

const REVIEW_STATES: Record<string, string> = {
	APPROVE: "APPROVED",
	REQUEST_CHANGES: "CHANGES_REQUESTED",
	COMMENT: "COMMENTED",
};

export class FakeHub {
	readonly accounts: Array<{ login: string; host: string; token: string; active?: boolean }> = [];
	readonly pulls = new Map<string, FakePull>();
	notifications: Array<{
		reason: string;
		type?: string;
		owner: string;
		repo: string;
		number: number;
		updatedAt?: string;
	}> = [];
	/** Whether the reviewing account may push to the repositories. */
	canPush = false;
	/** Team members by `org/slug`; a team that is not listed cannot be read (403). */
	teams: Record<string, string[]> = {};
	/** Search results by query substring (`review-requested:`, `mentions:`, `reviewed-by:`). */
	search: Record<string, Array<{ owner: string; repo: string; number: number }>> = {};
	readonly calls: RecordedCall[] = [];
	now = () => Date.parse("2026-10-04T10:00:00Z");
	/** Fail the next matching API calls: `METHOD path-substring` -> status, headers and message. */
	failures: Array<{
		match: string;
		status: number;
		headers?: Record<string, string>;
		message?: string;
		times: number;
	}> = [];
	cloneFails = false;
	/** Called for each inline comment of a posted review; return false to drop it (GitHub keeps the review). */
	keepComment: (comment: { path: string; line: number; body: string }) => boolean = () => true;
	/** Called after a review is posted or the engine ran, to move heads mid-review. */
	onPullRead: ((pull: FakePull, count: number) => void) | undefined;
	#ids = 1000;
	#pullReads = new Map<string, number>();
	pollInterval: number | undefined;
	notModified = false;

	addAccount(login: string, host = "github.com", active = false): string {
		const token = `ghp_${login.replace(/[^a-z0-9]/gi, "")}SECRETTOKEN0123456789abcdef`;
		this.accounts.push({ login, host, token, ...(active ? { active } : {}) });
		return token;
	}

	addPull(partial: Partial<FakePull> & { owner: string; repo: string; number: number }): FakePull {
		const pull: FakePull = {
			title: "Tweak total",
			body: "Sums prices.",
			state: "open",
			merged: false,
			author: "alice",
			headSha: "a".repeat(40),
			baseSha: "b".repeat(40),
			baseRef: "main",
			createdAt: "2026-10-04T08:00:00Z",
			requestedReviewers: [],
			requestedTeams: [],
			draft: false,
			reviews: [],
			issueComments: [],
			reviewComments: [],
			events: [],
			checkRuns: [],
			diff: DIFF,
			diffs: {},
			ancestors: [],
			threads: new Map(),
			resolved: new Set(),
			...partial,
		};
		this.pulls.set(`${pull.owner}/${pull.repo}#${pull.number}`, pull);
		return pull;
	}

	nextId(): number {
		this.#ids += 1;
		return this.#ids;
	}

	/** API calls (method and path) recorded so far, optionally only those whose path matches. */
	api(
		match?: RegExp,
	): Array<{ method: string; path: string; body?: Record<string, unknown>; env: Record<string, string> }> {
		const out = [];
		for (const call of this.calls) {
			if (call.argv[0] !== "gh" || call.argv[1] !== "api") continue;
			const method = call.argv[call.argv.indexOf("--method") + 1]!;
			const path = call.argv[call.argv.indexOf("--hostname") + 2]!;
			if (match && !match.test(`${method} ${path}`)) continue;
			out.push({
				method,
				path,
				env: call.env,
				...(call.input === undefined ? {} : { body: JSON.parse(call.input) as Record<string, unknown> }),
			});
		}
		return out;
	}

	readonly runner: Runner = async (argv, options = {}) => {
		this.calls.push({
			argv: [...argv],
			env: { ...options.env },
			...(options.input === undefined ? {} : { input: options.input }),
		});
		if (argv[0] === "gh" && argv[1] === "auth") return this.#auth(argv);
		if (argv[0] === "gh" && argv[1] === "api") return this.#api(argv, options);
		if (argv[0] === "git") return this.#git(argv);
		return { code: 127, stdout: "", stderr: `${argv[0]}: not found` };
	};

	#auth(argv: readonly string[]): RunResult {
		if (argv[2] === "status") {
			const hosts: Record<string, unknown[]> = {};
			for (const account of this.accounts) {
				const entries = hosts[account.host] ?? [];
				hosts[account.host] = entries;
				entries.push({
					login: account.login,
					host: account.host,
					state: "success",
					active: account.active === true,
				});
			}
			return { code: 0, stdout: JSON.stringify({ hosts }), stderr: "" };
		}
		if (argv[2] === "token") {
			const login = argv[argv.indexOf("--user") + 1];
			const host = argv[argv.indexOf("--hostname") + 1];
			const account = this.accounts.find((item) => item.login === login && item.host === host);
			return account
				? { code: 0, stdout: `${account.token}\n`, stderr: "" }
				: { code: 1, stdout: "", stderr: "no token" };
		}
		return { code: 1, stdout: "", stderr: "unknown gh auth command" };
	}

	#respond(status: number, body: unknown, headers: Record<string, string> = {}): RunResult {
		const head = [
			`HTTP/2.0 ${status} ${status < 400 ? "OK" : "Error"}`,
			...Object.entries(headers).map(([name, value]) => `${name}: ${value}`),
		];
		const text = typeof body === "string" ? body : body === undefined ? "" : JSON.stringify(body);
		return {
			code: status < 400 ? 0 : 1,
			stdout: `${head.join("\r\n")}\r\n\r\n${text}`,
			stderr: status < 400 ? "" : `gh: HTTP ${status}`,
		};
	}

	#pullJson(pull: FakePull): Record<string, unknown> {
		return {
			number: pull.number,
			title: pull.title,
			body: pull.body,
			state: pull.state,
			merged: pull.merged,
			draft: pull.draft,
			user: { login: pull.author },
			head: { sha: pull.headSha },
			base: { sha: pull.baseSha, ref: pull.baseRef },
			html_url: `https://github.com/${pull.owner}/${pull.repo}/pull/${pull.number}`,
			created_at: pull.createdAt,
			requested_reviewers: pull.requestedReviewers.map((login) => ({ login })),
			requested_teams: pull.requestedTeams.map((slug) => ({ slug })),
		};
	}

	#api(argv: readonly string[], options: RunOptions): RunResult {
		const method = argv[argv.indexOf("--method") + 1]!;
		const full = argv[argv.indexOf("--hostname") + 2]!;
		const [path, query = ""] = full.split("?") as [string, string?];
		const headers: Record<string, string> = {};
		argv.forEach((arg, index) => {
			if (arg !== "-H") return;
			const [name, ...rest] = argv[index + 1]!.split(":");
			headers[name!.toLowerCase()] = rest.join(":").trim();
		});
		const token = options.env?.GH_TOKEN ?? options.env?.GH_ENTERPRISE_TOKEN;
		const account = this.accounts.find((item) => item.token === token);
		if (!account) return this.#respond(401, { message: "Bad credentials" });
		const failure = this.failures.find((item) => item.times > 0 && `${method} ${path}`.includes(item.match));
		if (failure) {
			failure.times -= 1;
			return this.#respond(failure.status, { message: failure.message ?? "failed" }, failure.headers);
		}
		const body = options.input === undefined ? {} : (JSON.parse(options.input) as Record<string, unknown>);
		const page = Number(new URLSearchParams(query).get("page") ?? "1");
		const list = (items: unknown[]) => this.#respond(200, page === 1 ? items : []);
		const stamp = new Date(this.now()).toISOString();

		if (path === "notifications") {
			const extra = {
				"Last-Modified": "Sun, 04 Oct 2026 10:00:00 GMT",
				...(this.pollInterval ? { "X-Poll-Interval": String(this.pollInterval) } : {}),
			};
			if (this.notModified && headers["if-modified-since"]) return this.#respond(304, undefined, extra);
			return this.#respond(
				200,
				this.notifications.map((item) => ({
					reason: item.reason,
					updated_at: item.updatedAt ?? stamp,
					subject: {
						type: item.type ?? "PullRequest",
						url: `https://api.github.com/repos/${item.owner}/${item.repo}/pulls/${item.number}`,
					},
					repository: { full_name: `${item.owner}/${item.repo}` },
				})),
				extra,
			);
		}
		if (path === "search/issues") {
			const q = new URLSearchParams(query).get("q") ?? "";
			const key = Object.keys(this.search).find((name) => q.includes(name));
			return this.#respond(200, {
				items: (key ? this.search[key]! : []).map((item) => ({
					number: item.number,
					repository_url: `https://api.github.com/repos/${item.owner}/${item.repo}`,
					pull_request: {},
				})),
			});
		}
		if (path === "graphql") {
			const variables = (body.variables ?? {}) as Record<string, unknown>;
			if (String(body.query).includes("resolveReviewThread")) {
				const id = String(variables.threadId);
				for (const pull of this.pulls.values()) if ([...pull.threads.values()].includes(id)) pull.resolved.add(id);
				return this.#respond(200, { data: { resolveReviewThread: { thread: { id, isResolved: true } } } });
			}
			const pull = this.pulls.get(`${variables.owner}/${variables.repo}#${variables.number}`);
			const nodes = [...(pull?.threads ?? [])].map(([commentId, id]) => ({
				id,
				isResolved: pull!.resolved.has(id),
				comments: { nodes: [{ databaseId: commentId }] },
			}));
			return this.#respond(200, {
				data: {
					repository: {
						pullRequest: { reviewThreads: { pageInfo: { hasNextPage: false, endCursor: null }, nodes } },
					},
				},
			});
		}
		const repository = /^repos\/([^/]+)\/([^/]+)$/.exec(path);
		if (repository)
			return this.#respond(200, {
				full_name: `${repository[1]}/${repository[2]}`,
				permissions: { pull: true, push: this.canPush, admin: false },
			});
		const membership = /^orgs\/([^/]+)\/teams\/([^/]+)\/memberships\/([^/]+)$/.exec(path);
		if (membership) {
			const members = this.teams[`${membership[1]}/${membership[2]}`];
			if (!members) return this.#respond(403, { message: "Must have admin rights or read:org" });
			return members.includes(membership[3]!)
				? this.#respond(200, { state: "active", role: "member" })
				: this.#respond(404, { message: "Not Found" });
		}
		const match = /^repos\/([^/]+)\/([^/]+)\/(pulls|issues|commits)\/([^/]+)(?:\/(.*))?$/.exec(path);
		if (!match) return this.#respond(404, { message: "Not Found" });
		const [, owner, repo, kind, id, rest = ""] = match;
		if (kind === "commits") {
			const pull = [...this.pulls.values()].find(
				(item) => item.owner === owner && item.repo === repo && item.headSha === id,
			);
			return this.#respond(200, { check_runs: pull?.checkRuns ?? [] });
		}
		const pull = this.pulls.get(`${owner}/${repo}#${id}`);
		if (!pull) return this.#respond(404, { message: "Not Found" });
		if (kind === "issues") {
			if (rest === "events") return list(pull.events);
			if (rest === "comments" && method === "GET")
				return list(pull.issueComments.map((item) => ({ ...item, user: { login: item.user } })));
			if (rest === "comments" && method === "POST") {
				const comment: FakeComment = {
					id: this.nextId(),
					user: account.login,
					body: String(body.body),
					created_at: stamp,
				};
				pull.issueComments.push(comment);
				return this.#respond(201, { id: comment.id });
			}
			return this.#respond(404, { message: "Not Found" });
		}
		const comments = (items: FakeComment[]) => list(items.map((item) => ({ ...item, user: { login: item.user } })));
		if (rest === "" && method === "GET") {
			if ((headers.accept ?? "").includes("diff")) return this.#respond(200, pull.diff);
			const key = `${owner}/${repo}#${id}`;
			const count = (this.#pullReads.get(key) ?? 0) + 1;
			this.#pullReads.set(key, count);
			this.onPullRead?.(pull, count);
			return this.#respond(200, this.#pullJson(pull));
		}
		if (rest === "reviews" && method === "GET")
			return list(pull.reviews.map((item) => ({ ...item, user: { login: item.user } })));
		if (rest === "reviews" && method === "POST") {
			const review: FakeReview = {
				id: this.nextId(),
				user: account.login,
				state: REVIEW_STATES[String(body.event)] ?? String(body.event),
				commit_id: String(body.commit_id),
				submitted_at: stamp,
				body: String(body.body),
			};
			pull.reviews.push(review);
			pull.requestedReviewers = pull.requestedReviewers.filter((login) => login !== account.login);
			for (const item of (body.comments ?? []) as Array<{ path: string; line: number; body: string }>) {
				if (!this.keepComment(item)) continue;
				const comment: FakeComment = {
					id: this.nextId(),
					user: account.login,
					body: item.body,
					created_at: stamp,
					path: item.path,
					line: item.line,
					pull_request_review_id: review.id,
				};
				pull.reviewComments.push(comment);
				pull.threads.set(comment.id, `PRRT_${comment.id}`);
			}
			return this.#respond(200, { id: review.id });
		}
		const ofReview = /^reviews\/(\d+)\/comments$/.exec(rest);
		if (ofReview)
			return comments(pull.reviewComments.filter((item) => item.pull_request_review_id === Number(ofReview[1])));
		if (rest === "comments" && method === "GET") return comments(pull.reviewComments);
		if (rest === "comments" && method === "POST") {
			const comment: FakeComment = {
				id: this.nextId(),
				user: account.login,
				body: String(body.body),
				created_at: stamp,
				path: String(body.path),
				line: Number(body.line),
			};
			pull.reviewComments.push(comment);
			pull.threads.set(comment.id, `PRRT_${comment.id}`);
			return this.#respond(201, { id: comment.id });
		}
		return this.#respond(404, { message: "Not Found" });
	}

	#git(argv: readonly string[]): RunResult {
		const ok = (stdout = ""): RunResult => ({ code: 0, stdout, stderr: "" });
		// Skip `-c key=value` and `-C dir` to the subcommand.
		let index = 1;
		while (argv[index] === "-c" || argv[index] === "-C") index += 2;
		const [command, ...args] = argv.slice(index);
		const pullOf = (number: string | undefined) =>
			[...this.pulls.values()].find((item) => String(item.number) === number);
		if (command === "clone") {
			if (this.cloneFails) return { code: 128, stdout: "", stderr: "fatal: repository not found" };
			const target = args.at(-1)!;
			mkdirSync(target, { recursive: true });
			writeFileSync(join(target, "HEAD"), "ref: refs/heads/main\n");
			return ok();
		}
		if (command === "fetch") return ok();
		if (command === "rev-parse") return ok(`${pullOf(/pull\/(\d+)$/.exec(args[0] ?? "")?.[1])?.headSha ?? ""}\n`);
		if (command === "merge-base" && args[0] === "--is-ancestor") {
			const pull = [...this.pulls.values()].find((item) => item.headSha === args[2]);
			return pull?.ancestors.includes(args[1]!) ? ok() : { code: 1, stdout: "", stderr: "" };
		}
		if (command === "merge-base")
			return ok(`${[...this.pulls.values()].find((item) => item.headSha === args[1])?.baseSha ?? ""}\n`);
		if (command === "worktree") {
			if (args[0] === "add") mkdirSync(args.at(-2)!, { recursive: true });
			return ok();
		}
		if (command === "diff") {
			const [from, to] = args.filter((arg) => /^[0-9a-f]{40}$/.test(arg));
			const pull = [...this.pulls.values()].find((item) => item.headSha === to);
			return ok(pull?.diffs[`${from}..${to}`] ?? pull?.diff ?? "");
		}
		return ok();
	}
}

export function engineResult(partial: Partial<EngineResult> = {}): EngineResult {
	return {
		complete: true,
		label: "o/r#1",
		files: 1,
		added: 1,
		removed: 1,
		findings: [],
		alsoRaised: [],
		earlier: [],
		dropped: { rejected: 0, duplicates: 0 },
		timing: { totalMs: 42_000, scopeMs: 100, findMs: 30_000, verifyMs: 11_000 },
		usage: { inputTokens: 90_000, outputTokens: 4_000, costUsd: 0.31, frames: 12, tokens: 94_000, budget: 300_000 },
		model: "stub/frames",
		verifyModel: "stub/frames",
		notChecked: [],
		incomplete: [],
		diffLines: { "calc.py": [[1, 6]] },
		...partial,
	};
}

export const MAJOR = {
	file: "calc.py",
	line: 4,
	severity: "major",
	category: "correctness",
	claim: "total() skips the last item.",
	why: "range(len(items) - 1) stops one short.",
	suggestedFix: "Use range(len(items)).",
	replacement: "    for i in range(len(items)):",
	verification: "confirmed",
	confidence: 0.9,
} as const;

/** A scripted engine: each review takes the next result (the last one repeats) and records its spec. */
export class FakeEngine implements ReviewEngine {
	readonly specs: Array<EngineSpec & { diff: string; earlierDiff?: string }> = [];
	results: Array<EngineResult | Error>;
	onReview: (() => void | Promise<void>) | undefined;
	active = 0;
	maxActive = 0;

	constructor(...results: Array<EngineResult | Error>) {
		this.results = results.length ? results : [engineResult()];
	}

	async review(spec: EngineSpec): Promise<EngineResult> {
		this.specs.push({
			...spec,
			diff: spec.diffPath ? readFileSync(spec.diffPath, "utf8") : "",
			...(spec.earlierDiffPath ? { earlierDiff: readFileSync(spec.earlierDiffPath, "utf8") } : {}),
		});
		this.active += 1;
		this.maxActive = Math.max(this.maxActive, this.active);
		try {
			await this.onReview?.();
			const next = this.results.length > 1 ? this.results.shift()! : this.results[0]!;
			if (next instanceof Error) throw next;
			return next;
		} finally {
			this.active -= 1;
		}
	}

	async close(): Promise<void> {}
}
