/**
 * A fake Pi extension for extension-tool tests. It registers:
 * - `mcp`: Ultron's real MCP gateway (src/extensions/mcp), connected to two fake MCP servers ("exa-agent" and
 *   "docs") over the MCP package's in-memory transport (fake-mcp-server.ts): no process, no network. Loaded as an
 *   extension file it replaces the worker's built-in MCP extension, which would read the profile's mcp.json;
 * - `probe`: echoes its arguments (and repeats text, for output bounding);
 * - `slow`: waits `ms` milliseconds, honoring the abort signal;
 * - `boom`: throws.
 * Every execution is recorded on `globalThis.__fakeExtensionCalls`.
 */

import type { ExtensionAPI } from "../../src/core/extensions/types.ts";
import type { McpServerEntry } from "../../src/extensions/mcp/config.ts";
import { createMcpExtension } from "../../src/extensions/mcp/index.ts";
import { connectFakeMcpServer, type FakeMcpTool } from "./fake-mcp-server.ts";

type FakeTool = FakeMcpTool;

export interface FakeCall {
	tool: string;
	params: Record<string, unknown>;
	aborted?: boolean;
}

function calls(): FakeCall[] {
	const global = globalThis as { __fakeExtensionCalls?: FakeCall[] };
	global.__fakeExtensionCalls ??= [];
	return global.__fakeExtensionCalls;
}

function sleep(ms: number, signal: AbortSignal | undefined): Promise<void> {
	return new Promise((resolve, reject) => {
		if (signal?.aborted) return reject(new Error("aborted"));
		const timer = setTimeout(resolve, ms);
		signal?.addEventListener(
			"abort",
			() => {
				clearTimeout(timer);
				reject(new Error("aborted"));
			},
			{ once: true },
		);
	});
}

let runs = 0;
const queries = new Map<string, string>();

const SERVERS: Record<string, FakeTool[]> = {
	"exa-agent": [
		{
			name: "exa_agent_create_run",
			description: "Create an Exa Agent run without waiting for completion.",
			inputSchema: {
				type: "object",
				properties: { query: { type: "string" }, effort: { type: "string", default: "medium" } },
				required: ["query"],
			},
			run: async (args) => {
				runs += 1;
				const id = `run-${runs}`;
				queries.set(id, String(args.query));
				return { id, status: "running", query: args.query, effort: args.effort ?? "medium" };
			},
		},
		{
			name: "exa_agent_wait_run",
			description: "Wait for an existing Exa Agent run to complete.",
			inputSchema: {
				type: "object",
				properties: { run_id: { type: "string" }, poll_interval_ms: { type: "integer", default: 4000 } },
				required: ["run_id"],
			},
			run: async (args, signal) => {
				await sleep(Number(process.env.FAKE_MCP_WAIT_MS ?? "50"), signal);
				const id = String(args.run_id);
				return { id, status: "completed", output: `answer for ${queries.get(id) ?? "?"}` };
			},
		},
	],
	docs: [
		{
			name: "search_docs",
			description: "Search the docs.",
			inputSchema: { type: "object", properties: { q: { type: "string" } } },
			run: async (args) => ({ hits: [`doc about ${args.q}`] }),
		},
		{
			name: "fail",
			description: "Always fails.",
			inputSchema: { type: "object", properties: {} },
			run: async () => {
				throw new Error("upstream exploded");
			},
		},
	],
};

const ENTRIES: McpServerEntry[] = Object.keys(SERVERS).map((name) => ({
	name,
	config: { command: "fake-mcp-server", args: [name] },
	source: "fake-mcp-extension",
}));

/** What the tests hand over: the kernel tests only collect tools, the worker passes the whole extension API. */
interface FakeExtensionApi {
	registerTool(tool: Record<string, unknown>): void;
	registerCommand?: (name: string, options: unknown) => void;
	on?: (event: string, handler: unknown) => void;
}

export default function fakeExtension(pi: FakeExtensionApi): void {
	// The real gateway, with every call recorded like the other fake tools'.
	const recording = {
		registerCommand: (name: string, options: unknown) => pi.registerCommand?.(name, options),
		on: (event: string, handler: unknown) => pi.on?.(event, handler),
		registerTool: ((tool) => {
			const execute = tool.execute;
			pi.registerTool({
				...tool,
				execute: (id: string, params: unknown, signal: AbortSignal | undefined, onUpdate: never, ctx: never) => {
					const call: FakeCall = { tool: tool.name, params: structuredClone(params) as Record<string, unknown> };
					calls().push(call);
					signal?.addEventListener("abort", () => {
						call.aborted = true;
					});
					return execute(id, params as never, signal, onUpdate, ctx);
				},
			});
		}) as ExtensionAPI["registerTool"],
	};
	void createMcpExtension({
		servers: ENTRIES,
		createTransport: (entry) => connectFakeMcpServer(entry.name, { tools: SERVERS[entry.name] ?? [] }),
	})(recording as unknown as ExtensionAPI);
	pi.registerTool({
		name: "probe",
		label: "probe",
		description: "An extension tool that echoes its arguments.\n\nSecond paragraph that the list leaves out.",
		parameters: {
			type: "object",
			properties: { text: { type: "string" }, repeat: { type: "integer" } },
		},
		async execute(_id: string, params: { text?: string; repeat?: number }) {
			calls().push({ tool: "probe", params: structuredClone(params) });
			const body = (params.text ?? "probed").repeat(params.repeat ?? 1);
			return { content: [{ type: "text", text: body }], details: { length: body.length, echo: params } };
		},
	});
	pi.registerTool({
		name: "slow",
		label: "slow",
		description: "Waits `ms` milliseconds.",
		parameters: { type: "object", properties: { ms: { type: "integer" } }, required: ["ms"] },
		async execute(_id: string, params: { ms: number }, signal: AbortSignal | undefined) {
			const call: FakeCall = { tool: "slow", params: structuredClone(params) };
			calls().push(call);
			try {
				await sleep(params.ms, signal);
			} catch (error) {
				call.aborted = true;
				throw error;
			}
			return { content: [{ type: "text", text: `slept ${params.ms}` }], details: { ms: params.ms } };
		},
	});
	pi.registerTool({
		name: "boom",
		label: "boom",
		description: "Always throws.",
		parameters: { type: "object", properties: {} },
		async execute() {
			calls().push({ tool: "boom", params: {} });
			throw new Error("boom went the tool");
		},
	});
}
