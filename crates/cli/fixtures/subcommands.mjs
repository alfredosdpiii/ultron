// Generates crates/cli/fixtures/subcommands.json: the TypeScript CLI's `ultron` subcommands (usage, --export, auth,
// mcp, migrate, config/package listing, setup) run as processes with the same argv the Rust tests replay.
// Each case runs `node packages/coding-agent/src/cli.ts` in a private temp HOME, agent dir, server dir and cwd
// (TZ=UTC unless the case sets it), with the case's files written first (native session files built by the
// coding-agent test fixtures, with their modification times). Recorded: stdout, stderr, exit code, and the files the
// case names under `collect` afterwards (text, or sha256 and the decoded session data for exported HTML).
// Temp paths are replaced by <TMP> and the repository root by <REPO>; the Rust test substitutes them back.
// Usage: node crates/cli/fixtures/subcommands.mjs
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
	chmodSync,
	closeSync,
	lstatSync,
	readlinkSync,
	symlinkSync,
	existsSync,
	mkdirSync,
	mkdtempSync,
	openSync,
	readdirSync,
	readFileSync,
	realpathSync,
	rmSync,
	statSync,
	utimesSync,
	writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../..");
const support = join(repo, "packages/coding-agent/test/support");
const { FIXTURE_START, SessionFixture, fixtureStats } = await import(join(support, "session-fixture.ts"));
const sessions = await import(join(support, "report-sessions.ts"));

const HOUR = 3_600_000;
const SESSIONS = "agent/experimental/sessions";
const APP = `${SESSIONS}/--home-dev-app--`;

/** A native session file: its relative path, text and modification time. */
function session(dir, fixture, mtime, options = {}) {
	return {
		path: `${dir}/2026-09-01T10-00-00-000Z_${fixture.id}.jsonl`,
		text: fixture.text(options),
		mtime,
	};
}

/** The five report sessions, newest first, under the default sessions root. */
function reportSessions() {
	return [
		session(`${SESSIONS}/--home-dev-lib--`, sessions.subagentSession(), FIXTURE_START + 5 * HOUR),
		session(APP, sessions.framesOnlySession(), FIXTURE_START + 4 * HOUR),
		session(APP, sessions.claudeHostSession(false), FIXTURE_START + 3 * HOUR),
		session(APP, sessions.claudeRootSession(), FIXTURE_START + 2 * HOUR),
		session(APP, sessions.rootOnlySession(), FIXTURE_START + 1 * HOUR),
	];
}

/** A session of the case's own working directory (`<TMP>/work`). */
function workSession(id, build, mtime) {
	const fixture = new SessionFixture(id, "<TMP>/work");
	build(fixture);
	return session(`${SESSIONS}/--work--`, fixture, mtime);
}

const emptyWork = () => workSession("99999999-0009-7000-8000-000000000009", (s) => s.laneModel("main", "priced", "model-a"), FIXTURE_START + 9 * HOUR);
const ranWork = () =>
	workSession(
		"77777777-0007-7000-8000-000000000007",
		(s) => {
			s.laneModel("main", "priced", "model-a");
			s.run({ lane: "main", responses: [{ cells: [{ code: 'await bash("ls")' }], usage: { input: 5, output: 7, cost: 0.5 } }] });
			s.value("ultron.module", "stats", fixtureStats({ guards: { Loki: { checks: 3, blocked: 1, unchecked: 0, afterChecks: 2, afterFindings: 1, ms: 61_000 } } }));
		},
		FIXTURE_START + 8 * HOUR,
	);

// A Pi v3 session with a tree: a user prompt, a tool call and result, an abandoned branch, a compaction and a label.
const piSession = [
	{ type: "session", version: 3, id: "pi-sess-1", timestamp: "2025-01-01T00:00:00.000Z", cwd: "/home/dev/pi" },
	{ type: "message", id: "e1", parentId: null, timestamp: "2025-01-01T00:00:01.000Z", message: { role: "user", content: "Hello <b>there</b> & \"you\"", timestamp: 1735689601000 } },
	{ type: "message", id: "e2", parentId: "e1", timestamp: "2025-01-01T00:00:02.000Z", message: { role: "assistant", content: [{ type: "thinking", thinking: "hmm" }, { type: "text", text: "Running ls" }, { type: "toolCall", id: "t1", name: "bash", arguments: { command: "ls -la" } }], api: "anthropic-messages", provider: "anthropic", model: "claude-sonnet-4-5", usage: { input: 10, output: 5, cacheRead: 0, cacheWrite: 0, totalTokens: 15, cost: { input: 0.1, output: 0.2, cacheRead: 0, cacheWrite: 0, total: 0.30000000000000004 } }, stopReason: "toolUse", timestamp: 1735689602000 } },
	{ type: "message", id: "e3", parentId: "e2", timestamp: "2025-01-01T00:00:03.000Z", message: { role: "toolResult", toolCallId: "t1", toolName: "bash", content: [{ type: "text", text: "a\nb\n" }], isError: false, timestamp: 1735689603000 } },
	{ type: "message", id: "e4", parentId: "e2", timestamp: "2025-01-01T00:00:04.000Z", message: { role: "user", content: [{ type: "text", text: "abandoned ünïcödé ✓ 😀" }], timestamp: 1735689604000 } },
	{ type: "compaction", id: "e5", parentId: "e3", timestamp: "2025-01-01T00:00:05.000Z", summary: "Summary", firstKeptEntryId: "e3", tokensBefore: 1234 },
	{ type: "label", id: "e6", parentId: "e5", timestamp: "2025-01-01T00:00:06.000Z", targetId: "e1", label: "start" },
	{ type: "model_change", id: "e7", parentId: "e6", timestamp: "2025-01-01T00:00:07.000Z", provider: "openai", modelId: "gpt-5" },
	{ type: "message", id: "e8", parentId: "e7", timestamp: "2025-01-01T00:00:08.000Z", message: { role: "assistant", content: [{ type: "text", text: "Done. $& $' $` $$ {{JS}}" }], api: "openai-responses", provider: "openai", model: "gpt-5", usage: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0, totalTokens: 3, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } }, stopReason: "stop", timestamp: 1735689608000 } },
]
	.map((line) => JSON.stringify(line))
	.join("\n");

// A native session whose main branch has every entry type the exporter maps, a second lane, and a torn last line.
function exportSession(torn) {
	const fixture = new SessionFixture("eeee0000-0010-7000-8000-000000000010", "/home/dev/app");
	fixture.laneModel("main", "priced", "model-a");
	fixture.run({ lane: "main", prompt: "make <it> & go", responses: [{ cells: [{ code: 'print("hi")', output: "hi" }], usage: { input: 3, output: 4, cost: 0.001 } }, { usage: { input: 5, output: 6 } }] });
	const tip = fixture.tip("main");
	fixture.raw(
		{ kind: "entry", id: "x-compaction", parentId: tip, type: "compaction", summary: "Compacted: ünï", tokensBefore: 999, details: { readFiles: ["a"] }, usage: { input: 1, output: 1, cacheRead: 0, cacheWrite: 0, totalTokens: 2, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } }, fromHook: false, timestamp: FIXTURE_START + 100_000 },
		{ kind: "entry", id: "x-branch", parentId: "x-compaction", type: "branch_summary", fromId: null, summary: "Branch", fromHook: true, timestamp: FIXTURE_START + 101_000 },
		{ kind: "entry", id: "x-custom", parentId: "x-branch", type: "custom", customType: "ultron.note", data: { a: 1, b: [true, null] }, timestamp: FIXTURE_START + 102_000 },
		{ kind: "entry", id: "x-custom2", parentId: "x-custom", type: "custom", customType: "ultron.empty", timestamp: FIXTURE_START + 103_000 },
		{ kind: "value", op: "set", namespace: "pi.branch.tip", key: "main", value: "x-custom2" },
	);
	fixture.run({ lane: "ultron.rlm-frame.f1", prompt: "frame work", responses: [{ usage: { input: 1, output: 1 } }] });
	return session(APP, fixture, FIXTURE_START + 7 * HOUR, { torn });
}


// A minimal stdio MCP server: tools (one hidden by toolExposure), resources (one an MCP app), no templates.
const MCP_FAKE_SERVER = `import json, sys
def send(m):
    sys.stdout.write(json.dumps(m) + "\\n"); sys.stdout.flush()
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m: continue
    method = m.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": m["params"]["protocolVersion"], "capabilities": {"tools": {}, "resources": {}}, "serverInfo": {"name": "fake", "version": "1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}, {"name": "secret_dump", "inputSchema": {"type": "object"}}]}})
    elif method == "resources/list":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"resources": [{"uri": "file:///a", "name": "a"}, {"uri": "ui://app", "name": "app"}]}})
    else:
        send({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32601, "message": "Method not found"}})
`;
const MCP_FAKE_CONFIG = JSON.stringify({ mcpServers: { fake: { command: "/usr/bin/python3", args: ["-I", "srv.py"], toolExposure: { "secret_*": "hidden" } }, off: { command: "x", enabled: false } } });

const oauth = (access, expires) => ({ type: "oauth", access, refresh: "refresh-token", expires });
const FAR = 4_102_444_800_000; // 2100-01-01

// Each case: args, optional env, cwd (relative to the temp dir, default "work"), files ({path: text | null for a
// directory}; text may hold <TMP>), mtimes ({path: ms}), sessions ([{path, text, mtime}]) and collect (paths).
const CASES = [
	// ultron usage
	{ args: ["usage", "--help"] },
	{ args: ["usage", "-h", "--bogus"] },
	{ args: ["usage", "--bogus"] },
	{ args: ["usage", "--last", "0"] },
	{ args: ["usage", "--last=abc"] },
	{ args: ["usage", "--last"] },
	{ args: ["usage", "--last="] },
	{ args: ["usage", "--here"] },
	{ args: ["usage", "a", "b"] },
	{ args: ["usage", "a", "--last", "3"] },
	{ args: ["usage", "--sessions-root"] },
	{ args: ["usage"] },
	{ args: ["usage", "--last", "5"] },
	{ args: ["usage", "--last", "5", "--json"] },
	{ args: ["usage", "--utc"], sessions: reportSessions() },
	{ args: ["usage", "--utc"], sessions: [...reportSessions(), emptyWork()] },
	{ args: ["usage", "--utc"], sessions: [...reportSessions(), emptyWork(), ranWork()] },
	{ args: ["usage", "--json"], sessions: [...reportSessions(), emptyWork(), ranWork()] },
	{ args: ["usage"], env: { TZ: "Asia/Kolkata" }, sessions: [...reportSessions(), ranWork()] },
	{ args: ["usage"], env: { TZ: "America/New_York" }, sessions: [...reportSessions(), ranWork()] },
	{ args: ["usage", "--last", "10", "--utc"], sessions: reportSessions() },
	{ args: ["usage", "--last", "10", "--utc"], sessions: [...reportSessions(), emptyWork(), ranWork()] },
	{ args: ["usage", "--last", "10", "--utc", "--all"], sessions: [...reportSessions(), emptyWork(), ranWork()] },
	{ args: ["usage", "--last", "2", "--utc"], sessions: [...reportSessions(), emptyWork(), ranWork()] },
	{ args: ["usage", "--last", "1", "--utc"], sessions: [...reportSessions(), emptyWork()] },
	{ args: ["usage", "--last", "10", "--here", "--utc"], sessions: [...reportSessions(), ranWork()] },
	{ args: ["usage", "--last", "10", "--here"], sessions: reportSessions() },
	{ args: ["usage", "--last", "10", "--here", "--all", "--utc"], sessions: [...reportSessions(), emptyWork()] },
	{ args: ["usage", "--last", "3", "--json"], sessions: [...reportSessions(), emptyWork(), ranWork()] },
	{ args: ["usage", "--last", "10", "--json", "--here"], sessions: reportSessions() },
	...["aaaaaaaa-0001-7000-8000-000000000001", "bbbbbbbb", "cccccccc-0003-7000-8000-000000000003", "dddd", "eeeeeeee-0005-7000-8000-000000000005"].flatMap(
		(id) => [
			{ args: ["usage", id, "--utc"], sessions: reportSessions() },
			{ args: ["usage", id, "--json"], sessions: reportSessions() },
		],
	),
	{ args: ["usage", "--json"], sessions: [workSession("66666666-0006-7000-8000-000000000006", (s) => { s.value("ultron.module", "stats", fixtureStats({ since: FIXTURE_START + 500, hostCalls: { "rlm.map": 2 } })); s.run({ lane: "main", responses: [{ usage: { input: 1 } }] }); }, FIXTURE_START + HOUR)] },
	{ args: ["usage", "--utc"], sessions: [workSession("66666666-0006-7000-8000-000000000006", (s) => { s.value("ultron.module", "stats", fixtureStats({ since: FIXTURE_START + 1500 })); s.run({ lane: "main", responses: [{ usage: { input: 999_960 } }] }); }, FIXTURE_START + HOUR)] },
	{ args: ["usage", "nope"], sessions: reportSessions() },
	{ args: ["usage", "nope"] },
	{ args: ["usage", "a"], sessions: [...reportSessions(), session(APP, new SessionFixture("aaaaaaaa-0011-7000-8000-000000000011", "/home/dev/app"), FIXTURE_START)] },
	{ args: ["usage", "ffffffff"], sessions: [session(APP, sessions.claudeHostSession(true), FIXTURE_START + HOUR), ...["01", "02", "03", "04", "05", "06"].map((n, i) => session(APP, new SessionFixture(`ffffffff-00${n}-7000-8000-0000000000${n}`, "/home/dev/app"), FIXTURE_START + i))] },
	{
		args: ["usage", "claude-sess-1", "--utc"],
		sessions: reportSessions(),
		files: { "agent/claude-code/sessions/claude-sess-1.json": `{"path":"<TMP>/${SESSIONS}/--home-dev-app--/2026-09-01T10-00-00-000Z_dddddddd-0004-7000-8000-000000000004.jsonl"}` },
	},
	{ args: ["usage", "claude-sess-2"], files: { "agent/claude-code/sessions/claude-sess-2.json": '{"path":"<TMP>/missing.jsonl"}' } },
	{ args: ["usage", "pi.jsonl"], files: { "work/pi.jsonl": `${piSession}\n` } },
	{ args: ["usage", "notjson.jsonl"], files: { "work/notjson.jsonl": "not json\n" } },
	{ args: ["usage", "other.jsonl"], files: { "work/other.jsonl": '{"kind":"other"}\n' } },
	{ args: ["usage", "empty.jsonl"], files: { "work/empty.jsonl": "" } },
	{ args: ["usage", "--last", "10", "--utc", "--all"], sessions: reportSessions(), files: { [`${SESSIONS}/--x--/legacy.jsonl`]: `${piSession}\n`, [`${SESSIONS}/--x--/notes.txt`]: "x" }, mtimes: { [`${SESSIONS}/--x--/legacy.jsonl`]: FIXTURE_START + 20 * HOUR } },
	{ args: ["usage", "torn.jsonl", "--json"], files: { "work/torn.jsonl": sessions.rootOnlySession().text({ torn: true }) }, mtimes: { "work/torn.jsonl": FIXTURE_START } },
	{ args: ["usage", "--sessions-root", "<TMP>/elsewhere", "--last", "10", "--utc"], files: { "elsewhere/--a--/s.jsonl": sessions.framesOnlySession().text() }, mtimes: { "elsewhere/--a--/s.jsonl": FIXTURE_START } },
	{ args: ["usage", "--sessions-root=../elsewhere", "--last", "10", "--utc"], files: { "elsewhere/--a--/s.jsonl": sessions.framesOnlySession().text() }, mtimes: { "elsewhere/--a--/s.jsonl": FIXTURE_START } },
	{ args: ["usage", "--last", "10", "--utc"], env: { ULTRON_CODING_AGENT_SESSION_DIR: "<TMP>/elsewhere" }, files: { "elsewhere/--a--/s.jsonl": sessions.subagentSession().text() }, mtimes: { "elsewhere/--a--/s.jsonl": FIXTURE_START } },

	// ultron --export
	{ args: ["--export", "<TMP>/" + exportSession(false).path, "out.html"], sessions: [exportSession(false)], collect: ["work/out.html"] },
	{ args: ["--export", "<TMP>/" + exportSession(false).path], sessions: [exportSession(false)], collect: ["work/ultron-session-2026-09-01T10-00-00-000Z_eeee0000-0010-7000-8000-000000000010.html"] },
	{ args: ["--export", "<TMP>/" + exportSession(true).path, "torn.html"], sessions: [exportSession(true)], collect: ["work/torn.html", exportSession(true).path] },
	{ args: ["--export", "../" + reportSessions()[4].path, "root.html"], sessions: reportSessions(), collect: ["work/root.html"] },
	{ args: ["--export", "../" + reportSessions()[0].path, "<TMP>/sub.html"], sessions: reportSessions(), collect: ["sub.html"] },
	{ args: ["--export", "pi.jsonl"], files: { "work/pi.jsonl": `${piSession}\n` }, collect: ["work/ultron-session-pi.html"] },
	{ args: ["--export", "pi.jsonl", "light.html"], env: { COLORFGBG: "0;15" }, files: { "work/pi.jsonl": `${piSession}\n` }, collect: ["work/light.html"] },
	{ args: ["--export", "missing.jsonl"] },
	{ args: ["--export", "<TMP>/" + `${SESSIONS}/--a--/new.jsonl`], files: { [`${SESSIONS}/--a--/new.jsonl`]: '{"v":4,"kind":"header","id":"n1","storageVersion":1,"createdAt":1,"cwd":"/x"}\n' }, collect: ["work/ultron-session-new.html"] },
	{ args: ["--export", "<TMP>/" + `${SESSIONS}/--a--/bad.jsonl`], files: { [`${SESSIONS}/--a--/bad.jsonl`]: '{"kind":"header","id":"n1","createdAt":1,"cwd":"/x"}\n' } },
	{ args: ["--export", "<TMP>/" + `${SESSIONS}/--a--/badline.jsonl`], files: { [`${SESSIONS}/--a--/badline.jsonl`]: '{"v":4,"kind":"header","id":"n1","storageVersion":1,"createdAt":1,"cwd":"/x"}\n{"kind":"nope","seq":1}\n' } },

	// ultron auth
	{ args: ["auth"] },
	{ args: ["auth", "help"] },
	{ args: ["auth", "check", "--help"] },
	{ args: ["auth", "bogus"] },
	{ args: ["auth", "check"] },
	{ args: ["auth", "check", "--provider", "anthropic"] },
	{ args: ["auth", "check", "--provider", "anthropic", "--json"] },
	{ args: ["auth", "check", "--provider", "anthropic"], env: { ANTHROPIC_API_KEY: "sk-env" } },
	{ args: ["auth", "check", "--provider", "anthropic", "--json", "--credentials"], env: { ANTHROPIC_API_KEY: "sk-env" } },
	{ args: ["auth", "check", "--provider", "anthropic", "--credentials"], env: { ANTHROPIC_API_KEY: "sk-env" } },
	{ args: ["auth", "check", "--provider", "nope", "--json"] },
	{ args: ["auth", "check", "--provider", "openai", "--json", "--credentials"], files: { "agent/auth.json": JSON.stringify({ openai: { type: "api_key", key: "sk-stored" } }) } },
	{ args: ["auth", "check", "--provider", "anthropic", "--json", "--credentials", "--no-refresh"], files: { "agent/auth.json": JSON.stringify({ anthropic: oauth("oauth-access", FAR) }) } },
	{ args: ["auth", "check", "--provider", "anthropic", "--json", "--credentials"], files: { "agent/auth.json": JSON.stringify({ anthropic: oauth("oauth-access", FAR) }) } },
	{ args: ["auth", "check", "--provider", "groq", "--json"], files: { "agent/auth.json": "{not json" } },
	{ args: ["auth", "check", "--provider", "anthropic", "--bogus"] },
	{ args: ["auth", "check", "--provider", "anthropic", "hello"] },
	{ args: ["auth", "check", "--provider", "anthropic", "--api-key", "x"] },
	{ args: ["auth", "check", "--provider", "  "] },
	{ args: ["auth", "print-api-key"] },
	{ args: ["auth", "print-api-key", "--provider", "anthropic"] },
	{ args: ["auth", "print-api-key", "--provider", "anthropic"], env: { ANTHROPIC_API_KEY: "sk-env" } },
	{ args: ["auth", "print-api-key", "--provider", "nope"], env: { ANTHROPIC_API_KEY: "sk-env" } },
	{ args: ["auth", "print-api-key", "--provider", "anthropic", "--json"] },
	{ args: ["auth", "print-api-key", "--provider", "anthropic"], files: { "agent/auth.json": JSON.stringify({ anthropic: oauth("oauth-access", FAR) }) } },
	{ args: ["auth", "print-api-key", "--provider", "openai"], files: { "agent/auth.json": JSON.stringify({ openai: { type: "api_key", key: "sk-stored" } }) } },
	{ args: ["auth", "print-bearer-token", "--provider", "anthropic"], files: { "agent/auth.json": JSON.stringify({ anthropic: oauth("oauth-access", FAR) }) } },
	{ args: ["auth", "print-bearer-token", "--provider", "anthropic", "--min-expiry", "1h"], files: { "agent/auth.json": JSON.stringify({ anthropic: oauth("oauth-access", FAR) }) } },
	{ args: ["auth", "print-bearer-token", "--provider", "anthropic"], env: { ANTHROPIC_API_KEY: "sk-env" } },
	{ args: ["auth", "print-bearer-token", "--min-expiry", "soon"] },
	{ args: ["auth", "print-bearer-token", "--min-expiry"] },
	{ args: ["auth", "print-api-key", "--min-expiry", "1h"] },
	{ args: ["auth", "check", "--provider", "openai", "--json"], env: { OPENAI_API_KEY: "sk-env" } },
	{ args: ["auth", "check", "--provider", "groq"], files: { "agent/auth.json": JSON.stringify({ groq: { type: "api_key", key: "gsk" } }) } },
	{ args: ["auth", "check", "--provider", "anthropic", "--json"], files: { "agent/auth.json": JSON.stringify({ anthropic: oauth("old", 1000) }) } },
	{ args: ["auth", "check", "--provider", "anthropic", "--json", "--no-refresh", "--credentials"], files: { "agent/auth.json": JSON.stringify({ anthropic: oauth("old", 1000) }) } },
	{ args: ["auth", "print-api-key", "--provider", "groq"], files: { "agent/auth.json": "{not json" } },
	{ args: ["auth", "print-api-key", "--provider", "openai"], env: { MY_KEY: "from-env" }, files: { "agent/auth.json": JSON.stringify({ openai: { type: "api_key", key: "$MY_KEY" } }) } },
	{ args: ["auth", "check", "--provider", "openai", "--credentials"], files: { "agent/auth.json": JSON.stringify({ openai: { type: "api_key", key: "${UNSET_KEY}" } }) } },
	{ args: ["auth", "check", "--provider", "openai-codex", "--json"], files: { "agent/auth.json": JSON.stringify({ "openai-codex": { type: "api_key", key: "k" } }) } },
	{ args: ["auth", "check", "--provider", "groq", "--json"], files: { "agent/auth.json": JSON.stringify({ groq: oauth("tok", FAR) }) } },
	{ args: ["auth", "print-bearer-token", "--provider", "xai", "--min-expiry", "2H"], files: { "agent/auth.json": JSON.stringify({ xai: oauth("x-access", FAR) }) } },
	{ args: ["auth", "check", "--provider", "google-vertex"], env: { GOOGLE_CLOUD_PROJECT: "p", GOOGLE_CLOUD_LOCATION: "l", GOOGLE_APPLICATION_CREDENTIALS: "<TMP>/adc.json" }, files: { "adc.json": "{}" } },
	{ args: ["auth", "check", "--provider", "amazon-bedrock", "--credentials"], env: { AWS_PROFILE: "x" } },
	{ args: ["auth", "check", "--provider", "anthropic", "--thinking", "bogus"] },
	{ args: ["--help", "--export", "pi.jsonl", "h.html"], files: { "work/pi.jsonl": `${piSession}\n` }, collect: ["work/h.html"] },
	{ args: ["--version", "--export", "pi.jsonl"], files: { "work/pi.jsonl": `${piSession}\n` } },

	// ultron mcp add|remove|list
	{ args: ["mcp", "help"] },
	{ args: ["mcp", "--help"] },
	{ args: ["mcp", "list", "-h"] },
	{ args: ["mcp", "list"] },
	{ args: ["mcp", "list", "--json"] },
	{ args: ["mcp", "list", "extra"] },
	{ args: ["mcp", "list", "--bogus"] },
	{ args: ["mcp", "add"] },
	{ args: ["mcp", "add", "s1"] },
	{ args: ["mcp", "add", "s1", "--url", "https://x.test/mcp", "--", "cmd"] },
	{ args: ["mcp", "add", "s1", "--url"] },
	{ args: ["mcp", "add", "s1", "--header", "A=1", "--", "cmd"] },
	{ args: ["mcp", "add", "s1", "--url", "https://x.test/mcp", "--env", "A=1"] },
	{ args: ["mcp", "add", "s1", "--url", "https://x.test/mcp", "--header", "noequals"] },
	{ args: ["mcp", "add", "bad name", "--", "cmd"] },
	{ args: ["mcp", "add", "s1", "--exposure", "weird", "--", "cmd"] },
	{ args: ["mcp", "add", "s1", "--env", "A=1", "--env", "B=x=y", "--cwd", "sub", "--description", "Demo", "--", "node", "server.js", "--flag", "-l"], collect: ["agent/mcp.json"] },
	{ args: ["mcp", "add", "web", "--url", "https://x.test/mcp", "--header", "X-A=1", "--oauth-client-id", "id", "--oauth-client-secret", "${S}", "--oauth-callback-port", "8080", "--oauth-client-name", "n", "--exposure", "hidden"], collect: ["agent/mcp.json"] },
	{ args: ["mcp", "add", "web", "--url", "https://x.test/mcp", "--bearer-token-env-var", "TOKEN", "--oauth-callback-port", "abc"], collect: ["agent/mcp.json"] },
	{ args: ["mcp", "add", "s1", "-l", "--", "cmd", "a"], collect: ["work/.pi/mcp.json"] },
	{ args: ["mcp", "add", "s1", "--", "cmd2"], files: { "agent/mcp.json": '{\n\t"other": 1,\n\t"mcpServers": {\n\t\t"s1": { "command": "old" },\n\t\t"s2": { "command": "keep", "args": [] }\n\t}\n}\n' }, collect: ["agent/mcp.json"] },
	{ args: ["mcp", "add", "s3", "--", "cmd3"], files: { "agent/mcp.json": '{"x": [1, 2.5, {}], "10": true, "2": null}' }, collect: ["agent/mcp.json"] },
	{ args: ["mcp", "add", "s3", "--", "cmd3"], files: { "agent/mcp.json": '{"mcpServers": []}' }, collect: ["agent/mcp.json"] },
	{ args: ["mcp", "remove"] },
	{ args: ["mcp", "remove", "s1", "s2"] },
	{ args: ["mcp", "remove", "s1"] },
	{ args: ["mcp", "remove", "s1"], files: { "agent/mcp.json": '{\n    "mcpServers": {\n        "s1": { "command": "a" },\n        "s2": { "command": "b" }\n    }\n}\n' }, collect: ["agent/mcp.json"] },
	{ args: ["mcp", "remove", "s1"], files: { "work/.pi/mcp.json": '{"mcpServers": {"s1": {"command": "a"}}}' }, collect: ["work/.pi/mcp.json"] },
	{ args: ["mcp", "remove", "s1", "--local"], files: { "agent/mcp.json": '{"mcpServers": {"s1": {"command": "a"}}}' } },
	{ args: ["mcp", "remove", "s1", "-l"], files: { "work/.pi/mcp.json": '{"mcpServers": {"s1": {"command": "a"}}}' }, collect: ["work/.pi/mcp.json"] },
	{ args: ["mcp", "list"], files: { "agent/mcp.json": '{"mcpServers": {"off": {"command": "nope", "args": ["a"], "enabled": false}, "bad name": {"command": "x"}, "h": {"url": "https://x.test", "enabled": false, "exposure": "hidden"}}}', "work/.pi/mcp.json": "{}" } },
	{ args: ["mcp", "list", "--json"], files: { "agent/mcp.json": '{"mcpServers": {"off": {"command": "nope", "args": ["a"], "enabled": false}, "bad name": {"command": "x"}}}', "work/.pi/mcp.json": "{}" } },
	{ args: ["mcp", "list"], files: { "agent/mcp.json": "[1]" } },
	{ args: ["mcp", "list"], files: { "agent/mcp.json": MCP_FAKE_CONFIG, "work/srv.py": MCP_FAKE_SERVER } },
	{ args: ["mcp", "list", "--json"], files: { "agent/mcp.json": MCP_FAKE_CONFIG, "work/srv.py": MCP_FAKE_SERVER } },
	{ args: ["mcp", "list"], files: { "agent/mcp.json": '{"mcpServers": {"dies": {"command": "/usr/bin/python3", "args": ["-I", "-c", "import sys; sys.stderr.write(\\"boom\\\\n\\"); sys.exit(3)"]}}}' } },
	{ args: ["mcp", "logout", "nope"] },
	{ args: ["mcp", "logout"] },
	{ args: ["mcp", "login", "s1", "--bogus"], files: { "agent/mcp.json": '{"mcpServers": {"s1": {"command": "a"}}}' } },
	{ args: ["mcp", "logout", "s1"], files: { "agent/mcp.json": '{"mcpServers": {"s1": {"command": "a"}}}', "work/.pi/mcp.json": "{}" } },
	{ args: ["mcp", "logout", "h"], files: { "agent/mcp.json": '{"mcpServers": {"h": {"url": "https://x.test", "headers": {"authorization": "Bearer x"}}}}' } },
	// ultron migrate
	{ args: ["migrate"] },
	{ args: ["migrate", "help"] },
	{ args: ["migrate", "-h"] },
	{ args: ["migrate", "bogus"] },
	{ args: ["migrate", "import-pi"] },
	{ args: ["migrate", "import-pi", "a", "b"] },
	{ args: ["migrate", "import-pi", "a", "--bogus=1"] },
	{ args: ["migrate", "import-pi", "a", "--sessions-root"] },
	{ args: ["migrate", "export-pi", "a"] },
	{ args: ["migrate", "export-pi", "a", "b", "--sessions-root", "x"] },
	{ args: ["migrate", "backup", "a", "b"] },
	{ args: ["migrate", "backup", "--agent-dir="] },
	{ args: ["migrate", "backup", "--agent-dir", "<TMP>/nope"] },
	{ args: ["migrate", "restore"] },
	{
		args: ["migrate", "backup", "<TMP>/bk"],
		files: {
			"agent/settings.json": '{"a":1}',
			"agent/auth.json": '{"k":"secret"}',
			"agent/sessions/x.jsonl": "s",
			"agent/experimental/sessions/--a--/y.jsonl": "s",
			"agent/experimental/notes.txt": "n",
			"agent/npm/package.json": "{}",
			"agent/x.lock": "l",
			"agent/team-sessions/z": "z",
			"agent/B.md": "b",
			"agent/a.md": "a",
		},
		modes: { "agent/auth.json": 0o600, "agent/a.md": 0o640 },
		symlinks: { "agent/link": "settings.json" },
		collectTree: ["bk"],
	},
	{
		args: ["migrate", "backup"],
		files: { "agent/settings.json": '{"a":1}', "agent/backups/old/file": "o" },
		collectTree: ["backups", "agent/backups"],
	},
	{
		args: ["migrate", "backup", "--agent-dir", "<TMP>/agent", "<TMP>/agent/inner"],
		files: { "agent/settings.json": '{"a":1}' },
		collectTree: ["agent/inner"],
	},
	{
		args: ["migrate", "restore", "<TMP>/bk", "--agent-dir", "<TMP>/restored"],
		files: {
			"bk/manifest.json": JSON.stringify({ format: "ultron-profile-backup", version: 1, createdAt: "x", sourceAgentDir: "/a", entries: [
				{ path: "settings.json", type: "file", size: 7, sha256: "015abd7f5cc57a2dd94b7590f04ad8084273905ee33ec5cebeae62276a97f862", mode: 416 },
				{ path: "dir/sub.txt", type: "file", size: 3, sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", mode: 420 },
				{ path: "lnk", type: "symlink", target: "settings.json" },
			] }),
			"bk/files/settings.json": '{"a":1}',
			"bk/files/dir/sub.txt": "abc",
			"restored/keep.txt": "keep",
		},
		symlinks: { "bk/files/lnk": "settings.json" },
		collectTree: ["restored"],
	},
	{
		args: ["migrate", "restore", "<TMP>/bk", "--agent-dir", "<TMP>/restored"],
		files: {
			"bk/manifest.json": JSON.stringify({ format: "ultron-profile-backup", version: 1, createdAt: "x", sourceAgentDir: "/a", entries: [
				{ path: "settings.json", type: "file", size: 7, sha256: "0000", mode: 416 },
				{ path: "../evil", type: "file", size: 1, sha256: "x", mode: 416 },
				{ path: "gone", type: "file", size: 1, sha256: "x", mode: 416 },
				{ path: "lnk", type: "symlink", target: "other" },
				{ path: "weird", type: "fifo" },
			] }),
			"bk/files/settings.json": '{"a":1}',
			"bk/files/weird": "w",
			"bk/files/extra/file": "e",
		},
		symlinks: { "bk/files/lnk": "settings.json" },
		collectTree: ["restored"],
	},
	{ args: ["migrate", "restore", "<TMP>/bk"], files: { "bk/manifest.json": '{"format":"other"}' } },
	{ args: ["migrate", "restore", "<TMP>/bk"], files: { "bk/manifest.json": '{"format":"ultron-profile-backup","version":1}' } },
	{ args: ["migrate", "restore", "<TMP>/missing"] },
	{
		args: ["migrate", "restore", "bk"],
		cwd: ".",
		files: {
			"bk/manifest.json": JSON.stringify({ format: "ultron-profile-backup", version: 1, createdAt: "x", sourceAgentDir: "/a", entries: [
				{ path: "settings.json", type: "file", size: 7, sha256: "015abd7f5cc57a2dd94b7590f04ad8084273905ee33ec5cebeae62276a97f862", mode: 384 },
			] }),
			"bk/files/settings.json": '{"a":1}',
			"agent/settings.json": null,
		},
	},

	// ultron install|remove|update|list|config
	...["install", "remove", "uninstall", "update", "list"].map((c) => ({ args: [c, "--help"] })),
	{ args: ["install"] },
	{ args: ["install", "a", "b"] },
	{ args: ["install", "a", "--self"] },
	{ args: ["list", "-l"] },
	{ args: ["update", "--extension"] },
	{ args: ["update", "--extension", "-x"] },
	{ args: ["update", "--extension", "a", "--extension", "b"] },
	{ args: ["update", "--all", "--self"] },
	{ args: ["update", "--all", "x"] },
	{ args: ["update", "--models", "--extensions"] },
	{ args: ["update", "--models", "x"] },
	{ args: ["update", "--extension", "a", "--self"] },
	{ args: ["update", "--extension", "a", "b"] },
	{ args: ["update", "x", "--extensions"] },
	{ args: ["remove", "--bogus", "--other"] },
	{ args: ["list"] },
	{ args: ["list"], files: { "agent/settings.json": JSON.stringify({ packages: ["npm:@scope/pkg@1.2.3", "npm:plain", { source: "git:github.com/user/repo@v1", extensions: [] }, "https://gitlab.com/o/r.git", "./local", "../missing"] }), "agent/npm/node_modules/@scope/pkg/package.json": "{}", "agent/git/github.com/user/repo/x": "x", "agent/local/a": "a" } },
	{ args: ["list"], files: { "work/.pi/settings.json": JSON.stringify({ packages: ["./p"] }), "work/.pi/p/x": "x" } },
	{ args: ["list", "--approve"], files: { "agent/settings.json": JSON.stringify({ packages: ["npm:a"] }), "work/.pi/settings.json": JSON.stringify({ packages: ["./p"] }), "work/.pi/p/x": "x" } },
	{ args: ["install", "./pkg"], files: { "work/pkg/index.ts": "x" }, collect: ["agent/settings.json"] },
	{ args: ["install", "<TMP>/work/pkg"], files: { "work/pkg/index.ts": "x", "agent/settings.json": JSON.stringify({ theme: "dark", packages: [{ source: "../work/pkg", skills: [] }] }, null, 2) }, collect: ["agent/settings.json"] },
	{ args: ["install", "./pkg"], files: { "work/pkg/index.ts": "x", "agent/settings.json": JSON.stringify({ packages: ["../work/pkg"] }) }, collect: ["agent/settings.json"] },
	{ args: ["install", "./nope"] },
	{ args: ["install", "./pkg", "-l"], files: { "work/pkg/index.ts": "x" }, collect: ["work/.pi/settings.json"] },
	{ args: ["install", "./pkg", "-l", "--no-approve"], files: { "work/pkg/index.ts": "x" } },
	{ args: ["install", "./pkg", "-l"], files: { "work/pkg/index.ts": "x", "work/.pi/prompts/p.md": "x" } },
	{ args: ["remove", "./pkg"], files: { "agent/settings.json": JSON.stringify({ packages: ["../work/pkg", "npm:x"] }) }, collect: ["agent/settings.json"] },
	{ args: ["uninstall", "./other"], files: { "agent/settings.json": JSON.stringify({ packages: ["../work/pkg"] }) }, collect: ["agent/settings.json"] },
	{ args: ["remove", "./pkg", "-l"], files: { "work/.pi/settings.json": JSON.stringify({ packages: ["../pkg"] }) }, collect: ["work/.pi/settings.json"] },
	{ args: ["update", "--extensions"] },
	{ args: ["update", "--extensions"], files: { "agent/settings.json": JSON.stringify({ packages: ["./local"] }) } },
	{ args: ["update", "./local"], files: { "agent/settings.json": JSON.stringify({ packages: ["../work/local"] }) } },
	{ args: ["update", "--extension", "./nope"] },
	{ args: ["config", "--help"] },
	{ args: ["config", "--bogus"] },
	{ args: ["config", "extra"] },
	{ args: ["config", "-l", "-na"] },

	// ultron setup without a terminal
	{ args: ["setup", "--help"] },
	{ args: ["setup", "x", "-h"] },
	{ args: ["setup", "now"] },
	{ args: ["setup"] },
];

const node = process.execPath;
const sub = (s, tmp) => s.split(tmp).join("<TMP>").split(repo).join("<REPO>");

function writeCase(tmp, c) {
	const put = (rel, text, mtime) => {
		const path = join(tmp, rel);
		mkdirSync(dirname(path), { recursive: true });
		if (text === null) mkdirSync(path, { recursive: true });
		else writeFileSync(path, text.split("<TMP>").join(tmp));
		if (mtime !== undefined) utimesSync(path, new Date(mtime), new Date(mtime));
	};
	for (const s of c.sessions ?? []) put(s.path, s.text, s.mtime);
	for (const [rel, text] of Object.entries(c.files ?? {})) put(rel, text, c.mtimes?.[rel]);
	for (const [rel, mode] of Object.entries(c.modes ?? {})) chmodSync(join(tmp, rel), mode);
	for (const [rel, target] of Object.entries(c.symlinks ?? {})) {
		mkdirSync(dirname(join(tmp, rel)), { recursive: true });
		symlinkSync(target, join(tmp, rel));
	}
}

/** Timestamps a run takes from the clock (backup directory names, manifest times). */
const normalize = (s) => s.replace(/\d{4}-\d\d-\d\dT\d\d[-:]\d\d[-:]\d\d[-.]\d{3}Z/g, "<TIME>");

/** Every entry under a directory: path, kind, mode, and file text (link target for a symlink). */
function tree(tmp, rel) {
	const root = join(tmp, rel);
	if (!existsSync(root)) return { path: rel, missing: true };
	const entries = [];
	const walk = (dir) => {
		for (const name of readdirSync(join(root, dir)).sort()) {
			const p = dir ? `${dir}/${name}` : name;
			const info = lstatSync(join(root, p));
			const mode = info.mode & 0o777;
			if (info.isSymbolicLink()) entries.push({ path: normalize(p), kind: "symlink", target: readlinkSync(join(root, p)) });
			else if (info.isDirectory()) {
				entries.push({ path: normalize(p), kind: "dir", mode });
				walk(p);
			} else entries.push({ path: normalize(p), kind: "file", mode, text: normalize(sub(readFileSync(join(root, p), "utf8"), tmp)) });
		}
	};
	walk("");
	return { path: rel, entries };
}

function collected(tmp, rel) {
	const path = join(tmp, rel);
	if (!existsSync(path)) return { path: rel, missing: true };
	const bytes = readFileSync(path);
	if (!rel.endsWith(".html")) return { path: rel, text: sub(bytes.toString("utf8"), tmp) };
	const html = bytes.toString("utf8");
	const data = /<script id="session-data" type="application\/json">([^<]*)<\/script>/.exec(html)?.[1];
	return {
		path: rel,
		sha256: createHash("sha256").update(bytes).digest("hex"),
		bytes: bytes.length,
		sessionData: data === undefined ? null : Buffer.from(data, "base64").toString("utf8"),
	};
}

const runs = CASES.map((c) => {
	const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-subcmd-fixture-")));
	for (const d of ["work", "agent", "server"]) mkdirSync(join(tmp, d), { recursive: true });
	writeCase(tmp, c);
	const out = openSync(join(tmp, "stdout"), "w");
	const err = openSync(join(tmp, "stderr"), "w");
	const env = {
		PATH: `${dirname(node)}:/usr/bin:/bin`,
		HOME: tmp,
		TZ: "UTC",
		ULTRON_CODING_AGENT_DIR: join(tmp, "agent"),
		ULTRON_SERVER_DIR: join(tmp, "server"),
		...Object.fromEntries(Object.entries(c.env ?? {}).map(([k, v]) => [k, v.split("<TMP>").join(tmp)])),
	};
	const r = spawnSync(node, [join(repo, "packages/coding-agent/src/cli.ts"), ...c.args.map((a) => a.split("<TMP>").join(tmp))], {
		cwd: join(tmp, c.cwd ?? "work"),
		env,
		stdio: ["ignore", out, err],
		timeout: 120_000,
	});
	closeSync(out);
	closeSync(err);
	const rec = {
		...c,
		stdout: normalize(sub(readFileSync(join(tmp, "stdout"), "utf8"), tmp)),
		stderr: normalize(sub(readFileSync(join(tmp, "stderr"), "utf8"), tmp)),
		status: r.status,
		...(c.collect ? { collected: c.collect.map((rel) => collected(tmp, rel)) } : {}),
		...(c.collectTree ? { trees: c.collectTree.map((rel) => tree(tmp, rel)) } : {}),
	};
	rmSync(tmp, { recursive: true, force: true });
	return rec;
});

writeFileSync(join(here, "subcommands.json"), `${JSON.stringify({ cases: runs }, null, 1)}\n`);
