/**
 * Native MCP (src/extensions/mcp), without a model:
 * - `mcp.json` in Pi's format and as pi-mcp-adapter wrote it both load, and a project file only when trusted;
 * - the gateway connects a server on first use, lists and calls its tools, reads resources, and reports what went
 *   wrong in `details.error`;
 * - env values of a server are masked in cell output;
 * - pi-mcp-adapter is left out next to the built-in extension, and another `mcp` extension replaces the built-in;
 * - `ultron mcp <management command>` is told apart from Ultron's own MCP server.
 */
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { McpTransport } from "@ultron/mcp";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { CONFIG_DIR_NAME } from "../src/config.ts";
import type { Extension, ExtensionAPI, LoadExtensionsResult } from "../src/core/extensions/types.ts";
import { isMcpManagementCommand } from "../src/extensions/mcp/cli.lazy.ts";
import { ADAPTER_NOTICE, BUILTIN_MCP_EXTENSION_PATH, preferNativeMcp } from "../src/extensions/mcp/coexistence.ts";
import { loadMcpConfig, type McpServerEntry } from "../src/extensions/mcp/config.ts";
import { createMcpExtension } from "../src/extensions/mcp/index.ts";
import { createDefaultTransport } from "../src/extensions/mcp/runtime.ts";
import { maskCellOutput } from "../src/ultron/rlm/output-secrets.ts";
import { connectFakeMcpServer, type FakeMcpServer } from "./support/fake-mcp-server.ts";

type GatewayResult = { content: { type: string; text?: string }[]; details: Record<string, unknown> };
type Handler = (event: unknown, ctx: unknown) => Promise<void> | void;

/** The extension loaded into a stand-in for the extension API: its gateway tool, `/mcp`, and event handlers. */
function load(servers: Record<string, FakeMcpServer & { config?: Record<string, unknown> }>) {
	const connections: string[] = [];
	const notices: string[] = [];
	const handlers = new Map<string, Handler>();
	let execute: ((id: string, params: unknown, signal: AbortSignal | undefined) => Promise<GatewayResult>) | undefined;
	let command: ((args: string, ctx: unknown) => Promise<void>) | undefined;
	let description = "";
	const entries: McpServerEntry[] = Object.entries(servers).map(([name, server]) => ({
		name,
		config: { command: "fake", ...server.config } as McpServerEntry["config"],
		source: "test",
	}));
	const api = {
		registerTool: (tool: { description: string; execute: typeof execute }) => {
			description = tool.description;
			execute = tool.execute;
		},
		registerCommand: (_name: string, options: { handler: typeof command }) => {
			command = options.handler;
		},
		on: (event: string, handler: Handler) => handlers.set(event, handler),
	};
	void createMcpExtension({
		servers: entries,
		cwd: tmpdir(),
		projectTrusted: true,
		createTransport: (entry): McpTransport => {
			connections.push(entry.name);
			return connectFakeMcpServer(entry.name, servers[entry.name]!);
		},
	})(api as unknown as ExtensionAPI);
	const ctx = {
		cwd: tmpdir(),
		hasUI: false,
		isProjectTrusted: () => true,
		modelRegistry: undefined,
		ui: { notify: (message: string) => notices.push(message) },
	};
	return {
		connections,
		notices,
		description: () => description,
		gateway: (params: Record<string, unknown>) => execute!("call", params, undefined),
		command: (args: string) => command!(args, ctx),
		emit: (event: string) => handlers.get(event)?.({}, ctx),
	};
}

/** `${NAME}`, the form a config value names an environment variable in. */
const envRef = (name: string) => `$\{${name}}`;

const text = (result: GatewayResult) => result.content.map((part) => part.text ?? "").join("\n");

const echo: FakeMcpServer = {
	instructions: "Echo things.",
	resources: { "file:///notes.txt": "the notes" },
	tools: [
		{
			name: "echo",
			description: "Repeat the text.",
			inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
			run: async (args) => ({ said: args.text }),
		},
		{
			name: "secret_tool",
			description: "Not for the REPL.",
			inputSchema: { type: "object" },
			run: async () => "no",
		},
	],
};

describe("native MCP gateway", () => {
	test("servers connect on first use; tools are listed, described, searched and called", async () => {
		const mcp = load({
			"my-echo": { ...echo, config: { toolExposure: { secret_tool: "hidden" } } },
			off: { tools: [], config: { enabled: false } },
		});
		expect(mcp.description()).toContain("Servers: my-echo");
		const before = await mcp.gateway({});
		expect(before.details.servers).toMatchObject([
			{ name: "my-echo", status: "not connected", toolCount: null, transport: "stdio" },
			{ name: "off", status: "disabled", disabled: true },
		]);
		expect(mcp.connections).toEqual([]);

		const listed = await mcp.gateway({ server: "my-echo" });
		expect(listed.details).toMatchObject({ mode: "list", tools: ["my-echo_echo"] });
		expect(mcp.connections).toEqual(["my-echo"]);
		expect((await mcp.gateway({})).details.servers).toMatchObject([{ status: "connected", toolCount: 1 }, {}]);

		const described = await mcp.gateway({ describe: "my-echo_echo" });
		expect(described.details).toMatchObject({
			server: "my-echo",
			tool: { name: "my-echo_echo", originalName: "echo", inputSchema: { required: ["text"] } },
		});
		expect((await mcp.gateway({ search: "repeat" })).details.matches).toMatchObject([{ tool: "my-echo_echo" }]);
		expect(text(await mcp.gateway({ instructions: "my-echo" }))).toBe("Echo things.");

		for (const call of [
			{ tool: "my-echo_echo", args: { text: "one" } },
			{ tool: "echo", server: "my-echo", args: '{"text": "one"}' },
			{ tool: "mcp__my_echo__echo", args: { text: "one" } },
		]) {
			const result = await mcp.gateway(call);
			expect(JSON.parse(text(result))).toEqual({ said: "one" });
			expect(result.details).toMatchObject({ mode: "call", server: "my-echo", tool: "echo" });
		}
		// One connection served every call.
		expect(mcp.connections).toEqual(["my-echo"]);
	});

	test("resources are listed and read; errors name what went wrong", async () => {
		const mcp = load({
			"my-echo": { ...echo, config: { toolExposure: { secret_tool: "hidden" } } },
			off: { tools: [], config: { enabled: false } },
		});
		const resources = await mcp.gateway({ resources: true });
		expect(resources.details.resources).toMatchObject([{ server: "my-echo", uri: "file:///notes.txt" }]);
		expect(text(await mcp.gateway({ server: "my-echo", read: "file:///notes.txt" }))).toBe("the notes");

		expect((await mcp.gateway({ tool: "my-echo_secret_tool" })).details.error).toBe("tool_not_found");
		expect((await mcp.gateway({ tool: "nope" })).details).toMatchObject({
			error: "tool_not_found",
			requestedTool: "nope",
		});
		expect((await mcp.gateway({ server: "off" })).details.error).toBe("server_disabled");
		expect((await mcp.gateway({ server: "missing" })).details.error).toBe("server_not_found");
		expect((await mcp.gateway({ tool: "echo", args: "[1]" })).details.error).toBe("invalid_args");
	});

	test("/mcp reports status, connects, disconnects and disables; session start waits for no server", async () => {
		const mcp = load({ lazy: echo, eager: { ...echo, config: { lifecycle: "eager" } } });
		await mcp.emit("session_start");
		// The handler returned without a connection being established: only the eager server was even started.
		expect(mcp.connections).toEqual(["eager"]);
		await mcp.command("");
		expect(mcp.notices.at(-1)).toContain("lazy: not connected");
		await mcp.command("connect lazy");
		expect(mcp.notices.at(-1)).toBe("Connected to lazy (2 tools).");
		await mcp.command("tools lazy");
		expect(mcp.notices.at(-1)).toContain("lazy_echo: Repeat the text.");
		await mcp.command("disconnect lazy");
		await mcp.command("disable lazy");
		expect((await mcp.gateway({ server: "lazy" })).details.error).toBe("server_disabled");
		await mcp.command("enable lazy");
		expect((await mcp.gateway({ server: "lazy" })).details.count).toBe(2);
		await mcp.command("login lazy");
		expect(mcp.notices.at(-1)).toBe('MCP server "lazy" does not use OAuth sign-in.');
		await mcp.command("frobnicate");
		expect(mcp.notices.at(-1)).toContain("Usage: /mcp");
		await mcp.emit("session_shutdown");
	});
});

test("a profile without MCP servers registers no gateway tool", () => {
	const tools: string[] = [];
	void createMcpExtension({ servers: [], cwd: tmpdir(), projectTrusted: true })({
		registerTool: (tool: { name: string }) => tools.push(tool.name),
		registerCommand: () => {},
		on: () => {},
	} as unknown as ExtensionAPI);
	expect(tools).toEqual([]);
});

describe("mcp.json", () => {
	let root: string;
	beforeEach(() => {
		root = mkdtempSync(join(tmpdir(), "ultron-mcp-config-"));
		mkdirSync(join(root, "agent"), { recursive: true });
		mkdirSync(join(root, "project", CONFIG_DIR_NAME), { recursive: true });
	});
	afterEach(() => rmSync(root, { recursive: true, force: true }));

	const loadFrom = (projectTrusted: boolean) =>
		loadMcpConfig({ agentDir: join(root, "agent"), cwd: join(root, "project"), projectTrusted });

	test("a file written for pi-mcp-adapter loads unchanged, and is not rewritten", () => {
		const file = join(root, "agent", "mcp.json");
		const written = JSON.stringify({
			settings: { requestTimeoutMs: 45000, toolPrefix: "server", idleTimeout: 10 },
			mcpServers: {
				"exa-agent": {
					command: "exa-mcp",
					args: [],
					inheritEnv: false,
					env: { EXA_API_KEY: "exa-test-key-000111" },
					lifecycle: "lazy",
					requestTimeoutMs: 300000,
				},
				remote: {
					url: "https://mcp.example.com/mcp",
					auth: "bearer",
					bearerTokenEnv: "REMOTE_TOKEN",
					directTools: true,
				},
				off: { command: "x", disabled: true, lifecycle: "keep-alive" },
			},
		});
		writeFileSync(file, written);
		const loaded = loadFrom(false);
		expect(loaded.errors).toEqual([]);
		const byName = Object.fromEntries(loaded.servers.map((entry) => [entry.name, entry.config]));
		expect(byName["exa-agent"]).toMatchObject({
			command: "exa-mcp",
			inheritEnv: false,
			lifecycle: "lazy",
			timeout: 300,
		});
		expect(byName.remote).toMatchObject({
			url: "https://mcp.example.com/mcp",
			headers: { Authorization: `Bearer ${envRef("REMOTE_TOKEN")}` },
			timeout: 45,
		});
		expect(byName.remote).not.toHaveProperty("auth");
		expect(byName.off).toMatchObject({ enabled: false, lifecycle: "keep-alive" });
		expect(readFileSync(file, "utf8")).toBe(written);
	});

	test("Pi's format loads; the project file only in a trusted project; bad entries are reported", () => {
		writeFileSync(
			join(root, "agent", "mcp.json"),
			JSON.stringify({
				mcpServers: {
					docs: {
						type: "http",
						url: "https://docs.example.com/mcp",
						headers: { "X-Key": envRef("DOCS_KEY") },
						timeout: 20,
					},
					local: { command: "node", args: ["server.mjs"], env: { A: "b" }, cwd: "tools", enabled: false },
					legacy: { type: "sse", url: "https://old.example.com/sse" },
					broken: { lifecycle: "sometimes", command: "x" },
				},
			}),
		);
		writeFileSync(
			join(root, "project", CONFIG_DIR_NAME, "mcp.json"),
			JSON.stringify({ mcpServers: { project: { command: "project-server" } } }),
		);
		const untrusted = loadFrom(false);
		expect(untrusted.servers.map((entry) => [entry.name, entry.scope])).toEqual([
			["docs", "global"],
			["local", "global"],
		]);
		expect(untrusted.errors).toHaveLength(2);
		expect(untrusted.errors.join("\n")).toContain("legacy SSE transport is not supported");
		expect(untrusted.errors.join("\n")).toContain("lifecycle must be one of");
		expect(loadFrom(true).servers.map((entry) => [entry.name, entry.scope])).toContainEqual(["project", "project"]);
	});

	test("a server's env values are masked in cell output, and inheritEnv: false keeps Ultron's environment out", () => {
		const transport = createDefaultTransport(
			{
				name: "exa",
				config: { command: "exa-mcp", env: { EXA_API_KEY: "plainvalue-42-not-a-pattern" }, inheritEnv: false },
				source: "test",
			},
			root,
			undefined,
		);
		expect(maskCellOutput("the key is plainvalue-42-not-a-pattern, ok", {})).toBe(
			"the key is [REDACTED:known_secret], ok",
		);
		expect((transport as unknown as { options: { inheritEnv?: boolean } }).options.inheritEnv).toBe(false);
	});
});

describe("MCP next to other extensions and commands", () => {
	const extension = (path: string, tools: string[] = []): Extension =>
		({ path, resolvedPath: path, tools: new Map(tools.map((name) => [name, {}])) }) as unknown as Extension;
	const result = (extensions: Extension[]): LoadExtensionsResult =>
		({ extensions, errors: [], runtime: {} }) as unknown as LoadExtensionsResult;

	test("pi-mcp-adapter is left out with a notice; another mcp extension replaces the built-in one", () => {
		const builtin = extension(BUILTIN_MCP_EXTENSION_PATH, ["mcp"]);
		const adapter = extension("/home/u/.ultron/agent/npm/node_modules/pi-mcp-adapter/index.ts", ["mcp"]);
		const other = extension("/home/u/.ultron/agent/extensions/loki.ts", ["loki"]);
		const preferred = preferNativeMcp(result([other, adapter, builtin]));
		expect(preferred.adapterSkipped).toBe(true);
		expect(preferred.result.extensions).toEqual([other, builtin]);
		expect(ADAPTER_NOTICE).toContain("ultron remove npm:pi-mcp-adapter");

		const custom = extension("/home/u/.ultron/agent/extensions/my-mcp.ts", ["mcp"]);
		const replaced = preferNativeMcp(result([custom, builtin]));
		expect(replaced.result.extensions).toEqual([custom]);
		expect(replaced.adapterSkipped).toBe(false);
		// Without the built-in extension (--no-extensions) nothing is touched.
		expect(preferNativeMcp(result([adapter])).result.extensions).toEqual([adapter]);
	});

	test("`ultron mcp` alone, or with its server flags, is Ultron's own MCP server", () => {
		for (const first of ["add", "remove", "list", "login", "logout", "help", "--help", "-h"]) {
			expect(isMcpManagementCommand(first)).toBe(true);
		}
		for (const first of [undefined, "--bridge", "--child", "--frame-model", "--no-instructions"]) {
			expect(isMcpManagementCommand(first)).toBe(false);
		}
	});
});
