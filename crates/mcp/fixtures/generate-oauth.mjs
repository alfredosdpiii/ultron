// Generates crates/mcp/fixtures/oauth.json from the TypeScript MCP extension (extensions/mcp/index.ts, oauth.ts):
// HTTP servers that sign in with OAuth (401 -> needs sign-in, `/mcp login` through the browser flow against a
// loopback callback on a free port, the stored credentials in mcp-auth.json, refresh after a 401 with rotating refresh
// tokens, `/mcp logout`), an eager server that needs sign-in (its startup notice), a server with `auth.provider`
// (the provider's current key on every request), and a server whose own Authorization header is rejected; all against
// fixtures/fake_oauth_server.py. Ports, PKCE values and the OAuth state are replaced by placeholders.
// Usage: node crates/mcp/fixtures/generate-oauth.mjs   (from the repository root; uses ULTRON_PYTHON or /usr/bin/python3)
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const python = process.env.ULTRON_PYTHON ?? (existsSync("/usr/bin/python3") ? "/usr/bin/python3" : "python3");
const base = realpathSync(mkdtempSync(join(tmpdir(), "mcp-oauth-fixture-")));
const agentDir = join(base, "agent");
const cwd = join(base, "project");
mkdirSync(agentDir, { recursive: true });
mkdirSync(cwd, { recursive: true });
process.env.ULTRON_CODING_AGENT_DIR = agentDir;
process.env.ULTRON_SERVER_DIR = join(base, "server");
for (const key of ["ULTRON_OAUTH_CALLBACK_HOST", "PI_OAUTH_CALLBACK_HOST", "SSH_CONNECTION", "SSH_TTY", "container"])
	delete process.env[key];

const imp = (p) => import(pathToFileURL(join(root, p)).href);
const { createMcpExtension } = await imp("packages/coding-agent/src/extensions/mcp/index.ts");

const CONFIG = {
	mcpServers: {
		issues: { url: "<ORIGIN>/mcp" },
		"eager-issues": { url: "<ORIGIN>/mcp", lifecycle: "eager" },
		prov: { url: "<ORIGIN>/prov-mcp", auth: { provider: "fakeprov" } },
		keyed: { url: "<ORIGIN>/mcp", headers: { Authorization: "Bearer nope" } },
	},
};
const call = (params) => ({ op: "call", params });
const command = (args) => ({ op: "command", args });
const STEPS = [
	{ op: "start" },
	call({ server: "issues" }),
	call({}),
	command("login issues"),
	call({ tool: "issues_whoami" }),
	{ op: "authFile" },
	{ op: "expire" },
	call({ tool: "issues_whoami" }),
	{ op: "authFile" },
	command("logout issues"),
	call({}),
	command("logout issues"),
	call({ tool: "prov_whoami" }),
	{ op: "providerToken", token: "prov-2" },
	call({ tool: "prov_whoami" }),
	call({ server: "keyed" }),
	command("login prov"),
	command("login keyed"),
	{ op: "serverLog" },
	{ op: "shutdown" },
];

const server = spawn(python, [join(here, "fake_oauth_server.py")], { stdio: ["ignore", "pipe", "inherit"] });
const port = await new Promise((ok) => server.stdout.once("data", (d) => ok(String(d).trim())));
const origin = `http://127.0.0.1:${port}`;
writeFileSync(join(agentDir, "mcp.json"), JSON.stringify(JSON.parse(JSON.stringify(CONFIG).replaceAll("<ORIGIN>", origin)), null, 2));

function normalize(value) {
	const text = JSON.stringify(value)
		.replaceAll(agentDir, "<AGENT>")
		.replaceAll(cwd, "<CWD>")
		.replace(/127\.0\.0\.1(:|%3A)(\d+)/g, (_m, sep, p) => `127.0.0.1${sep}${p === port ? "<PORT>" : "<CB>"}`)
		.replace(/(code_challenge=)[A-Za-z0-9_-]+/g, "$1<CHALLENGE>")
		.replace(/(state=)[0-9a-f]{64}/g, "$1<STATE>")
		.replace(/("codeVerifier":")[A-Za-z0-9_-]+/g, "$1<VERIFIER>")
		.replace(/("oauthState":")[0-9a-f]{64}/g, "$1<STATE>")
		.replace(/("tokensExpireAt":)\d+/g, '$1"<EXPIRES>"');
	return JSON.parse(text);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let tool;
let handler;
const handlers = {};
const notices = [];
const prompts = [];
const opened = [];
let providerToken = "prov-1";
createMcpExtension({
	cwd,
	agentDir,
	projectTrusted: false,
	openUrl: (url) => {
		opened.push(url);
		// The browser: follows the authorization server's redirect to the loopback callback.
		void fetch(url).catch(() => {});
	},
})({
	registerTool: (definition) => (tool = definition),
	registerCommand: (_name, options) => (handler = options.handler),
	on: (event, h) => (handlers[event] = h),
});
const ctx = {
	cwd,
	hasUI: false,
	isProjectTrusted: () => false,
	modelRegistry: { getApiKeyForProvider: async (provider) => (provider === "fakeprov" ? providerToken : undefined) },
	ui: {
		notify: (message, level) => notices.push({ message, level }),
		// Nobody pastes a redirect URL: the prompt stays open until the browser's callback ends it.
		input: (title, placeholder, options) => {
			prompts.push({ title, placeholder });
			return new Promise((resolve) => options.signal.addEventListener("abort", () => resolve(undefined), { once: true }));
		},
	},
};
const events = [];
for (const step of STEPS) {
	if (step.op === "start") {
		await handlers.session_start({}, ctx);
		// The eager server's sign-in notice arrives once its background connect fails.
		for (let i = 0; i < 2000 && !notices.some((n) => n.message.includes("eager-issues")); i++) await sleep(10);
		events.push({ ...step, notices: notices.splice(0) });
	} else if (step.op === "call") {
		const result = await tool.execute("c", step.params, new AbortController().signal);
		events.push({ ...step, text: result.content.map((c) => c.text).join("\n"), details: result.details });
	} else if (step.op === "command") {
		await handler(step.args, ctx);
		events.push({ ...step, notices: notices.splice(0), prompts: prompts.splice(0), opened: opened.splice(0) });
	} else if (step.op === "authFile") {
		const path = join(agentDir, "mcp-auth.json");
		events.push({ ...step, content: existsSync(path) ? JSON.parse(readFileSync(path, "utf8")) : null });
	} else if (step.op === "expire") {
		await fetch(`${origin}/__expire`, { method: "POST", body: "{}" });
		events.push(step);
	} else if (step.op === "providerToken") {
		providerToken = step.token;
		events.push(step);
	} else if (step.op === "serverLog") {
		events.push({ ...step, log: await (await fetch(`${origin}/__log`)).json() });
	} else if (step.op === "shutdown") {
		await handlers.session_shutdown({}, ctx);
		events.push(step);
	}
}
server.kill();
rmSync(base, { recursive: true, force: true });
writeFileSync(join(here, "oauth.json"), `${JSON.stringify({ config: CONFIG, events: normalize(events) }, null, "\t")}\n`);
process.exit(0);
