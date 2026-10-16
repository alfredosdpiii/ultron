// Records the TypeScript `ultron server` command (PI_EXPERIMENTAL=1, experimental/commands.ts `runServerCommand`)
// against a scripted Radius gateway on 127.0.0.1: what it prints (`Server:`, `Socket:`, the `Radius: …` lines), its
// stderr and exit code after SIGTERM, and what the gateway saw (each upgrade's path, offered subprotocol and
// authorization, and the close frame the relay host sent). Scenarios: a gateway that accepts the host; one that
// closes the first host connection with 4000 (the relay reconnects); one that refuses the upgrade (401); no
// credential; offline mode; and the stored RADIUS_API_KEY. offline-guard.mjs is loaded into the TypeScript process;
// the generator fails if anything tried to leave loopback. crates/cli/tests/radius_server.rs replays it against the
// Rust command.
// Usage: node crates/cli/fixtures/radius_server.mjs   (from the repository root)
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { assertOffline, guardNodeOptions } from "./offline-guard.mjs";

const root = new URL("../../../", import.meta.url).pathname;
const SERVER_ID = "00000000-0000-4000-8000-0000000000aa";
const GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/** One unmasked server frame. */
function frame(opcode, payload) {
	const head = payload.length < 126 ? Buffer.from([0x80 | opcode, payload.length]) : null;
	if (head === null) throw new Error("long frames are not scripted");
	return Buffer.concat([head, payload]);
}

function closeFrame(code, reason) {
	const payload = Buffer.alloc(2 + Buffer.byteLength(reason));
	payload.writeUInt16BE(code, 0);
	payload.write(reason, 2);
	return frame(0x8, payload);
}

/** The close frame (`code reason`) in the masked client bytes `data`, if one is there. */
function readClose(data) {
	let offset = 0;
	while (offset + 2 <= data.length) {
		const opcode = data[offset] & 0x0f;
		let length = data[offset + 1] & 0x7f;
		let at = offset + 2;
		if (length === 126) {
			length = data.readUInt16BE(at);
			at += 2;
		} else if (length === 127) {
			length = Number(data.readBigUInt64BE(at));
			at += 8;
		}
		const masked = (data[offset + 1] & 0x80) !== 0;
		const mask = masked ? data.subarray(at, at + 4) : null;
		if (masked) at += 4;
		if (at + length > data.length) return undefined;
		const payload = Buffer.from(data.subarray(at, at + length));
		if (mask) for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i % 4];
		if (opcode === 0x8) {
			const code = payload.length >= 2 ? payload.readUInt16BE(0) : 1005;
			return `${code} ${payload.subarray(2).toString("utf8")}`;
		}
		offset = at + length;
	}
	return undefined;
}

/** The scripted gateway: `mode` is "accept", "close-first" (4000 on the first host connection) or "refuse" (401). */
async function gateway(mode) {
	const upgrades = [];
	const waiters = [];
	const changed = () => {
		for (const waiter of waiters.splice(0)) waiter();
	};
	const server = createServer((_req, res) => {
		res.writeHead(404);
		res.end();
	});
	server.on("upgrade", (req, socket) => {
		const upgrade = {
			path: req.url,
			protocol: req.headers["sec-websocket-protocol"] ?? "",
			authorization: req.headers.authorization ?? "",
			closed: null,
		};
		upgrades.push(upgrade);
		changed();
		if (mode === "refuse") {
			socket.end("HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
			return;
		}
		const accept = createHash("sha1")
			.update(req.headers["sec-websocket-key"] + GUID)
			.digest("base64");
		socket.write(
			`HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: ${accept}\r\nsec-websocket-protocol: ${upgrade.protocol}\r\n\r\n`,
		);
		let received = Buffer.alloc(0);
		socket.on("data", (chunk) => {
			received = Buffer.concat([received, chunk]);
			const closed = readClose(received);
			if (closed !== undefined && upgrade.closed === null) {
				upgrade.closed = closed;
				changed();
				socket.end(closeFrame(1000, ""));
			}
		});
		socket.on("error", () => {});
		socket.on("close", () => changed());
		if (mode === "close-first" && upgrades.length === 1) {
			socket.write(closeFrame(4000, "gateway restarting"));
		}
	});
	await new Promise((r) => server.listen(0, "127.0.0.1", r));
	return {
		origin: `http://127.0.0.1:${server.address().port}`,
		upgrades,
		until: async (done) => {
			while (!done()) await new Promise((r) => waiters.push(r));
		},
		close: () => new Promise((r) => server.close(r)),
	};
}

/** The provider (models.json points anthropic here): any request gets a short text reply. */
function textReply(text) {
	const events = [
		{ type: "message_start", message: { id: "msg_1", type: "message", role: "assistant", model: "claude-sonnet-4-5", content: [], stop_reason: null, usage: { input_tokens: 10, output_tokens: 1 } } },
		{ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
		{ type: "content_block_delta", index: 0, delta: { type: "text_delta", text } },
		{ type: "content_block_stop", index: 0 },
		{ type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 5 } },
		{ type: "message_stop" },
	];
	return events.map((e) => `event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`).join("");
}
const provider = createServer((req, res) => {
	req.resume();
	req.on("end", () => {
		res.writeHead(200, { "content-type": "text/event-stream" });
		res.end(textReply("ok"));
	});
});
await new Promise((r) => provider.listen(0, "127.0.0.1", r));
const providerOrigin = `http://127.0.0.1:${provider.address().port}`;

const cases = [
	{ name: "connected", gateway: "accept", args: ["--auth-token", "host-secret"], until: ["Radius: connected"] },
	{
		name: "reconnects-after-close",
		gateway: "close-first",
		args: ["--auth-token", "host-secret"],
		until: ["Radius: connected", "Radius: reconnecting: Radius relay host closed (4000: gateway restarting)", "Radius: connected"],
	},
	{ name: "refused", gateway: "refuse", args: ["--auth-token", "host-secret"], until: ["Radius: reconnecting:"] },
	{ name: "no-credential", gateway: "accept", args: [], until: ["Radius: not connected; local only"] },
	{ name: "offline", gateway: "accept", args: ["--auth-token", "t"], env: { PI_OFFLINE: "1" }, until: ["Radius: not connected; local only"] },
	{ name: "stored-key", gateway: "accept", args: [], env: { RADIUS_API_KEY: "stored-key" }, until: ["Radius: connected"] },
];

const out = [];
for (const c of cases) {
	const tmp = mkdtempSync(join(tmpdir(), "ultron-radius-server-"));
	for (const dir of ["agent", "cwd", "server"]) mkdirSync(join(tmp, dir));
	writeFileSync(join(tmp, "agent", "settings.json"), JSON.stringify({ sessionNaming: { enabled: false } }));
	writeFileSync(join(tmp, "agent", "models.json"), JSON.stringify({ providers: { anthropic: { baseUrl: providerOrigin } } }));
	const gw = await gateway(c.gateway);
	const env = {
		PATH: "/usr/bin:/bin",
		HOME: tmp,
		ULTRON_CODING_AGENT_DIR: join(tmp, "agent"),
		ULTRON_SERVER_DIR: join(tmp, "server"),
		PI_EXPERIMENTAL: "1",
		PI_RADIUS_GATEWAY: gw.origin,
		NO_COLOR: "1",
		TZ: "UTC",
		ANTHROPIC_API_KEY: "test-key",
		NODE_OPTIONS: guardNodeOptions,
		ULTRON_GUARD_LOG: join(tmp, "guard.log"),
		...(c.env ?? {}),
	};
	const child = spawn(process.execPath, [join(root, "packages/coding-agent/src/experimental/cli.ts"), "server", "--server-id", SERVER_ID, ...c.args], {
		cwd: join(tmp, "cwd"),
		env,
		stdio: ["ignore", "pipe", "pipe"],
	});
	let stdout = "";
	let stderr = "";
	const lineWaiters = [];
	child.stdout.on("data", (d) => {
		stdout += d;
		for (const waiter of lineWaiters.splice(0)) waiter();
	});
	child.stderr.on("data", (d) => (stderr += d));
	let ended = false;
	const exited = new Promise((r) =>
		child.on("close", (code, signal) => {
			ended = true;
			for (const waiter of lineWaiters.splice(0)) waiter();
			r({ code, signal });
		}),
	);
	// Wait for the expected lines (after `Server:` and `Socket:`), each a prefix, in order.
	const hasLines = () => {
		const lines = stdout.split("\n").slice(2);
		let at = 0;
		for (const want of c.until) {
			while (at < lines.length && !lines[at].startsWith(want)) at++;
			if (at >= lines.length) return false;
			at++;
		}
		return true;
	};
	const deadline = setTimeout(() => {
		throw new Error(`${c.name}: timed out; stdout so far:\n${stdout}\nstderr:\n${stderr}`);
	}, 120_000);
	while (!hasLines()) {
		if (ended) throw new Error(`${c.name}: exited early; stdout:\n${stdout}\nstderr:\n${stderr}`);
		await new Promise((r) => lineWaiters.push(r));
	}
	child.kill("SIGTERM");
	const { code, signal } = await exited;
	// A connected host sends its close frame before the process exits.
	await gw.until(() => gw.upgrades.every((u) => u.closed !== null || c.gateway === "refuse" || u !== gw.upgrades.at(-1)));
	clearTimeout(deadline);
	await gw.close();
	assertOffline(join(tmp, "guard.log"));
	const normalize = (text) => text.split(tmp).join("<TMP>");
	out.push({
		name: c.name,
		gateway: c.gateway,
		args: c.args,
		env: c.env ?? {},
		stdout: normalize(stdout),
		stderr: normalize(stderr),
		code,
		signal,
		// A refused relay retries on its own schedule: only the first attempt is recorded.
		upgrades: (c.gateway === "refuse" ? gw.upgrades.slice(0, 1) : gw.upgrades).map((u) => ({ ...u })),
	});
	rmSync(tmp, { recursive: true, force: true });
}
provider.close();
writeFileSync(new URL("./radius_server.json", import.meta.url), `${JSON.stringify(out, null, 1)}\n`);
