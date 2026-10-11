// Records the TypeScript interactive TUI (`node packages/coding-agent/src/cli.ts` with no -p, which opens
// experimental/client-tui.ts) in tmux: each scenario starts it against a scripted Anthropic SSE server in private
// HOME / ULTRON_CODING_AGENT_DIR / ULTRON_SERVER_DIR temp dirs, sends keystrokes and captures the screen after each
// step once it is stable. crates/cli/tests/interactive.rs replays the same scenarios against the Rust binary.
// `--latency` records the TS time to first frame (the startup budget) in interactive-latency.json instead.
// Usage: node crates/cli/fixtures/interactive.mjs [scenario-name... | --latency]
import { execFileSync, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const out = join(here, "interactive.json");

function sse(events) {
	return events.map((e) => `event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`);
}
const start = (id) => ({ type: "message_start", message: { id, type: "message", role: "assistant", model: "claude-sonnet-4-5", content: [], stop_reason: null, usage: { input_tokens: 10, output_tokens: 1 } } });
/** A text reply; `hold` stops after the first delta until the step that releases it. */
function text(parts, hold = false) {
	const chunks = sse([
		start("msg_1"),
		{ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
		...parts.map((t) => ({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: t } })),
		{ type: "content_block_stop", index: 0 },
		{ type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 5 } },
		{ type: "message_stop" },
	]);
	return hold ? { chunks, holdAfter: 3 } : { chunks };
}
function cell(id, code, hold = false) {
	const chunks = sse([
		start("msg_2"),
		{ type: "content_block_start", index: 0, content_block: { type: "tool_use", id, name: "rlm", input: {} } },
		{ type: "content_block_delta", index: 0, delta: { type: "input_json_delta", partial_json: JSON.stringify({ code }) } },
		{ type: "content_block_stop", index: 0 },
		{ type: "message_delta", delta: { stop_reason: "tool_use" }, usage: { output_tokens: 7 } },
		{ type: "message_stop" },
	]);
	return hold ? { chunks, holdAfter: 1 } : { chunks };
}

// Steps: `type` (literal text), `keys` (tmux key names), `release` (finish the held reply), then wait for `wait`
// (a substring of the screen) and a stable screen, and capture it.
const COLS = 100;
const ROWS = 30;
export const scenarios = [
	{
		name: "startup",
		replies: [],
		steps: [{ wait: "Session:" }],
	},
	{
		name: "prompt-rlm-cell",
		replies: [cell("toolu_01", "x = 6 * 7\nprint('x is', x)\nx + 1"), text(["Done: **43**."])],
		steps: [{ wait: "Session:" }, { type: "compute", keys: ["Enter"], wait: "Done: 43." }],
	},
	{
		name: "streaming-and-steer",
		replies: [text(["Hello", " there", " friend."], true), text(["Steered reply."])],
		steps: [
			{ wait: "Session:" },
			{ type: "hi", keys: ["Enter"], wait: "Hello" },
			{ type: "also this", keys: ["Enter"], wait: "Steering: also this" },
			{ release: true, wait: "Steered reply." },
		],
	},
	{
		name: "escape-abort",
		replies: [text(["Partial"], true)],
		steps: [
			{ wait: "Session:" },
			{ type: "go", keys: ["Enter"], wait: "Partial" },
			{ keys: ["Escape"], wait: "operation was aborted" },
		],
	},
	{
		name: "ctrl-c-clear",
		replies: [],
		steps: [{ wait: "Session:" }, { type: "draft text", wait: "draft text" }, { keys: ["C-c"], wait: "Session:" }],
	},
	{
		name: "slash-list",
		replies: [],
		steps: [{ wait: "Session:" }, { type: "/", wait: "(1/38)" }, { type: "se", wait: "(1/6)" }, { keys: ["Down", "Down"], wait: "(3/6)" }],
	},
	{
		name: "unknown-command",
		replies: [],
		steps: [{ wait: "Session:" }, { type: "/nope", keys: ["Enter"], wait: "Unknown slash command" }],
	},
	{
		name: "session-info",
		replies: [text(["Hi."])],
		steps: [{ wait: "Session:" }, { type: "hello", keys: ["Enter"], wait: "Hi." }, { type: "/session", keys: ["Enter"], wait: "Session Info" }],
	},
	{
		name: "hotkeys",
		rows: 90,
		replies: [],
		steps: [{ wait: "Session:" }, { type: "/hotkeys", keys: ["Enter"], wait: "excluded from context" }],
	},
	{
		name: "new-session",
		replies: [text(["First."])],
		steps: [{ wait: "Session:" }, { type: "one", keys: ["Enter"], wait: "First." }, { type: "/new", keys: ["Enter"], wait: "New session started" }],
	},
	{
		name: "copy-empty",
		replies: [],
		steps: [{ wait: "Session:" }, { type: "/copy", keys: ["Enter"], wait: "No agent messages" }],
	},
	{
		name: "model-selector",
		replies: [],
		steps: [{ wait: "Session:" }, { type: "/model", keys: ["Enter"], wait: "claude" }, { keys: ["Escape"], wait: "Session:" }],
	},
	{
		name: "quit",
		replies: [],
		steps: [{ wait: "Session:" }, { type: "/quit", keys: ["Enter"], wait: "<exited>" }],
	},
	{
		name: "quit-after-turn",
		replies: [text(["Bye now."])],
		steps: [{ wait: "Session:" }, { type: "hello", keys: ["Enter"], wait: "Bye now." }, { type: "/quit", keys: ["Enter"], wait: "<exited>" }],
	},
	{
		name: "ctrl-d-exit",
		replies: [],
		steps: [{ wait: "Session:" }, { keys: ["C-d"], wait: "<exited>" }],
	},
	{
		name: "splash",
		rows: 46,
		splash: true,
		replies: [],
		steps: [{ wait: "Session:" }],
	},
];

/** Scripted SSE server: reply `i` to request `i`; a held reply stops after `holdAfter` chunks until released. */
async function mockServer(replies) {
	let n = 0;
	let release;
	let held = Promise.resolve();
	const server = createServer((req, res) => {
		req.resume();
		req.on("end", async () => {
			const reply = replies[n++] ?? text(["(no more replies)"]);
			res.writeHead(200, { "content-type": "text/event-stream" });
			const cut = reply.holdAfter ?? reply.chunks.length;
			for (const c of reply.chunks.slice(0, cut)) res.write(c);
			if (reply.holdAfter !== undefined) {
				held = new Promise((r) => (release = r));
				await held;
			}
			for (const c of reply.chunks.slice(cut)) res.write(c);
			res.end();
		});
	});
	await new Promise((r) => server.listen(0, "127.0.0.1", r));
	return { port: server.address().port, release: () => release?.(), close: () => server.close() };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export function normalize(screen, tmp) {
	return screen
		.split(tmp)
		.join("<TMP>")
		.split(tmp.slice(1).replace(/[/\\:]/g, "-"))
		.join("<TMPNAME>")
		// Session file names start with their creation time (it may wrap across lines).
		.replace(/\d{4}-\d\d-\d\dT[0-9\-\n ]{12,16}Z/g, "<TIME>")
		.replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g, "<ID>")
		.replace(/[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]/g, "*");
}

function tmux(socket, ...args) {
	return execFileSync("tmux", ["-S", socket, "-f", "/dev/null", ...args], { encoding: "utf8" });
}

export async function runScenario(scenario, command) {
	const server = await mockServer(scenario.replies);
	const tmp = mkdtempSync("/tmp/ultron-tui-");
	const agentDir = join(tmp, "agent");
	const cwd = join(tmp, "cwd");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(cwd);
	mkdirSync(join(tmp, "server"));
	writeFileSync(join(agentDir, "models.json"), JSON.stringify({ providers: { anthropic: { baseUrl: `http://127.0.0.1:${server.port}` } } }));
	const env = {
		// No `claude` CLI on the path, so the catalog offers the provider with a key only.
		PATH: "/usr/bin:/bin",
		HOME: tmp,
		ULTRON_CODING_AGENT_DIR: agentDir,
		ULTRON_SERVER_DIR: join(tmp, "server"),
		ANTHROPIC_API_KEY: "test-key",
		TZ: "UTC",
		TERM: "xterm-256color",
		COLORTERM: "truecolor",
		LANG: "C.UTF-8",
		// The Rust build names a session from its first prompt (a user requirement); TS does not.
		ULTRON_AUTO_NAME: "off",
		...(scenario.splash ? {} : { ULTRON_SPLASH: "off" }),
		...(scenario.env ?? {}),
	};
	const script = join(tmp, "run.sh");
	const quoted = (s) => `'${s.replace(/'/g, "'\\''")}'`;
	writeFileSync(
		script,
		`#!/bin/sh\ncd ${quoted(cwd)}\nexec env -i ${Object.entries(env).map(([k, v]) => quoted(`${k}=${v}`)).join(" ")} ${command.map(quoted).join(" ")} ${(scenario.args ?? []).map(quoted).join(" ")}\n`,
		{ mode: 0o755 },
	);
	// The tmux server's socket lives in the temp dir, so nothing is left behind.
	const socket = join(tmp, "tmux.sock");
	const cols = scenario.cols ?? COLS;
	const rows = scenario.rows ?? ROWS;
	const started = Date.now();
	tmux(socket, "new-session", "-d", "-s", "t", "-x", String(cols), "-y", String(rows), `${script}; echo '<exited>'; sleep 60`);
	const capture = () => tmux(socket, "capture-pane", "-p", "-t", "t");
	let firstFrameMs;
	const captures = [];
	try {
		for (const step of scenario.steps) {
			if (step.release) server.release();
			if (step.type !== undefined) tmux(socket, "send-keys", "-t", "t", "-l", step.type);
			if (step.keys) tmux(socket, "send-keys", "-t", "t", ...step.keys);
			const deadline = Date.now() + 30_000;
			let screen = capture();
			while (!screen.includes(step.wait)) {
				if (Date.now() > deadline) throw new Error(`${scenario.name}: timed out waiting for ${step.wait}\n${screen}`);
				await sleep(20);
				screen = capture();
			}
			if (firstFrameMs === undefined) firstFrameMs = Date.now() - started;
			// Stable: unchanged (spinners normalized) for 400 ms.
			let stable = normalize(screen, tmp);
			let since = Date.now();
			while (Date.now() - since < 400) {
				await sleep(50);
				const next = normalize(capture(), tmp);
				if (next !== stable) {
					stable = next;
					since = Date.now();
				}
			}
			captures.push(stable);
		}
	} finally {
		try {
			tmux(socket, "kill-server");
		} catch {}
		server.close();
		await sleep(300);
		rmSync(tmp, { recursive: true, force: true });
	}
	return { captures, firstFrameMs };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	const ts = [process.execPath, join(root, "packages/coding-agent/src/cli.ts"), "--model", "anthropic/claude-sonnet-4-5"];
	if (process.argv[2] === "--latency") {
		// Time to the first frame showing the header, best of five runs: the Rust startup budget. Timing is not
		// deterministic, so it lives in its own file.
		const times = [];
		for (let i = 0; i < 5; i++) times.push((await runScenario(scenarios[0], ts)).firstFrameMs);
		writeFileSync(join(here, "interactive-latency.json"), `${JSON.stringify({ tsFirstFrameMs: Math.min(...times), runs: times.length }, null, 1)}\n`);
		process.exit(0);
	}
	const only = process.argv.slice(2);
	const recorded = existsSync(out) && only.length > 0 ? JSON.parse(readFileSync(out, "utf8")) : { scenarios: [] };
	for (const scenario of scenarios) {
		if (only.length > 0 && !only.includes(scenario.name)) continue;
		const { captures } = await runScenario(scenario, ts);
		const entry = { ...scenario, cols: scenario.cols ?? COLS, rows: scenario.rows ?? ROWS, captures };
		const index = recorded.scenarios.findIndex((s) => s.name === scenario.name);
		if (index >= 0) recorded.scenarios[index] = entry;
		else recorded.scenarios.push(entry);
		process.stderr.write(`${scenario.name}: ${captures.length} captures\n`);
	}
	recorded.scenarios.sort((a, b) => scenarios.findIndex((s) => s.name === a.name) - scenarios.findIndex((s) => s.name === b.name));
	writeFileSync(out, `${JSON.stringify(recorded, null, 1)}\n`);
}
