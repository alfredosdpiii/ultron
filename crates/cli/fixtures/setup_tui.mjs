// Steps send `keys`, then the text `type`, then the keys `then`.
// Records `ultron setup` (cli/setup/{command,wizard,tui}.ts) in tmux, with fakes only: PATH holds a fake python3 and
// a no-op browser opener and nothing else (no docker, no analyzers), memory starts turned off (hindsightUrl "off", so
// nothing probes a Hindsight server), and the custom endpoint is a local OpenAI-compatible stand-in on an ephemeral
// 127.0.0.1 port (GET /v1/models, streaming POST /v1/chat/completions). Scenarios: every step skipped with Esc; a
// custom endpoint with its default model and live test; Ctrl+C in the middle. Records every stable screen (the
// stand-in's port as <PORT>, the live test's seconds as <S>), the files the run leaves, and the requests the
// stand-in saw. offline-guard.mjs is loaded into the TypeScript process; the generator fails if anything tried to
// leave loopback. crates/cli/tests/setup_tui.rs replays it.
// Usage: node crates/cli/fixtures/setup_tui.mjs
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { normalize } from "./interactive.mjs";
import { assertOffline, guardNodeOptions } from "./offline-guard.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const COLS = 100;
const ROWS = 60;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const MEMORY_OFF = JSON.stringify({ hindsightUrl: "off" });

export const scenarios = [
	{
		name: "skip every step",
		files: { "agent/settings.json": MEMORY_OFF },
		steps: [
			{ wait: "How should Ultron reach a model?" },
			{ keys: ["Escape"], wait: "Turn memory on?" },
			{ keys: ["Escape"], wait: "Keep these settings" },
			{ keys: ["Escape"], wait: "<exited>" },
		],
	},
	{
		name: "custom endpoint",
		files: { "agent/settings.json": MEMORY_OFF },
		steps: [
			{ wait: "How should Ultron reach a model?" },
			{ keys: ["Down", "Down"], wait: "→ Custom OpenAI-compatible endpoint" },
			{ keys: ["Enter"], wait: "Base URL of the endpoint" },
			{ keys: ["C-u"], type: "http://127.0.0.1:<PORT>/v1", wait: "<PORT>/v1" },
			{ keys: ["Enter"], wait: "API key (leave empty" },
			{ type: "sk-test", then: ["Enter"], wait: "A short name for this provider" },
			{ keys: ["C-u"], type: "proxy", wait: "proxy" },
			{ keys: ["Enter"], wait: "Which API does it speak?" },
			{ keys: ["Enter"], wait: "Model ids to use" },
			{ keys: ["Enter"], wait: "Default model (" },
			{ keys: ["Enter"], wait: "Turn memory on?" },
			{ keys: ["Escape"], wait: "Keep these settings" },
			{ keys: ["Enter"], wait: "<exited>" },
		],
		artifacts: ["agent/models.json", "agent/settings.json"],
	},
	{
		// `ultron` with no model on a terminal: the first-run offer (main.ts offerSetupOnFirstRun).
		name: "first run offer, run setup, quit",
		args: [],
		files: { "agent/settings.json": MEMORY_OFF },
		steps: [
			{ wait: "Run the guided setup now?" },
			{ keys: ["Enter"], wait: "How should Ultron reach a model?" },
			{ keys: ["C-c"], wait: "<exited>" },
		],
	},
	{
		// "Don't ask again" is saved; the start then goes on without a model (its error is not compared).
		name: "first run offer, never",
		args: [],
		files: { "agent/settings.json": MEMORY_OFF },
		steps: [
			{ wait: "Run the guided setup now?" },
			{ keys: ["Down", "Down"], wait: "→ Don't ask again" },
			{ keys: ["Enter"], wait: "You can run \"ultron setup\" at any time.", partial: true },
		],
		artifacts: ["agent/settings.json"],
	},
	{
		name: "quit with ctrl+c",
		files: { "agent/settings.json": MEMORY_OFF },
		steps: [
			{ wait: "How should Ultron reach a model?" },
			{ keys: ["Down"], wait: "→ Use an API key" },
			{ keys: ["C-c"], wait: "<exited>" },
		],
	},
];

/** The OpenAI-compatible stand-in: one model, and a streamed "ok". */
export function startEndpoint() {
	const requests = [];
	const server = createServer((req, res) => {
		let body = "";
		req.on("data", (c) => {
			body += c;
		});
		req.on("end", () => {
			requests.push(`${req.method} ${req.url}`);
			if (req.method === "GET" && req.url === "/v1/models") {
				res.writeHead(200, { "content-type": "application/json" });
				res.end(JSON.stringify({ object: "list", data: [{ id: "model-one", object: "model" }] }));
				return;
			}
			if (req.method === "POST" && req.url === "/v1/chat/completions") {
				res.writeHead(200, { "content-type": "text/event-stream" });
				const chunk = (delta, finish) =>
					`data: ${JSON.stringify({ id: "c1", object: "chat.completion.chunk", created: 0, model: "model-one", choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
				res.end(`${chunk({ role: "assistant", content: "ok" }, null)}${chunk({}, "stop")}data: [DONE]\n\n`);
				return;
			}
			res.writeHead(404);
			res.end();
		});
	});
	return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve({ server, port: server.address().port, requests })));
}

function tmux(socket, ...args) {
	return execFileSync("tmux", ["-S", socket, "-f", "/dev/null", ...args], { encoding: "utf8" });
}

export async function runScenario(scenario, command) {
	const { server, port, requests } = await startEndpoint();
	const tmp = mkdtempSync("/tmp/ultron-tui-");
	const agentDir = join(tmp, "agent");
	const cwd = join(tmp, "cwd");
	const bin = join(tmp, "bin");
	for (const dir of [agentDir, cwd, bin, join(tmp, "server")]) mkdirSync(dir, { recursive: true });
	writeFileSync(join(bin, "python3"), "#!/bin/sh\necho 'Python 3.12.0'\n", { mode: 0o755 });
	writeFileSync(join(bin, "xdg-open"), "#!/bin/sh\nexit 0\n", { mode: 0o755 });
	for (const [rel, text] of Object.entries(scenario.files ?? {})) {
		mkdirSync(dirname(join(tmp, rel)), { recursive: true });
		writeFileSync(join(tmp, rel), text);
	}
	const env = {
		PATH: bin,
		HOME: tmp,
		XDG_CONFIG_HOME: join(tmp, ".config"),
		XDG_DATA_HOME: join(tmp, ".local/share"),
		ULTRON_CODING_AGENT_DIR: agentDir,
		ULTRON_SERVER_DIR: join(tmp, "server"),
		ULTRON_PYTHON: "python3",
		TZ: "UTC",
		TERM: "xterm-256color",
		COLORTERM: "truecolor",
		LANG: "C.UTF-8",
		PI_OFFLINE: "1",
		NODE_OPTIONS: guardNodeOptions,
		ULTRON_GUARD_LOG: join(tmp, "guard.log"),
	};
	const script = join(tmp, "run.sh");
	const quoted = (s) => `'${s.replace(/'/g, "'\\''")}'`;
	writeFileSync(
		script,
		`#!/bin/sh\ncd ${quoted(cwd)}\nexec /usr/bin/env -i ${Object.entries(env).map(([k, v]) => quoted(`${k}=${v}`)).join(" ")} ${[...command, ...(scenario.args ?? ["setup"])].map(quoted).join(" ")}\n`,
		{ mode: 0o755 },
	);
	const socket = join(tmp, "tmux.sock");
	tmux(socket, "new-session", "-d", "-s", "t", "-x", String(COLS), "-y", String(ROWS), `sh ${quoted(script)}; echo '<exited>'; sleep 60`);
	const norm = (s) => normalize(s, tmp).split(`:${port}`).join(":<PORT>").replace(/in \d+\.\ds/g, "in <S>s");
	const capture = () => tmux(socket, "capture-pane", "-p", "-t", "t");
	const captures = [];
	const artifacts = {};
	try {
		for (const step of scenario.steps) {
			if (step.keys) tmux(socket, "send-keys", "-t", "t", ...step.keys);
			if (step.type !== undefined) tmux(socket, "send-keys", "-t", "t", "-l", step.type.split("<PORT>").join(String(port)));
			if (step.then) tmux(socket, "send-keys", "-t", "t", ...step.then);
			const deadline = Date.now() + 30_000;
			let screen = norm(capture());
			while (!screen.includes(step.wait)) {
				if (Date.now() > deadline) throw new Error(`${scenario.name}: timed out waiting for ${step.wait}\n${screen}`);
				await sleep(20);
				screen = norm(capture());
			}
			let since = Date.now();
			while (Date.now() - since < 400) {
				await sleep(50);
				const next = norm(capture());
				if (next !== screen) {
					screen = next;
					since = Date.now();
				}
			}
			captures.push(screen);
		}
		for (const rel of scenario.artifacts ?? []) {
			const path = join(tmp, rel);
			artifacts[rel] = existsSync(path) ? readFileSync(path, "utf8").split(tmp).join("<TMP>").split(`:${port}`).join(":<PORT>") : null;
		}
	} finally {
		try {
			tmux(socket, "kill-server");
		} catch {}
		server.close();
		await sleep(300);
		try {
			assertOffline(join(tmp, "guard.log"));
		} finally {
			rmSync(tmp, { recursive: true, force: true });
		}
	}
	return { captures, artifacts, requests };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	const ts = [process.execPath, join(root, "packages/coding-agent/src/cli.ts")];
	const recorded = { cols: COLS, rows: ROWS, scenarios: [] };
	for (const scenario of scenarios) {
		const result = await runScenario(scenario, ts);
		recorded.scenarios.push({ ...scenario, ...result });
		process.stderr.write(`${scenario.name}: ${result.captures.length} captures\n`);
	}
	writeFileSync(join(here, "setup_tui.json"), `${JSON.stringify(recorded, null, 1)}\n`);
}
