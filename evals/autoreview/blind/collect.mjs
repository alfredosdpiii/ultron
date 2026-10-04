#!/usr/bin/env node
/**
 * Collector of the blind comparison benchmark: finds pull requests a reference reviewer reviewed, takes that
 * reviewer's first review with inline comments as ground truth, and builds a local repository per case holding the
 * real base and head commits, so the reviewer under test can review the same commit offline.
 *
 *   node evals/autoreview/blind/collect.mjs --account <gh login> --reviewer <login> --since <YYYY-MM-DD> --set <name>
 *                                           [--cases 30] [--seed s] [--max-changed-lines 600] [--max-files 15]
 *                                           [--min-comments 2] [--repos-max-share 0.25] [--min-interval-ms 1000]
 *                                           [--plan] [--offline]
 *
 * Read-only on GitHub: `gh api` GET requests and `git fetch`. The token comes from `gh auth token --user <account>`
 * and is passed to children only as `GH_TOKEN` in their environment. Everything is written under
 * `~/.cache/ultron-autoreview-bench/blind/<set>/` with owner-only permissions; nothing goes into the repository.
 *
 * Small footprint on the account's limits: requests are serial and `--min-interval-ms` apart, every answer is kept
 * on disk and never asked for again, pull requests are looked at only until the sample is full, an exhausted quota
 * is waited for, and a secondary (abuse) limit stops the collection (exit 3) instead of being retried. `--offline`
 * asks the API nothing and draws the sample from the pull requests already looked at. `--plan` builds no case and
 * writes no case file. An unknown flag exits 2. See ../README.md.
 */

import { createHash } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { git } from "../cases.mjs";
import { BENCH_HOME, log, parseArgs, readJson, run } from "../harness.mjs";
import {
	caseId,
	caseRecord,
	DEFAULT_CASES,
	DEFAULT_MAX_CHANGED_LINES,
	DEFAULT_MAX_FILES,
	DEFAULT_MIN_COMMENTS,
	DEFAULT_REPOS_MAX_SHARE,
	DEFAULT_SEED,
	DROP,
	diffShape,
	priorReviews,
	repoCap,
	sampleOrder,
	sampleShape,
	selectReferenceReview,
	sizeDropReason,
} from "./lib.mjs";

const VALUE_FLAGS = ["account", "reviewer", "since", "set", "cases", "seed", "max-changed-lines", "max-files", "min-comments", "repos-max-share", "min-interval-ms"];
const SWITCH_FLAGS = ["plan", "offline"];
const PER_PAGE = 100;
/** The search API returns at most this many results of one query. */
const SEARCH_LIMIT = 1000;
/** Requests are serial and at least this far apart, unless `--min-interval-ms` says otherwise. */
export const DEFAULT_MIN_INTERVAL_MS = 1000;
/** An exhausted quota is waited for at most this long; beyond it the collection stops. */
const MAX_QUOTA_WAIT_MS = 15 * 60_000;
const CREDENTIAL_HELPER = ["-c", "credential.helper=", "-c", "credential.helper=!gh auth git-credential"];

/** The directory of a set: cases, clones, evidence and reports. */
export function setDir(name, home = BENCH_HOME) {
	if (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(String(name ?? ""))) throw new Error(`--set must be a plain name, got ${JSON.stringify(name)}`);
	return join(home, "blind", name);
}

function privateDir(path) {
	mkdirSync(path, { recursive: true, mode: 0o700 });
	return path;
}

function writePrivate(path, text) {
	writeFileSync(path, text, { mode: 0o600 });
	chmodSync(path, 0o600);
}

const sleepFor = (ms) => new Promise((done) => setTimeout(done, ms));

// ---------------------------------------------------------------------------------------------------------------
// GitHub, read-only

/** Thrown when GitHub's abuse limit refuses a request: the collection stops, nothing is retried. */
export class RateLimitStop extends Error {}

/** `{ status, headers, body }` of `gh api --include` output: a status line, headers, a blank line, the body. */
export function parseResponse(stdout) {
	const text = String(stdout ?? "");
	const split = /\r?\n\r?\n/.exec(text);
	const head = split ? text.slice(0, split.index) : text;
	const lines = head.split(/\r?\n/);
	const status = Number(/^HTTP\/\S+ (\d{3})/.exec(lines[0] ?? "")?.[1] ?? 0);
	const headers = {};
	for (const line of lines.slice(1)) {
		const colon = line.indexOf(":");
		if (colon > 0) headers[line.slice(0, colon).trim().toLowerCase()] = line.slice(colon + 1).trim();
	}
	return { status, headers, body: status && split ? text.slice(split.index + split[0].length) : "" };
}

/**
 * A read-only GitHub client on the `gh` CLI, built to leave a small footprint on the account's limits.
 *
 * - One request at a time, at least `minIntervalMs` after the previous one.
 * - Every answer (also "not found") is kept in `cacheDir` and never asked for again.
 * - An exhausted quota (`x-ratelimit-remaining: 0`) waits for `x-ratelimit-reset`, at most `MAX_QUOTA_WAIT_MS`;
 *   `Retry-After` is honoured on server errors.
 * - A secondary (abuse) limit stops the collection with `RateLimitStop`. It is never retried.
 * - With `offline`, nothing is requested: an answer that is not in the cache throws `RateLimitStop`.
 *
 * `exec(command, args, { env })` runs a child and resolves to `{ code, stdout, stderr }` (injected in tests). The
 * token is fetched once with `gh auth token --user <account>` and reaches children only as `GH_TOKEN` in their
 * environment: never argv, never a file, never a log line.
 */
export async function githubClient({ account, exec = run, sleep = sleepFor, logger = log, env = process.env, now = Date.now, cacheDir = null, minIntervalMs = DEFAULT_MIN_INTERVAL_MS, offline = false }) {
	const auth = await exec("gh", ["auth", "token", "--user", account], { env });
	const token = auth.code === 0 ? auth.stdout.trim() : "";
	if (!token) throw new Error(`gh has no token for account ${account}: run \`gh auth login\` for it`);
	const childEnv = { ...env, GH_TOKEN: token, GH_PROMPT_DISABLED: "1" };
	delete childEnv.GITHUB_TOKEN;
	const clean = (text) => String(text ?? "").split(token).join("[token]");
	const counts = { requests: 0, cached: 0 };
	/** The last `x-ratelimit-*` seen per resource (core, search): `{ remaining, reset }`. */
	const quota = new Map();
	let lastAt = null;
	let queue = Promise.resolve();

	const fail = (endpoint, status) => {
		const error = new Error(`GET ${endpoint.replace(/^repos\/[^/]+\/[^/]+/, "repos/<repo>")} failed (HTTP ${status || "?"})`);
		error.status = status;
		return error;
	};

	async function waitForQuota(resource) {
		const seen = quota.get(resource);
		if (!seen || seen.remaining > 0) return;
		const wait = seen.reset * 1000 - now() + 2000;
		if (wait <= 0) return;
		if (wait > MAX_QUOTA_WAIT_MS) throw new RateLimitStop(`The ${resource} quota of this account is used up and resets in ${Math.ceil(wait / 60_000)} minutes. Stopped; what was fetched is kept, run the same command again after that.`);
		logger(`${resource} quota used up: waiting ${Math.ceil(wait / 1000)}s for its reset`);
		await sleep(wait);
		quota.delete(resource);
	}

	async function request(endpoint, params) {
		const file = cacheDir ? join(cacheDir, `${createHash("sha256").update(JSON.stringify([endpoint, params])).digest("hex")}.json`) : null;
		const kept = file ? readJson(file) : null;
		if (kept) {
			counts.cached++;
			if (kept.status !== 200) throw fail(endpoint, kept.status);
			return kept.body;
		}
		if (offline) throw new RateLimitStop("--offline: an answer that is not kept under the set directory would be needed.");
		const args = ["api", "--include", "-X", "GET", endpoint, ...Object.entries(params).flatMap(([name, value]) => ["-f", `${name}=${value}`])];
		const resource = endpoint.startsWith("search/") ? "search" : "core";
		for (let attempt = 1; ; attempt++) {
			await waitForQuota(resource);
			if (lastAt !== null && now() - lastAt < minIntervalMs) await sleep(minIntervalMs - (now() - lastAt));
			counts.requests++;
			const result = await exec("gh", args, { env: childEnv });
			lastAt = now();
			const { status, headers, body } = parseResponse(result.stdout);
			if (headers["x-ratelimit-remaining"] !== undefined) quota.set(headers["x-ratelimit-resource"] ?? resource, { remaining: Number(headers["x-ratelimit-remaining"]), reset: Number(headers["x-ratelimit-reset"] ?? 0) });
			if (status === 200) {
				const value = JSON.parse(body);
				if (file) writePrivate(file, JSON.stringify({ status, body: value }));
				return value;
			}
			const retryAfter = Number(headers["retry-after"] ?? Number.NaN);
			const limited = (status === 403 || status === 429) && (Number.isFinite(retryAfter) || /rate limit|abuse|scraping/i.test(body));
			if (limited && headers["x-ratelimit-remaining"] === "0") continue; // the quota: wait for its reset, then ask again
			if (limited) {
				throw new RateLimitStop(
					`GitHub refused a request with its secondary rate limit (HTTP ${status})${Number.isFinite(retryAfter) ? `, asking to wait ${retryAfter}s` : ""}. Stopped without retrying; what was fetched is kept. Wait until the limit has cleared, then run the same command again (a higher --min-interval-ms slows it down).`,
				);
			}
			if (status >= 400 && status < 500) {
				if (file) writePrivate(file, JSON.stringify({ status }));
				throw fail(endpoint, status);
			}
			if (attempt >= 3) throw fail(endpoint, status);
			const wait = Number.isFinite(retryAfter) ? retryAfter * 1000 : 5000 * attempt;
			logger(`GitHub error (HTTP ${status || "?"}): waiting ${Math.round(wait / 1000)}s (attempt ${attempt})`);
			await sleep(wait);
		}
	}

	/** GET `endpoint` with query `params`. Returns the parsed body, or throws an error carrying `status`. */
	function get(endpoint, params = {}) {
		const next = queue.then(() => request(endpoint, params));
		queue = next.catch(() => {});
		return next;
	}

	/** Every item of a paginated list endpoint. */
	async function list(endpoint, params = {}) {
		const out = [];
		for (let page = 1; ; page++) {
			const items = await get(endpoint, { ...params, per_page: PER_PAGE, page });
			out.push(...items);
			if (items.length < PER_PAGE) return out;
		}
	}

	return { get, list, env: childEnv, clean, counts };
}

const day = (date) => date.toISOString().slice(0, 10);

/**
 * Every pull request `reviewer` reviewed that was updated on or after `since`, as search items, deduplicated. A
 * query the search API truncates (more than 1000 results) is split into two date ranges, down to single days.
 */
export async function searchReviewed({ github, reviewer, since, today = new Date() }) {
	const seen = new Map();
	async function window(range) {
		const q = `type:pr reviewed-by:${reviewer} updated:${range.query}`;
		let total = null;
		for (let page = 1; page <= SEARCH_LIMIT / PER_PAGE; page++) {
			const body = await github.get("search/issues", { q, sort: "updated", order: "asc", per_page: PER_PAGE, page });
			total = body.total_count;
			if (page === 1 && total > SEARCH_LIMIT && range.from < range.to) {
				const middle = new Date((Date.parse(range.from) + Date.parse(range.to)) / 2);
				const next = new Date(Date.parse(day(middle)) + 86_400_000);
				await window({ from: range.from, to: day(middle), query: `${range.from}..${day(middle)}` });
				await window({ from: day(next), to: range.to, query: `${day(next)}..${range.to}` });
				return;
			}
			for (const item of body.items) seen.set(item.html_url ?? `${item.repository_url}#${item.number}`, item);
			if (body.items.length < PER_PAGE) break;
		}
	}
	await window({ from: since, to: day(today), query: `>=${since}` });
	return [...seen.values()];
}

/** `owner/name` of a search item. */
export function repoOf(item) {
	return String(item.repository_url ?? "").replace(/^.*\/repos\//, "");
}

/**
 * One search item as a candidate case, or the reason it is dropped: `{ candidate }` or `{ reason }`. A candidate is
 * a `cases.jsonl` record whose commits are not yet known to be fetchable.
 */
export async function evaluateCandidate({ github, item, reviewer, options }) {
	const repo = repoOf(item);
	const pr = item.number;
	if (String(item.user?.login ?? "").toLowerCase() === reviewer.toLowerCase()) return { reason: DROP.selfAuthored };
	try {
		// Comments first: most pull requests end here, with one request.
		const comments = await github.list(`repos/${repo}/pulls/${pr}/comments`);
		const opened = comments.filter((comment) => !comment.in_reply_to_id && String(comment.user?.login ?? "").toLowerCase() === reviewer.toLowerCase()).length;
		if (opened === 0) return { reason: DROP.noInlineReview };
		if (opened < options.minComments) return { reason: DROP.fewComments };
		const reviews = await github.list(`repos/${repo}/pulls/${pr}/reviews`);
		const reference = selectReferenceReview(reviews, comments, reviewer);
		if (!reference) return { reason: DROP.noInlineReview };
		if (reference.comments.length < options.minComments) return { reason: DROP.fewComments };
		const pull = await github.get(`repos/${repo}/pulls/${pr}`);
		const head = reference.review.commit_id;
		let compare;
		try {
			compare = await github.get(`repos/${repo}/compare/${pull.base.sha}...${head}`, { per_page: 1 });
		} catch (error) {
			if (error.status === 404 || error.status === 422) return { reason: DROP.commitNotFound };
			throw error;
		}
		const base = compare.merge_base_commit?.sha;
		if (!base || base === head) return { reason: DROP.emptyDiff };
		const shape = diffShape(compare.files);
		const tooBig = sizeDropReason(shape, options);
		if (tooBig) return { reason: tooBig };
		return {
			candidate: caseRecord({
				repo,
				pr,
				author: pull.user?.login ?? item.user?.login ?? "",
				title: pull.title,
				body: pull.body,
				baseRef: pull.base.ref,
				base,
				head,
				shape,
				reference,
				prior: priorReviews(reviews, comments, reference.review),
			}),
		};
	} catch (error) {
		if (error instanceof RateLimitStop || error.status === undefined) throw error;
		return { reason: `${DROP.apiError} (HTTP ${error.status || "?"})` };
	}
}

// ---------------------------------------------------------------------------------------------------------------
// Case repositories

const repoSlug = (repo) => repo.replace("/", "__");

/** Blob ids of a commit's tree that the repository does not have. Never fetches. */
function missingBlobs(cache, commits) {
	const env = { GIT_NO_LAZY_FETCH: "1" };
	const blobs = new Set();
	for (const commit of commits) {
		for (const line of git(["-C", cache, "ls-tree", "-r", "--format=%(objecttype) %(objectname)", commit], { env }).split("\n")) {
			if (line.startsWith("blob ")) blobs.add(line.slice(5));
		}
	}
	if (!blobs.size) return [];
	const checked = git(["-C", cache, "cat-file", "--batch-check=%(objectname) %(objecttype)"], { env, input: `${[...blobs].join("\n")}\n` });
	return checked
		.split("\n")
		.filter((line) => line.endsWith(" missing"))
		.map((line) => line.split(" ")[0]);
}

/** Whether a case repository is built: its marker names these commits and its checkout is at the head. */
export function builtCase(dir, spec) {
	const repoDir = join(dir, "cases", spec.id);
	try {
		const marker = JSON.parse(readFileSync(join(dir, "built", `${spec.id}.json`), "utf8"));
		if (marker.base !== spec.base || marker.head !== spec.head || !existsSync(join(repoDir, ".git"))) return null;
		return { repoDir, base: spec.base, head: spec.head };
	} catch {
		return null;
	}
}

/**
 * Build one case repository: `<set>/cases/<id>`, checked out (detached) at the reviewed commit, with the merge base
 * and its history reachable and no remote, so nothing in it can reach the network.
 *
 * Objects live in a blobless clone per repository (`<set>/repos/<owner>__<name>.git`): the two commits are fetched
 * with their history but without file contents, then the contents of exactly the two trees. The case repository
 * borrows those objects. `url` is the remote; `env` carries `GH_TOKEN` for the credential helper.
 */
export function materialise({ dir, spec, url, env = {} }) {
	const ready = builtCase(dir, spec);
	if (ready) return ready;
	const cache = join(privateDir(join(dir, "repos")), `${repoSlug(spec.repo)}.git`);
	if (!existsSync(join(cache, "HEAD"))) {
		git(["init", "-q", "--bare", cache]);
		git(["-C", cache, "config", "remote.origin.url", url]);
		git(["-C", cache, "config", "remote.origin.promisor", "true"]);
		git(["-C", cache, "config", "remote.origin.partialclonefilter", "blob:none"]);
	}
	const fetch = [...CREDENTIAL_HELPER, "-C", cache, "fetch", "-q", "--no-tags", "--filter=blob:none"];
	git([...fetch, "origin", `+${spec.head}:refs/bench/${spec.head}`, `+${spec.base}:refs/bench/${spec.base}`], { env });
	const missing = missingBlobs(cache, [spec.head, spec.base]);
	// The way git itself fetches missing objects of a partial clone: no negotiation, or the server, told that we
	// have the commits, leaves out the very file contents we ask for.
	if (missing.length) {
		const wanted = [...CREDENTIAL_HELPER, "-c", "fetch.negotiationAlgorithm=noop", "-C", cache, "fetch", "--no-tags", "--no-write-fetch-head", "--recurse-submodules=no", "--filter=blob:none", "--stdin", "origin"];
		git(wanted, { env, input: `${missing.join("\n")}\n` });
	}
	if (missingBlobs(cache, [spec.head, spec.base]).length) throw new Error("file contents of the two commits are incomplete after fetching");
	git(["-C", cache, "merge-base", "--is-ancestor", spec.base, spec.head]);

	const repoDir = join(privateDir(join(dir, "cases")), spec.id);
	rmSync(repoDir, { recursive: true, force: true });
	git(["init", "-q", repoDir]);
	writeFileSync(join(repoDir, ".git", "objects", "info", "alternates"), `${resolve(cache, "objects")}\n`);
	const offline = { GIT_NO_LAZY_FETCH: "1" };
	git(["-C", repoDir, "update-ref", "refs/bench/base", spec.base], { env: offline });
	git(["-C", repoDir, "update-ref", "refs/bench/head", spec.head], { env: offline });
	git(["-C", repoDir, "checkout", "-q", "--detach", spec.head], { env: offline });
	git(["-C", repoDir, "diff", "--stat", spec.base, spec.head], { env: offline });
	writePrivate(join(privateDir(join(dir, "built")), `${spec.id}.json`), `${JSON.stringify({ base: spec.base, head: spec.head })}\n`);
	return { repoDir, base: spec.base, head: spec.head };
}

// ---------------------------------------------------------------------------------------------------------------
// Collection

function tallyReasons(dropped) {
	const counts = {};
	for (const reason of dropped) counts[reason] = (counts[reason] ?? 0) + 1;
	return Object.fromEntries(Object.entries(counts).sort(([a], [b]) => (a < b ? -1 : 1)));
}

/**
 * Collect a set. Returns the manifest. With `plan`, nothing is built and no case file is written.
 *
 * Pull requests are looked at lazily: the search results are put in the seeded sample order and looked at one by
 * one, and the collection stops as soon as `options.cases` cases are found, so GitHub is asked about as few pull
 * requests as the sample needs. What GitHub said about a pull request is kept under `candidates/` and every raw
 * answer under `api/`, so a rerun asks nothing twice. With `offline` GitHub's API is not asked at all: the sample
 * is drawn from the pull requests already looked at.
 *
 * `remoteUrl(repo)` names the git remote of a repository and `build` builds one case (both injected in tests).
 */
export async function collect({
	account,
	reviewer,
	since,
	dir,
	options,
	plan = false,
	offline = false,
	minIntervalMs = DEFAULT_MIN_INTERVAL_MS,
	exec = run,
	sleep = sleepFor,
	logger = log,
	remoteUrl = (repo) => `https://github.com/${repo}.git`,
	build = materialise,
	today = new Date(),
	now = Date.now,
}) {
	privateDir(dir);
	chmodSync(dir, 0o700);
	const outcomes = privateDir(join(dir, "candidates"));
	const github = await githubClient({ account, exec, sleep, logger, now, minIntervalMs, offline, cacheDir: privateDir(join(dir, "api")) });
	const settings = [reviewer.toLowerCase(), options.minComments, options.maxChangedLines, options.maxFiles];
	const dropped = [];
	let searchResults = null;
	let looked = 0;
	let entries;
	if (offline) {
		// The pull requests already looked at with these settings: the passing ones are the pool, the others are counted.
		entries = [];
		for (const name of readdirSync(outcomes)) {
			const kept = readJson(join(outcomes, name));
			if (!kept || JSON.stringify(JSON.parse(kept.key).slice(1)) !== JSON.stringify(settings)) continue;
			looked++;
			if (kept.outcome.candidate) entries.push({ id: kept.outcome.candidate.id, repo: kept.outcome.candidate.repo, author: kept.outcome.candidate.author, outcome: kept.outcome });
			else dropped.push(kept.outcome.reason);
		}
		logger(`offline: ${looked} pull requests were looked at before, ${entries.length} of them passed the filters`);
	} else {
		const items = await searchReviewed({ github, reviewer, since, today });
		searchResults = items.length;
		logger(`search: ${items.length} pull requests reviewed since ${since}`);
		entries = items.map((item) => ({ id: caseId(repoOf(item), item.number), repo: repoOf(item), author: item.user?.login ?? "", item }));
	}
	const order = sampleOrder(entries, options.seed);
	const cap = repoCap(options.cases, options.reposMaxShare);
	const perRepo = new Map();
	const sample = [];
	let passed = offline ? entries.length : 0;
	let overCap = 0;
	let position = 0;
	for (; position < order.length && sample.length < options.cases; position++) {
		const entry = order[position];
		if ((perRepo.get(entry.repo) ?? 0) >= cap) {
			overCap++;
			continue;
		}
		let outcome = entry.outcome;
		if (!outcome) {
			const file = join(outcomes, `${entry.id}.json`);
			const key = JSON.stringify([entry.item.updated_at, ...settings]);
			const kept = readJson(file);
			outcome = kept?.key === key ? kept.outcome : await evaluateCandidate({ github, item: entry.item, reviewer, options });
			if (kept?.key !== key && !outcome.reason?.startsWith(DROP.apiError)) writePrivate(file, `${JSON.stringify({ key, outcome })}\n`);
			looked++;
			if (outcome.candidate) passed++;
		}
		if (!outcome.candidate) {
			dropped.push(outcome.reason);
			continue;
		}
		const spec = outcome.candidate;
		if (!plan && !builtCase(dir, spec)) {
			try {
				build({ dir, spec, url: remoteUrl(spec.repo), env: { GH_TOKEN: github.env.GH_TOKEN } });
				logger(`built ${spec.id} (${spec.files} files, ${spec.changedLines} changed lines, ${spec.referenceComments.length} reference comments)`);
			} catch (error) {
				// A case whose commits cannot be fetched is replaced by the next candidate in the order.
				logger(`${spec.id}: not built, dropped: ${github.clean(error.message).replaceAll(spec.repo, "<repo>")}`);
				dropped.push(DROP.notFetchable);
				continue;
			}
		}
		perRepo.set(spec.repo, (perRepo.get(spec.repo) ?? 0) + 1);
		sample.push(spec);
	}
	const manifest = {
		benchmark: "autoreview-blind",
		set: dir.split("/").at(-1),
		collectedAt: today.toISOString(),
		query: { account, reviewer, since },
		options,
		offline,
		repoCap: cap,
		searchResults,
		lookedAt: looked,
		passedFilters: passed,
		dropped: tallyReasons(dropped),
		skippedForRepoCap: overCap,
		notLookedAt: order.length - position,
		sample: sampleShape(sample),
		cases: sample.map((spec) => spec.id),
		apiRequests: github.counts.requests,
		answersFromCache: github.counts.cached,
	};
	if (!plan) {
		writePrivate(join(dir, "cases.jsonl"), sample.map((spec) => `${JSON.stringify(spec)}\n`).join(""));
		writePrivate(join(dir, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
	}
	return manifest;
}

/** The counts of a manifest as text: no repository, login or title. */
export function renderCounts(manifest) {
	const shape = manifest.sample;
	const d = (dist) => `min ${dist.min ?? "-"}, p50 ${dist.p50 ?? "-"}, p90 ${dist.p90 ?? "-"}, max ${dist.max ?? "-"}`;
	return [
		`Search results: ${manifest.searchResults ?? "not searched (offline)"}`,
		`Pull requests looked at: ${manifest.lookedAt}; passed the filters: ${manifest.passedFilters}`,
		`Not looked at because the sample was full: ${manifest.notLookedAt}; skipped because their repository had its share: ${manifest.skippedForRepoCap}`,
		"Dropped:",
		...Object.entries(manifest.dropped).map(([reason, count]) => `  ${count}\t${reason}`),
		`Sample: ${shape.cases} cases from ${shape.repositories} repositories and ${shape.authors} authors (at most ${manifest.repoCap} per repository)`,
		`  cases per repository: ${shape.casesPerRepository.join(", ")}`,
		`  reference comments: ${shape.referenceComments} (per case: ${d(shape.commentsPerCase)})`,
		`  changed lines per case: ${d(shape.changedLines)}`,
		`  files per case: ${d(shape.files)}`,
		`  first reviewer on the commit: ${shape.firstReviewerOnCommit}; had prior reviews: ${shape.hadPriorReviews}`,
		`  review states: ${Object.entries(shape.reviewStates).map(([state, count]) => `${state} ${count}`).join(", ")}`,
		`API requests: ${manifest.apiRequests} (answers from the cache: ${manifest.answersFromCache})`,
	].join("\n");
}

function numberFlag(flags, name, fallback, { integer = true, min = 1, max = Number.POSITIVE_INFINITY } = {}) {
	if (flags[name] === undefined) return fallback;
	const value = Number(flags[name]);
	if (!Number.isFinite(value) || (integer && !Number.isInteger(value)) || value < min || value > max) {
		console.error(`--${name} must be ${integer ? "an integer" : "a number"} from ${min}${max === Number.POSITIVE_INFINITY ? "" : ` to ${max}`}, got ${flags[name]}`);
		process.exit(2);
	}
	return value;
}

async function main() {
	const { flags, unknown } = parseArgs(process.argv.slice(2), VALUE_FLAGS, SWITCH_FLAGS);
	if (unknown.length) {
		console.error(`Unknown arguments: ${unknown.join(" ")}`);
		console.error("See the header of evals/autoreview/blind/collect.mjs for the flags.");
		process.exit(2);
	}
	for (const name of ["account", "reviewer", "since", "set"]) {
		if (!flags[name]) {
			console.error(`--${name} is required`);
			process.exit(2);
		}
	}
	if (!/^\d{4}-\d\d-\d\d$/.test(flags.since) || Number.isNaN(Date.parse(flags.since))) {
		console.error(`--since must be a date as YYYY-MM-DD, got ${flags.since}`);
		process.exit(2);
	}
	let dir;
	try {
		dir = setDir(flags.set);
	} catch (error) {
		console.error(error.message);
		process.exit(2);
	}
	const options = {
		cases: numberFlag(flags, "cases", DEFAULT_CASES),
		seed: flags.seed ?? DEFAULT_SEED,
		maxChangedLines: numberFlag(flags, "max-changed-lines", DEFAULT_MAX_CHANGED_LINES),
		maxFiles: numberFlag(flags, "max-files", DEFAULT_MAX_FILES),
		minComments: numberFlag(flags, "min-comments", DEFAULT_MIN_COMMENTS),
		reposMaxShare: numberFlag(flags, "repos-max-share", DEFAULT_REPOS_MAX_SHARE, { integer: false, min: 0.01, max: 1 }),
	};
	const minIntervalMs = numberFlag(flags, "min-interval-ms", DEFAULT_MIN_INTERVAL_MS, { min: 0 });
	let manifest;
	try {
		manifest = await collect({ account: flags.account, reviewer: flags.reviewer, since: flags.since, dir, options, plan: flags.plan === true, offline: flags.offline === true, minIntervalMs });
	} catch (error) {
		if (!(error instanceof RateLimitStop)) throw error;
		console.error(error.message);
		process.exit(3);
	}
	console.log(renderCounts(manifest));
	if (flags.plan) console.log("Plan only: no case was built or written.");
	else log(`wrote ${manifest.cases.length} cases to the set directory`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
	main().catch((error) => {
		console.error(error.stack ?? String(error));
		process.exit(1);
	});
}
