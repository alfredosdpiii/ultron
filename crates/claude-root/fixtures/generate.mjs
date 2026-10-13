// Generates crates/claude-root/fixtures/claude.json from the TypeScript CLI in this checkout
// (packages/coding-agent/src/ultron/claude/*, reached through `node packages/coding-agent/src/cli.ts <cmd>`).
//
// Every run gets a private temp root <T> (HOME, ULTRON_CODING_AGENT_DIR, ULTRON_SERVER_DIR, XDG_RUNTIME_DIR, TMPDIR,
// a working directory) and a FAKE `claude` (fixtures/fake_claude.py) first on PATH; the real Claude Code is never run.
// Three tables:
//   processes: one command per case (guide, hook, watch, claude, mcp argument errors), stdout/stderr/exit code, and
//              for launches what the fake claude was started with;
//   mcp:       JSON-RPC sessions with `ultron mcp`: steps of request lines, each waiting for the listed responses;
//              then control-socket requests, `ultron hook` runs and `ultron watch --list` against the live server;
//   (all strings normalized: <T>, <REPO>, L-<ID>, <RND>, <DATE>, pids and times).
// Usage: node crates/claude-root/fixtures/generate.mjs   (needs /usr/bin/python3 or ULTRON_PYTHON for the kernel)
import { spawn, spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { createConnection, createServer } from "node:net";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../..");
const cli = join(repo, "packages/coding-agent/src/cli.ts");
const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";
const fake = readFileSync(join(here, "fake_claude.py"), "utf8");

function makeRoot() {
	const root = mkdtempSync("/tmp/ucg-");
	for (const dir of ["h", "a", "s", "r", "w", "bin", "tmp"]) mkdirSync(join(root, dir));
	writeFileSync(join(root, "bin", "claude"), `#!${python} -I\n${fake}`);
	chmodSync(join(root, "bin", "claude"), 0o755);
	return root;
}

function baseEnv(root, extra = {}) {
	return {
		PATH: `${join(root, "bin")}:/usr/bin:/bin`,
		HOME: join(root, "h"),
		ULTRON_CODING_AGENT_DIR: join(root, "a"),
		ULTRON_SERVER_DIR: join(root, "s"),
		XDG_RUNTIME_DIR: join(root, "r"),
		TMPDIR: join(root, "tmp"),
		ULTRON_SELF_COMMAND: '["/opt/ultron/bin/ultron"]',
		ULTRON_LOKI: "off",
		ULTRON_PYTHON: python,
		FAKE_CLAUDE_OUT: join(root, "launch.json"),
		LANG: "C.UTF-8",
		...extra,
	};
}

function normalize(text, root) {
	if (typeof text !== "string") return text;
	return text
		.split(realpathSync(root))
		.join("<T>")
		.split(root)
		.join("<T>")
		.split(repo)
		.join("<REPO>")
		.replace(/L-[0-9a-z]+-[0-9a-f]{8}/g, "L-<ID>")
		.replace(/ultron-claude-[A-Za-z0-9]{6}/g, "ultron-claude-<RND>")
		.replace(/- Date: \d{4}-\d{2}-\d{2}/g, "- Date: <DATE>");
}

// `--print-config` reports the system prompt's length, which embeds checkout and temp paths: after normalizing,
// the count is taken again from the normalized prompt (the inline `--system-prompt` value), so the fixture does not
// depend on where it was recorded or replayed.
function stablePrintConfig(stdout) {
	let printed;
	try {
		printed = JSON.parse(stdout);
	} catch {
		return stdout;
	}
	const index = Array.isArray(printed?.args) ? printed.args.indexOf("--system-prompt") : -1;
	if (typeof printed?.systemPrompt?.chars !== "number" || index === -1) return stdout;
	printed.systemPrompt.chars = printed.args[index + 1].length;
	return `${JSON.stringify(printed, null, 2)}\n`;
}

function deep(value, root) {
	if (typeof value === "string") return normalize(value, root);
	if (Array.isArray(value)) return value.map((item) => deep(item, root));
	if (value && typeof value === "object") return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, deep(v, root)]));
	return value;
}

// Files a case puts in <T> before it runs (paths relative to <T>).
const PROJECT = {
	"w/AGENTS.md": "# Project rules\nUse tabs.\n",
	"w/.pi/skills/deploy/SKILL.md": "---\nname: deploy\ndescription: Deploy the app to staging.\n---\nRun make deploy.\n",
	"a/skills/notes/SKILL.md": "---\nname: notes\ndescription: Keep meeting notes tidy.\n---\nWrite notes.\n",
	"a/skills/hidden/SKILL.md": "---\nname: hidden\ndescription: Not for the model.\ndisable-model-invocation: true\n---\nx\n",
};

const PROCESSES = [
	{ name: "guide-ultron", args: ["guide"] },
	{ name: "guide-ultron-hints", args: ["guide"], env: { ULTRON_HINTS: "on" } },
	{ name: "guide-help", args: ["guide", "--help"] },
	{ name: "guide-claude", args: ["guide", "--for", "claude", "--cwd", "<T>/w"], files: PROJECT },
	{ name: "guide-claude-default-cwd", args: ["guide", "--for", "claude"], files: PROJECT },
	{ name: "guide-claude-no-bundled", args: ["guide", "--for", "claude", "--cwd", "<T>/w"], files: PROJECT, env: { ULTRON_BUNDLED_SKILLS: "off" } },
	{ name: "guide-child-nc", args: ["guide", "--for", "claude-child", "--cwd", "<T>/w", "-nc"], files: PROJECT },
	{ name: "guide-claude-none", args: ["guide", "--for", "claude", "--cwd", "<T>/w", "--no-context-files", "--no-skills"], files: PROJECT },
	{ name: "guide-engineering", args: ["guide", "--for", "claude", "--cwd", "<T>/w", "-nc"], files: { ...PROJECT, "a/settings.json": '{"engineering": true}\n' } },
	{ name: "guide-bad-for", args: ["guide", "--for", "bogus"] },
	{ name: "guide-cwd-missing", args: ["guide", "--cwd"] },
	{ name: "guide-unknown", args: ["guide", "--wat"] },
	{ name: "hook-none", args: ["hook"] },
	{ name: "hook-bogus", args: ["hook", "bogus"] },
	{ name: "hook-unknown-option", args: ["hook", "stop", "--wat"] },
	{ name: "hook-dead-socket", args: ["hook", "stop", "--socket", "<T>/r/nope.sock"], stdin: "{}" },
	{ name: "hook-no-server", args: ["hook", "user-prompt"], stdin: '{"session_id":"zzz","cwd":"/nowhere"}' },
	{ name: "hook-bad-json", args: ["hook", "stop"], stdin: "{not json" },
	{ name: "watch-list-empty", args: ["watch", "--list"] },
	{ name: "watch-width", args: ["watch", "--width", "5"] },
	{ name: "watch-socket-missing", args: ["watch", "--socket"] },
	{ name: "watch-unknown", args: ["watch", "--bogus"] },
	{ name: "watch-no-session", args: ["watch"] },
	{ name: "claude-help", args: ["claude", "--help"] },
	{ name: "claude-bad-children", args: ["claude", "--children", "x"] },
	{ name: "claude-frame-model-missing", args: ["claude", "--frame-model"] },
	{ name: "claude-not-installed", args: ["claude"], env: { PATH: "/usr/bin:/bin" } },
	{ name: "claude-print-config", args: ["claude", "--print-config"], files: PROJECT },
	{
		name: "claude-print-config-options",
		args: ["claude", "--print-config", "--keep-mcp", "--keep-settings", "--no-hooks", "-nc", "-ns", "--frame-model", "openai/gpt-5", "--children", "ultron", "--child-model", "sonnet", "-p", "hi", "--", "--model", "x"],
	},
	{
		name: "claude-print-config-env",
		args: ["claude", "--print-config", "--resume", "abc"],
		env: { ULTRON_CLAUDE_MODEL: "sonnet", MCP_TOOL_TIMEOUT: "5", ULTRON_CLAUDE_SKIP_PERMISSIONS: "off" },
	},
	{ name: "claude-print-config-setting-model", args: ["claude", "--print-config", "--model=opus"], files: { "a/settings.json": '{"claudeCode": {"model": "haiku"}}\n' } },
	{ name: "claude-print-config-setting-model-used", args: ["claude", "--print-config"], files: { "a/settings.json": '{"claudeCode": {"model": "haiku"}}\n' } },
	{ name: "claude-print-config-basic-flags", args: ["claude", "--print-config", "-nc", "-ns"], env: { FAKE_CLAUDE_HELP: "basic" } },
	{ name: "claude-old", args: ["claude", "--print-config"], env: { FAKE_CLAUDE_HELP: "old" } },
	{ name: "claude-old-bin-env", args: ["claude", "--print-config"], env: { FAKE_CLAUDE_HELP: "old", ULTRON_CLAUDE_BIN: "claude" } },
	{ name: "claude-logged-out", args: ["claude"], env: { FAKE_CLAUDE_LOGGED_IN: "0" } },
	{ name: "claude-logged-out-print-config", args: ["claude", "--print-config", "-nc", "-ns"], env: { FAKE_CLAUDE_LOGGED_IN: "0" } },
	{ name: "claude-launch-print", args: ["claude", "-p", "hi", "-nc", "-ns"], launch: true },
	{ name: "claude-launch-watch", args: ["claude", "--watch", "--no-hooks", "-p", "x"], files: PROJECT, launch: true },
	{ name: "claude-launch-interactive", args: ["claude", "--keep-mcp", "-ns"], files: PROJECT, launch: true },
	{ name: "mcp-unknown", args: ["mcp", "--bogus"] },
	{ name: "mcp-level-negative", args: ["mcp", "--level", "-1"] },
	{ name: "mcp-level-missing", args: ["mcp", "--level"] },
	{ name: "mcp-children-bad", args: ["mcp", "--children", "x"] },
	{ name: "mcp-child-no-parent", args: ["mcp", "--child"] },
	{ name: "mcp-bridge-missing", args: ["mcp", "--bridge"] },
	{ name: "mcp-bridge-unknown", args: ["mcp", "--bridge", "x", "--wat"] },
];

function place(root, files = {}) {
	for (const [path, content] of Object.entries(files)) {
		mkdirSync(dirname(join(root, path)), { recursive: true });
		writeFileSync(join(root, path), content);
	}
}

const sub = (value, root) => value.split("<T>").join(root);

function runProcess(c) {
	const root = makeRoot();
	place(root, c.files);
	const env = baseEnv(root, Object.fromEntries(Object.entries(c.env ?? {}).map(([k, v]) => [k, sub(v, root)])));
	const result = spawnSync(process.execPath, [cli, ...c.args.map((a) => sub(a, root))], {
		cwd: join(root, "w"),
		env,
		input: c.stdin ?? "",
		encoding: "utf8",
		timeout: 60_000,
	});
	const out = {
		...c,
		stdout: stablePrintConfig(normalize(result.stdout, root)),
		stderr: normalize(result.stderr, root),
		code: result.status,
	};
	if (c.launch) {
		out.launch = deep(JSON.parse(readFileSync(join(root, "launch.json"), "utf8")), root);
		out.tempDirLeft = existsSync(join(root, "tmp")) ? spawnSync("ls", [join(root, "tmp")], { encoding: "utf8" }).stdout : "";
	}
	rmSync(root, { recursive: true, force: true });
	return out;
}

const req = (id, method, params) => JSON.stringify({ jsonrpc: "2.0", id, method, ...(params === undefined ? {} : { params }) });
const note = (method, params) => JSON.stringify({ jsonrpc: "2.0", method, ...(params === undefined ? {} : { params }) });
const call = (id, args, extra = {}) => req(id, "tools/call", { name: "rlm", arguments: args, ...extra });

const MCP = [
	{
		name: "protocol",
		args: ["--socket", "<T>/r/s.sock"],
		steps: [
			{ send: [req(1, "initialize", { protocolVersion: "2025-06-18", clientInfo: { name: "fake", version: "1" } }), note("notifications/initialized")], wait: [1] },
			{ send: [req(2, "ping"), req("s-3", "tools/list"), req(4, "prompts/list"), req(5, "prompts/get", { name: "ultron-guide" })], wait: [2, "s-3", 4, 5] },
			{ send: [req(6, "prompts/get", { name: "nope" }), req(7, "prompts/get", {}), req(8, "resources/list"), req(9, "resources/read", { uri: "ultron://guide" })], wait: [6, 7, 8, 9] },
			{ send: [req(10, "resources/read", { uri: 3 }), req(11, "resources/templates/list"), req(12, "foo/bar"), "not json", "[1,2]"], wait: [10, 11, 12, null] },
			{ send: [JSON.stringify([JSON.parse(req(13, "ping")), JSON.parse(req(14, "ping"))]), JSON.stringify({ jsonrpc: "2.0", id: 15 }), JSON.stringify({ jsonrpc: "2.0", id: 16, result: {} })], wait: [13, 14, 15] },
			{ send: [req(17, "tools/call", {}), req(18, "tools/call", { name: "bash" }), call(19, {}), call(20, { code: 5 })], wait: [17, 18, 19, 20] },
			{ send: [req(21, "initialize", { protocolVersion: "1999-01-01" })], wait: [21] },
			{ send: [req(22, "initialize", {})], wait: [22] },
		],
	},
	{
		name: "cells",
		args: ["--no-instructions", "--socket", "<T>/r/s.sock"],
		env: { CLAUDE_CODE_SESSION_ID: "sess-1", CLAUDE_PID: "4242" },
		steps: [
			{ send: [req(1, "initialize", { protocolVersion: "2025-03-26" })], wait: [1] },
			{ send: [call(2, { code: "print(1+1)" }, { _meta: { "claudecode/toolUseId": "toolu_1" } })], wait: [2] },
			{ send: [call(3, { code: "x = 41\nx + 1" })], wait: [3] },
			{ send: [call(4, { code: "print(x)" })], wait: [4] },
			{ send: [call(5, { code: "1/0" })], wait: [5] },
			{ send: [call(6, { code: "import os\nprint(os.getcwd() == state.get('nope', os.getcwd()))" })], wait: [6] },
		],
		control: [
			{ op: "cells" },
			{ op: "inspect", request: "bogus.op" },
			{ op: "nope" },
			{ op: "hook", event: "weird" },
			{ op: "hook", event: "stop" },
			{ op: "child.finish", token: "t", payload: {} },
		],
		hooks: [
			{ args: ["hook", "session-start"], stdin: '{"session_id":"sess-1"}' },
			{ args: ["hook", "user-prompt"], stdin: '{"session_id":"sess-1","prompt":"hi"}' },
			{ args: ["hook", "stop", "--socket", "<T>/r/s.sock"], stdin: "" },
			{ args: ["hook", "user-prompt"], stdin: '{"cwd":"<T>/w"}' },
		],
		watchList: true,
	},
	{
		name: "loki",
		args: ["--socket", "<T>/r/s.sock"],
		env: { ULTRON_LOKI: "on", CLAUDE_CODE_SESSION_ID: "sess-loki" },
		steps: [{ send: [req(1, "initialize", {})], wait: [1] }],
		control: [{ op: "hook", event: "session-start" }, { op: "hook", event: "user-prompt" }],
		hooks: [{ args: ["hook", "session-start"], stdin: '{"session_id":"sess-loki"}' }],
	},
	{
		name: "child",
		args: ["--child", "--parent-socket", "<T>/r/parent.sock", "--parent-lane", "lane-7", "--allowance", "2", "--level", "1", "--socket", "<T>/r/c.sock"],
		parent: true,
		steps: [
			{ send: [req(1, "initialize", { protocolVersion: "2025-11-25" })], wait: [1] },
			{ send: [req(2, "prompts/get", { name: "ultron-guide" }), req(3, "resources/read", { uri: "ultron://guide" })], wait: [2, 3] },
			{ send: [call(4, { code: "print('child')" })], wait: [4] },
		],
		watchList: true,
	},
	{
		name: "child-unlimited",
		args: ["--child", "--parent-socket", "<T>/r/parent.sock", "--parent-lane", "L", "--allowance", "0"],
		parent: true,
		steps: [{ send: [req(1, "prompts/get", { name: "ultron-guide" })], wait: [1] }],
	},
];

function stable(value) {
	// Times and pids vary between runs.
	if (Array.isArray(value)) return value.map(stable);
	if (value && typeof value === "object")
		return Object.fromEntries(
			Object.entries(value).map(([k, v]) => [
				k,
				["startedAt", "endedAt", "pid"].includes(k) && typeof v === "number"
					? 0
					: k === "sessionId" && typeof v === "string"
						? "<SID>"
						: k === "turn" && typeof v === "string"
							? v.replace(/^cc-[0-9a-z]+-[0-9a-f]{8}-/, "cc-<ID>-")
							: stable(v),
			]),
		);
	return value;
}

async function controlRequests(socket, requests) {
	const conn = createConnection(socket);
	await new Promise((ok, fail) => {
		conn.once("connect", ok);
		conn.once("error", fail);
	});
	conn.setEncoding("utf8");
	let buffered = "";
	const results = [];
	for (const [index, request] of requests.entries()) {
		conn.write(`${JSON.stringify({ ...request, id: index + 1 })}\n`);
		const line = await new Promise((done) => {
			const onData = (chunk) => {
				buffered += chunk;
				const nl = buffered.indexOf("\n");
				if (nl !== -1) {
					conn.off("data", onData);
					const first = buffered.slice(0, nl);
					buffered = buffered.slice(nl + 1);
					done(first);
				}
			};
			conn.on("data", onData);
		});
		results.push(JSON.parse(line));
	}
	conn.destroy();
	return results;
}

async function runMcp(c) {
	const root = makeRoot();
	let parentServer;
	if (c.parent) {
		parentServer = createServer((socket) => socket.on("data", () => {}));
		await new Promise((ok) => parentServer.listen(join(root, "r", "parent.sock"), ok));
	}
	const env = baseEnv(root, Object.fromEntries(Object.entries(c.env ?? {}).map(([k, v]) => [k, sub(v, root)])));
	const child = spawn(process.execPath, [cli, "mcp", ...c.args.map((a) => sub(a, root))], { cwd: join(root, "w"), env, stdio: ["pipe", "pipe", "pipe"] });
	child.stdout.setEncoding("utf8");
	const responses = new Map();
	let nulls = 0;
	let buffered = "";
	let wake = () => {};
	child.stdout.on("data", (chunk) => {
		buffered += chunk;
		let nl = buffered.indexOf("\n");
		while (nl !== -1) {
			const line = buffered.slice(0, nl);
			buffered = buffered.slice(nl + 1);
			nl = buffered.indexOf("\n");
			const message = JSON.parse(line);
			responses.set(message.id === null ? `null#${nulls++}` : JSON.stringify(message.id), message);
			wake();
		}
	});
	let stderr = "";
	child.stderr.on("data", (chunk) => (stderr += chunk));
	const transcript = [];
	for (const step of c.steps) {
		for (const line of step.send) child.stdin.write(`${line}\n`);
		const keys = step.wait.map((id) => (id === null ? "null#0" : JSON.stringify(id)));
		const deadline = Date.now() + 120_000;
		while (!keys.every((key) => responses.has(key)) && Date.now() < deadline) await new Promise((done) => ((wake = done), setTimeout(done, 200)));
		transcript.push({ send: step.send.map((l) => normalize(l, root)), receive: keys.map((key) => deep(responses.get(key) ?? null, root)) });
	}
	const out = { name: c.name, args: c.args, env: c.env ?? {}, ...(c.parent ? { parent: true } : {}), steps: transcript, extra: Object.keys(Object.fromEntries(responses)).length - transcript.reduce((n, s) => n + s.receive.length, 0) };
	const socket = join(root, "r", c.args.includes("--socket") ? c.args[c.args.indexOf("--socket") + 1].split("/").pop() : "");
	if (c.control) {
		out.controlRequests = c.control;
		out.control = deep(stable(await controlRequests(socket, c.control)), root);
	}
	if (c.hooks)
		out.hooks = c.hooks.map((h) => {
			const r = spawnSync(process.execPath, [cli, ...h.args.map((a) => sub(a, root))], { cwd: join(root, "w"), env, input: sub(h.stdin, root), encoding: "utf8", timeout: 60_000 });
			return { args: h.args, stdin: h.stdin, stdout: normalize(r.stdout, root), stderr: normalize(r.stderr, root), code: r.status };
		});
	if (c.control) out.status = deep(stable(await controlRequests(socket, [{ op: "status" }])), root);
	if (c.watchList) {
		const r = spawnSync(process.execPath, [cli, "watch", "--list"], { cwd: join(root, "w"), env, encoding: "utf8" });
		out.watchList = normalize(r.stdout, root).replace(/\tpid \d+\t/g, "\tpid <PID>\t").replace(/child-\d+-[0-9a-f]{6}/g, "child-<PID>-<RND>");
	}
	child.stdin.end();
	const code = await new Promise((done) => child.once("exit", (status) => done(status)));
	out.code = code;
	out.registryLeft = spawnSync("ls", [join(root, "r", "ultron-claude")], { encoding: "utf8" }).stdout;
	out.stderrEmpty = stderr.trim() === "";
	parentServer?.close();
	rmSync(root, { recursive: true, force: true });
	return out;
}

const processes = PROCESSES.map(runProcess);
const mcp = [];
for (const c of MCP) mcp.push(await runMcp(c));
writeFileSync(join(here, "claude.json"), `${JSON.stringify({ processes, mcp }, null, "\t")}\n`);
console.log(`${processes.length} processes, ${mcp.length} mcp sessions`);
