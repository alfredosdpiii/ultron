// Drives the TypeScript coordinator process (experimental/coordinator.ts, started as an internal process) through a
// scripted control-socket and public-socket exchange and records, per socket, every line (or byte chunk) it received
// and whether the coordinator closed it, plus the coordinator's exit after its empty shutdown.
// crates/cli/tests/daemon_coordinator.rs runs the same script against the Rust coordinator and compares the bytes.
// Usage: node --import packages/coding-agent/src/experimental/source-resolver.ts crates/daemon/fixtures/generate-coordinator.mjs
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createConnection, createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");

/** `{TMP}` in a line stands for the run's temp dir. */
export const SCRIPT = [
	{ op: "endpoint", name: "E1" },
	{ op: "endpoint", name: "E2" },
	{ op: "connect", name: "S1", to: "control" },
	{ op: "send", name: "S1", line: { type: "register_server", protocol: 4, serverConnectionId: "s1", endpoint: "{TMP}/E1.sock" } },
	{ op: "expect", name: "S1", count: 1 },
	{ op: "connect", name: "W1", to: "control" },
	{ op: "send", name: "W1", line: { type: "register_peer", protocol: 4, peerId: "w1" } },
	{ op: "expect", name: "W1", count: 1 },
	{ op: "expect", name: "S1", count: 2 },
	{ op: "send", name: "W1", line: { type: "send", to: "server", payload: { type: "worker_ready", n: 1.5, text: "é " } } },
	{ op: "expect", name: "S1", count: 3 },
	{ op: "send", name: "S1", line: { type: "send", to: "w1", payload: { type: "shutdown" } } },
	{ op: "expect", name: "W1", count: 2 },
	{ op: "send", name: "S1", line: { type: "send", to: "nobody", payload: 1 } },
	{ op: "connect", name: "S2", to: "control" },
	{ op: "send", name: "S2", line: { type: "register_server", protocol: 4, serverConnectionId: "s2", endpoint: "{TMP}/E2.sock" } },
	{ op: "expect", name: "S2", count: 1 },
	{ op: "expect", name: "W1", count: 3 },
	{ op: "send", name: "W1", line: { type: "send", to: "server", payload: { to: "newest" } } },
	{ op: "expect", name: "S2", count: 2 },
	{ op: "send", name: "W1", line: { type: "send", to: "s1", payload: { to: "s1" } } },
	{ op: "expect", name: "S1", count: 4 },
	{ op: "send", name: "S2", line: { type: "broadcast", payload: { type: "discover_workers" } } },
	{ op: "expect", name: "W1", count: 4 },
	{ op: "connect", name: "P1", to: "public" },
	{ op: "write", name: "P1", data: "ping" },
	{ op: "expect", name: "E2", count: 1 },
	{ op: "expect", name: "P1", count: 1 },
	{ op: "connect", name: "X1", to: "control" },
	{ op: "send", name: "X1", line: { type: "register_peer", protocol: 3, peerId: "x1" } },
	{ op: "expect", name: "X1", count: 0, closed: true },
	{ op: "connect", name: "D1", to: "control" },
	{ op: "send", name: "D1", line: { type: "register_server", protocol: 4, serverConnectionId: "s1", endpoint: "{TMP}/E1.sock" } },
	{ op: "expect", name: "D1", count: 0, closed: true },
	{ op: "connect", name: "N1", to: "control" },
	{ op: "send", name: "N1", line: { type: "hello" } },
	{ op: "expect", name: "N1", count: 0, closed: true },
	{ op: "send", name: "W1", line: { type: "broadcast", payload: {} } },
	{ op: "expect", name: "W1", count: 4, closed: true },
	{ op: "expect", name: "S1", count: 5 },
	{ op: "expect", name: "S2", count: 3 },
	{ op: "close", name: "S2" },
	{ op: "close", name: "P1" },
	{ op: "close", name: "S1" },
	{ op: "exit" },
];

async function run() {
	const tmp = mkdtempSync(join(tmpdir(), "ultron-coord-"));
	const publicPath = join(tmp, "public.sock"), controlPath = join(tmp, "control.sock");
	const child = spawn(process.execPath, [...process.execArgv, join(root, "packages/coding-agent/src/experimental/coordinator-entry.ts"), publicPath, controlPath], {
		env: { ...process.env, __PI_INTERNAL_SPAWN: "coordinator" },
		stdio: "ignore",
	});
	const exited = new Promise((r) => child.once("exit", (code) => r(code)));
	while (!existsSync(publicPath)) await new Promise((r) => setTimeout(r, 10));
	const sockets = new Map();
	const received = new Map();
	const closed = new Map();
	const record = (name) => {
		received.set(name, []);
		closed.set(name, false);
	};
	const waitFor = async (check) => {
		const deadline = Date.now() + 5000;
		while (!check()) {
			if (Date.now() > deadline) throw new Error("timed out");
			await new Promise((r) => setTimeout(r, 5));
		}
	};
	for (const step of SCRIPT) {
		const name = step.name;
		if (step.op === "endpoint") {
			record(name);
			const server = createServer((socket) => socket.on("data", (d) => {
				received.get(name).push(d.toString());
				socket.write(`pong:${d}`);
			}));
			await new Promise((r) => server.listen(join(tmp, `${name}.sock`), r));
			sockets.set(name, server);
		} else if (step.op === "connect") {
			record(name);
			const socket = createConnection(step.to === "public" ? publicPath : controlPath);
			await new Promise((r) => socket.once("connect", r));
			let buffered = "";
			socket.on("data", (d) => {
				if (step.to === "public") {
					received.get(name).push(d.toString());
					return;
				}
				buffered += d.toString();
				let newline;
				while ((newline = buffered.indexOf("\n")) !== -1) {
					received.get(name).push(buffered.slice(0, newline + 1));
					buffered = buffered.slice(newline + 1);
				}
			});
			socket.on("close", () => closed.set(name, true));
			socket.on("error", () => {});
			sockets.set(name, socket);
		} else if (step.op === "send") {
			sockets.get(name).write(`${JSON.stringify(step.line).split("{TMP}").join(tmp)}\n`);
		} else if (step.op === "write") {
			sockets.get(name).write(step.data);
		} else if (step.op === "expect") {
			await waitFor(() => received.get(name).length >= step.count && (!step.closed || closed.get(name)));
		} else if (step.op === "close") {
			sockets.get(name).destroy();
		} else if (step.op === "exit") {
			const code = await exited;
			received.set("exit", [String(code)]);
			received.set("sockets", [String(existsSync(publicPath)), String(existsSync(controlPath))]);
		}
	}
	for (const [name, socket] of sockets) if (socket.close) socket.close();
	rmSync(tmp, { recursive: true, force: true });
	const out = {};
	for (const [name, lines] of received) out[name] = { lines: lines.map((l) => l.split(tmp).join("<TMP>")), closed: closed.get(name) ?? null };
	return out;
}

const result = await run();
writeFileSync(join(here, "coordinator.json"), `${JSON.stringify({ script: SCRIPT, received: result }, null, 1)}\n`);
