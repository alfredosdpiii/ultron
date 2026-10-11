// Generates crates/mcp/fixtures/client.json: runs the TypeScript McpClient over StdioTransport against
// fixtures/fake_server.py and records each step's result or error, the client's error and notification events,
// and every raw line the fake server read and wrote. The Rust client replays the same steps against the same server.
// Usage: node crates/mcp/fixtures/generate.mjs   (from the repository root; uses ULTRON_PYTHON or /usr/bin/python3)
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const { McpClient, StdioTransport } = await import(pathToFileURL(join(root, "packages/mcp/src/index.ts")).href);
const python = process.env.ULTRON_PYTHON ?? (existsSync("/usr/bin/python3") ? "/usr/bin/python3" : "python3");

const ROOTS = [{ uri: "file:///workspace", name: "workspace" }];
const SESSIONS = [
	{
		client: { name: "fixture-client", version: "1.0.0", title: "Fixture", roots: true },
		steps: [
			{ op: "listTools" },
			{ op: "listResources" },
			{ op: "listResourceTemplates" },
			{ op: "listResourcesPage" },
			{ op: "listResourcesPage", cursor: "2" },
			{ op: "readResource", uri: "file:///a" },
			{ op: "readResource", uri: "file:///bad" },
			{ op: "callTool", name: "echo", args: { text: "hi" } },
			{ op: "callTool", name: "structured" },
			{ op: "callTool", name: "broken" },
			{ op: "callTool", name: "fail", args: {} },
			{ op: "callTool", name: "nope" },
			{ op: "callTool", name: "garbage" },
			{ op: "callTool", name: "roots" },
			{ op: "callTool", name: "progress", args: { n: 1 }, timeoutMs: 450, progress: true },
			{ op: "ping" },
			{ op: "request", method: "bogus/method", params: { a: [1, "x", null] } },
			{ op: "callTool", name: "hang", abort: "stop" },
			{ op: "callTool", name: "hang", timeoutMs: 100 },
			{ op: "notify", method: "notifications/custom", params: { k: "v" } },
			{ op: "callTool", name: "echo", args: { text: "after" } },
			{ op: "close" },
			{ op: "callTool", name: "echo", args: { text: "closed" } },
		],
	},
	{ client: { name: "old", version: "0.1.0", protocolVersion: "2024-11-05" }, steps: [{ op: "ping" }, { op: "close" }] },
	{ client: { name: "ancient", version: "0.1.0" }, steps: [] },
];

const errorOf = (e) => ({
	name: e.name,
	message: e.message,
	...(e.code === undefined ? {} : { code: e.code }),
	...(e.data === undefined ? {} : { data: e.data }),
});
const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));

async function runStep(client, step, events) {
	switch (step.op) {
		case "listTools":
			return client.listTools();
		case "listResources":
			return client.listResources();
		case "listResourceTemplates":
			return client.listResourceTemplates();
		case "listResourcesPage":
			return client.listResourcesPage(step.cursor);
		case "readResource":
			return client.readResource(step.uri);
		case "ping":
			return client.ping();
		case "request":
			return client.request(step.method, step.params);
		case "notify":
			return client.notify(step.method, step.params);
		case "close":
			return client.close();
		case "callTool": {
			const options = {};
			if (step.timeoutMs !== undefined) options.timeoutMs = step.timeoutMs;
			if (step.progress) options.onProgress = (p) => events.push({ progress: clone(p) });
			let controller;
			if (step.abort !== undefined) {
				controller = new AbortController();
				options.signal = controller.signal;
			}
			const call = client.callTool(step.name, step.args, options);
			controller?.abort(step.abort);
			return call;
		}
	}
	throw new Error(`unknown step ${step.op}`);
}

const sessions = [];
for (const session of SESSIONS) {
	const dir = mkdtempSync(join(tmpdir(), "mcp-fixture-"));
	const log = join(dir, "log.jsonl");
	const events = [];
	const { roots, ...options } = session.client;
	const client = new McpClient({ ...options, ...(roots ? { roots: ROOTS } : {}) });
	client.onError((e) => events.push({ error: e.message }));
	client.onNotification("notifications/tools/list_changed", (params) => events.push({ notification: clone(params) }));
	client.onClose(() => events.push({ closed: true }));
	const transport = new StdioTransport({
		command: python,
		args: ["-I", join(here, "fake_server.py")],
		env: { MCP_FAKE_LOG: log },
	});
	const results = [];
	let connect;
	try {
		const result = await client.connect(transport);
		connect = { ok: clone(result) };
	} catch (e) {
		connect = { error: errorOf(e) };
	}
	results.push({ op: "connect", state: client.connectionState, ...connect });
	for (const step of session.steps) {
		try {
			results.push({ ...step, ok: clone(await runStep(client, step, events)) });
		} catch (e) {
			results.push({ ...step, error: errorOf(e) });
		}
	}
	await client.close();
	const frames = readFileSync(log, "utf8")
		.split("\n")
		.filter(Boolean)
		.map((line) => JSON.parse(line));
	sessions.push({
		client: session.client,
		results,
		state: {
			connectionState: client.connectionState,
			protocolVersion: client.protocolVersion ?? null,
			serverInfo: clone(client.serverInfo),
			serverCapabilities: clone(client.serverCapabilities),
			instructions: client.instructions ?? null,
		},
		events,
		frames,
		stderr: transport.stderr,
	});
	rmSync(dir, { recursive: true, force: true });
}
writeFileSync(join(here, "client.json"), `${JSON.stringify({ roots: ROOTS, sessions }, null, 1)}\n`);
