// Generates crates/mcp/fixtures/gateway.json from the TypeScript MCP extension (extensions/mcp/index.ts): the `mcp`
// gateway tool the REPL's `mcp` namespace calls, run against fixtures/fake_gateway_server.py (stdio) and
// fixtures/fake_http_server.py (streamable HTTP) through mcp.json files in a temp agent directory and project.
// Every step's params and result (content and details) are recorded with paths and ports replaced by placeholders;
// tests/gateway_fixture.rs runs the same steps through the Rust gateway and compares.
// Usage: node crates/mcp/fixtures/generate-gateway.mjs   (from the repository root; uses ULTRON_PYTHON or /usr/bin/python3)
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const python = process.env.ULTRON_PYTHON ?? (existsSync("/usr/bin/python3") ? "/usr/bin/python3" : "python3");
const base = realpathSync(mkdtempSync(join(tmpdir(), "mcp-gateway-fixture-")));
const agentDir = join(base, "agent");
const cwd = join(base, "project");
mkdirSync(agentDir, { recursive: true });
mkdirSync(join(cwd, ".pi"), { recursive: true });
// Private profile for anything the extension reads or writes (OAuth store, mcp.log).
process.env.ULTRON_CODING_AGENT_DIR = agentDir;
process.env.ULTRON_SERVER_DIR = join(base, "server");
delete process.env.ULTRON_MASK_SECRETS;

const ENV = { GW_SECRET_SRC: "fixture-not-a-secret-0001", GW_HTTP_TOKEN: "fixture-http-token-0002" };
Object.assign(process.env, ENV);

const imp = (p) => import(pathToFileURL(join(root, p)).href);
const { createMcpExtension } = await imp("packages/coding-agent/src/extensions/mcp/index.ts");
const { maskCellOutput } = await imp("packages/coding-agent/src/ultron/rlm/output-secrets.ts");

const server = join(here, "fake_gateway_server.py");
const stdio = (variant, extra = {}) => ({ command: "<PY>", args: ["<SERVER>", variant, "-"], ...extra });
const CONFIG = {
	global: {
		mcpServers: {
			alpha: {
				command: "<PY>",
				args: ["<SERVER>", "alpha", "<LOG_ALPHA>"],
				env: { GW_TOKEN: "${GW_SECRET_SRC}" },
				description: "Alpha test server",
				timeout: 1.5,
				toolExposure: { hidden_tool: "hidden" },
			},
			"beta-srv": stdio("beta", { lifecycle: "eager" }),
			plain: stdio("plain"),
			dies: stdio("dies"),
			missing: { command: "/nonexistent/gw-server" },
			off: stdio("beta", { enabled: false }),
			beta_srv: stdio("beta"),
			web: { url: "http://127.0.0.1:<PORT>/mcp", headers: { Authorization: "Bearer ${GW_HTTP_TOKEN}" } },
		},
	},
	project: {
		mcpServers: {
			plain: stdio("plain", { description: "Project plain" }),
			"bad name!": stdio("beta"),
		},
	},
};

const call = (params) => ({ op: "call", params });
const MAIN = [
	{ op: "tool" },
	call({}),
	{ op: "serverLog" },
	{ op: "start" },
	{ op: "waitConnected", server: "beta-srv" },
	call({}),
	{ op: "serverLog" },
	call({ server: "alpha" }),
	{ op: "serverLog" },
	call({ server: "alpha" }),
	call({}),
	call({ describe: "alpha_echo" }),
	call({ describe: "echo" }),
	call({ describe: "echo", server: "beta-srv" }),
	call({ describe: "mcp__beta_srv__echo" }),
	call({ describe: "alpha_add" }),
	call({ describe: "nope" }),
	call({ describe: "dies_x" }),
	call({ describe: "missing_x" }),
	call({ tool: "alpha_echo", args: { text: "hi" } }),
	call({ tool: "echo", args: '{"text":"json string"}' }),
	call({ tool: "beta-srv_echo", args: { text: "b" } }),
	call({ tool: "alpha_echo", args: "[1]" }),
	call({ tool: "alpha_echo", args: "" }),
	call({ tool: "alpha_add", args: { a: 2, b: 3 } }),
	call({ tool: "alpha_structured" }),
	call({ tool: "alpha_fails" }),
	call({ tool: "alpha_fails_text" }),
	call({ tool: "alpha_image" }),
	call({ tool: "alpha_link" }),
	call({ tool: "alpha_blob" }),
	call({ tool: "alpha_jsonblob" }),
	call({ tool: "alpha_env" }),
	call({ tool: "alpha_cwd" }),
	call({ tool: "alpha_hidden_tool" }),
	call({ tool: "beta_only", server: "alpha" }),
	call({ tool: "web_echo", args: { text: "over http" } }),
	call({ server: "web" }),
	call({ search: "echo" }),
	call({ search: "ECHO text" }),
	call({ search: "", limit: 3 }),
	call({ search: "add", server: "alpha" }),
	call({ search: "zzz" }),
	call({ search: "only" }),
	call({ instructions: "alpha" }),
	call({ instructions: "beta-srv" }),
	call({ instructions: "nope" }),
	call({ resources: true }),
	call({ resources: "alpha" }),
	call({ resources: true, server: "plain" }),
	call({ resources: "dies" }),
	call({ read: "file:///notes.txt", server: "alpha" }),
	call({ read: "file:///data.bin", server: "alpha" }),
	call({ read: "file:///missing", server: "alpha" }),
	call({ read: "file:///notes.txt" }),
	call({ connect: "alpha" }),
	call({ connect: "off" }),
	call({ server: "off" }),
	call({ tool: "echo", server: "off" }),
	call({ server: "nope" }),
	call({ tool: "alpha_log" }),
	{ op: "log" },
	call({ tool: "alpha_hang" }),
	{ op: "cancel", params: { tool: "alpha_hang" } },
	call({ tool: "alpha_crash" }),
	call({}),
	call({ tool: "alpha_echo", args: { text: "back" } }),
	call({ tool: "alpha_quit" }),
	call({}),
	call({ tool: "alpha_echo", args: { text: "again" } }),
	call({ tool: "alpha_change" }),
	{ op: "waitTool", server: "alpha", tool: "alpha_added" },
	call({ server: "alpha" }),
	{ op: "mask", text: "env fixture-not-a-secret-0001 and fixture-http-token-0002 and Bearer fixture-http-token-0002" },
	{ op: "shutdown" },
	{ op: "serverLog" },
	call({ server: "alpha" }),
	call({}),
];
const UNTRUSTED = [{ op: "tool" }, { op: "start" }, call({ server: "plain" })];
const EMPTY = [{ op: "tool" }];

function normalize(value, port) {
	const text = JSON.stringify(value)
		.replaceAll(join(here, "fake_gateway_server.py"), "<SERVER>")
		.replaceAll(python, "<PY>")
		.replaceAll(cwd, "<CWD>")
		.replaceAll(agentDir, "<AGENT>")
		.replaceAll(base, "<BASE>")
		.replaceAll(`127.0.0.1:${port}`, "127.0.0.1:<PORT>")
		.replace(/\/[^"\s]*\/ultron-mcp-[0-9a-f]{16}(\.[A-Za-z0-9]+)/g, "<SAVED>$1");
	return JSON.parse(text);
}

function fill(config, port, logAlpha) {
	return JSON.parse(
		JSON.stringify(config)
			.replaceAll("<PY>", python)
			.replaceAll("<SERVER>", server)
			.replaceAll("<LOG_ALPHA>", logAlpha)
			.replaceAll("<PORT>", port),
	);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function run(name, steps, { trusted, config }) {
	const dir = join(base, name);
	mkdirSync(dir, { recursive: true });
	const logAlpha = join(dir, "alpha.log");
	const httpLog = join(dir, "http.log");
	const http = spawn(python, [join(here, "fake_http_server.py")], { env: { ...process.env, MCP_FAKE_LOG: httpLog } });
	const port = await new Promise((ok) => http.stdout.once("data", (d) => ok(String(d).trim())));
	rmSync(join(agentDir, "mcp.json"), { force: true });
	rmSync(join(agentDir, "mcp.log"), { force: true });
	rmSync(join(cwd, ".pi", "mcp.json"), { force: true });
	if (config.global) writeFileSync(join(agentDir, "mcp.json"), JSON.stringify(fill(config.global, port, logAlpha), null, 2));
	if (config.project) writeFileSync(join(cwd, ".pi", "mcp.json"), JSON.stringify(fill(config.project, port, logAlpha), null, 2));

	let tool;
	const handlers = {};
	const notices = [];
	const pi = {
		registerTool: (definition) => {
			tool = definition;
		},
		registerCommand: () => {},
		on: (event, handler) => {
			handlers[event] = handler;
		},
	};
	createMcpExtension({ cwd, agentDir, projectTrusted: trusted })(pi);
	const ctx = {
		cwd,
		modelRegistry: undefined,
		isProjectTrusted: () => trusted,
		hasUI: false,
		ui: { notify: (message, level) => notices.push({ message, level }) },
	};
	const execute = async (params) => {
		try {
			return { result: await tool.execute("call-1", params, new AbortController().signal) };
		} catch (error) {
			return { threw: error.message };
		}
	};
	const statusOf = async (server) => (await execute({})).result.details.servers.find((s) => s.name === server)?.status;
	const events = [];
	for (const step of steps) {
		if (step.op === "tool") {
			events.push({
				...step,
				tool: tool
					? { name: tool.name, label: tool.label, description: tool.description, parameters: tool.parameters }
					: null,
			});
		} else if (step.op === "call") {
			events.push({ ...step, ...(await execute(step.params)) });
		} else if (step.op === "cancel") {
			// Abort the call once the server has received it (its log names it a second time).
			const controller = new AbortController();
			const seen = () => (existsSync(logAlpha) ? readFileSync(logAlpha, "utf8").split("tools/call hang").length - 1 : 0);
			const before = seen();
			const pending = tool.execute("call-2", step.params, controller.signal).then(
				(result) => ({ result }),
				(error) => ({ threw: error.message }),
			);
			for (let i = 0; i < 4800 && seen() === before; i++) await sleep(25);
			controller.abort();
			events.push({ ...step, ...(await pending) });
		} else if (step.op === "start") {
			await handlers.session_start({}, ctx);
			events.push({ ...step, notices: notices.splice(0) });
		} else if (step.op === "waitConnected") {
			for (let i = 0; i < 1200 && ["connecting", "not connected"].includes(await statusOf(step.server)); i++)
				await sleep(25);
			events.push({ ...step, status: await statusOf(step.server) });
		} else if (step.op === "waitTool") {
			let found = false;
			for (let i = 0; i < 1200 && !found; i++) {
				found = ((await execute({ server: step.server })).result.details.tools ?? []).includes(step.tool);
				if (!found) await sleep(25);
			}
			events.push({ ...step, found });
		} else if (step.op === "log") {
			const text = existsSync(join(agentDir, "mcp.log")) ? readFileSync(join(agentDir, "mcp.log"), "utf8") : "";
			events.push({ ...step, text: text.replace(/^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z/gm, "<TIME>") });
		} else if (step.op === "mask") {
			events.push({ ...step, masked: maskCellOutput(step.text) });
		} else if (step.op === "shutdown") {
			await handlers.session_shutdown({}, ctx);
			events.push(step);
		} else if (step.op === "serverLog") {
			// Sorted: a connect lists tools and resources concurrently, so the order of those requests is not fixed.
			const lines = existsSync(logAlpha) ? readFileSync(logAlpha, "utf8").trim().split("\n").filter(Boolean) : [];
			events.push({ ...step, lines: lines.sort() });
		}
	}
	http.kill();
	return { name, trusted, events: normalize(events, port) };
}

const scenarios = [
	await run("main", MAIN, { trusted: true, config: CONFIG }),
	await run("untrusted", UNTRUSTED, { trusted: false, config: CONFIG }),
	await run("empty", EMPTY, { trusted: true, config: {} }),
];
rmSync(base, { recursive: true, force: true });
writeFileSync(join(here, "gateway.json"), `${JSON.stringify({ env: ENV, config: CONFIG, scenarios }, null, "\t")}\n`);
process.exit(0);
