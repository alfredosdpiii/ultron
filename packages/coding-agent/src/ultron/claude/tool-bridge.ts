/**
 * The MCP bridge between a `claude -p` lane process and the session worker that runs the lane (`ultron --claude`,
 * see root-runner.ts). The worker listens on an owner-only Unix socket; Claude Code starts `ultron mcp --bridge
 * <socket> --token <token>` as its only MCP server, which lists the lane's tools and forwards each `tools/call`
 * (with Claude Code's tool-use id) to the worker, where the call waits for the harness to run it.
 *
 * Protocol: the control socket's newline-delimited JSON (`{"id", "op", ...}` -> `{"id", "ok", "result"|"error"}`),
 * ops `tools` and `call`, each naming the lane process by its token.
 */
import { randomBytes } from "node:crypto";
import { chmodSync, rmSync } from "node:fs";
import { createServer, type Server, type Socket } from "node:net";
import { join } from "node:path";
import { VERSION } from "../../config.ts";
import { connectControl, controlDir } from "./control-socket.ts";
import { type McpContent, serveMcp } from "./mcp-protocol.ts";
import type { BridgeResult, LaneBridge } from "./root-runner.ts";

const MAX_LINE = 64 * 1024 * 1024;
/** How long the bridge waits for one call's result (the CLI's own MCP timeout is set to two hours). */
const CALL_TIMEOUT_MS = 3 * 60 * 60 * 1000;

export interface BridgeServer {
	readonly socket: string;
	register(token: string, lane: LaneBridge): () => void;
	close(): Promise<void>;
}

/** Listen for bridges of this worker's lane processes. */
export async function startBridgeServer(dir = controlDir()): Promise<BridgeServer> {
	const socketPath = join(dir, `w-${process.pid}-${randomBytes(4).toString("hex")}.sock`);
	rmSync(socketPath, { force: true });
	const lanes = new Map<string, LaneBridge>();
	const connections = new Set<Socket>();
	const handle = async (request: Record<string, unknown>, signal: AbortSignal): Promise<unknown> => {
		const lane = typeof request.token === "string" ? lanes.get(request.token) : undefined;
		if (!lane) throw new Error("unknown or finished Claude Code lane");
		if (request.op === "tools") return lane.tools();
		if (request.op === "call") {
			const args =
				typeof request.args === "object" && request.args !== null ? (request.args as Record<string, unknown>) : {};
			return lane.call(
				typeof request.toolUseId === "string" ? request.toolUseId : undefined,
				String(request.name ?? ""),
				args,
				signal,
			);
		}
		throw new Error(`unknown bridge op: ${String(request.op)}`);
	};
	const server: Server = createServer((socket) => {
		connections.add(socket);
		// A bridge that goes away (Claude Code exited) cancels its open calls.
		const closed = new AbortController();
		socket.on("close", () => {
			connections.delete(socket);
			closed.abort();
		});
		socket.on("error", () => socket.destroy());
		socket.setEncoding("utf8");
		let buffered = "";
		socket.on("data", (chunk: string) => {
			buffered += chunk;
			if (buffered.length > MAX_LINE) {
				socket.destroy();
				return;
			}
			let newline = buffered.indexOf("\n");
			while (newline !== -1) {
				const line = buffered.slice(0, newline);
				buffered = buffered.slice(newline + 1);
				newline = buffered.indexOf("\n");
				if (!line.trim()) continue;
				let request: Record<string, unknown>;
				try {
					request = JSON.parse(line) as Record<string, unknown>;
				} catch {
					continue;
				}
				void handle(request, closed.signal).then(
					(result) => {
						if (!socket.destroyed) socket.write(`${JSON.stringify({ id: request.id, ok: true, result })}\n`);
					},
					(error: unknown) => {
						if (!socket.destroyed)
							socket.write(
								`${JSON.stringify({ id: request.id, ok: false, error: error instanceof Error ? error.message : String(error) })}\n`,
							);
					},
				);
			}
		});
	});
	const oldMask = process.umask(0o177);
	try {
		await new Promise<void>((resolve, reject) => {
			server.once("error", reject);
			server.listen(socketPath, () => resolve());
		});
	} finally {
		process.umask(oldMask);
	}
	chmodSync(socketPath, 0o600);
	server.unref();
	const removeSocket = () => rmSync(socketPath, { force: true });
	process.once("exit", removeSocket);
	let closing: Promise<void> | undefined;
	return {
		socket: socketPath,
		register: (token, lane) => {
			lanes.set(token, lane);
			return () => {
				if (lanes.get(token) === lane) lanes.delete(token);
			};
		},
		close: () => {
			closing ??= new Promise<void>((resolve) => {
				for (const socket of connections) socket.destroy();
				server.close(() => resolve());
				removeSocket();
				process.off("exit", removeSocket);
			});
			return closing;
		},
	};
}

/** `ultron mcp --bridge <socket> --token <token>`: serve one lane's tools to Claude Code over stdio. */
export async function runMcpBridge(socket: string, token: string): Promise<void> {
	const protocolWrite = process.stdout.write.bind(process.stdout);
	// Stdout carries only JSON-RPC.
	process.stdout.write = process.stderr.write.bind(process.stderr) as typeof process.stdout.write;
	console.log = console.error;
	const control = await connectControl(socket, 5_000);
	const tools = (await control.request("tools", { token }, 30_000)) as Array<{
		name: string;
		description: string;
		inputSchema: Record<string, unknown>;
	}>;
	await serveMcp(
		{
			name: "ultron",
			version: VERSION,
			tools: () => tools,
			callTool: async (name, args, _signal, meta) => {
				const toolUseId = meta?.["claudecode/toolUseId"];
				try {
					const result = (await control.request(
						"call",
						{ token, name, args, ...(typeof toolUseId === "string" ? { toolUseId } : {}) },
						CALL_TIMEOUT_MS,
					)) as BridgeResult;
					return { content: result.content as McpContent[], ...(result.isError ? { isError: true } : {}) };
				} catch (error) {
					return {
						content: [{ type: "text", text: error instanceof Error ? error.message : String(error) }],
						isError: true,
					};
				}
			},
		},
		process.stdin,
		{ write: (text: string) => protocolWrite(text) },
	);
	control.close();
	process.exit(0);
}
