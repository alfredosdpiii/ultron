// Records the protocol traffic between the TypeScript client runtime (experimental/client.ts runClient) and the
// TypeScript server generation (experimental/server.ts startForegroundServer, with its coordinator and session worker
// processes) for scripted client exchanges against a scripted Anthropic SSE server. A recording Unix proxy sits
// between client and server; every frame is decoded and normalized (temp paths, IDs, times) so the fixture is
// deterministic. crates/cli/tests/daemon_traffic.rs runs the same exchanges against the Rust server and worker.
// Usage: node --import packages/coding-agent/src/experimental/source-resolver.ts crates/daemon/fixtures/generate-traffic.mjs
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer as createHttp } from "node:http";
import { createConnection, createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const load = (path) => import(pathToFileURL(join(root, path)).href);

const sse = (events) => events.map((e) => `event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`).join("");
export function textReply(text) {
	return sse([
		{ type: "message_start", message: { id: "msg_1", type: "message", role: "assistant", model: "claude-sonnet-4-5", content: [], stop_reason: null, usage: { input_tokens: 10, output_tokens: 1 } } },
		{ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
		{ type: "content_block_delta", index: 0, delta: { type: "text_delta", text } },
		{ type: "content_block_stop", index: 0 },
		{ type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 5 } },
		{ type: "message_stop" },
	]);
}

/** The scripted exchanges: `setup` runs unrecorded first, then `command` is recorded. */
export const CASES = [
	{ name: "prompt", replies: ["Hi there."], command: { command: "client", prompt: "hello" } },
	{ name: "resume", replies: ["One.", "Two."], setup: { command: "client", prompt: "first" }, command: { command: "client", prompt: "second" }, resume: true },
	{ name: "no-session", replies: ["ok"], command: { command: "client", prompt: "forget" }, noSession: true },
	{ name: "list", replies: ["One."], setup: { command: "client", prompt: "first" }, command: { command: "client" } },
];

const UUID = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g;

/** Placeholders for run-specific values: the temp dir, IDs (numbered by first appearance), times and ports. */
export function normalize(frames, tmp) {
	const ids = new Map();
	const tmpName = tmp.slice(1).replace(/[/\\:]/g, "-");
	const text = (s) =>
		s
			.split(tmp).join("<TMP>")
			.split(tmpName).join("<TMPNAME>")
			.replace(/\d{4}-\d\d-\d\dT\d\d-\d\d-\d\d-\d{3}Z/g, "<FILETIME>")
			.replace(/127\.0\.0\.1:\d+/g, "127.0.0.1:<PORT>")
			.replace(UUID, (id) => {
				if (!ids.has(id)) ids.set(id, `<id:${ids.size}>`);
				return ids.get(id);
			});
	const walk = (value, key) => {
		// The built-in model catalog is not compared (see daemon_traffic.rs); only its size is kept.
		if (key === "availableModels" && Array.isArray(value)) return { count: value.length };
		if (typeof value === "string") return text(value);
		if (typeof value === "number") return value > 1e12 ? "<time>" : value;
		if (Array.isArray(value)) return value.map(walk);
		if (value !== null && typeof value === "object") return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, walk(v, k)]));
		return value;
	};
	return frames.map(({ dir, frame }) => ({ dir, frame: walk(frame) }));
}

async function record(caseName) {
	const spec = CASES.find((c) => c.name === caseName);
	const tmp = mkdtempSync(join(tmpdir(), "ultron-traffic-"));
	const agentDir = join(tmp, "agent"), serverDir = join(tmp, "server"), cwd = join(tmp, "cwd");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(cwd);
	let served = 0;
	const http = createHttp((req, res) => {
		req.resume();
		req.on("end", () => {
			res.writeHead(200, { "content-type": "text/event-stream" });
			res.end(textReply(spec.replies[served++] ?? "(no more replies)"));
		});
	});
	await new Promise((r) => http.listen(0, "127.0.0.1", r));
	writeFileSync(join(agentDir, "models.json"), JSON.stringify({ providers: { anthropic: { baseUrl: `http://127.0.0.1:${http.address().port}` } } }));
	Object.assign(process.env, { HOME: tmp, ULTRON_CODING_AGENT_DIR: agentDir, ULTRON_SERVER_DIR: serverDir, ANTHROPIC_API_KEY: "test-key", NO_COLOR: "1", TZ: "UTC" });
	process.chdir(cwd);
	const { startForegroundServer, resolveSessionDirectory } = await load("packages/coding-agent/src/experimental/server.ts");
	const { runClient } = await load("packages/coding-agent/src/experimental/client.ts");
	const protocol = await load("packages/protocol/src/index.ts");
	const sessionDir = resolveSessionDirectory(undefined);
	const server = await startForegroundServer({ model: "anthropic/claude-sonnet-4-5", sessionDir, extensionMode: "print" });
	const route = { serverId: server.serverId, path: server.endpointPath };
	let sessionId;
	if (spec.setup) {
		sessionId = (await runClient(spec.setup, { sessionDir, route, followEvents: true })).sessionId;
		// Let the idle worker retire, as it does between two separate `ultron` runs.
		await new Promise((r) => setTimeout(r, 1500));
	}
	const proxyPath = join(serverDir, "proxy.sock");
	const frames = [];
	const decoders = { c2s: new protocol.FrameDecoder(), s2c: new protocol.FrameDecoder() };
	const capture = (dir, chunk) => {
		for (const frame of decoders[dir].push(chunk)) frames.push({ dir, frame: protocol.decodeCbor(frame) });
	};
	const proxy = createServer((client) => {
		const upstream = createConnection(server.endpointPath);
		client.on("data", (d) => { capture("c2s", d); upstream.write(d); });
		upstream.on("data", (d) => { capture("s2c", d); client.write(d); });
		client.on("close", () => upstream.destroy());
		upstream.on("close", () => client.destroy());
	});
	await new Promise((r) => proxy.listen(proxyPath, r));
	const command = spec.resume ? { ...spec.command, sessionId } : spec.command;
	const result = await runClient(command, { sessionDir, route: { serverId: server.serverId, path: proxyPath }, followEvents: true, noSession: spec.noSession === true });
	await new Promise((r) => setTimeout(r, 100));
	await server.close();
	proxy.close();
	http.close();
	rmSync(tmp, { recursive: true, force: true });
	const normalized = normalize([...frames, { dir: "result", frame: result }], tmp);
	process.stdout.write(JSON.stringify({ ...spec, frames: normalized }));
	process.exit(0);
}

if (process.argv[2] === "--case") {
	await record(process.argv[3]);
} else {
	const out = [];
	for (const { name } of CASES) {
		const child = spawnSync(process.execPath, [...process.execArgv, fileURLToPath(import.meta.url), "--case", name], { encoding: "utf8", maxBuffer: 1 << 28 });
		if (child.status !== 0) throw new Error(`case ${name} failed: ${child.stderr}`);
		out.push(JSON.parse(child.stdout));
	}
	writeFileSync(join(here, "traffic.json"), `${JSON.stringify(out, null, 1)}\n`);
}
