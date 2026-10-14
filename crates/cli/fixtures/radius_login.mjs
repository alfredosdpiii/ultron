// Records the TypeScript client TUI's `/login` for a models.json Radius provider (`"oauth": "radius"`) in tmux:
// the auth-type choice, Radius's sign-in method prompt, the device-code flow against a loopback Radius gateway
// (device authorization, pending polls, then the token), the saved credential and the final status, offline
// (PI_OFFLINE=1: no catalog refresh reaches the network afterwards). Also
// `Esc` while the code is shown (the login stops, nothing is saved). Same terminal harness as interactive.mjs
// (private HOME / ULTRON_CODING_AGENT_DIR / ULTRON_SERVER_DIR, screens normalized); the gateway's port is recorded
// as <port>. crates/cli/tests/radius_login.rs replays the scenarios against the Rust binary.
// Usage: node crates/cli/fixtures/radius_login.mjs
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
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
		name: "device-code-login",
		steps: [
			{ wait: "Server:" },
			{ type: "/login gw", keys: ["Enter"], wait: "Select authentication method for My Radius:" },
			{ keys: ["Enter"], wait: "Sign in to My Radius:" },
			{ keys: ["Down"], wait: "→ Sign in with device code" },
			{ keys: ["Enter"], wait: "Enter code: WXYZ-1234" },
			{ release: true, wait: "/model switches to My Radius's models" },
		],
		artifacts: ["agent/auth.json"],
	},
	{
		name: "device-code-cancel",
		steps: [
			{ wait: "Server:" },
			{ type: "/login gw", keys: ["Enter"], wait: "Select authentication method for My Radius:" },
			{ keys: ["Enter"], wait: "Sign in to My Radius:" },
			{ keys: ["Down", "Enter"], wait: "Enter code: WXYZ-1234" },
			{ keys: ["Escape"], wait: "Server:" },
		],
		artifacts: ["agent/auth.json"],
	},
];

/** The loopback Radius gateway: device authorization, then token polls that stay pending until released. */
async function gateway() {
	const requests = [];
	let released = false;
	const server = createServer((req, res) => {
		let body = "";
		req.on("data", (chunk) => {
			body += chunk;
		});
		req.on("end", () => {
			const form = Object.fromEntries(new URLSearchParams(body));
			requests.push({ method: req.method, path: req.url, body: form });
			const port = server.address().port;
			res.setHeader("content-type", "application/json");
			if (req.url === "/v1/oauth/device") {
				res.end(
					JSON.stringify({
						device_code: "dev-1",
						user_code: "WXYZ-1234",
						verification_uri: `http://127.0.0.1:${port}/device`,
						expires_in: 600,
						interval: 1,
					}),
				);
			} else if (req.url === "/v1/oauth/token" && !released) {
				res.statusCode = 400;
				res.end(JSON.stringify({ error: "authorization_pending" }));
			} else if (req.url === "/v1/oauth/token") {
				res.end(JSON.stringify({ access_token: "radius-access", refresh_token: "radius-refresh", expires_in: 3600 }));
			} else {
				res.statusCode = 404;
				res.end("{}");
			}
		});
	});
	await new Promise((r) => server.listen(0, "127.0.0.1", r));
	return {
		port: server.address().port,
		requests,
		release: () => {
			released = true;
		},
		close: () => server.close(),
	};
}

function tmux(socket, ...args) {
	return execFileSync("tmux", ["-S", socket, "-f", "/dev/null", ...args], { encoding: "utf8" });
}

export async function runScenario(scenario, command) {
	const gw = await gateway();
	const tmp = mkdtempSync("/tmp/ultron-tui-");
	const agentDir = join(tmp, "agent");
	const cwd = join(tmp, "cwd");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(cwd);
	mkdirSync(join(tmp, "server"));
	writeFileSync(
		join(agentDir, "models.json"),
		JSON.stringify({
			providers: {
				// No model request is made: the scenarios only sign in.
				anthropic: { baseUrl: "http://127.0.0.1:9" },
				gw: { oauth: "radius", baseUrl: `http://127.0.0.1:${gw.port}/v1`, name: "My Radius" },
			},
		}),
	);
	const env = {
		PATH: "/usr/bin:/bin",
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
		// No catalog refresh from the network after the login (pi.dev, the gateway's /v1/config): offline.
		PI_OFFLINE: "1",
		// Every TypeScript process (client, server, worker) refuses non-loopback sockets and logs any attempt.
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
	const port = (text) => text.split(String(gw.port)).join("<port>");
	const captures = [];
	let artifacts = {};
	try {
		for (const step of scenario.steps) {
			if (step.release) gw.release();
			if (step.type !== undefined) tmux(socket, "send-keys", "-t", "t", "-l", step.type);
			if (step.keys) tmux(socket, "send-keys", "-t", "t", ...step.keys);
			const deadline = Date.now() + 30_000;
			let screen = capture();
			while (!screen.includes(step.wait)) {
				if (Date.now() > deadline) throw new Error(`${scenario.name}: timed out waiting for ${step.wait}\n${screen}`);
				await sleep(20);
				screen = capture();
			}
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
			captures.push(port(stable));
		}
		for (const path of scenario.artifacts ?? []) {
			let content = null;
			try {
				content = readFileSync(join(tmp, path), "utf8");
			} catch {}
			// The token's expiry is the login's own clock.
			artifacts[path] = content === null ? null : content.replace(/"expires":\s*\d+/g, '"expires":0');
		}
	} finally {
		try {
			tmux(socket, "kill-server");
		} catch {}
		gw.close();
		await sleep(300);
		try {
			assertOffline(join(tmp, "guard.log"));
		} finally {
			rmSync(tmp, { recursive: true, force: true });
		}
	}
	// Distinct requests: how many pending polls run before the release is the clock's.
	const requests = [...new Set(gw.requests.map((r) => port(JSON.stringify(r))))].map((r) => JSON.parse(r));
	return { captures, artifacts, requests };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	const ts = [process.execPath, join(root, "packages/coding-agent/src/cli.ts"), "--model", "anthropic/claude-sonnet-4-5"];
	const recorded = { cols: COLS, rows: ROWS, scenarios: [] };
	for (const scenario of scenarios) {
		const result = await runScenario(scenario, ts);
		recorded.scenarios.push({ ...scenario, ...result });
		process.stderr.write(`${scenario.name}: ${result.captures.length} captures\n`);
	}
	writeFileSync(join(here, "radius_login.json"), `${JSON.stringify(recorded, null, 1)}\n`);
}
