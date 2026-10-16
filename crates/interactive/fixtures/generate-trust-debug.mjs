// Generates crates/interactive/fixtures/trust-debug.json from the TypeScript client TUI's `/trust` and `/debug`
// (experimental/client-tui-pi-commands.ts `showTrustSelector`, `writeDebugLog`; the trust selector of
// modes/interactive/components/trust-selector.ts): the selector's rendered rows after every key in both themes and
// several widths, the selection's `setProjectTrust` updates and the status after it, and the debug log text with its
// notice, over a recording host. Machine facts (time, version, Node, platform, pid, cwd) are replaced by
// placeholders the Rust test passes back in. crates/interactive/tests/trust_debug.rs replays it.
// Usage: node --import packages/coding-agent/src/experimental/source-resolver.ts \
//   crates/interactive/fixtures/generate-trust-debug.mjs
process.env.FORCE_COLOR = "3";
import "../../cli/fixtures/offline-guard.mjs";
import { mkdtempSync, readFileSync, realpathSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { writeFileSync } from "node:fs";

const here = dirname(fileURLToPath(import.meta.url));
const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-trust-debug-")));
for (const name of Object.keys(process.env)) {
	if (/(_API_KEY|_TOKEN|_SECRET)$/u.test(name)) delete process.env[name];
}
Object.assign(process.env, {
	HOME: join(tmp, "home"),
	ULTRON_CODING_AGENT_DIR: join(tmp, "agent"),
	ULTRON_SERVER_DIR: join(tmp, "server"),
});
const ca = (p) => import(join(here, "../../../packages/coding-agent/src", p));
const T = await ca("modes/interactive/theme/theme.ts");
const { validateThemeJson } = await ca("modes/interactive/theme/theme-json.ts");
T.setThemeJsonValidator(validateThemeJson);
const { KeybindingsManager } = await ca("core/keybindings.ts");
const tuiUrl = pathToFileURL(createRequire(join(here, "../../../packages/coding-agent/package.json")).resolve("@ultron/tui")).href;
const { setKeybindings, visibleWidth } = await import(tuiUrl);
setKeybindings(new KeybindingsManager({}));
const { showTrustSelector, writeDebugLog } = await ca("experimental/client-tui-pi-commands.ts");
const { APP_NAME, VERSION } = await ca("config.ts");
const themeDir = join(here, "../../../packages/coding-agent/src/modes/interactive/theme");

const KEYS = { up: "\x1b[A", down: "\x1b[B", enter: "\r", esc: "\x1b", j: "j", k: "k", x: "x" };
// Paths that do not exist: the trust options resolve them without following links.
const CWD = "/work/project";
const SAVED = [
	null,
	{ path: "/work/project", decision: true },
	{ path: "/work/project", decision: false },
	{ path: "/work", decision: true },
	{ path: "/", decision: false },
];
const scripts = [["enter"], ["down", "enter"], ["down", "down", "enter"], ["up", "j", "j", "j", "k", "esc"], ["x", "down", "enter"]];

const trust = [];
for (const [name, mode] of [["dark", "truecolor"], ["light", "256color"]]) {
	T.setThemeInstance(T.loadThemeFromPath(join(themeDir, `${name}.json`), mode));
	for (const width of [30, 80]) {
		for (const saved of SAVED) {
			for (const projectTrusted of [true, false]) {
				for (const script of scripts) {
					let component;
					let status = null;
					let updates = null;
					let closed = false;
					const host = {
						control: () => ({
							readSettings: async () => ({ cwd: CWD, savedTrust: saved, projectTrusted }),
							setProjectTrust: async (u) => {
								updates = u;
							},
						}),
						showStatus: (text) => {
							status = text;
						},
						requestRender: () => {},
						showComponent: (c) => {
							component = c;
							return () => {
								closed = true;
							};
						},
					};
					await showTrustSelector(host);
					const steps = [{ key: null, lines: component.render(width) }];
					for (const key of script) {
						if (closed) break;
						component.handleInput(KEYS[key]);
						await new Promise((resolve) => setImmediate(resolve));
						steps.push({ key, lines: closed ? null : component.render(width) });
					}
					trust.push({ theme: `${name}-${mode}`, width, saved, projectTrusted, script, steps, updates, status, closed });
				}
			}
		}
	}
}
// A failed read shows its error.
const failed = [];
for (const control of [undefined, { readSettings: async () => Promise.reject(new Error("worker gone")) }]) {
	let status = null;
	await showTrustSelector({ control: () => control, showStatus: (t) => (status = t) });
	failed.push(status);
}
// A failed save shows its error.
{
	let component;
	let status = null;
	await showTrustSelector({
		control: () => ({
			readSettings: async () => ({ cwd: CWD, savedTrust: null, projectTrusted: true }),
			setProjectTrust: async () => Promise.reject(new Error("trust.json is read-only")),
		}),
		showStatus: (t) => (status = t),
		requestRender: () => {},
		showComponent: (c) => ((component = c), () => {}),
	});
	component.handleInput("\r");
	await new Promise((resolve) => setImmediate(resolve));
	failed.push(status);
}

// ── /debug ─────────────────────────────────────────────────────────────────
T.setThemeInstance(T.loadThemeFromPath(join(themeDir, "dark.json"), "truecolor"));
const worker = {
	pid: 4242,
	parentPid: 4000,
	nodeVersion: "v0",
	version: "1.0.0",
	platform: "linux-x64",
	cwd: "/work/project",
	agentDir: "/home/u/.ultron/agent",
	sessionFile: "/s/a.jsonl",
	uptimeMs: 1500,
	rssBytes: 1024,
	model: "anthropic/claude-sonnet-4-5",
	thinkingLevel: "high",
	kernelPool: { live: 1, pinned: [] },
	environment: { PI_OFFLINE: "1", ULTRON_API_KEY: "<redacted>" },
};
const transcript = [
	{ type: "message", message: { role: "user", content: [{ type: "text", text: "hi \"there\"\n\u001b" }], timestamp: 1 } },
	{ type: "compaction", summary: "x" },
	{ type: "message", message: { role: "assistant", content: [{ type: "text", text: "héllo 😀" }], usage: { cost: 0.5 } } },
];
const lines = ["", "\u001b[1mbold\u001b[22m line", "wide 😀 字", "\t tab"];
const debugCases = [
	{ name: "worker", control: { debugInfo: async () => worker }, server: { serverId: "srv-1", transport: "unix", socketPath: "/run/u/srv-1.sock" }, sessionId: "sess-1", transcript },
	{ name: "worker-without-model", control: { debugInfo: async () => ({ ...worker, model: null }) }, server: { serverId: "srv-1", transport: "radius", socketPath: undefined }, sessionId: "sess-1", transcript: [] },
	{ name: "worker-error", control: { debugInfo: async () => Promise.reject(new Error("Session worker is stopping")) }, server: { serverId: undefined, transport: undefined, socketPath: undefined }, sessionId: undefined, transcript: undefined },
	{ name: "no-session", control: undefined, server: { serverId: "srv-2", transport: "unix", socketPath: "/x/y.sock" }, sessionId: undefined, transcript: [] },
];
const debug = [];
for (const c of debugCases) {
	let notice = null;
	let status = null;
	const path = join(tmp, `debug-${c.name}.log`);
	await writeDebugLog(
		{
			renderedLines: () => ({ width: 100, height: 30, lines }),
			control: () => c.control,
			serverInfo: () => c.server,
			snapshot: () => (c.transcript === undefined ? undefined : { transcript: c.transcript }),
			sessionId: () => c.sessionId,
			notice: (component) => {
				notice = component.render(200);
			},
			showStatus: (t) => (status = t),
			agentDir: tmp,
		},
		path,
	);
	const text = readFileSync(path, "utf8")
		.replace(/^Debug output at .*$/mu, "Debug output at <time>")
		.replaceAll(`${APP_NAME} ${VERSION} · node ${process.version} · ${process.platform}-${process.arch} · pid ${process.pid}`, "<client>")
		.replaceAll(`cwd: ${process.cwd()}`, "cwd: <cwd>");
	debug.push({
		name: c.name,
		input: { width: 100, height: 30, lines, widths: lines.map((l) => visibleWidth(l)), sessionId: c.sessionId ?? null, server: { serverId: c.server.serverId ?? null, transport: c.server.transport ?? null, socketPath: c.server.socketPath ?? null }, messages: (c.transcript ?? []).filter((e) => e.type === "message").map((e) => e.message) },
		worker: c.name === "worker-error" ? { error: "Session worker is stopping" } : c.name === "no-session" ? { error: "No Session is attached" } : await c.control.debugInfo(),
		text,
		notice: notice?.map((l) => l.replaceAll(path, "<path>")) ?? null,
		status,
	});
}
// A log that cannot be written: its directory is a regular file.
const blocker = join(tmp, "not-a-directory");
writeFileSync(blocker, "");
{
	let status = null;
	await writeDebugLog(
		{
			renderedLines: () => ({ width: 1, height: 1, lines: [] }),
			control: () => undefined,
			serverInfo: () => ({}),
			snapshot: () => undefined,
			sessionId: () => undefined,
			notice: () => {},
			showStatus: (t) => (status = t),
			agentDir: tmp,
		},
		join(blocker, "debug.log"),
	);
	debug.push({ name: "unwritable", status: status.replace(/(debug log): .*$/u, "$1: <reason>") });
}

writeFileSync(join(here, "trust-debug.json"), `${JSON.stringify({ app: APP_NAME, cwd: CWD, trust, failed, debug })}\n`);
rmSync(tmp, { recursive: true, force: true });
