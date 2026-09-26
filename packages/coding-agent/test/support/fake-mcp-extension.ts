/**
 * A fake Pi extension for extension-tool tests. It registers:
 * - `mcp`: shaped like pi-mcp-adapter's gateway tool (the same parameters: tool/args as an object or a JSON string,
 *   connect, describe, instructions, search, server, action...), with the adapter's mode precedence and `details`
 *   shapes, over two fake servers ("exa-agent" and "docs");
 * - `probe`: echoes its arguments (and repeats text, for output bounding);
 * - `slow`: waits `ms` milliseconds, honoring the abort signal;
 * - `boom`: throws.
 * Every execution is recorded on `globalThis.__fakeExtensionCalls`.
 */

type Result = { content: { type: "text"; text: string }[]; details?: Record<string, unknown> };

interface FakeTool {
	originalName: string;
	description: string;
	inputSchema: Record<string, unknown>;
	run(args: Record<string, unknown>, signal: AbortSignal | undefined): Promise<unknown>;
}

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

const text = (value: string, details?: Record<string, unknown>): Result => ({
	content: [{ type: "text", text: value }],
	...(details === undefined ? {} : { details }),
});

let runs = 0;
const queries = new Map<string, string>();

const SERVERS: Record<string, FakeTool[]> = {
	"exa-agent": [
		{
			originalName: "exa_agent_create_run",
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
			originalName: "exa_agent_wait_run",
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
			originalName: "search_docs",
			description: "Search the docs.",
			inputSchema: { type: "object", properties: { q: { type: "string" } } },
			run: async (args) => ({ hits: [`doc about ${args.q}`] }),
		},
		{
			originalName: "fail",
			description: "Always fails.",
			inputSchema: { type: "object", properties: {} },
			run: async () => {
				throw new Error("upstream exploded");
			},
		},
	],
};

const displayed = (server: string, tool: FakeTool) => `${server}_${tool.originalName}`;

function find(name: string, server?: string): { server: string; tool: FakeTool } | undefined {
	for (const [serverName, tools] of Object.entries(SERVERS)) {
		if (server !== undefined && server !== serverName) continue;
		for (const tool of tools)
			if (displayed(serverName, tool) === name || tool.originalName === name) return { server: serverName, tool };
	}
	return undefined;
}

type GatewayParams = {
	tool?: string;
	args?: string | Record<string, unknown>;
	connect?: string;
	describe?: string;
	instructions?: string;
	search?: string;
	server?: string;
	action?: string;
};

async function gateway(params: GatewayParams, signal: AbortSignal | undefined): Promise<Result> {
	// The adapter's argument parsing: an object, or a JSON string encoding one.
	let args: Record<string, unknown> | undefined;
	if (typeof params.args === "string" && params.args !== "") {
		const parsed = JSON.parse(params.args) as unknown;
		if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed))
			throw new Error("Invalid args: expected a JSON object");
		args = parsed as Record<string, unknown>;
	} else if (params.args !== undefined && typeof params.args === "object") args = params.args;
	if (params.tool) {
		const found = find(params.tool, params.server);
		if (!found)
			return text(`Tool "${params.tool}" not found. Use mcp({ search: "..." }) to search.`, {
				mode: "call",
				error: "tool_not_found",
				requestedTool: params.tool,
			});
		try {
			const value = await found.tool.run(args ?? {}, signal);
			// Like the adapter: the text content, then a copy of the structured content.
			return {
				content: [
					{ type: "text", text: JSON.stringify(value) },
					{ type: "text", text: `structuredContent:\n${JSON.stringify(value, null, 2)}` },
				],
				details: { mode: "call", server: found.server, tool: found.tool.originalName },
			};
		} catch (error) {
			if (signal?.aborted) throw error;
			return text(`Error: ${(error as Error).message}`, {
				mode: "call",
				error: "tool_error",
				server: found.server,
				tool: found.tool.originalName,
			});
		}
	}
	if (params.connect) return text(`Connected to ${params.connect}.`, { mode: "connect", server: params.connect });
	if (params.describe) {
		const found = find(params.describe, params.server);
		if (!found) return text(`Tool "${params.describe}" not found.`, { mode: "describe", error: "tool_not_found" });
		const tool = {
			name: displayed(found.server, found.tool),
			originalName: found.tool.originalName,
			description: found.tool.description,
			inputSchema: found.tool.inputSchema,
		};
		return text(`${tool.name}\nServer: ${found.server}\n\n${tool.description}`, {
			mode: "describe",
			tool,
			server: found.server,
		});
	}
	if (params.search !== undefined) {
		const matches = Object.entries(SERVERS).flatMap(([server, tools]) =>
			tools
				.filter((tool) => displayed(server, tool).includes(params.search ?? ""))
				.map((tool) => ({ server, tool: displayed(server, tool), score: 1 })),
		);
		return text(`Found ${matches.length} tools`, { mode: "search", matches, count: matches.length });
	}
	if (params.server) {
		const tools = (SERVERS[params.server] ?? []).map((tool) => displayed(params.server!, tool));
		return text(`${params.server} (${tools.length} tools)`, {
			mode: "list",
			server: params.server,
			tools,
			count: tools.length,
		});
	}
	const servers = Object.entries(SERVERS).map(([name, tools]) => ({
		name,
		status: "connected",
		listenState: "active",
		toolCount: tools.length,
		failedAgo: null,
	}));
	return text(`MCP: ${servers.length}/${servers.length} servers`, { mode: "status", servers });
}

export default function fakeExtension(pi: { registerTool(tool: Record<string, unknown>): void }): void {
	pi.registerTool({
		name: "mcp",
		label: "MCP",
		description: `MCP gateway — server status, tool search/describe, and single MCP tool calls.\n\nServers: ${Object.keys(SERVERS).join(", ")}\n\nUsage:\n  mcp({ })  → status`,
		parameters: {
			type: "object",
			properties: {
				tool: { type: "string", description: "Tool name to call" },
				args: {
					anyOf: [{ type: "string" }, { type: "object", additionalProperties: true }],
					description: "Tool arguments as a JSON object, or as a JSON string encoding one",
				},
				connect: { type: "string" },
				describe: { type: "string" },
				instructions: { type: "string" },
				search: { type: "string" },
				searchMode: { type: "string", enum: ["lexical", "semantic"] },
				regex: { type: "boolean" },
				includeSchemas: { type: "boolean" },
				limit: { type: "number" },
				offset: { type: "number" },
				server: { type: "string" },
				action: { type: "string" },
				url: { type: "string" },
				target: { type: "string" },
			},
		},
		async execute(_id: string, params: GatewayParams, signal: AbortSignal | undefined) {
			const call: FakeCall = { tool: "mcp", params: structuredClone(params) as Record<string, unknown> };
			calls().push(call);
			signal?.addEventListener("abort", () => {
				call.aborted = true;
			});
			return gateway(params, signal);
		},
	});
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
