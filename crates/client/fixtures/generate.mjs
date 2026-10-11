// Generates crates/client/fixtures/client.json by driving the TypeScript packages/client Client over an in-memory
// ByteTransport: records every frame the client sends and how it settles connect() and request() against scripted
// server bytes. The Rust Client replays the same server bytes and must send the same frames and settle the same way.
// Usage: node crates/client/fixtures/generate.mjs [output.json]
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "../../..");
const load = (path) => import(pathToFileURL(join(root, path)).href);
const p = await load("packages/protocol/src/index.ts");
const { Client } = await load("packages/client/src/client.ts");

const SID = "00000000-0000-4000-8000-000000000001";
const OTHER = "00000000-0000-4000-8000-000000000002";
const hex = (b) => Buffer.from(b).toString("hex");
const unhex = (h) => new Uint8Array(Buffer.from(h, "hex"));
const settle = async () => {
	for (let i = 0; i < 20; i++) await new Promise((resolve) => setImmediate(resolve));
};
const outcome = (error) => ({ error: { name: error.name, message: error.message, code: error.code ?? null } });

async function run(script) {
	const sent = [];
	let handlers;
	const client = new Client({
		serverId: SID,
		transportFactory: (h) => {
			handlers = h;
			return { async send(chunk) { sent.push(hex(chunk)); }, close() {} };
		},
	});
	const results = [];
	for (const step of script.steps) {
		sent.length = 0;
		let settled;
		const promise = step.action === "connect" ? client.connect() : client.request(step.target, step.call);
		promise.then((value) => (settled = { ok: value === undefined ? null : value }), (e) => (settled = outcome(e)));
		await settle();
		for (const reply of step.replies) {
			if (reply === "close") handlers.onClose();
			else handlers.onData(unhex(reply));
			await settle();
		}
		await settle();
		results.push({ sent: [...sent], settled: settled ?? "pending", attachment: client.attachment ?? null });
	}
	await client.dispose();
	return { ...script, results };
}

const s = (m) => hex(p.encodeServerMessage(m));
const hello = s({ type: "hello", version: 8, serverId: SID });
const ok = (id, result) => s(result === undefined ? { type: "response", id, ok: true } : { type: "response", id, ok: true, result });
const target = { serverId: SID };
const call = { serviceId: "pi.models", member: "list", args: [] };
const attachment = { serverId: SID, sessionId: "session-1", attachmentId: "attachment-1" };
const connect = { action: "connect", replies: [hello] };
const scripts = [
	{ name: "connect then request", steps: [connect, { action: "request", target, call, replies: [ok("request-1", { models: ["a"] })] },
		{ action: "request", target: { ...attachment }, call: { ...call, instance: { key: "k", generation: 1 } }, replies: [ok("request-2")] }] },
	{ name: "hello error", steps: [{ action: "connect", replies: [s({ type: "hello_error", error: { code: "version", message: "Unsupported protocol version 9; expected 8" } })] }] },
	{ name: "wrong server", steps: [{ action: "connect", replies: [s({ type: "hello", version: 8, serverId: OTHER })] }] },
	{ name: "response before hello", steps: [{ action: "connect", replies: [ok("request-1")] }] },
	{ name: "server error response", steps: [connect, { action: "request", target, call, replies: [s({ type: "response", id: "request-1", ok: false, error: { code: "wrong_server", message: "Request was addressed to another server" } })] },
		{ action: "request", target, call, replies: [ok("request-2", 5)] }] },
	{ name: "attachment updates", steps: [connect, { action: "request", target, call, replies: [s({ type: "attachment", attachment }) + ok("request-1", null)] },
		{ action: "request", target, call, replies: [s({ type: "attachment", attachment: null }), ok("request-2")] }] },
	{ name: "foreign attachment", steps: [connect, { action: "request", target, call, replies: [s({ type: "attachment", attachment: { ...attachment, serverId: OTHER } })] }] },
	{ name: "unmatched response", steps: [connect, { action: "request", target, call, replies: [ok("request-9")] },
		{ action: "request", target, call, replies: [] }] },
	{ name: "second hello", steps: [connect, { action: "request", target, call, replies: [hello] }] },
	{ name: "truncated frame then close", steps: [connect, { action: "request", target, call, replies: ["0000000201", "close"] }] },
	{ name: "clean close", steps: [connect, { action: "request", target, call, replies: ["close"] }] },
	{ name: "malformed frame", steps: [connect, { action: "request", target, call, replies: ["00000001ff"] }] },
	{ name: "fragmented reply", steps: [connect, { action: "request", target, call, replies: [ok("request-1", "abc").slice(0, 8), ok("request-1", "abc").slice(8)] }] },
	{ name: "invalid call", steps: [connect, { action: "request", target, call: { serviceId: "", member: "m", args: [] }, replies: [] },
		{ action: "request", target, call, replies: [ok("request-1")] }] },
];
const out = [];
for (const script of scripts) out.push(await run(script));
const text = `${JSON.stringify({ source: "packages/client/src/client.ts", serverId: SID, scripts: out }, null, 1)}\n`;
writeFileSync(process.argv[2] ?? join(root, "crates/client/fixtures/client.json"), text);
