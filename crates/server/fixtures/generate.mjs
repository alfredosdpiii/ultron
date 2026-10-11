// Generates crates/server/fixtures/server.json by driving the TypeScript packages/server Server (no listeners, an
// in-memory ByteConnection) through scripted client byte sequences. Records every frame the server sends, every close
// (with its final frame), and every error it reports; the Rust Connection replays the same bytes and must match.
// Usage: node crates/server/fixtures/generate.mjs [output.json]
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "../../..");
const load = (path) => import(pathToFileURL(join(root, path)).href);
const p = await load("packages/protocol/src/index.ts");
const { Server } = await load("packages/server/src/server.ts");
const { ServerError } = await load("packages/server/src/errors.ts");

const SID = "00000000-0000-4000-8000-000000000001";
const OTHER = "00000000-0000-4000-8000-000000000002";
const hex = (b) => Buffer.from(b).toString("hex");
const unhex = (h) => new Uint8Array(Buffer.from(h, "hex"));
const settle = async () => {
	for (let i = 0; i < 20; i++) await new Promise((resolve) => setImmediate(resolve));
};

// The fixture host's server-scoped services; the Rust test host implements the same table.
const services = {
	attachClient() {
		return {
			async invokeService(call) {
				if (call.serviceId === "test.echo") return { echo: call.args, member: call.member };
				if (call.serviceId === "test.void") return undefined;
				if (call.serviceId === "test.fail") throw new ServerError("session_not_found", String(call.args[0] ?? "nope"));
				if (call.serviceId === "test.big") return "x".repeat(Number(call.args[0]));
				throw new Error(`Unsupported test server service ${call.serviceId}.${call.member}`);
			},
			release() {},
		};
	},
};

async function run(script) {
	const events = [];
	const reported = [];
	const server = new Server(
		{ serverServices: services, resolveSession: async () => { throw new Error("no sessions"); }, openSession: async () => { throw new Error("no sessions"); } },
		{ listeners: [], serverId: SID, maxFrameLength: script.max ?? undefined, handshakeTimeoutMs: 20, onError: (e) => reported.push(e.message) },
	);
	let closed = false;
	const connection = {
		get closed() {
			return closed;
		},
		async send(chunk) {
			events.push({ send: hex(chunk) });
		},
		close(finalChunk) {
			if (closed) return;
			closed = true;
			events.push({ close: finalChunk ? hex(finalChunk) : null });
		},
	};
	const handler = server.accept(connection);
	const results = [];
	for (const op of script.ops) {
		events.length = 0;
		reported.length = 0;
		if (op === "close") handler.onClose();
		else if (op === "timeout") await new Promise((resolve) => setTimeout(resolve, 60));
		else handler.onData(unhex(op));
		await settle();
		results.push({ op, events: [...events], reported: [...reported] });
	}
	await server.close();
	return { ...script, results };
}

const frame = (m) => hex(p.encodeClientMessage(m));
const raw = (v) => hex(p.encodeFrame(p.encodeCbor(v)));
const hello = frame({ type: "hello", version: 8 });
const req = (id, call, target = { serverId: SID }) => frame({ type: "request", id, target, call });
const call = (serviceId, args = [], member = "invoke") => ({ serviceId, member, args });
const echo = req("r1", call("test.echo", [1, "two", { three: [3] }]));
const scripts = [
	{ name: "handshake then echo", ops: [hello, echo] },
	{ name: "request before hello", ops: [req("r1", call("test.echo"))] },
	{ name: "unsupported version", ops: [frame({ type: "hello", version: 7 })] },
	{ name: "version zero", ops: [frame({ type: "hello", version: 0 })] },
	{ name: "second hello", ops: [hello, hello, echo] },
	{ name: "coalesced hello and request", ops: [hello + echo] },
	{ name: "fragmented hello and request", ops: [hello.slice(0, 6), hello.slice(6) + echo.slice(0, 10), echo.slice(10)] },
	{ name: "void, failing and crashing services", ops: [hello, req("a", call("test.void")), req("b", call("test.fail", ["gone"])), req("c", call("test.crash"))] },
	{ name: "keyed instance call", ops: [hello, req("k", { serviceId: "test.echo", instance: { key: "i", generation: 2 }, member: "m", args: [] })] },
	{ name: "invalid service calls", ops: [hello,
		req("x1", { serviceId: "test.echo", member: "m" }),
		req("x2", { serviceId: "", member: "m", args: [] }),
		req("x3", { serviceId: "test.echo", member: "m", args: {} }),
		req("x4", { serviceId: "test.echo", member: "m", args: [], extra: 1 }),
		req("x5", { serviceId: "test.echo", member: "m", args: [], instance: { key: "i", generation: 0 } }),
		req("x6", { serviceId: "test.echo", member: "m", args: [], instance: { key: "", generation: 1 } }),
		req("x7", "not an object"),
		req("x8", [1]),
		req("x9", { serviceId: "test.echo", member: "m", args: [], instance: { key: "i", generation: 1.5 } }),
	] },
	{ name: "invalid call before wrong server", ops: [hello, req("w1", { member: "m" }, { serverId: OTHER }), req("w2", call("test.echo"), { serverId: OTHER })] },
	{ name: "session target without attachment", ops: [hello, req("s1", call("test.echo"), { serverId: SID, sessionId: "session-1", attachmentId: "a-1" }),
		req("s2", call("test.echo"), { serverId: OTHER, sessionId: "session-1", attachmentId: "a-1" })] },
	{ name: "cancel unknown request", ops: [hello, frame({ type: "cancel", id: "zz", target: { serverId: SID } }), echo] },
	{ name: "schema-invalid frame", ops: [hello, raw({ type: "request", id: "", target: { serverId: SID }, call: {} }), echo] },
	{ name: "malformed cbor", ops: ["00000001ff"] },
	{ name: "truncated frame then close", ops: [hello, "0000000201", "close"] },
	{ name: "clean close", ops: [hello, "close", echo] },
	{ name: "handshake timeout", ops: ["timeout", hello] },
	{ name: "oversized frame", max: 100, ops: [hello, "00000065"] },
	{ name: "oversized response closes", max: 150, ops: [hello, req("b", call("test.big", [120])), echo] },
	{ name: "response under the limit", max: 150, ops: [hello, req("b", call("test.big", [20]))] },
	{ name: "request over the limit", max: 100, ops: [hello, req("b", call("test.big", [20]))] },
	{ name: "hello error too large for limit", max: 8, ops: ["00000001ff"] },
];
const out = [];
for (const script of scripts) out.push(await run(script));
const text = `${JSON.stringify({ source: "packages/server/src/server.ts", serverId: SID, scripts: out }, null, 1)}\n`;
writeFileSync(process.argv[2] ?? join(root, "crates/server/fixtures/server.json"), text);
