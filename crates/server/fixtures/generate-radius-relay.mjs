// Generates crates/server/fixtures/radius-relay.json by driving the TypeScript Radius relay
// (packages/coding-agent/src/experimental/radius-relay.ts) through scripted gateways: the host (`RadiusRelayHost`) with
// a fake WebSocket factory and a fake `server.accept`, the client byte transport (`createRadiusClientTransportFactory`)
// and the reconnector (`RadiusClientReconnect`) with a fake client. Timers fire at once and their delays are recorded.
// Each scenario records what the far side would see: every socket opened (URL, subprotocol, authorization), every
// message the relay sent on it (text, or binary as hex) and the close it sent; plus statuses, the server's accepted
// connections and their handler events, and errors. tests/radius_relay.rs (and crates/client/tests/radius.rs) replay
// the same scripts against a fake gateway on 127.0.0.1 and must record the same.
// Usage: node crates/server/fixtures/generate-radius-relay.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

for (const name of Object.keys(process.env)) {
	if (/^(PI_|ULTRON_|RADIUS)/.test(name)) delete process.env[name];
}
const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const relay = await import(pathToFileURL(join(root, "packages/coding-agent/src/experimental/radius-relay.ts")).href);
const { RadiusRelayHost, createRadiusClientTransportFactory, RadiusClientReconnect, encodeRelayDataFrame, parseRelayDataFrame } = relay;

// Timers fire on the next turn; the delays asked for are recorded.
let delays = [];
const realSetTimeout = globalThis.setTimeout;
globalThis.setTimeout = (fn, ms, ...args) => {
	if (ms !== 5) delays.push(ms);
	const handle = realSetTimeout(fn, 0, ...args);
	return handle;
};
const settle = async () => {
	for (let i = 0; i < 8; i++) {
		await new Promise((resolve) => realSetTimeout(resolve, 2));
		for (let j = 0; j < 5; j++) await new Promise((resolve) => setImmediate(resolve));
	}
};

const SERVER_ID = "00000000-0000-4000-8000-000000000001";
const C1 = "00000000-0000-4000-8000-000000000002";
const C2 = "00000000-0000-4000-8000-0000000000a3";
const hex = (bytes) => Buffer.from(bytes).toString("hex");
const unhex = (text) => new Uint8Array(Buffer.from(text, "hex"));
const frame = (id, payloadHex) => hex(new Uint8Array(encodeRelayDataFrame(id, unhex(payloadHex))));

class FakeWebSocket {
	constructor(record) {
		this.record = record;
		this.listeners = new Map();
		this.binaryType = "blob";
		this.bufferedAmount = 0;
		this.protocol = "";
		this.readyState = 0;
		this.OPEN = 1;
	}
	addEventListener(type, listener) {
		const set = this.listeners.get(type) ?? new Set();
		set.add(listener);
		this.listeners.set(type, set);
	}
	removeEventListener(type, listener) {
		this.listeners.get(type)?.delete(listener);
	}
	send(data) {
		if (this.readyState !== this.OPEN) throw new Error("socket is not open");
		this.record.sent.push(typeof data === "string" ? { text: data } : { binary: hex(new Uint8Array(data)) });
	}
	close(code = 1000, reason = "") {
		if (this.readyState === 3) return;
		this.readyState = 3;
		if (!this.record.failed) this.record.closed = { code, reason };
		// undici reports the closure after the closing handshake, never inside close().
		setImmediate(() => this.emit("close", { code, reason }));
	}
	emit(type, event) {
		for (const listener of [...(this.listeners.get(type) ?? [])]) listener(event);
	}
}

function gateway() {
	const sockets = [];
	const pending = [];
	const factory = (options) => {
		const url = new URL(options.url);
		const record = { scheme: url.protocol, path: url.pathname, protocol: options.protocol, authorization: options.authorization, sent: [], closed: null };
		const socket = new FakeWebSocket(record);
		sockets.push({ socket, record });
		pending.push({ socket, record });
		return socket;
	};
	return { sockets, pending, factory };
}

/** Run gateway `steps` on the sockets the relay opens; `server` is the relay's far side for server-side steps. */
async function runSteps(steps, gw, server) {
	let current = gw.current;
	for (const step of steps) {
		if (step.connect) {
			await settle();
			const next = gw.pending.shift();
			if (!next) throw new Error(`the relay did not open a socket; delays ${JSON.stringify(delays)} sockets ${gw.sockets.length}`);
			current = next;
			gw.current = next;
			if (step.connect.fail) {
				current.record.failed = true;
				current.socket.emit("error", { error: new Error(""), message: "" });
			} else {
				current.socket.protocol = current.record.protocol;
				current.socket.readyState = 1;
				current.socket.emit("open", {});
			}
		} else if (step.text !== undefined) {
			current.socket.emit("message", { data: step.text });
		} else if (step.binary !== undefined) {
			current.socket.emit("message", { data: unhex(step.binary).buffer });
		} else if (step.close) {
			current.socket.readyState = 3;
			current.socket.emit("close", { code: step.close[0], reason: step.close[1] });
		} else if (step.drop) {
			current.socket.readyState = 3;
			current.socket.emit("error", { error: new Error(""), message: "" });
			current.socket.emit("close", { code: 1006, reason: "" });
		} else if (step.serverSend) {
			await server.connections[step.serverSend[0]].send(unhex(step.serverSend[1])).catch((e) => server.events.push({ sendError: e.message }));
		} else if (step.serverClose) {
			const [index, final] = step.serverClose;
			await server.connections[index].close(final === null ? undefined : unhex(final));
		} else if (step.sync) {
			current.socket.emit("message", { data: JSON.stringify({ version: 1, type: "ping" }) });
		}
		await settle();
	}
}

const ping = { sync: true };
const control = (type, id, extra = {}) => ({ text: JSON.stringify({ version: 1, type, connection_id: id, ...extra }) });

// ------------------------------------------------------------------------------------------------------- the host

async function runHost(scenario) {
	delays = [];
	const gw = gateway();
	const statuses = [];
	const authResults = [...scenario.auth];
	const server = { connections: [], events: [], accepted: [] };
	const host = new RadiusRelayHost({
		serverId: SERVER_ID,
		server: {
			accept(connection) {
				const index = server.connections.length;
				server.connections.push(connection);
				server.accepted.push(index);
				if (scenario.refuse) void connection.close();
				return {
					onData: (chunk) => server.events.push({ connection: index, data: hex(chunk) }),
					onClose: () => server.events.push({ connection: index, close: true }),
					onError: (error) => server.events.push({ connection: index, error: error.message }),
				};
			},
		},
		auth: {
			resolve({ signal }) {
				const next = authResults.shift();
				if (next === undefined) {
					return new Promise((_, reject) => signal.addEventListener("abort", () => reject(new Error("aborted")), { once: true }));
				}
				if (next.error) return Promise.reject(new Error(next.error));
				if (next.none) return Promise.resolve(undefined);
				return Promise.resolve({ gateway: next.gateway ?? "https://radius.example.test", token: next.token });
			},
		},
		webSocketFactory: gw.factory,
		onStatus: (status) => statuses.push(status.status === "retrying" ? { status: status.status, error: status.error } : { status: status.status }),
	});
	host.start();
	await runSteps(scenario.steps, gw, server);
	await host.close();
	await settle();
	const lines = [];
	let previous = "";
	for (const status of statuses) {
		const description =
			status.status === "connected"
				? "connected"
				: status.status === "not_authenticated"
					? "not connected; local only"
					: status.status === "retrying"
						? `reconnecting: ${status.error}`
						: "connecting";
		if (description === previous || status.status === "connecting") continue;
		previous = description;
		lines.push(`Radius: ${description}`);
	}
	return {
		name: scenario.name,
		kind: "host",
		auth: scenario.auth,
		refuse: scenario.refuse ?? false,
		steps: scenario.steps,
		sockets: gw.sockets.slice(0, scenario.steps.filter((s) => s.connect).length).map(({ record }) => ({ scheme: record.scheme, path: record.path, protocol: record.protocol, authorization: record.authorization, sent: record.sent, closed: record.closed })),
		statuses,
		statusLines: lines,
		delays,
		accepted: server.accepted,
		events: server.events,
	};
}

const token = (t) => ({ token: t });
const hostScenarios = [
	{
		name: "host/bridges-multiplexed-clients",
		auth: [token("secret")],
		steps: [
			{ connect: {} },
			control("connection_open", C1),
			{ binary: frame(C1, "010203") },
			control("connection_open", C2, { code: 1000 }),
			{ binary: frame(C2, "") },
			{ serverSend: [0, "040506"] },
			{ serverSend: [1, "ff"] },
			ping,
			control("connection_close", C1),
			{ serverSend: [0, "07"] },
			{ serverClose: [1, "0809"] },
			ping,
		],
	},
	{
		name: "host/ping-pong-and-ignored-pong",
		auth: [token("t")],
		steps: [{ connect: {} }, { text: JSON.stringify({ version: 1, type: "pong" }) }, ping, ping],
	},
	{
		name: "host/data-for-unknown-connection",
		auth: [token("t")],
		steps: [{ connect: {} }, { binary: frame(C1, "aa") }, ping],
	},
	{
		name: "host/refused-connection",
		auth: [token("t")],
		refuse: true,
		steps: [{ connect: {} }, control("connection_open", C1), { binary: frame(C1, "bb") }, ping],
	},
	{
		name: "host/server-closes-without-final-chunk",
		auth: [token("t")],
		steps: [{ connect: {} }, control("connection_open", C1), { serverClose: [0, null] }, { serverClose: [0, null] }, ping],
	},
	...[
		["invalid-json", { text: "{nope" }],
		["array", { text: "[1]" }],
		["version-2", { text: JSON.stringify({ version: 2, type: "ping" }) }],
		["unknown-type", { text: JSON.stringify({ version: 1, type: "hello" }) }],
		["bad-connection-id", control("connection_open", "00000000-0000-1000-8000-000000000002")],
		["uppercase-connection-id", control("connection_open", "00000000-0000-4000-8000-00000000000A")],
		["code-out-of-range", control("connection_close", C1, { code: 999 })],
		["code-not-integer", control("connection_close", C1, { code: 1000.5 })],
		["short-binary", { binary: "0101" }],
		["binary-wrong-version", { binary: `02${frame(C1, "00").slice(2)}` }],
	].map(([name, step]) => ({
		name: `host/protocol-error-${name}`,
		auth: [token("t"), token("t2")],
		steps: [{ connect: {} }, control("connection_open", C1), step, { connect: {} }, ping],
	})),
	{
		name: "host/reused-connection-id",
		auth: [token("t"), token("t2")],
		steps: [{ connect: {} }, control("connection_open", C1), control("connection_open", C1), { connect: {} }, ping],
	},
	{
		name: "host/remote-close-normal",
		auth: [token("t"), token("t2")],
		steps: [{ connect: {} }, control("connection_open", C1), { close: [1000, "bye"] }, { connect: {} }, ping],
	},
	{
		name: "host/remote-close-abnormal-with-reason",
		auth: [token("t"), token("t2")],
		steps: [{ connect: {} }, control("connection_open", C1), { close: [4003, "evicted"] }, { connect: {} }, ping],
	},
	{
		name: "host/remote-close-without-reason",
		auth: [token("t"), token("t2")],
		steps: [{ connect: {} }, { close: [1011, ""] }, { connect: {} }, ping],
	},
	{
		name: "host/connection-dropped",
		auth: [token("t"), token("t2")],
		steps: [{ connect: {} }, control("connection_open", C1), { drop: true }, { connect: {} }, ping],
	},
	{
		name: "host/backoff-doubles-then-resets",
		auth: [token("a"), token("b"), token("c"), token("d"), token("e")],
		steps: [{ connect: { fail: true } }, { connect: { fail: true } }, { connect: { fail: true } }, { connect: {} }, { close: [1001, "going"] }, { connect: {} }, ping],
	},
	{
		name: "host/missing-auth-then-token",
		auth: [{ none: true }, { none: true }, token("late")],
		steps: [{ connect: {} }, ping],
	},
	{
		name: "host/auth-error-retries",
		auth: [{ error: "Radius authentication token must not be empty" }, token("ok")],
		steps: [{ connect: {} }, ping],
	},
	{
		name: "host/unsupported-gateway-protocol",
		auth: [{ gateway: "ftp://radius.example.test", token: "t" }, { gateway: "http://127.0.0.1:9", token: "t2" }],
		steps: [{ connect: {} }, ping],
	},
	{
		name: "host/gateway-with-path",
		auth: [{ gateway: "https://radius.example.test/base/", token: "t" }],
		steps: [{ connect: {} }, ping],
	},
];

// ------------------------------------------------------------------------------------------ the client transport

async function runClientTransport(scenario) {
	delays = [];
	const gw = gateway();
	const events = [];
	const opening = createRadiusClientTransportFactory({
		serverId: SERVER_ID,
		auth: { resolve: async () => ({ gateway: "https://radius.example.test", token: "secret" }) },
		webSocketFactory: gw.factory,
	})({
		onData: (chunk) => events.push({ data: hex(chunk) }),
		onClose: () => events.push({ close: true }),
		onError: (error) => events.push({ error: error.message }),
	});
	let transport;
	let openError = null;
	opening.then(
		(t) => {
			transport = t;
		},
		(e) => {
			openError = e.message;
		},
	);
	for (const step of scenario.steps) {
		if (step.clientSend) {
			await transport.send(unhex(step.clientSend)).catch((e) => events.push({ sendError: e.message }));
			await settle();
		} else if (step.clientClose) {
			transport.close();
			await settle();
		} else {
			await runSteps([step], gw, null);
		}
	}
	await settle();
	return {
		name: scenario.name,
		kind: "client",
		steps: scenario.steps,
		sockets: gw.sockets.map(({ record }) => ({ scheme: record.scheme, path: record.path, protocol: record.protocol, authorization: record.authorization, sent: record.sent, closed: record.closed })),
		openError,
		events,
	};
}

const clientScenarios = [
	{
		name: "client/data-both-ways-then-remote-close",
		steps: [{ connect: {} }, { clientSend: "010203" }, { binary: "040506" }, { binary: "" }, { close: [1000, ""] }, { clientSend: "07" }],
	},
	{ name: "client/abnormal-closure-is-an-error", steps: [{ connect: {} }, { drop: true }] },
	{ name: "client/text-message-is-a-transport-error", steps: [{ connect: {} }, { text: "hello" }, { binary: "01" }] },
	{ name: "client/local-close", steps: [{ connect: {} }, { clientClose: true }, { clientSend: "01" }] },
	{ name: "client/handshake-failure", steps: [{ connect: { fail: true } }] },
	{ name: "client/remote-close-with-code", steps: [{ connect: {} }, { close: [4004, "gone"] }] },
];

// ---------------------------------------------------------------------------------------------- the reconnector

async function runReconnect(scenario) {
	delays = [];
	const log = [];
	const connectionListeners = new Set();
	const attachmentListeners = new Set();
	const outcomes = [...scenario.reconnects];
	const reattachOutcomes = [...(scenario.reattaches ?? [])];
	const client = {
		connected: true,
		connectionState: "connected",
		attachment: scenario.attached === null ? undefined : { sessionId: scenario.attached },
		onConnectionStateChange(listener) {
			connectionListeners.add(listener);
			return () => connectionListeners.delete(listener);
		},
		onAttachmentChange(listener) {
			attachmentListeners.add(listener);
			return () => attachmentListeners.delete(listener);
		},
		async reconnect() {
			const outcome = outcomes.shift() ?? "ok";
			log.push("reconnect");
			if (outcome !== "ok") throw new Error(outcome);
			this.connected = true;
			this.connectionState = "connected";
			for (const listener of [...connectionListeners]) listener({ state: "connected" });
			return { serverId: SERVER_ID };
		},
		disconnect(reason) {
			log.push(`disconnect: ${reason ?? ""}`);
			this.connected = false;
			this.connectionState = "disconnected";
			for (const listener of [...connectionListeners]) listener({ state: "disconnected" });
		},
	};
	const reconnect = new RadiusClientReconnect(client, async (sessionId) => {
		log.push(`reattach ${sessionId}`);
		const outcome = reattachOutcomes.shift() ?? "ok";
		if (outcome !== "ok") throw new Error(outcome);
	});
	for (const step of scenario.steps) {
		if (step === "disconnect") client.disconnect("lost");
		else if (step.attach !== undefined) {
			client.attachment = step.attach === null ? undefined : { sessionId: step.attach };
			for (const listener of [...attachmentListeners]) listener(client.attachment);
		} else if (step === "dispose") await reconnect.dispose();
		await settle();
	}
	await reconnect.dispose();
	await settle();
	return { name: scenario.name, kind: "reconnect", attached: scenario.attached, reconnects: scenario.reconnects, reattaches: scenario.reattaches ?? [], steps: scenario.steps, log, delays };
}

const reconnectScenarios = [
	{ name: "reconnect/retries-then-restores-session", attached: "demo-1", reconnects: ["temporary failure", "ok"], steps: ["disconnect"] },
	{ name: "reconnect/backoff-caps-at-thirty-seconds", attached: "s", reconnects: ["e1", "e2", "e3", "e4", "e5", "e6", "ok"], steps: ["disconnect"] },
	{ name: "reconnect/reattach-failure-disconnects-and-retries", attached: "s", reconnects: ["ok", "ok"], reattaches: ["session gone", "ok"], steps: ["disconnect"] },
	{ name: "reconnect/follows-the-latest-attachment", attached: "a", reconnects: ["ok"], steps: [{ attach: "b" }, "disconnect"] },
	{ name: "reconnect/detach-while-connected-forgets-the-session", attached: "a", reconnects: ["ok"], steps: [{ attach: null }, "disconnect"] },
	{ name: "reconnect/no-session", attached: null, reconnects: ["ok"], steps: ["disconnect"] },
	{ name: "reconnect/dispose-disconnects-a-connected-client", attached: "a", reconnects: [], steps: ["dispose"] },
];

const scenarios = [];
for (const scenario of hostScenarios) scenarios.push(await runHost(scenario));
for (const scenario of clientScenarios) scenarios.push(await runClientTransport(scenario));
for (const scenario of reconnectScenarios) scenarios.push(await runReconnect(scenario));
assertOffline();
const out = join(here, "radius-relay.json");
writeFileSync(out, `${JSON.stringify({ generator: "crates/server/fixtures/generate-radius-relay.mjs", serverId: SERVER_ID, scenarios }, null, "\t")}\n`);
console.log(`wrote ${scenarios.length} scenarios to ${out}`);
