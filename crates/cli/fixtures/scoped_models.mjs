// Records the TypeScript client TUI's `/scoped-models` (client-tui-pi-commands.ts `showScopedModelsSelector`,
// `ScopedModelsSelectorComponent`) and Ctrl+P over the session scope it sets, in tmux: open it (the catalog refresh
// is offline, PI_OFFLINE=1), toggle a model, clear all, filter and enable the filtered models, close, cycle with
// Ctrl+P inside the scope, reopen (the session scope first), save it to settings with Ctrl+S, enable every model
// (the list collapses back to "all enabled"). Records every stable
// screen and the settings.json it leaves. offline-guard.mjs is loaded into every TypeScript process; the generator
// fails if anything tried to leave loopback. crates/cli/tests/scoped_models.rs replays it.
// Usage: node crates/cli/fixtures/scoped_models.mjs
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { normalize } from "./interactive.mjs";
import { assertOffline, guardNodeOptions } from "./offline-guard.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const COLS = 100;
const ROWS = 30;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export const scenarios = [
	{
		name: "scoped-models",
		steps: [
			{ wait: "Server:" },
			{ type: "/scoped-models", keys: ["Enter"], wait: "Model catalogs refreshed." },
			{ keys: ["Enter"], wait: "(unsaved)" },
			{ keys: ["C-x"], wait: "0/17 enabled" },
			{ type: "opus-5", wait: "claude-opus-5-5" },
			{ keys: ["C-a"], wait: "8/17 enabled" },
			{ keys: ["Escape"], wait: "Server:", gone: "Model Configuration" },
			{ keys: ["C-p"], wait: "Switched to Claude Sonnet 4.5 (" },
			{ keys: ["C-p"], wait: "Switched to Claude Sonnet 5 (" },
			{ type: "/scoped-models", keys: ["Enter"], wait: "Model catalogs refreshed." },
			{ keys: ["C-s"], wait: "Model selection saved to settings" },
			{ keys: ["C-a"], wait: "all enabled (unsaved)" },
			{ keys: ["Escape"], wait: "Server:", gone: "Model Configuration" },
		],
		artifacts: ["agent/settings.json"],
	},
];
/** The dialog's authorize URL as `<url>` lines (random state, PKCE values and ports), keeping its line count. */
export function maskUrl(screen) {
	const lines = screen.split("\n");
	const start = lines.findIndex((line) => line.includes(" Login to "));
	const end = lines.findIndex((line, i) => i > start && line.includes("Ctrl+click to open"));
	if (start < 0 || end < 0) return screen;
	for (let i = start + 1; i < end; i++) if (lines[i].trim() !== "") lines[i] = "<url>";
	return lines.join("\n");
}

function tmux(socket, ...args) {
	return execFileSync("tmux", ["-S", socket, "-f", "/dev/null", ...args], { encoding: "utf8" });
}

export async function runScenario(scenario, command) {
	const tmp = mkdtempSync("/tmp/ultron-tui-");
	const agentDir = join(tmp, "agent");
	const cwd = join(tmp, "cwd");
	const bin = join(tmp, "bin");
	for (const dir of [agentDir, cwd, bin, join(tmp, "server")]) mkdirSync(dir, { recursive: true });
	writeFileSync(join(agentDir, "models.json"), JSON.stringify({ providers: { anthropic: { baseUrl: "http://127.0.0.1:9" } } }));
	// The browser opener: logs the address instead of opening anything.
	writeFileSync(join(bin, "xdg-open"), `#!/bin/sh\necho "$1" >> ${JSON.stringify(join(tmp, "opened.log"))}\n`, { mode: 0o755 });
	const env = {
		PATH: `${bin}:/usr/bin:/bin`,
		HOME: tmp,
		ULTRON_CODING_AGENT_DIR: agentDir,
		ULTRON_SERVER_DIR: join(tmp, "server"),
		ANTHROPIC_API_KEY: "test-key",
		TZ: "UTC",
		TERM: "xterm-256color",
		COLORTERM: "truecolor",
		LANG: "C.UTF-8",
		ULTRON_AUTO_NAME: "off",
		ULTRON_SPLASH: "off",
		PI_OFFLINE: "1",
		NODE_OPTIONS: guardNodeOptions,
		ULTRON_GUARD_LOG: join(tmp, "guard.log"),
	};
	const script = join(tmp, "run.sh");
	const quoted = (s) => `'${s.replace(/'/g, "'\\''")}'`;
	writeFileSync(
		script,
		`#!/bin/sh\ncd ${quoted(cwd)}\nexec env -i ${Object.entries(env).map(([k, v]) => quoted(`${k}=${v}`)).join(" ")} ${command.map(quoted).join(" ")}\n`,
		{ mode: 0o755 },
	);
	const socket = join(tmp, "tmux.sock");
	tmux(socket, "new-session", "-d", "-s", "t", "-x", String(COLS), "-y", String(ROWS), `${script}; echo '<exited>'; sleep 60`);
	const capture = () => tmux(socket, "capture-pane", "-p", "-t", "t");
	const captures = [];
	let opened = 0;
	const artifacts = {};
	try {
		for (const step of scenario.steps) {
			if (step.type !== undefined) tmux(socket, "send-keys", "-t", "t", "-l", step.type);
			if (step.keys) tmux(socket, "send-keys", "-t", "t", ...step.keys);
			const deadline = Date.now() + 30_000;
			let screen = capture();
			while (!screen.includes(step.wait) || (step.gone && screen.includes(step.gone))) {
				if (Date.now() > deadline) throw new Error(`${scenario.name}: timed out waiting for ${step.wait}\n${screen}`);
				await sleep(20);
				screen = capture();
			}
			let stable = maskUrl(normalize(screen, tmp));
			let since = Date.now();
			while (Date.now() - since < 400) {
				await sleep(50);
				const next = maskUrl(normalize(capture(), tmp));
				if (next !== stable) {
					stable = next;
					since = Date.now();
				}
			}
			captures.push(stable);
		}
		try {
			opened = readFileSync(join(tmp, "opened.log"), "utf8").split("\n").filter(Boolean).length;
		} catch {}
		for (const path of scenario.artifacts ?? []) {
			try {
				artifacts[path] = readFileSync(join(tmp, path), "utf8");
			} catch {
				artifacts[path] = null;
			}
		}
	} finally {
		try {
			tmux(socket, "kill-server");
		} catch {}
		await sleep(300);
		try {
			assertOffline(join(tmp, "guard.log"));
		} finally {
			rmSync(tmp, { recursive: true, force: true });
		}
	}
	return { captures, opened, artifacts };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	const ts = [process.execPath, join(root, "packages/coding-agent/src/cli.ts"), "--model", "anthropic/claude-sonnet-4-5"];
	const recorded = { cols: COLS, rows: ROWS, scenarios: [] };
	for (const scenario of scenarios) {
		const result = await runScenario(scenario, ts);
		recorded.scenarios.push({ ...scenario, ...result });
		process.stderr.write(`${scenario.name}: ${result.captures.length} captures, ${result.opened} opened\n`);
	}
	writeFileSync(join(here, "scoped_models.json"), `${JSON.stringify(recorded, null, 1)}\n`);
}
