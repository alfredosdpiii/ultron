// Records the TypeScript client TUI's `/login` dialogs for the built-in account logins up to their first question,
// then Esc: Anthropic (the auth URL and the paste prompt), OpenAI Codex (its method choice), GitHub Copilot (the
// enterprise-domain prompt) and OpenRouter (the auth URL on its system-chosen callback port and the paste prompt).
// No step reaches a provider: offline-guard.mjs is loaded into every TypeScript process and the generator fails if
// anything tried to leave loopback; the browser opener is a stub on PATH that only logs the address. Screens are
// normalized as interactive.mjs does, and the authorize URL (random state and PKCE values, the callback port) is
// recorded as `<url>` line by line. crates/cli/tests/oauth_login_tui.rs replays the scenarios.
// Usage: node crates/cli/fixtures/oauth_login_tui.mjs
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
		name: "anthropic",
		steps: [
			{ wait: "Server:" },
			{ type: "/login anthropic", keys: ["Enter"], wait: "Select authentication method for Anthropic:" },
			{ keys: ["Enter"], wait: "paste the authorization code" },
			{ keys: ["Escape"], wait: "Server:", gone: "Login to" },
		],
	},
	{
		name: "openai-codex",
		steps: [
			{ wait: "Server:" },
			{ type: "/login openai-codex", keys: ["Enter"], wait: "Select OpenAI Codex login method:" },
			{ keys: ["Down"], wait: "→ Device code login" },
			{ keys: ["Escape"], wait: "Server:", gone: "login method" },
		],
	},
	{
		name: "github-copilot",
		steps: [
			{ wait: "Server:" },
			{ type: "/login github-copilot", keys: ["Enter"], wait: "Select authentication method for GitHub Copilot:" },
			{ keys: ["Enter"], wait: "GitHub Enterprise URL/domain" },
			{ type: "company.ghe", wait: "company.ghe" },
			{ keys: ["Escape"], wait: "Server:", gone: "Login to" },
		],
	},
	{
		name: "openrouter",
		steps: [
			{ wait: "Server:" },
			{ type: "/login openrouter", keys: ["Enter"], wait: "Select authentication method for OpenRouter:" },
			{ keys: ["Enter"], wait: "paste the authorization code" },
			{ keys: ["Escape"], wait: "Server:", gone: "Login to" },
		],
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
	return { captures, opened };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	const ts = [process.execPath, join(root, "packages/coding-agent/src/cli.ts"), "--model", "anthropic/claude-sonnet-4-5"];
	const recorded = { cols: COLS, rows: ROWS, scenarios: [] };
	for (const scenario of scenarios) {
		const result = await runScenario(scenario, ts);
		recorded.scenarios.push({ ...scenario, ...result });
		process.stderr.write(`${scenario.name}: ${result.captures.length} captures, ${result.opened} opened\n`);
	}
	writeFileSync(join(here, "oauth_login_tui.json"), `${JSON.stringify(recorded, null, 1)}\n`);
}
