// Generates crates/mcp/fixtures/http.json: runs the TypeScript McpClient over StreamableHttpTransport against
// fixtures/fake_http_server.py and records each step's result or error and every request the server received.
// Usage: node crates/mcp/fixtures/generate-http.mjs   (from the repository root; uses ULTRON_PYTHON or /usr/bin/python3)
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const { McpClient, StreamableHttpTransport } = await import(pathToFileURL(join(root, "packages/mcp/src/index.ts")).href);
const python = process.env.ULTRON_PYTHON ?? (existsSync("/usr/bin/python3") ? "/usr/bin/python3" : "python3");

const STEPS = [
	{ op: "listTools" },
	{ op: "callTool", name: "echo", args: { text: "hi" } },
	{ op: "callTool", name: "overloaded" },
	{ op: "callTool", name: "expire" },
	{ op: "callTool", name: "accepted" },
	{ op: "callTool", name: "weird" },
	{ op: "callTool", name: "unanswered" },
	{ op: "callTool", name: "denied" },
	{ op: "close" },
];

const dir = mkdtempSync(join(tmpdir(), "mcp-http-fixture-"));
const log = join(dir, "log.jsonl");
const server = spawn(python, [join(here, "fake_http_server.py")], { env: { ...process.env, MCP_FAKE_LOG: log } });
const port = await new Promise((ok) => server.stdout.once("data", (d) => ok(String(d).trim())));
const client = new McpClient({ name: "fixture-client", version: "1.0.0" });
const transport = new StreamableHttpTransport({ url: `http://127.0.0.1:${port}/mcp`, headers: { "X-Custom": "c1" } });
const steps = [];
try {
	await client.connect(transport);
	steps.push({ op: "connect", ok: { protocolVersion: client.protocolVersion, sessionId: transport.sessionId } });
	for (const step of STEPS) {
		try {
			const value =
				step.op === "listTools" ? await client.listTools()
				: step.op === "close" ? await client.close()
				: await client.callTool(step.name, step.args ?? {});
			steps.push({ ...step, ok: value ?? null });
		} catch (e) {
			steps.push({ ...step, err: { name: e.name, message: e.message, ...(e.status === undefined ? {} : { status: e.status }), ...(e.code === undefined ? {} : { code: e.code }) } });
		}
	}
} finally {
	await new Promise((r) => setTimeout(r, 200));
	server.kill();
}
const requests = readFileSync(log, "utf8").trim().split("\n").map((l) => JSON.parse(l));
rmSync(dir, { recursive: true, force: true });
writeFileSync(join(here, "http.json"), `${JSON.stringify({ port: "dynamic", steps, requests }, null, "\t")}\n`);
