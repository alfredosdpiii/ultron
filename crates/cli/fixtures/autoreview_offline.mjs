// Records the TypeScript `ultron autoreview review --repo-dir ... --json` (the offline entry, no GitHub access) against
// a local stub chat-completions provider (the reply rules of packages/coding-agent/test/ultron-autoreview-engine.test.ts,
// "the offline JSON contract, with a stub provider"), over a repository built with a fixed identity and clock so its
// commits are the same in the Rust test. Recorded per case: the exit code, stdout (the JSON with times zeroed and
// frame timings sorted, or the text), the stderr lines that are the command's own, and every provider request (model,
// messages, output cap) sorted. Built-in groq models are pointed at the stub through models.json.
// Usage: node crates/cli/fixtures/autoreview_offline.mjs
import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const cliPath = join(root, "packages/coding-agent/src/cli.ts");
const resolver = join(root, "packages/coding-agent/src/experimental/source-resolver.ts");

export const FRAMES = "groq/llama-3.1-8b-instant";
export const VERIFY = "groq/llama-3.3-70b-versatile";

export const CALC = `def total(items):
    """Sum of item prices."""
    result = 0
    for i in range(len(items)):
        result += items[i]["price"]
    return result


def average(items, count):
    if count == 0:
        return 0
    return total(items) / count
`;

const GIT_ENV = {
	GIT_AUTHOR_NAME: "Review Test",
	GIT_AUTHOR_EMAIL: "review@test",
	GIT_COMMITTER_NAME: "Review Test",
	GIT_COMMITTER_EMAIL: "review@test",
	GIT_AUTHOR_DATE: "2026-10-01T00:00:00Z",
	GIT_COMMITTER_DATE: "2026-10-01T00:00:00Z",
	GIT_CONFIG_GLOBAL: "/dev/null",
	GIT_CONFIG_NOSYSTEM: "1",
	PATH: "/usr/bin:/bin",
};

function git(cwd, ...args) {
	return execFileSync("git", ["-c", "commit.gpgsign=false", ...args], { cwd, encoding: "utf8", env: GIT_ENV }).trim();
}

const state = { rateLimited: 0, scenarioHolds: true, requests: [] };

function verdictFor(body) {
	if (body.includes("wrong mean"))
		return { verdict: "confirmed", evidence: "`return total(items) / count` divides the truncated sum by the full count.", corrected_line: null, severity: "major", scenario_holds: true };
	if (body.includes("skips the last item"))
		return { verdict: "confirmed", evidence: "`for i in range(len(items) - 1):` stops before the last index.", corrected_line: null, severity: "major", scenario_holds: state.scenarioHolds };
	return { verdict: "rejected", evidence: "`if count == 0:` returns first.", corrected_line: null, severity: "minor", scenario_holds: false };
}

function reply(body) {
	if (body.includes("You check several findings of an automated pull request review")) {
		const sections = body.split("=== Finding ").slice(1);
		return JSON.stringify(sections.map((section, index) => ({ ...verdictFor(section), finding: index + 1 })));
	}
	if (body.includes("You check one finding of an automated pull request review")) return JSON.stringify(verdictFor(body));
	if (body.includes("You investigate one pull request beyond its diff")) {
		if (!body.includes("Your part: the claims")) return JSON.stringify({ findings: [], requests: [], done: true });
		if (!body.includes("Results of your requests, round 1"))
			return JSON.stringify({ findings: [], requests: [{ read: { path: "calc.py", start: 1, end: 12 } }, { read: { path: "../etc/passwd" } }], done: false });
		return JSON.stringify({
			findings: [{ file: "calc.py", line: 12, severity: "major", category: "correctness", claim: "average() returns a wrong mean for every non-empty list.", why: "average() divides the truncated total by the full count.", scenario: "average([{'price': 2}, {'price': 4}], 2) returns 1.0; it should return 3.0.", suggested_fix: "Fix total().", confidence: 0.8, evidence: [{ path: "calc.py", line: 12, quote: "return total(items) / count" }] }],
			requests: [],
			checked: ["average() guards count == 0 before dividing (calc.py)"],
			done: true,
		});
	}
	if (body.includes("Your specialty: Correctness") && body.includes("File: calc.py"))
		return JSON.stringify([
			{ file: "calc.py", line: 4, end_line: 4, severity: "major", category: "correctness", claim: "total() skips the last item.", why: "range(len(items) - 1) stops one short.", suggested_fix: "Use range(len(items)).", confidence: 0.9, replacement: "    for i in range(len(items)):", scenario: "total([{'price': 1}, {'price': 2}]) returns 1; it should return 3." },
			{ file: "calc.py", line: 12, end_line: 12, severity: "major", category: "correctness", claim: "average() divides by zero.", why: "count may be 0.", suggested_fix: "Guard it.", confidence: 0.6, replacement: null, scenario: "average([], 0) raises ZeroDivisionError." },
		]);
	return "[]";
}

/** Frame and budget ids are random. */
function normalizeIds(text) {
	return text.replace(/(frame-|budget-)[0-9a-f-]{36}/g, "$1ID");
}

function textOf(content) {
	return Array.isArray(content) ? content.map((part) => part.text ?? "").join("") : content;
}

function requestRecord(body) {
	const parsed = JSON.parse(body);
	return {
		model: parsed.model,
		max_completion_tokens: parsed.max_completion_tokens ?? parsed.max_tokens ?? null,
		messages: parsed.messages.map((m) => ({ role: m.role, content: normalizeIds(textOf(m.content)) })),
	};
}

/** Every duration under `timing` (keys `ms` or ending in `Ms`) zeroed; frame timings sorted (they finish in any order). */
function zeroDurations(value) {
	if (Array.isArray(value)) return value.map(zeroDurations);
	if (value === null || typeof value !== "object") return value;
	const out = {};
	for (const [key, item] of Object.entries(value))
		out[key] = (key === "ms" || key.endsWith("Ms")) && typeof item === "number" ? 0 : zeroDurations(item);
	return out;
}

function zeroTimes(json) {
	const timing = zeroDurations(json.timing);
	const key = (item) => JSON.stringify(item);
	timing.frames = timing.frames.sort((a, b) => key(a).localeCompare(key(b)));
	timing.investigators = timing.investigators.sort((a, b) => key(a).localeCompare(key(b)));
	json.timing = timing;
	return json;
}

const work = realpathSync(mkdtempSync(join(tmpdir(), "ultron-autoreview-cli-")));
try {
	const repo = join(work, "repo");
	mkdirSync(repo);
	git(repo, "init", "-q", "-b", "main");
	writeFileSync(join(repo, "calc.py"), CALC);
	git(repo, "add", ".");
	git(repo, "commit", "-qm", "calc");
	const base = git(repo, "rev-parse", "HEAD");
	writeFileSync(join(repo, "calc.py"), CALC.replace("range(len(items))", "range(len(items) - 1)"));
	git(repo, "commit", "-qam", "tweak total");
	const head = git(repo, "rev-parse", "HEAD");

	const provider = createServer(async (incoming, response) => {
		const chunks = [];
		for await (const chunk of incoming) chunks.push(chunk);
		const body = Buffer.concat(chunks).toString("utf8");
		state.requests.push(body);
		if (state.rateLimited > 0) {
			state.rateLimited -= 1;
			response.writeHead(429, { "content-type": "application/json", "retry-after": "1" });
			response.end(JSON.stringify({ error: { code: "1302", message: "Rate limit reached for requests" } }));
			return;
		}
		const content = reply(JSON.stringify(JSON.parse(body).messages).replace(/\\n/g, "\n").replace(/\\"/g, '"'));
		response.writeHead(200, { "content-type": "text/event-stream" });
		const chunk = (delta, finish, usage) =>
			`data: ${JSON.stringify({ id: "x", object: "chat.completion.chunk", created: 0, model: "stub", choices: [{ index: 0, delta, finish_reason: finish }], ...(usage ? { usage } : {}) })}\n\n`;
		response.write(chunk({ role: "assistant", content }, null));
		response.end(`${chunk({}, "stop", { prompt_tokens: 100, completion_tokens: 20, total_tokens: 120 })}data: [DONE]\n\n`);
	});
	await new Promise((done) => provider.listen(0, "127.0.0.1", done));
	const port = provider.address().port;
	const agentDir = join(work, "agent");
	mkdirSync(agentDir, { recursive: true, mode: 0o700 });
	mkdirSync(join(work, "run"), { recursive: true, mode: 0o700 });
	writeFileSync(join(agentDir, "models.json"), JSON.stringify({ providers: { groq: { baseUrl: `http://127.0.0.1:${port}/v1` } } }));
	writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ defaultProvider: "groq", defaultModel: "llama-3.1-8b-instant", hindsightUrl: "off" }));

	const run = (args) =>
		new Promise((done) => {
			const child = spawn(process.execPath, ["--import", resolver, cliPath, "autoreview", ...args], {
				cwd: work,
				env: {
					// No `claude` on PATH: the recommended Claude Code model must not lead the fallback chain.
					PATH: "/usr/bin:/bin",
					HOME: work,
					TZ: "UTC",
					ULTRON_CODING_AGENT_DIR: agentDir,
					ULTRON_SERVER_DIR: join(work, "server"),
					XDG_RUNTIME_DIR: join(work, "run"),
					ULTRON_LOKI: "off",
					ULTRON_HINDSIGHT_URL: "off",
					GROQ_API_KEY: "stub-key",
					GIT_CONFIG_GLOBAL: "/dev/null",
					GIT_CONFIG_NOSYSTEM: "1",
					...(process.env.ULTRON_PYTHON ? { ULTRON_PYTHON: process.env.ULTRON_PYTHON } : {}),
				},
				stdio: ["ignore", "pipe", "pipe"],
			});
			let stdout = "";
			let stderr = "";
			child.stdout.setEncoding("utf8").on("data", (c) => (stdout += c));
			child.stderr.setEncoding("utf8").on("data", (c) => (stderr += c));
			child.on("close", (code) => done({ code, stdout, stderr }));
		});

	const offline = ["review", "--repo-dir", "<repo>", "--base", "<base>", "--head", "<head>"];
	const cases = [
		{ name: "help", args: ["--help"] },
		{ name: "bare", args: [] },
		{ name: "fast", args: ["review", "--mode", "fast", "--repo-dir", "<repo>", "--base", "<base>", "--head", "<head>", "--model", FRAMES, "--verify-model", VERIFY, "--budget", "200k", "--thinking", "medium", "--deadline", "120", "--json", "--dry-run"] },
		{ name: "rate_limited", rateLimited: 1, args: ["review", "--mode", "fast", ...offline.slice(1), "--json", "--dry-run"] },
		{ name: "scenario_fails", scenarioHolds: false, args: ["review", "--mode", "fast", ...offline.slice(1), "--json", "--dry-run"] },
		{ name: "text", args: ["review", "--mode", "fast", ...offline.slice(1), "--dry-run"] },
		{ name: "both", args: ["review", "--mode", "both", ...offline.slice(1), "--deep-thinking", "low", "--json", "--dry-run"] },
		{ name: "bad_commit", args: ["review", "--mode", "fast", "--repo-dir", "<repo>", "--base", "nope", "--head", "<head>", "--json", "--dry-run"] },
		{ name: "usage_error", args: ["review", "--repo-dir", "<repo>", "--base", "<base>"] },
		{ name: "unknown_option", args: ["review", "--frobnicate"] },
	];
	const recorded = [];
	for (const item of cases) {
		state.requests = [];
		state.rateLimited = item.rateLimited ?? 0;
		state.scenarioHolds = item.scenarioHolds ?? true;
		const args = item.args.map((arg) => (arg === "<repo>" ? repo : arg === "<base>" ? base : arg === "<head>" ? head : arg));
		const result = await run(args);
		let stdout = result.stdout;
		try {
			stdout = zeroTimes(JSON.parse(result.stdout));
		} catch {
			stdout = result.stdout.split(work).join("<work>").replace(/Reviewed in \d+(\.\d+)? s/g, "Reviewed in N s");
		}
		const stderr = result.stderr
			.split("\n")
			.filter((line) => line.length > 0)
			.map((line) =>
				line
					.split(work)
					.join("<work>")
					.replace(/^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z /, "<time> ")
					.replace(/engine ready in \d+ ms/, "engine ready in N ms"),
			);
		recorded.push({
			name: item.name,
			args: item.args,
			rateLimited: item.rateLimited ?? 0,
			scenarioHolds: item.scenarioHolds ?? true,
			code: result.code,
			stdout,
			stderr,
			requests: state.requests
				.map(requestRecord)
				.map((r) => JSON.parse(JSON.stringify(r).split(work).join("<work>")))
				.sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b))),
		});
	}
	await new Promise((done) => provider.close(() => done()));
	process.stdout.write(`${JSON.stringify({ base, head, cases: recorded }, null, 1)}\n`);
} finally {
	rmSync(work, { recursive: true, force: true });
}
