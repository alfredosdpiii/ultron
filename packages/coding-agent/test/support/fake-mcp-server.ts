/**
 * A fake MCP server on the MCP package's in-memory transport: enough of the protocol (initialize, tools, resources,
 * cancellation) for the real client in `@ultron/mcp` to connect to it, with no process and no network.
 */

import type { McpTransport } from "@ultron/mcp";
import { createInMemoryTransportPair } from "@ultron/mcp/testing";

export interface FakeMcpTool {
	name: string;
	description: string;
	inputSchema: Record<string, unknown>;
	/** The tool's result, sent as JSON text. A thrown error becomes an `isError` result. */
	run(args: Record<string, unknown>, signal: AbortSignal): Promise<unknown>;
}

export interface FakeMcpServer {
	tools: FakeMcpTool[];
	instructions?: string;
	/** Text resources by URI. */
	resources?: Record<string, string>;
}

type Message = { id?: number | string; method?: string; params?: Record<string, unknown> };

/** The client side of a new connection to `server`. Every call is a fresh connection. */
export function connectFakeMcpServer(name: string, server: FakeMcpServer): McpTransport {
	const pair = createInMemoryTransportPair();
	const running = new Map<number | string, AbortController>();
	const reply = (id: number | string, result: unknown) =>
		void pair.server.send({ jsonrpc: "2.0", id, result } as never);
	const fail = (id: number | string, code: number, message: string) =>
		void pair.server.send({ jsonrpc: "2.0", id, error: { code, message } } as never);

	const handle = async (message: Message): Promise<void> => {
		const { id, method, params = {} } = message;
		if (method === "notifications/cancelled") {
			running.get(params.requestId as number | string)?.abort();
			return;
		}
		if (id === undefined || method === undefined) return;
		switch (method) {
			case "initialize":
				return reply(id, {
					protocolVersion: params.protocolVersion,
					capabilities: { tools: {}, ...(server.resources ? { resources: {} } : {}) },
					serverInfo: { name, version: "1.0.0" },
					...(server.instructions ? { instructions: server.instructions } : {}),
				});
			case "ping":
				return reply(id, {});
			case "tools/list":
				return reply(id, {
					tools: server.tools.map(({ name, description, inputSchema }) => ({ name, description, inputSchema })),
				});
			case "tools/call": {
				const tool = server.tools.find((candidate) => candidate.name === params.name);
				if (!tool) return fail(id, -32602, `Unknown tool: ${String(params.name)}`);
				const controller = new AbortController();
				running.set(id, controller);
				try {
					const value = await tool.run((params.arguments ?? {}) as Record<string, unknown>, controller.signal);
					if (!controller.signal.aborted) reply(id, { content: [{ type: "text", text: JSON.stringify(value) }] });
				} catch (error) {
					if (!controller.signal.aborted) {
						reply(id, { isError: true, content: [{ type: "text", text: `Error: ${(error as Error).message}` }] });
					}
				} finally {
					running.delete(id);
				}
				return;
			}
			case "resources/list":
				return reply(id, {
					resources: Object.keys(server.resources ?? {}).map((uri) => ({
						uri,
						name: uri,
						mimeType: "text/plain",
					})),
				});
			case "resources/templates/list":
				return reply(id, { resourceTemplates: [] });
			case "resources/read": {
				const body = server.resources?.[String(params.uri)];
				if (body === undefined) return fail(id, -32002, `Resource not found: ${String(params.uri)}`);
				return reply(id, { contents: [{ uri: params.uri, mimeType: "text/plain", text: body }] });
			}
			default:
				return fail(id, -32601, `Method not found: ${method}`);
		}
	};

	pair.server.onMessage((message) => void handle(message as Message));
	pair.server.onClose(() => {
		for (const controller of running.values()) controller.abort();
	});
	void pair.server.start();
	return pair.client;
}
