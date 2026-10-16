// Records `ultron config` (cli/config-selector.ts, ConfigSelectorComponent) in tmux: the resource list of a local
// package and the agent dir's auto-discovered resources, toggling resources (global mode writes `+`/`-` patterns,
// project mode cycles inherit/load/unload overrides), filtering by typing, Tab between modes, and Esc. Records every
// stable screen and the settings files the run leaves. offline-guard.mjs is loaded into the TypeScript process and
// the generator fails if anything tried to leave loopback. crates/cli/tests/config_tui.rs replays it.
// Usage: node crates/cli/fixtures/config_tui.mjs
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { normalize } from "./interactive.mjs";
import { assertOffline, guardNodeOptions } from "./offline-guard.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const COLS = 100;
const ROWS = 30;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const resources = {
	"agent/extensions/alpha.ts": "",
	"agent/extensions/tool/index.ts": "",
	"agent/skills/writer/SKILL.md": "---\nname: writer\ndescription: d\n---\n",
	"agent/prompts/review.md": "",
	"pkg/extensions/x.ts": "",
	"pkg/extensions/y.ts": "",
	"pkg/prompts/pp.md": "",
};

export const scenarios = [
	{
		name: "global",
		args: ["config"],
		files: { ...resources, "agent/settings.json": JSON.stringify({ packages: ["../pkg"] }) },
		steps: [
			{ wait: "Global Resources" },
			{ keys: ["Space"], wait: "[ ] x.ts" },
			{ keys: ["Down", "Down", "Down"], wait: "> " },
			{ keys: ["Space"], wait: "[ ] alpha.ts" },
			{ type: "rev", wait: "review.md", gone: "y.ts" },
			{ keys: ["BSpace", "BSpace", "BSpace", "Up"], wait: "y.ts" },
			{ keys: ["Escape"], wait: "<exited>" },
		],
		artifacts: ["agent/settings.json", "cwd/.pi/settings.json"],
	},
	{
		name: "project",
		args: ["config", "-l", "--approve"],
		files: {
			...resources,
			"agent/settings.json": JSON.stringify({ packages: ["../pkg"], extensions: ["-extensions/alpha.ts"] }),
			"cwd/.pi/extensions/local.ts": "",
			"cwd/.pi/settings.json": "{}",
		},
		steps: [
			{ wait: "Project Local Resources" },
			{ keys: ["Space"], wait: "project unload" },
			{ keys: ["Space"], wait: "project load" },
			{ keys: ["Space"], wait: "Project Local Resources", gone: "project load" },
			{ keys: ["Down", "Down", "Space"], wait: "project unload" },
			{ keys: ["Tab"], wait: "Global Resources" },
			{ keys: ["Tab"], wait: "Project Local Resources" },
			{ keys: ["Escape"], wait: "<exited>" },
		],
		artifacts: ["agent/settings.json", "cwd/.pi/settings.json"],
	},
	{
		name: "untrusted local",
		args: ["config", "-l", "--no-approve"],
		files: {},
		steps: [{ wait: "<exited>" }],
		artifacts: [],
	},
];

function tmux(socket, ...args) {
	return execFileSync("tmux", ["-S", socket, "-f", "/dev/null", ...args], { encoding: "utf8" });
}

export async function runScenario(scenario, command) {
	const tmp = mkdtempSync("/tmp/ultron-tui-");
	const agentDir = join(tmp, "agent");
	const cwd = join(tmp, "cwd");
	for (const dir of [agentDir, cwd, join(tmp, "server")]) mkdirSync(dir, { recursive: true });
	writeFileSync(join(agentDir, "models.json"), JSON.stringify({ providers: { anthropic: { baseUrl: "http://127.0.0.1:9" } } }));
	for (const [rel, text] of Object.entries(scenario.files)) {
		mkdirSync(dirname(join(tmp, rel)), { recursive: true });
		writeFileSync(join(tmp, rel), text);
	}
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
		PI_OFFLINE: "1",
		NODE_OPTIONS: guardNodeOptions,
		ULTRON_GUARD_LOG: join(tmp, "guard.log"),
	};
	const script = join(tmp, "run.sh");
	const quoted = (s) => `'${s.replace(/'/g, "'\\''")}'`;
	writeFileSync(
		script,
		`#!/bin/sh\ncd ${quoted(cwd)}\nexec env -i ${Object.entries(env).map(([k, v]) => quoted(`${k}=${v}`)).join(" ")} ${[...command, ...scenario.args].map(quoted).join(" ")}\n`,
		{ mode: 0o755 },
	);
	const socket = join(tmp, "tmux.sock");
	tmux(socket, "new-session", "-d", "-s", "t", "-x", String(COLS), "-y", String(ROWS), `sh ${quoted(script)}; echo '<exited>'; sleep 60`);
	const capture = () => tmux(socket, "capture-pane", "-p", "-t", "t");
	const captures = [];
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
		for (const rel of scenario.artifacts) {
			const path = join(tmp, rel);
			artifacts[rel] = existsSync(path) ? readFileSync(path, "utf8").split(tmp).join("<TMP>") : null;
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
	return { captures, artifacts };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
	const ts = [process.execPath, join(root, "packages/coding-agent/src/cli.ts")];
	const recorded = { cols: COLS, rows: ROWS, scenarios: [] };
	for (const scenario of scenarios) {
		const result = await runScenario(scenario, ts);
		recorded.scenarios.push({ ...scenario, ...result });
		process.stderr.write(`${scenario.name}: ${result.captures.length} captures\n`);
	}
	writeFileSync(join(here, "config_tui.json"), `${JSON.stringify(recorded, null, 1)}\n`);
}
