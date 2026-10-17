// Records the TypeScript `ultron autoreview review --repo-dir ...` (the offline entry, no GitHub access) with its
// review frames on the Claude Code CLI: a fake `claude` (fixtures/fake_claude_review.py, never the real CLI) is the
// only `claude` on PATH and no model is configured, so the recommended model claude-code/claude-haiku-5-5 leads the
// chain (`hasModel`: the CLI would be found) and every frame is one `claude -p` request to the fake. Also an
// explicit `--model claude-code/haiku --verify-model claude-code/sonnet`. Recorded per case: the exit code, stdout
// (JSON with times zeroed and frame timings sorted, or the text), the command's own stderr lines, and every request
// the fake served (model, effort, schema flag, output cap, prompts) sorted.
// Runs behind the offline guard (in every spawned node process); provider keys are never in the environment.
// Usage: node crates/cli/fixtures/autoreview_claude_code.mjs > crates/cli/fixtures/autoreview_claude_code.json
import { assertOffline, guardNodeOptions } from "./offline-guard.mjs";
import { execFileSync, spawn } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const cliPath = join(root, "packages/coding-agent/src/cli.ts");
const resolver = join(root, "packages/coding-agent/src/experimental/source-resolver.ts");
const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";

/** The repository of autoreview_offline.mjs. */
const CALC = `def total(items):
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

const git = (cwd, ...args) =>
	execFileSync("git", ["-c", "commit.gpgsign=false", ...args], { cwd, encoding: "utf8", env: GIT_ENV }).trim();

/** Frame and budget ids are random. */
const normalizeIds = (text) => text.replace(/(frame-|budget-)[0-9a-f-]{36}/g, "$1ID");

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

const work = realpathSync(mkdtempSync(join(tmpdir(), "ultron-autoreview-claude-")));
const guardLog = join(work, "guard.log");
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

	const bin = join(work, "bin");
	mkdirSync(bin);
	writeFileSync(join(bin, "claude"), `#!/bin/sh\nexec ${python} ${join(here, "fake_claude_review.py")} "$@"\n`);
	chmodSync(join(bin, "claude"), 0o755);
	const agentDir = join(work, "agent");
	mkdirSync(agentDir, { recursive: true, mode: 0o700 });
	mkdirSync(join(work, "run"), { recursive: true, mode: 0o700 });
	writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ hindsightUrl: "off" }));
	const fakeLog = join(work, "claude.log");

	const run = (args) =>
		new Promise((done) => {
			const child = spawn(process.execPath, ["--import", resolver, cliPath, "autoreview", ...args], {
				cwd: work,
				env: {
					PATH: `${bin}:/usr/bin:/bin`,
					HOME: work,
					TZ: "UTC",
					NODE_OPTIONS: guardNodeOptions,
					ULTRON_GUARD_LOG: guardLog,
					ULTRON_CODING_AGENT_DIR: agentDir,
					ULTRON_SERVER_DIR: join(work, "server"),
					XDG_RUNTIME_DIR: join(work, "run"),
					TMPDIR: join(work, "run"),
					ULTRON_LOKI: "off",
					ULTRON_HINDSIGHT_URL: "off",
					FAKE_CLAUDE_LOG: fakeLog,
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

	const offline = ["--repo-dir", "<repo>", "--base", "<base>", "--head", "<head>"];
	const cases = [
		{ name: "recommended", args: ["review", "--mode", "fast", ...offline, "--json", "--dry-run"] },
		{
			name: "explicit",
			args: ["review", "--mode", "fast", ...offline, "--model", "claude-code/haiku", "--verify-model", "claude-code/sonnet", "--thinking", "low", "--dry-run"],
		},
	];
	const recorded = [];
	for (const item of cases) {
		writeFileSync(fakeLog, "");
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
		const requests = readFileSync(fakeLog, "utf8")
			.split("\n")
			.filter(Boolean)
			.map((line) => JSON.parse(normalizeIds(line).split(work).join("<work>")))
			.sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b)));
		recorded.push({ name: item.name, args: item.args, code: result.code, stdout, stderr, requests });
	}
	assertOffline(guardLog);
	process.stdout.write(`${JSON.stringify({ base, head, cases: recorded }, null, 1)}\n`);
} finally {
	rmSync(work, { recursive: true, force: true });
}
