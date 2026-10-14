// Generates crates/mcp/fixtures/ordering.json: the TypeScript gateway (extensions/mcp/index.ts) over a scripted
// transport (createTransport), forcing each order of a server's death against the calls and the tool-list refresh:
//   detected:   the transport closed before the next call: the call reconnects, and the restart fails;
//   undetected: the server is dead but its close has not been seen: the call reads the cached tool list;
//   refresh:    the server dies while answering the refresh that tools/list_changed started.
// Usage: node crates/mcp/fixtures/generate-ordering.mjs   (from the repository root)
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const base = mkdtempSync(join(tmpdir(), "mcp-ordering-fixture-"));
process.env.ULTRON_CODING_AGENT_DIR = join(base, "agent");
process.env.ULTRON_SERVER_DIR = join(base, "server");
const imp = (p) => import(pathToFileURL(join(root, p)).href);
const { createMcpExtension } = await imp("packages/coding-agent/src/extensions/mcp/index.ts");
const { TransportEvents } = await imp("packages/mcp/src/transports/transport.ts");

/** A fake server behind a transport: answers initialize and tools/list; `die()` is the close the client sees. */
class Scripted extends TransportEvents {
	constructor(world) {
		super();
		this.world = world;
	}
	async start() {
		if (this.world.restartFails) throw new Error("spawn gw ENOENT");
		this.world.live = this;
	}
	async send(message) {
		if (message.id === undefined) return;
		if (message.method === "tools/list" && this.world.dieOnList) {
			this.world.dieOnList = false;
			queueMicrotask(() => this.die());
			return;
		}
		const result =
			message.method === "initialize"
				? { protocolVersion: message.params.protocolVersion, capabilities: { tools: { listChanged: true } }, serverInfo: { name: "gw", version: "1" } }
				: message.method === "tools/list"
					? { tools: this.world.tools.map((name) => ({ name, inputSchema: { type: "object" } })) }
					: {};
		queueMicrotask(() => this.emitMessage({ jsonrpc: "2.0", id: message.id, result }));
	}
	async close() {
		this.die();
	}
	die() {
		this.emitClose();
	}
	notify(method) {
		this.emitMessage({ jsonrpc: "2.0", method });
	}
}

const tick = () => new Promise((r) => setTimeout(r, 10));

async function scenario(name, steps) {
	const world = { tools: ["ping"], restartFails: false, dieOnList: false, live: undefined };
	let tool;
	createMcpExtension({
		servers: [{ name: "srv", config: { command: "gw" }, source: "test" }],
		cwd: base,
		agentDir: base,
		projectTrusted: false,
		createTransport: () => new Scripted(world),
	})({ registerTool: (t) => (tool = t), registerCommand: () => {}, on: () => {} });
	const events = [];
	const call = async (params) => {
		const result = await tool.execute("c", params, undefined);
		return { text: result.content.map((c) => c.text).join("\n"), details: result.details };
	};
	for (const step of steps) {
		if (step.op === "call") events.push({ ...step, ...(await call(step.params)) });
		else if (step.op === "die") {
			world.live.die();
			await tick();
			events.push(step);
		} else if (step.op === "dieUnseen") events.push(step);
		else if (step.op === "restartFails") {
			world.restartFails = true;
			events.push(step);
		} else if (step.op === "changeAndDieOnRefresh") {
			world.dieOnList = true;
			world.live.notify("notifications/tools/list_changed");
			for (let i = 0; i < 200 && (await call({})).details.servers[0].status !== "disconnected"; i++) await tick();
			await tick();
			events.push(step);
		}
	}
	return { name, events };
}

const call = (params) => ({ op: "call", params });
const scenarios = [
	await scenario("detected", [
		call({ server: "srv" }),
		{ op: "restartFails" },
		{ op: "die" },
		call({}),
		call({ describe: "srv_x" }),
		call({}),
	]),
	await scenario("undetected", [
		call({ server: "srv" }),
		{ op: "restartFails" },
		{ op: "dieUnseen" },
		call({ describe: "srv_x" }),
		call({ describe: "srv_ping" }),
		{ op: "die" },
		call({ describe: "srv_x" }),
	]),
	await scenario("refresh", [call({ server: "srv" }), { op: "changeAndDieOnRefresh" }, call({}), call({ server: "srv" })]),
];
rmSync(base, { recursive: true, force: true });
writeFileSync(join(here, "ordering.json"), `${JSON.stringify({ scenarios }, null, "\t")}\n`);
process.exit(0);
