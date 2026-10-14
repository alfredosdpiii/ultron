// Records the GitHub side of `ultron autoreview` (packages/coding-agent/src/ultron/autoreview: accounts.ts,
// github.ts, local.ts, state.ts, reviewer.ts) to fixtures/github.json: pure functions over recorded inputs, and the
// GitHub client over scripted `gh api -i` replies (every call's argv, environment and stdin recorded). No process
// is run and nothing touches GitHub. The Rust tests (tests/github_fixtures.rs) replay every case exactly.
// Usage: node crates/review/fixtures/generate_github.mjs (deterministic; the sha256 is stable across runs).
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
process.env.TZ = "UTC";
const load = (name) => import(pathToFileURL(join(root, "packages/coding-agent/src/ultron/autoreview", name)).href);
const accounts = await load("accounts.ts");
const github = await load("github.ts");
const local = await load("local.ts");
const plan = await load("plan.ts");
const reviewer = await load("reviewer.ts");
const state = await load("state.ts");

// ---- accounts ----
const auth = [
	[JSON.stringify({ hosts: { "github.com": [{ login: "bot", host: "github.com", state: "success", active: true },
		{ login: "second", state: "success", active: false }], "ghe.example.com": [{ login: "corp", state: "success" }] } }), ""],
	["", "github.com\n  ✓ Logged in to github.com account octocat (keyring)\n  ✓ Logged in to ghe.io as hubot (oauth)\n"],
	[JSON.stringify({ hosts: { "github.com": [{ login: "a", state: "failed" }, { state: "success" }, { login: 7 },
		{ login: "a" }, { login: "a", active: true }, { login: "b", active: "yes" }, "text", 5] } }), ""],
	["null", "Logged in to github.com account fallback (keyring)"],
	[JSON.stringify({ hosts: { "a.com": [{ login: "x" }, null] } }), "Logged in to b.com as y\nLogged in to a.com as x"],
	["", ""], ["5", "Logged in to h account z"], ["[]", ""], [JSON.stringify({ hosts: "ab" }), ""],
	[JSON.stringify({ hosts: [[{ login: "z" }]] }), ""], [JSON.stringify({ hosts: null }), ""], [JSON.stringify({}), ""],
	[JSON.stringify({ hosts: { h: { login: "obj" } } }), ""],
	["not json\nLogged in to github.com account first", "Logged in to github.com account first\nLogged in to  as nobody"],
	[" \n" + JSON.stringify({ hosts: { h: [{ login: "spaced" }] } }) + "\n", ""],
].map(([stdout, stderr]) => ({ stdout, stderr, accounts: accounts.parseAuthStatus(stdout, stderr) }));
const authEnv = [["bot", "github.com"], ["corp", "ghe.example.com"], ["x", "acme.ghe.com"], ["y", "ghe.com"]].map(
	([login, host]) => ({ login, host, env: accounts.authEnv({ login, host }, "tok") }));

// ---- github.ts pure parts ----
const apiOutputs = [
	'HTTP/2.0 200 OK\r\nEtag: "abc"\r\nX-Poll-Interval: 60\r\n\r\n{"a":1}', "HTTP/2.0 304 Not Modified\nLast-Modified: x\n\n",
	"gh: connection refused", "HTTP/1.1 200\n\n", "HTTP/1.1 200 OK\nno colon here\n:empty name\nA: b: c\na: again\n\nbody\n\nmore",
	"HTTP/2.0 200 OK\r\nA: b", "HTTP/2 404 Not Found\r\n\r\n{}", "HTTP/2.0 2000 OK\n\nx", "HTTP/2.0 99 x\n\n",
	"HTTP/2.0 200 OK\n\r\nbody after a CR blank line", "HTTP/2.0 201 Created\r\n  Spaced  :  value  \r\n\r\n",
	" HTTP/2.0 200 OK\n\n", "HTTP/1.0 500 Internal\r\nRetry-After: 5\r\n\r\n<html>", "HTTP/2.0 200 OK\n\n\n\nlead",
];
const parsedApi = apiOutputs.map((stdout) => ({ stdout, parsed: github.parseApiOutput(stdout) }));
const urls = [
	["https://api.github.com/repos/o/r/pulls/7", "github.com"], ["https://api.github.com/repos/o/r/issues/12", "ghe.io"],
	["https://github.com/o/r/pull/3/files", "github.com"], ["https://github.com/o/r/pull/3?x=1#y", "h"],
	["https://github.com/o/r/pulls/abc", "h"], ["o/r/pulls/5", "h"], ["https://api.github.com/repos/o/r/commits/5", "h"],
	["https://x/a/b/pull/007/", "h"], ["https://x/a/b/pull/8\nmore", "h"], ["https://x/a/b/pull/9/ok tail", "h"],
	["/pulls/1", "h"], ["https://x/a/b/pulls/1/x/y/z", "h"],
].map(([url, host]) => ({ url, host, ref: github.pullRefFromUrl(url, host) ?? null }));
const targets = [
	["o/r#12"], ["https://ghe.io/o/r/pull/7/files"], ["12"], ["o/r#12", "ghe.io"], ["https://github.com/o/r/pull/1"],
	["http://h/o/r/pull/2?x"], ["o.x/r_y-z#3"], ["o/r #3"], ["o/r#"], ["https://github.com/o/r/pulls/1"], ["o/r/x#1"],
	["https://github.com/o/r/pull/1\nx"], ["github.com/o/r/pull/1"],
].map(([target, host]) => ({ target, host: host ?? null, ref: (host ? github.parsePullTarget(target, host) : github.parsePullTarget(target)) ?? null }));
const pullKeys = [{ host: "github.com", owner: "o", repo: "r", number: 1 }, { host: "h", owner: "a-b", repo: "c.d", number: 99 }]
	.map((ref) => ({ ref, key: github.pullKey(ref) }));

// ---- the GitHub client over scripted replies ----
const reply = (status, body, headers = {}) => {
	const head = [`HTTP/2.0 ${status} ${status < 400 ? "OK" : "Error"}`, ...Object.entries(headers).map(([n, v]) => `${n}: ${v}`)];
	const text = typeof body === "string" ? body : body === undefined ? "" : JSON.stringify(body);
	return { code: status < 400 ? 0 : 1, stdout: `${head.join("\r\n")}\r\n\r\n${text}`, stderr: status < 400 ? "" : `gh: HTTP ${status}` };
};
const NOW = Date.parse("2026-10-04T10:00:00Z");
const pullJson = { number: 1, title: "T", body: "B", state: "open", merged: false, merged_at: null, draft: true,
	user: { login: "Renovate[BOT]", type: "User" }, head: { sha: "h".repeat(40) }, base: { sha: "b".repeat(40), ref: "main" },
	html_url: "https://github.com/o/r/pull/1", created_at: "2026-10-01T00:00:00Z",
	requested_reviewers: [{ login: "bot" }, null, { login: 5 }], requested_teams: [{ slug: "core" }, {}] };
const reviewsPage = Array.from({ length: 100 }, (_, i) => ({ id: i + 1, user: { login: `u${i}` }, state: "COMMENTED",
	commit_id: "c", submitted_at: `2026-10-01T00:00:${String(i % 60).padStart(2, "0")}Z`, body: `r${i}` }));
const thread = (id, db, resolved, up, down, total) => ({ id, isResolved: resolved, comments: { totalCount: total,
	nodes: [{ databaseId: db, reactionGroups: [{ content: "THUMBS_UP", reactors: { totalCount: up } },
		{ content: "THUMBS_DOWN", reactors: { totalCount: down } }] }] } });
const REF = { host: "github.com", owner: "o", repo: "r", number: 1 };
const client = [
	["pull", [REF], [reply(200, pullJson)]],
	["pull", [REF], [reply(200, { ...pullJson, state: "closed", merged_at: "2026-10-02T00:00:00Z", draft: false,
		user: { login: "dependabot", type: "Bot" }, requested_reviewers: "x", head: null })]],
	["pull", [REF], [reply(200, "not json")]],
	["pull", [REF], [reply(403, { message: "secondary rate limit" }, { "Retry-After": "120" })]],
	["pull", [REF], [reply(403, { message: "API rate limit exceeded" }, { "X-RateLimit-Remaining": "0", "X-RateLimit-Reset": String(NOW / 1000 + 600) })]],
	["pull", [REF], [reply(403, { message: "API rate limit exceeded" }, { "X-RateLimit-Remaining": "0", "X-RateLimit-Reset": String(NOW / 1000 - 600) })]],
	["pull", [REF], [reply(403, { message: "Resource not accessible" })]],
	["pull", [REF], [reply(429, {})]],
	["pull", [REF], [reply(403, { message: "You have triggered an Abuse detection mechanism" }, { "Retry-After": "0" })]],
	["pull", [REF], [reply(403, { message: "x" }, { "Retry-After": " 1.5 " })]],
	["pull", [REF], [reply(403, { message: "x" }, { "Retry-After": "0x10" })]],
	["pull", [REF], [reply(403, { message: "x" }, { "Retry-After": "soon", "X-RateLimit-Remaining": "1", "X-RateLimit-Reset": "99" })]],
	["pull", [REF], [{ code: 1, stdout: "", stderr: `  gh: connection refused ${"x".repeat(400)}` }]],
	["pull", [REF], [{ code: 1, stdout: "garbage only", stderr: "" }]],
	["pull", [REF], [reply(500, "<html>")]],
	["pull", [REF], [reply(404, { message: `Not Found ${"y".repeat(500)}` })]],
	["pullDiff", [REF], [reply(200, "diff --git a/x b/x\n")]],
	["reviews", [REF], [reply(200, reviewsPage), reply(200, [{ id: "101", user: null, state: 3, commit_id: "c2", body: "last" }, { id: null }])]],
	["issueComments", [REF], [reply(200, [{ id: 1, user: { login: "a" }, body: "b", created_at: "t1" },
		{ id: 2, user: { login: "b" }, body: "c", created_at: "t2", updated_at: "t3", path: "f.py", original_line: 4, pull_request_review_id: 9 },
		{ id: 3, user: { login: "c" }, body: 5, created_at: "t4", path: 7, line: 8, original_line: 2 }])]],
	["reviewComments", [{ ...REF, owner: "x", repo: "y", number: 9 }], [reply(200, "not a list")]],
	["lastReviewRequestAt", [REF], [reply(200, [{ event: "review_requested", created_at: "2026-10-01T00:00:00Z", requested_reviewer: { login: "BOT" } },
		{ event: "review_requested", created_at: "2026-10-03T00:00:00Z", requested_reviewer: { login: "other" } },
		{ event: "review_requested", created_at: "2026-10-02T00:00:00Z", requested_reviewer: { login: "bot" } },
		{ event: "review_request_removed", created_at: "2026-10-05T00:00:00Z", requested_reviewer: { login: "bot" } }])]],
	["lastReviewRequestAt", [REF], [reply(200, [])]],
	["inTeam", ["o", "core"], [reply(200, { state: "active" })]],
	["inTeam", ["o", "core"], [reply(200, { state: "pending" })]],
	["inTeam", ["o", "core"], [reply(404, { message: "Not Found" })]],
	["inTeam", ["o", "core"], [reply(403, { message: "Must have admin rights" })]],
	["inTeam", ["o", "core"], [reply(429, { message: "slow down" }, { "Retry-After": "7" })]],
	["canPush", [REF], [reply(200, { permissions: { push: false, maintain: true } })]],
	["canPush", [REF], [reply(200, { permissions: { push: "true" } })]],
	["canPush", [REF], [reply(404, { message: "Not Found" })]],
	["canPush", [REF], [reply(403, { message: "rate limit" })]],
	["checkSummary", [REF, "abc"], [reply(200, { check_runs: [
		{ name: "a", status: "completed", conclusion: "success" }, { name: "b", status: "completed", conclusion: "neutral" },
		{ name: "c", status: "completed", conclusion: "skipped" }, { name: "d", status: "queued", conclusion: null },
		...["e", "f", "g", "h", "i", "j", "k"].map((name) => ({ name, status: "completed", conclusion: name === "k" ? null : "failure" }))] })]],
	["checkSummary", [REF, "abc"], [reply(200, { check_runs: [] })]],
	["checkSummary", [REF, "abc"], [reply(200, { check_runs: [{ name: "only", status: "in_progress" }] })]],
	["checkSummary", [REF, "abc"], [reply(500, { message: "boom" })]],
	["postComment", [REF, "hello \"world\"\n"], [reply(201, { id: 55 })]],
	["postReview", [REF, { commit_id: "h", event: "COMMENT", body: "b", comments: [{ path: "f", line: 3, side: "RIGHT", body: "x" },
		{ path: "g", line: 9, side: "RIGHT", start_line: 7, start_side: "RIGHT", body: "y" }] }], [reply(200, { id: 77 })]],
	["postReview", [REF, { commit_id: "h", event: "APPROVE", body: "b", comments: [] }], [reply(422, { message: "Unprocessable" })]],
	["commentsOfReview", [REF, 77], [reply(200, [{ id: 1, user: { login: "bot" }, body: "m", created_at: "t", line: 3, pull_request_review_id: 77 }])]],
	["postReviewComment", [REF, "h", { path: "f", line: 3, side: "RIGHT", body: "x" }], [reply(201, { id: 88 })]],
	["graphql", ["query{x}", { a: 1 }], [reply(200, { data: { x: 1 } })]],
	["graphql", ["query{x}", {}], [reply(200, { errors: [{ message: `bad ${"z".repeat(400)}` }], data: { x: 1 } })]],
	["graphql", ["query{x}", {}], [reply(200, { errors: [], data: null })]],
	["reviewThreads", [REF], [
		reply(200, { data: { repository: { pullRequest: { reviewThreads: { pageInfo: { hasNextPage: true, endCursor: "C1" },
			nodes: [thread("T1", 11, true, 2, 0, 3), thread("T2", 12, false, 0, 1, 1), { id: 5, comments: { nodes: [{ databaseId: 1 }] } },
				{ id: "T3", comments: { nodes: [{ databaseId: "13" }] } }, { id: "T4", isResolved: "yes", comments: { totalCount: 0, nodes: [{ databaseId: 14, reactionGroups: "x" }] } }] } } } } }),
		reply(200, { data: { repository: { pullRequest: { reviewThreads: { pageInfo: { hasNextPage: true, endCursor: null },
			nodes: [thread("T5", 15, false, 1, 1, 2)] } } } } })]],
	["reviewThreads", [REF], [reply(200, { data: { repository: null } })]],
	["resolveThread", ["PRRT_1"], [reply(200, { data: { resolveReviewThread: { thread: { id: "PRRT_1", isResolved: true } } } })]],
	["notifications", [{ since: "2026-10-04T09:00:00.000Z" }], [reply(200, [
		{ reason: "review_requested", updated_at: "u1", subject: { type: "PullRequest", url: "https://api.github.com/repos/o/r/pulls/1" } },
		{ reason: "mention", subject: { type: "Issue", url: "https://api.github.com/repos/o/r/issues/2" } },
		{ reason: "subscribed", subject: { type: "PullRequest", url: "https://api.github.com/repos/o/r/pulls/3" } },
		{ reason: "team_mention", updated_at: "u4", subject: { type: "PullRequest", url: "https://api.github.com/repos/o/r/pulls/4" } },
		{ reason: "mention", updated_at: "u5", subject: { type: "PullRequest", url: "nope" } }],
		{ "Last-Modified": "Sun, 04 Oct 2026 10:00:00 GMT", ETag: 'W/"e1"', "X-Poll-Interval": "60" })]],
	["notifications", [{ since: "a b&c=d/é", lastModified: "LM", etag: "E" }], [reply(304, undefined, { "X-Poll-Interval": "0" })]],
	["notifications", [{ since: "s", lastModified: "LM" }], [reply(304, undefined, { "Last-Modified": "LM2", "X-Poll-Interval": "30.5" })]],
	["notifications", [{ since: "s", etag: "" , lastModified: "" }], [reply(200, { not: "a list" }, { ETag: "" })]],
	["search", ["is:pr review-requested:bot archived:false"], [reply(200, { items: [
		{ number: 5, repository_url: "https://api.github.com/repos/o/r", pull_request: {} },
		{ number: 6, repository_url: "https://api.github.com/repos/o/r" },
		{ number: "7", repository_url: "https://api.github.com/repos/a/b", pull_request: null },
		{ repository_url: "https://api.github.com/repos/a/b", pull_request: {} }] })]],
	["list", ["repos/o/r/things?state=all", 2], [reply(200, reviewsPage), reply(200, reviewsPage), reply(200, [])]],
	["list", ["repos/o/r/things", 5], [reply(200, [])]],
	["requestWithHeaders", ["GET", "x", { headers: { Accept: "a", "X-Y": "z" }, allow: [418] }], [reply(418, { message: "teapot" })]],
];
const serialize = (value) => {
	if (value instanceof Map) return [...value.entries()];
	return value === undefined ? null : value;
};
const clientCases = [];
for (const [method, args, replies] of client) {
	const calls = [];
	const queue = [...replies];
	const runner = async (argv, options = {}) => {
		calls.push({ argv: [...argv], env: { ...options.env }, input: options.input ?? null });
		return queue.shift() ?? { code: 1, stdout: "", stderr: "no reply" };
	};
	const account = { login: "bot", host: method === "pull" ? "github.com" : "ghe.io" };
	const gh = new github.GitHub(runner, account, async () => ({ GH_TOKEN: "t", GH_HOST: account.host }), { now: () => NOW });
	let result = null;
	let error = null;
	try {
		if (method === "requestWithHeaders") result = await gh.request(...args);
		else if (method === "notifications") result = await gh.notifications(args[0]);
		else result = serialize(await gh[method](...args));
	} catch (caught) {
		error = { name: caught.name, message: caught.message, status: caught.status ?? null, resumeAt: caught.resumeAt ?? null };
	}
	clientCases.push({ method, args, account, replies, calls, result, error });
}

// ---- local.ts ----
const REFS = [REF, { host: "GitHub.com", owner: "O", repo: "R", number: 1 }, { host: "ghe.io", owner: "team", repo: "svc", number: 2 }];
const remotes = ["https://github.com/o/r.git", "https://token@github.com/O/R", "git@github.com:o/r.git", "ssh://git@github.com:22/o/r",
	"https://github.com/o/r-fork.git", "https://github.com/someone/r.git", "https://gitlab.com/o/r.git", "not a url",
	"  git@ghe.io:team/svc  ", "https://ghe.io/team/svc/", "git://github.com/o/r.git/", "github.com:o/r", "https://github.com/o/r.git.git",
	"https://github.com/o/r\n", "ssh://github.com/o/r.git", "https://u:p@github.com:8443/o/r", "git@github.com:/o/r.git", ""];
const remoteCases = remotes.flatMap((url) => REFS.map((ref) => ({ url, ref, matches: local.remoteMatches(url, ref) })));
const NAMES = ["house-rules.md", "app.md", "/home/u/guides/house-rules.md", "/home/u/guides/more/app.md", "abc", "Rules.MD", "a.b", "x+y.md"];
const mentionTexts = ["As house-rules.md says, hash the id.", "see /home/u/guides/more/app.md", "webapp.md and app.mdx are fine; so is the app module.",
	"APP.MD.", "(app.md)", "app.md-x", "app.md_x", "app.md.x", ".app.md", "-app.md", "abc and ABC", "RULES.md!", "a.b.c a.b", "x+y.md here",
	"/HOME/U/GUIDES/HOUSE-RULES.MD", "app.md\n", "line\napp.md", ""];
const guideCases = mentionTexts.map((text) => ({ text, names: NAMES, found: local.guideMentions(text, NAMES) }));
const expandCases = [["~", "/home/u"], ["~/code", "/home/u"], [" ~/a/../b ", "/h"], ["/abs/./x", "/h"], ["~/", "/h"]]
	.map(([path, home]) => ({ path, home, expanded: local.expandPath(path, home) }));

const MAJOR = { file: "calc.py", line: 4, severity: "major", category: "correctness", claim: "total() skips the last item.",
	why: "range(len(items) - 1) stops one short.", suggestedFix: "Use range(len(items)).", verification: "confirmed", confidence: 0.9 };
const engineResult = (partial = {}) => ({ complete: true, label: "o/r#1", files: 1, added: 1, removed: 1, findings: [], alsoRaised: [],
	earlier: [], dropped: { rejected: 0, duplicates: 0 }, timing: { totalMs: 42000, scopeMs: 100, findMs: 30000, verifyMs: 11000 },
	usage: { inputTokens: 90000, outputTokens: 4000, costUsd: 0.31, frames: 12, tokens: 94000, budget: 300000 }, model: "stub/frames",
	verifyModel: "stub/frames", notChecked: [], incomplete: [], diffLines: { "calc.py": [[1, 6]] }, ...partial });
const HEAD = "a".repeat(40);
const guideNames = ["house-rules.md", "app.md", "/home/u/guides/house-rules.md", "/home/u/guides/more/app.md"];
const planInputs = [
	{ result: engineResult({ findings: [{ ...MAJOR, claim: "The id is logged unhashed, which house-rules.md forbids" }, { ...MAJOR, line: 5, claim: "The total is wrong" }],
		assurance: ["The retry path was checked. It follows the rule in app.md. The tests pin it."] }), names: guideNames },
	{ result: engineResult({ findings: [{ ...MAJOR, claim: "Per app.md! the cache is stale?  Really. Yes" }],
		assurance: ["Line one is fine.", "See /home/u/guides/house-rules.md for more.   Then this.\tAnd that? app.md"] }), names: guideNames },
	{ result: engineResult({ findings: [MAJOR] }), names: guideNames },
	{ result: engineResult({ findings: [MAJOR] }), names: [] },
	{ result: engineResult({ findings: [{ ...MAJOR, line: 3, claim: "app.md" }, { ...MAJOR, line: 2, claim: "fine" }, { ...MAJOR, line: 1, claim: "house-rules.md" }] }), names: ["app.md", "house-rules.md"] },
];
const planCases = planInputs.map(({ result, names }) => {
	const options = { selfAuthored: false, state: "open", headSha: HEAD, signature: true };
	const planned = plan.planReview(result, options);
	const checked = local.withoutGuideMentions(planned, names);
	return { result, options, names, plan: planned, checked };
});

// ---- reviewer.ts ----
const BOT = "ultron-bot";
const mentionCases = [["cc @Ultron-Bot please look"], ["@ultron-bot"], ["mail me@ultron-bot.dev"], ["@ultron-bot-two"], ["@ultron-bot/team"],
	["(@ultron-bot)"], ["x/@ultron-bot"], ["_@ultron-bot"], ["-@ultron-bot"], ["@ultron-bot."], ["@ultron-bot_x"], ["@ultron-bot\n"],
	["first me@ultron-bot then @ultron-bot!"], ["@@ultron-bot"], ["é@ultron-bot"], ["@ultron-bot", "ULTRON-BOT"], ["@a.b", "a.b"], ["@axb", "a.b"],
	["@ultron-boté"], [""], ["@ultron-bo"], ["@ultron-bot @ultron-bot-x"]]
	.map(([body, login]) => ({ body, login: login ?? BOT, mentions: reviewer.mentions(body, login ?? BOT) }));
const view = (extra = {}) => ({ number: 1, title: "", body: "", state: "open", merged: false, draft: false, author: "alice",
	authorIsBot: false, headSha: HEAD, baseSha: "", baseRef: "main", url: "", createdAt: "2026-10-01T00:00:00Z",
	requestedReviewers: [], requestedTeams: [], ...extra });
const comment = (user, body, at, updated) => ({ id: 1, user, body, createdAt: at, updatedAt: updated ?? at });
const review = (user, body, at) => ({ id: 2, user, state: "COMMENTED", commitId: "c", submittedAt: at, body });
const latestCases = [
	[view({ body: `@${BOT} first` }), [], []],
	[view({ body: `@${BOT} first` }), [comment("bob", `@${BOT} again`, "2026-10-03T00:00:00Z"), comment(BOT, `@${BOT} self`, "2026-10-09T00:00:00Z")], []],
	[view({ body: "no mention" }), [], []],
	[view({ body: `@${BOT}`, author: "ULTRON-BOT" }), [comment("x", `@${BOT}`, "", "")], [review("y", `@${BOT}`, "2026-10-02T00:00:00Z"), review("z", `@${BOT}`, "")]],
	[view({ body: `@${BOT}`, createdAt: "" }), [comment("x", `hi @${BOT}`, "2026-10-01T00:00:00Z", "2026-10-05T00:00:00Z")], [review("y", `@${BOT}`, "2026-10-04T00:00:00Z")]],
].map(([pull, comments, reviews]) => ({ login: BOT, pull, comments, reviews, latest: reviewer.latestMention(BOT, pull, comments, reviews) ?? null }));
const OLD = "c".repeat(40);
const decideInputs = [];
const base = view();
const reviewed = { lastSha: HEAD, lastAt: "2026-10-03T00:00:00Z" };
const bot = view({ author: "dependabot[bot]", authorIsBot: true });
const draft = view({ draft: true });
const closed = view({ state: "closed", merged: true });
const closedOnly = view({ state: "closed" });
const moved = { lastSha: OLD, lastAt: reviewed.lastAt };
for (const input of [
	{ pull: base, requested: true }, { pull: base, requested: false, mentionAt: "2026-10-02T00:00:00Z" }, { pull: base, requested: false },
	{ pull: base, ...reviewed, requested: false }, { pull: base, ...reviewed, requested: false, mentionAt: "2026-10-02T00:00:00Z" },
	{ pull: base, ...reviewed, requested: false, mentionAt: "2026-10-03T12:00:00Z" },
	{ pull: base, ...reviewed, requested: true, requestedAt: "2026-10-03T12:00:00Z" },
	{ pull: base, ...reviewed, requested: true, requestedAt: "2026-10-02T12:00:00Z" },
	{ pull: base, ...reviewed, requested: true, requestedAt: null },
	{ pull: base, lastSha: HEAD, requested: true, requestedAt: "2026-10-03T12:00:00Z" },
	{ pull: base, ...moved, requested: false }, { pull: base, ...moved, requested: true },
	{ pull: base, ...moved, requested: false, mentionAt: "2026-10-03T12:00:00Z" }, { pull: base, ...moved, requested: false, mentionAt: "2026-10-02T12:00:00Z" },
	{ pull: base, ...moved, requested: false, lastRequestedChanges: true }, { pull: base, ...moved, requested: true, lastRequestedChanges: true },
	{ pull: bot, requested: true, skipBots: true }, { pull: bot, requested: true, skipBots: true, mentionAt: "2026-10-02T00:00:00Z" },
	{ pull: bot, requested: true, skipBots: false }, { pull: bot, ...moved, requested: false, skipBots: true, lastRequestedChanges: true },
	{ pull: draft, requested: true }, { pull: draft, requested: false, mentionAt: "2026-10-02T00:00:00Z" },
	{ pull: draft, ...moved, requested: false, lastRequestedChanges: true }, { pull: draft, requested: true, skipBots: true },
	{ pull: closed, requested: true }, { pull: closed, ...reviewed, requested: false, mentionAt: "2026-10-03T12:00:00Z" },
	{ pull: closedOnly, requested: false }, { pull: closedOnly, requested: false, mentionAt: "2026-10-01T00:00:00Z" },
	{ pull: base, lastSha: "", lastAt: "", requested: false, mentionAt: "" },
	{ pull: base, lastSha: "short", requested: false, lastRequestedChanges: true },
]) {
	let asked = 0;
	const decision = await reviewer.decide({ ...input, requestedAt: async () => { asked += 1; return input.requestedAt ?? undefined; } });
	const { requestedAt, ...rest } = input;
	decideInputs.push({ input: { ...rest, requestedAt: requestedAt ?? null }, decision, asked });
}
const ackCases = [
	["Peace in our time.", HEAD, ""], ["x", HEAD, "\n /\\_/\\\n( o.o )\n"], ["x", HEAD, "a\n```\n# injected\n````"],
	["x", HEAD, "#".repeat(70000)], ["x", HEAD, "   \n"], ["  multi\n line\t quote  ", "abc", "\r\n\r\nart\r\nrow  \r\n"],
	["x", HEAD, "`"], ["x", HEAD, "``"], ["x", HEAD, "``` and `````"], ["x", HEAD, "\n\n  indented\n\n"], ["", "", ""],
	["x", HEAD, "é".repeat(59900)], ["x", HEAD, "😀".repeat(29950)],
].map(([line, sha, art]) => ({ line, sha, art, body: reviewer.ackBody(line, sha, art) }));
const LINES = ["one", "two", "three", "four"];
const pickCases = [];
for (const [lines, previous] of [[LINES, undefined], [LINES, "two"], [LINES, "five"], [["only"], "only"], [[], undefined], [["a", "a"], "a"], [["a", "b", "a"], "a"]])
	for (const random of [0, 0.1, 0.25, 0.5, 0.74, 0.75, 0.9999, 0.99999999999])
		pickCases.push({ lines, previous: previous ?? null, random, line: reviewer.pickAckLine(lines, previous, () => random) });
const keys = [[{ login: "bot", host: "github.com" }, REF]].map(([account, ref]) => ({ account, ref, key: reviewer.pullStateKey(account, ref),
	repo: reviewer.repoKey({ ...ref, owner: "O", repo: "R" }) }));

// ---- state.ts ----
const claims = ["total() skips the last item.", "  Total()\tSKIPS\n the  last item. ", "", "Ünïcödé  ΣΑΣ", "a b c﻿"]
	.map((claim) => ({ claim, hash: state.claimHash(claim) }));

const out = { auth, authEnv, apiOutputs: parsedApi, urls, targets, pullKeys, client: clientCases, remotes: remoteCases, guides: guideCases,
	expand: expandCases, plans: planCases, mentions: mentionCases, latest: latestCases, decide: decideInputs, ack: ackCases,
	maxAckChars: reviewer.MAX_ACK_CHARS, pick: pickCases, keys, claims };
writeFileSync(join(here, "github.json"), `${JSON.stringify(out, null, 1)}\n`);
