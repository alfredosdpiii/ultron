/**
 * A minimal Model Context Protocol server over stdio: newline-delimited JSON-RPC 2.0 (MCP "stdio" transport).
 * It serves tools, prompts and resources and honours `notifications/cancelled`; nothing else of MCP is needed
 * by `ultron mcp`. Requests are handled concurrently; responses may come back in any order.
 */
import type { Readable } from "node:stream";

export const MCP_PROTOCOL_VERSIONS = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"] as const;

export type McpContent = { type: "text"; text: string } | { type: "image"; data: string; mimeType: string };

export interface McpTool {
	readonly name: string;
	readonly title?: string;
	readonly description: string;
	readonly inputSchema: Record<string, unknown>;
	readonly annotations?: Record<string, unknown>;
}

export interface McpPrompt {
	readonly name: string;
	readonly title?: string;
	readonly description: string;
	text(): string;
}

export interface McpResource {
	readonly uri: string;
	readonly name: string;
	readonly title?: string;
	readonly description: string;
	readonly mimeType: string;
	text(): string;
}

export interface McpServerHandlers {
	readonly name: string;
	readonly version: string;
	/** Server instructions (Claude Code shows them in its system prompt under "MCP Server Instructions"). */
	instructions?(): string | undefined;
	tools(): McpTool[];
	callTool(
		name: string,
		args: Record<string, unknown>,
		signal: AbortSignal,
	): Promise<{ content: McpContent[]; isError?: boolean }>;
	prompts?(): McpPrompt[];
	resources?(): McpResource[];
	/** The client said hello (after `initialize`). */
	onInitialized?(client: { name?: string; version?: string }): void;
}

type JsonRpcId = string | number;
type Message = { jsonrpc?: unknown; id?: JsonRpcId | null; method?: unknown; params?: unknown };

const PARSE_ERROR = -32700;
const INVALID_REQUEST = -32600;
const METHOD_NOT_FOUND = -32601;
const INVALID_PARAMS = -32602;
const INTERNAL_ERROR = -32603;
/** Lines longer than this are rejected (a cell's code is well under it). */
const MAX_LINE_BYTES = 16 * 1024 * 1024;

class RpcError extends Error {
	readonly code: number;
	constructor(code: number, message: string) {
		super(message);
		this.code = code;
	}
}

function record(value: unknown): Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: {};
}

/**
 * Serve MCP on `input`/`output` until `input` ends. Resolves when the client disconnects (then running tool calls
 * are aborted).
 */
/** Where responses go: stdout in `ultron mcp` (any writer of text lines). */
export interface McpOutput {
	write(text: string): unknown;
}

export function serveMcp(handlers: McpServerHandlers, input: Readable, output: McpOutput): Promise<void> {
	const running = new Map<JsonRpcId, AbortController>();
	let client: { name?: string; version?: string } = {};
	const send = (message: Record<string, unknown>): void => {
		output.write(`${JSON.stringify({ jsonrpc: "2.0", ...message })}\n`);
	};
	const reply = (id: JsonRpcId, result: unknown): void => send({ id, result });
	const fail = (id: JsonRpcId | null, code: number, message: string): void => send({ id, error: { code, message } });

	const request = async (id: JsonRpcId, method: string, params: Record<string, unknown>): Promise<unknown> => {
		switch (method) {
			case "initialize": {
				const requested = typeof params.protocolVersion === "string" ? params.protocolVersion : undefined;
				const protocolVersion =
					requested !== undefined && (MCP_PROTOCOL_VERSIONS as readonly string[]).includes(requested)
						? requested
						: MCP_PROTOCOL_VERSIONS[0];
				const info = record(params.clientInfo);
				client = {
					...(typeof info.name === "string" ? { name: info.name } : {}),
					...(typeof info.version === "string" ? { version: info.version } : {}),
				};
				const instructions = handlers.instructions?.();
				return {
					protocolVersion,
					capabilities: {
						tools: { listChanged: false },
						...(handlers.prompts ? { prompts: { listChanged: false } } : {}),
						...(handlers.resources ? { resources: { listChanged: false, subscribe: false } } : {}),
					},
					serverInfo: { name: handlers.name, version: handlers.version },
					...(instructions ? { instructions } : {}),
				};
			}
			case "ping":
				return {};
			case "tools/list":
				return { tools: handlers.tools() };
			case "tools/call": {
				const name = params.name;
				if (typeof name !== "string") throw new RpcError(INVALID_PARAMS, "tools/call needs a tool name");
				if (!handlers.tools().some((tool) => tool.name === name))
					throw new RpcError(INVALID_PARAMS, `Unknown tool: ${name}`);
				const controller = new AbortController();
				running.set(id, controller);
				try {
					return await handlers.callTool(name, record(params.arguments), controller.signal);
				} finally {
					running.delete(id);
				}
			}
			case "prompts/list":
				return {
					prompts: (handlers.prompts?.() ?? []).map(({ name, title, description }) => ({
						name,
						...(title === undefined ? {} : { title }),
						description,
						arguments: [],
					})),
				};
			case "prompts/get": {
				const prompt = handlers.prompts?.().find((item) => item.name === params.name);
				if (!prompt) throw new RpcError(INVALID_PARAMS, `Unknown prompt: ${String(params.name)}`);
				return {
					description: prompt.description,
					messages: [{ role: "user", content: { type: "text", text: prompt.text() } }],
				};
			}
			case "resources/list":
				return {
					resources: (handlers.resources?.() ?? []).map(({ uri, name, title, description, mimeType }) => ({
						uri,
						name,
						...(title === undefined ? {} : { title }),
						description,
						mimeType,
					})),
				};
			case "resources/templates/list":
				return { resourceTemplates: [] };
			case "resources/read": {
				const resource = handlers.resources?.().find((item) => item.uri === params.uri);
				if (!resource) throw new RpcError(INVALID_PARAMS, `Unknown resource: ${String(params.uri)}`);
				return { contents: [{ uri: resource.uri, mimeType: resource.mimeType, text: resource.text() }] };
			}
			default:
				throw new RpcError(METHOD_NOT_FOUND, `Method not found: ${method}`);
		}
	};

	const onMessage = (message: Message): void => {
		const method = message.method;
		const hasId = message.id !== undefined && message.id !== null;
		if (typeof method !== "string") {
			// A response to a request we never send, or garbage: answer only what carries an id.
			if (hasId && !("result" in message) && !("error" in message))
				fail(message.id as JsonRpcId, INVALID_REQUEST, "Invalid request");
			return;
		}
		if (!hasId) {
			if (method === "notifications/cancelled") {
				const requestId = record(message.params).requestId;
				if (typeof requestId === "string" || typeof requestId === "number")
					running.get(requestId)?.abort(new DOMException("Cancelled by the client", "AbortError"));
			} else if (method === "notifications/initialized") handlers.onInitialized?.(client);
			return;
		}
		const id = message.id as JsonRpcId;
		void request(id, method, record(message.params)).then(
			(result) => reply(id, result),
			(error: unknown) =>
				fail(
					id,
					error instanceof RpcError ? error.code : INTERNAL_ERROR,
					error instanceof Error ? error.message : String(error),
				),
		);
	};

	return new Promise((resolve) => {
		let buffered = "";
		input.setEncoding?.("utf8");
		input.on("data", (chunk: string | Buffer) => {
			buffered += typeof chunk === "string" ? chunk : chunk.toString("utf8");
			if (buffered.length > MAX_LINE_BYTES && !buffered.includes("\n")) {
				buffered = "";
				fail(null, INVALID_REQUEST, "Message too large");
				return;
			}
			let newline = buffered.indexOf("\n");
			while (newline !== -1) {
				const line = buffered.slice(0, newline).trim();
				buffered = buffered.slice(newline + 1);
				newline = buffered.indexOf("\n");
				if (!line) continue;
				let message: unknown;
				try {
					message = JSON.parse(line);
				} catch {
					fail(null, PARSE_ERROR, "Parse error");
					continue;
				}
				// Batches are not part of current MCP; treat each element as a message.
				for (const item of Array.isArray(message) ? message : [message]) onMessage(record(item) as Message);
			}
		});
		const end = (): void => {
			for (const controller of running.values()) controller.abort(new Error("MCP client disconnected"));
			running.clear();
			resolve();
		};
		input.once("end", end);
		input.once("close", end);
	});
}
