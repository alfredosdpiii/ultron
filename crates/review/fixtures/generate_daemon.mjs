// Records the pure parts of the autoreview daemon (packages/coding-agent/src/ultron/autoreview/daemon.ts) to
// fixtures/daemon.json: blockedPulls over recorded states, discover over scripted `gh api -i` replies (every call's
// argv and environment recorded), pruneOld over files with recorded ages, and createLogger's lines and files.
// Temporary paths are written as <root>. No process is run and nothing touches GitHub. The Rust tests
// (tests/daemon.rs) replay every case exactly.
// Usage: node crates/review/fixtures/generate_daemon.mjs (deterministic; the sha256 is stable across runs).
import { lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, symlinkSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
process.env.TZ = "UTC";
const load = (name) => import(pathToFileURL(join(root, "packages/coding-agent/src/ultron/autoreview", name)).href);
const daemon = await load("daemon.ts");
const github = await load("github.ts");

const NOW = Date.parse("2026-10-04T10:00:00Z");
const DAY = 24 * 60 * 60 * 1000;

// ---- blockedPulls ----
const pulls = {
	"bot@github.com/o/r#1": { lastVerdict: "request_changes" },
	"bot@github.com/o/r#2": { lastVerdict: "approve" },
	"bot@github.com/o/r#3": { lastVerdict: "request_changes", closed: true },
	"bot@github.com/o/r#4": { lastVerdict: "request_changes", lastReviewDryRun: true },
	"other@github.com/o/r#5": { lastVerdict: "request_changes" },
	"bot@ghe.io/team/svc#6": { lastVerdict: "request_changes", closed: false, lastReviewDryRun: false },
	"bot@github.com/o/r/x#7": { lastVerdict: "request_changes" },
	"bot@github.com/bad": { lastVerdict: "request_changes" },
	"bot@github.com/o/r#08": { lastVerdict: "request_changes" },
	"bot@github.com/o/r#1x": { lastVerdict: "request_changes" },
	"bot@github.comx/o/r#9": { lastVerdict: "request_changes" },
	"bot@github.com/a-b/c.d#10": { lastVerdict: "request_changes", lastReviewDryRun: false },
	"bot@github.com/o/r#11": { lastVerdict: "comment" },
	"bot@github.com/o/r#12": {},
};
const blockedState = { version: 1, accounts: {}, pulls, recent: [] };
const blocked = [["bot", "github.com"], ["bot", "ghe.io"], ["other", "github.com"], ["BOT", "github.com"], ["nobody", "github.com"]]
	.map(([login, host]) => ({ account: { login, host }, blocked: daemon.blockedPulls(blockedState, { login, host }) }));

// ---- discover over scripted replies ----
const reply = (status, body, headers = {}) => {
	const head = [`HTTP/2.0 ${status} ${status < 400 ? "OK" : "Error"}`, ...Object.entries(headers).map(([n, v]) => `${n}: ${v}`)];
	const text = typeof body === "string" ? body : body === undefined ? "" : JSON.stringify(body);
	return { code: status < 400 ? 0 : 1, stdout: `${head.join("\r\n")}\r\n\r\n${text}`, stderr: status < 400 ? "" : `gh: HTTP ${status}` };
};
const note = (reason, number, updated_at, type = "PullRequest", host = "api.github.com") => ({
	reason, ...(updated_at === undefined ? {} : { updated_at }),
	subject: { type, url: `https://${host}/repos/o/r/pulls/${number}` },
});
const items = (...numbers) => ({ items: numbers.map((number) => ({ number, repository_url: "https://api.github.com/repos/o/r", pull_request: {} })) });
const LM = { "Last-Modified": "Sun, 04 Oct 2026 10:00:00 GMT" };
const ref = (number) => ({ host: "github.com", owner: "o", repo: "r", number });
const discoverInputs = [
	{ name: "first poll: a day back; reasons merged; the latest notification time", account: ["bot", "github.com"], state: {}, now: NOW, search: false,
		replies: [reply(200, [note("review_requested", 1, "2026-10-04T09:59:56Z"), note("mention", 1, "2026-10-04T09:59:58.500Z"),
			note("mention", 1, "2026-10-04T09:00:00Z"), note("review_requested", 1, "2026-10-04T09:59:56Z"), note("mention", 2, ""),
			note("team_mention", 3, "garbage"), note("subscribed", 4, "2026-10-04T09:00:00Z"), note("mention", 5, "2026-10-04T09:00:00Z", "Issue"),
			note("review_requested", 2, "2026-10-04T09:00:00Z"), note("mention", 6), note("review_requested", 7, "2026-10-04T11:00:00+02:00"),
			note("mention", 8, "2026-10-03")], { ...LM, "X-Poll-Interval": "60" })] },
	{ name: "a later poll: If-Modified-Since, two minutes of overlap; 304 keeps the cursor", account: ["bot", "github.com"],
		state: { lastPollAt: "2026-10-04T09:00:00.000Z", lastModified: "Sun, 04 Oct 2026 09:00:00 GMT", etag: "\"e\"", pausedUntil: "2026-10-04T09:30:00.000Z",
			lastError: "old", custom: 1 }, now: NOW, search: false, replies: [reply(304, undefined, { "X-Poll-Interval": "120" })] },
	{ name: "a 304 with a new Last-Modified", account: ["bot", "github.com"], state: { lastPollAt: "2026-10-04T09:59:00Z", lastModified: "old" },
		now: NOW, search: false, replies: [reply(304, undefined, LM)] },
	{ name: "search cycle: review requests, mentions of two days, blocked pulls", account: ["bot", "github.com"],
		state: { lastPollAt: "garbage", pollInterval: 60 }, now: NOW, search: true, blocked: [ref(1), ref(9)],
		replies: [reply(200, [note("mention", 2, "2026-10-04T09:59:00Z")], LM), reply(200, items(1, 2)), reply(200, items(2, 7, 2))] },
	{ name: "search window across a day boundary", account: ["bot", "github.com"], state: {}, now: Date.parse("2026-10-04T00:30:00Z"), search: true,
		replies: [reply(200, []), reply(200, { items: [] }), reply(200, { items: [{ number: 3, repository_url: "https://api.github.com/repos/o/r" }] })] },
	{ name: "search without blocked pulls", account: ["bot", "github.com"], state: { lastPollAt: "2026-10-03" }, now: NOW, search: true,
		replies: [reply(200, []), reply(200, items(4)), reply(200, items())] },
	{ name: "an enterprise account", account: ["corp", "ghe.io"], state: { lastPollAt: "2026-10-04T12:00:00+02:00" }, now: NOW, search: false,
		replies: [reply(200, [note("review_requested", 1, "2026-10-04T09:59:56Z", "PullRequest", "ghe.io/api/v3")], { ...LM, "X-Poll-Interval": "0" })] },
	{ name: "a rate limit on notifications", account: ["bot", "github.com"], state: {}, now: NOW, search: false,
		replies: [reply(403, { message: "secondary rate limit" }, { "Retry-After": "300" })] },
	{ name: "a failed search", account: ["bot", "github.com"], state: {}, now: NOW, search: true,
		replies: [reply(200, []), reply(200, items(1)), reply(500, { message: "boom" })] },
	{ name: "gh produced no response", account: ["bot", "github.com"], state: {}, now: NOW, search: false,
		replies: [{ code: 1, stdout: "", stderr: "gh: connection refused" }] },
	{ name: "a poll interval that is not a number", account: ["bot", "github.com"], state: { pollInterval: 90 }, now: NOW, search: false,
		replies: [reply(200, [], { "X-Poll-Interval": "soon" })] },
];
const discoverCases = [];
for (const input of discoverInputs) {
	const calls = [];
	const queue = [...input.replies];
	const runner = async (argv, options = {}) => {
		calls.push({ argv: [...argv], env: { ...options.env } });
		return queue.shift() ?? { code: 1, stdout: "", stderr: "no reply" };
	};
	const [login, host] = input.account;
	const gh = new github.GitHub(runner, { login, host }, async () => ({ GH_TOKEN: "t", GH_HOST: host }), { now: () => input.now });
	let result = null;
	let error = null;
	try {
		result = await daemon.discover(gh, input.state, { now: input.now, search: input.search, ...(input.blocked ? { blocked: input.blocked } : {}) });
	} catch (caught) {
		error = { name: caught.name, message: caught.message, status: caught.status ?? null, resumeAt: caught.resumeAt ?? null };
	}
	discoverCases.push({ ...input, blocked: input.blocked ?? [], calls, result, error });
}

// ---- pruneOld ----
const tmp = mkdtempSync(join(tmpdir(), "ultron-daemon-gen-"));
const hide = (text) => text.split(tmp).join("<root>");
const FILES = [["a/old.md", 15 * DAY], ["a/edge.md", 14 * DAY], ["a/past-edge.md", 14 * DAY + 1000], ["a/recent.md", 13 * DAY],
	["a/future.md", -DAY], ["b/autoreview-2026-09-01.log", 30 * DAY], ["b/fresh.log", 0], ["b/hour.log", 60 * 60 * 1000]];
const pruneCases = [];
for (const [days, dirs] of [[undefined, ["a", "b", "missing"]], [0, ["a", "b"]], [1, ["b"]], [14, ["b", "a"]], [undefined, []], [30, ["a", "b"]]]) {
	rmSync(join(tmp, "prune"), { recursive: true, force: true });
	const base = join(tmp, "prune");
	for (const [name, age] of FILES) {
		mkdirSync(dirname(join(base, name)), { recursive: true });
		writeFileSync(join(base, name), "x");
		const at = new Date(NOW - age);
		utimesSync(join(base, name), at, at);
	}
	// Not files: a directory and a symbolic link, both old.
	mkdirSync(join(base, "a/sub"));
	const old = new Date(NOW - 40 * DAY);
	utimesSync(join(base, "a/sub"), old, old);
	symlinkSync(join(base, "a/old.md"), join(base, "a/link.md"));
	const removed = days === undefined ? daemon.pruneOld(dirs.map((dir) => join(base, dir)), NOW) : daemon.pruneOld(dirs.map((dir) => join(base, dir)), NOW, days);
	const left = [];
	for (const dir of ["a", "b"]) for (const name of readdirSync(join(base, dir)).sort()) left.push(`${dir}/${name}`);
	pruneCases.push({ days: days ?? null, dirs, removed, left });
}
const pruneFiles = FILES.map(([name, age]) => ({ name, age }));

// ---- createLogger ----
const loggerCases = [];
for (const [logs, nows, lines] of [
	["logs", [NOW, NOW + 14 * 60 * 60 * 1000], ["hello", "multi\nline", "unicode é ✓ 😀", ""]],
	["deep/er/logs", [Date.parse("2026-12-31T23:59:59.999Z"), Date.parse("2027-01-01T00:00:00Z")], ["one", "two"]],
	[null, [NOW], ["no file"]],
]) {
	rmSync(join(tmp, "log"), { recursive: true, force: true });
	mkdirSync(join(tmp, "log"));
	const dir = logs === null ? undefined : join(tmp, "log", logs);
	const written = [];
	let index = 0;
	const log = daemon.createLogger(dir, () => nows[Math.min(index, nows.length - 1)], (text) => written.push(text));
	for (const line of lines) {
		log(line);
		index += 1;
	}
	const files = [];
	if (dir !== undefined)
		for (const name of readdirSync(dir).sort())
			files.push({ name, text: readFileSync(join(dir, name), "utf8"), mode: lstatSync(join(dir, name)).mode & 0o777 });
	loggerCases.push({ logs, nows, lines, written: written.map(hide), files, dirMode: dir === undefined ? null : lstatSync(dir).mode & 0o777 });
}
rmSync(tmp, { recursive: true, force: true });

const out = { now: NOW, constants: { searchEvery: daemon.SEARCH_EVERY }, blockedState, blocked, discover: discoverCases, pruneFiles,
	prune: pruneCases, logger: loggerCases };
writeFileSync(join(here, "daemon.json"), `${JSON.stringify(out, null, 1)}\n`);
