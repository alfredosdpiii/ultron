/**
 * Native MCP support, as a built-in extension of the Session worker.
 *
 * Servers come from `mcp.json` in the agent directory and, in trusted projects, `<project>/.ultron/mcp.json`
 * (the `mcpServers` shape shared by other MCP clients; files written for pi-mcp-adapter keep working, see
 * core/mcp-servers.ts). The client, transports, OAuth, config and CLI are ported from Pi 1.0.0.
 *
 * What differs from Pi: MCP tools are never model tools. The model has one tool, the Python REPL, and reaches MCP
 * from there through the `mcp` namespace (`await mcp.servers()`, `mcp.tools(server)`, `mcp.call(tool, ...)`,
 * see ultron/rlm/tools_api.py). That namespace talks to the single `mcp` gateway tool registered here, which is an
 * extension tool and therefore lives in the REPL, not in the model's tool list. So nothing is registered per MCP
 * tool, and Pi's codemode and tool-search paths do not exist.
 *
 * Servers connect on first use (`lifecycle: "eager"` or `"keep-alive"`: in the background when the session
 * starts), so starting a session never waits for a server. `/mcp` shows and manages them.
 */

import { join } from "node:path";
import type { ImageContent, TextContent } from "@ultron/ai";
import type { Tool as McpTool } from "@ultron/mcp";
import type { TSchema } from "typebox";
import { getAgentDir } from "../../config.ts";
import type {
	ExtensionAPI,
	ExtensionCommandContext,
	ExtensionContext,
	ExtensionFactory,
} from "../../core/extensions/types.ts";
import { getMcpToolExposure } from "../../core/mcp-servers.ts";
import type { ModelRegistry } from "../../core/model-registry.ts";
import { unreachableCallbackHint } from "../../modes/interactive/components/login-dialog.ts";
import { openBrowser } from "../../utils/open-browser.ts";
import { browserReach } from "../../utils/remote-browser.ts";
import { type LoadedMcpConfig, loadMcpConfig, type McpServerEntry, updateMcpServerConfig } from "./config.ts";
import type { McpSignInPrompt } from "./oauth.ts";
import { isMcpAppResource, listed } from "./resources.ts";
import {
	createDefaultTransport,
	McpOAuthCredentialStore,
	McpServerConnection,
	McpServerLog,
	McpSignInCancelledError,
	type McpTransportFactory,
	signInMcpServer,
} from "./runtime.ts";
import { convertMcpResult, textOf, toModelContent } from "./tools.ts";

/** The gateway tool's name: the REPL's `mcp` namespace calls it, and `/mcp` is the command. */
export const MCP_GATEWAY_TOOL = "mcp";

export interface McpExtensionOptions {
	/** The session's working directory (project `mcp.json`, relative `cwd` of stdio servers). Default: the context's. */
	cwd?: string;
	/** Default: the agent directory. */
	agentDir?: string;
	/** Whether the project's `mcp.json` may be read. Default: asks the context when the session starts. */
	projectTrusted?: boolean;
	/** Servers to use instead of the `mcp.json` files (tests). */
	servers?: McpServerEntry[];
	/** Default: stdio and streamable HTTP transports. */
	createTransport?: McpTransportFactory;
	credentials?: McpOAuthCredentialStore;
	/** Default: the platform browser. */
	openUrl?: (url: string) => void;
	/** A notice to show once when the session starts (pi-mcp-adapter left out, see coexistence.ts). */
	startupNotice?: () => string | undefined;
}

interface GatewayParams {
	tool?: string;
	args?: string | Record<string, unknown>;
	connect?: string;
	describe?: string;
	instructions?: string;
	search?: string;
	limit?: number;
	server?: string;
	resources?: boolean | string;
	read?: string;
}

interface GatewayResult {
	content: (TextContent | ImageContent)[];
	details: Record<string, unknown>;
}

interface Server {
	entry: McpServerEntry;
	connection?: McpServerConnection;
}

/** An error the gateway reports in `details.error`; the REPL raises it as `McpError`. */
class GatewayError extends Error {
	readonly code: string;
	readonly details: Record<string, unknown>;

	constructor(code: string, message: string, details: Record<string, unknown> = {}) {
		super(message);
		this.code = code;
		this.details = details;
	}
}

const GATEWAY_PARAMETERS = {
	type: "object",
	properties: {
		tool: { type: "string", description: "MCP tool to call" },
		args: {
			anyOf: [{ type: "string" }, { type: "object", additionalProperties: true }],
			description: "Tool arguments as a JSON object, or as a JSON string encoding one",
		},
		connect: { type: "string", description: "Server to connect (or reconnect)" },
		describe: { type: "string", description: "Tool to describe" },
		instructions: { type: "string", description: "Server whose usage instructions to return" },
		search: { type: "string", description: "Words to find tools by" },
		limit: { type: "number" },
		server: { type: "string", description: "Server name: lists its tools, or narrows the other modes" },
		resources: { description: "true (or a server name) lists resources" },
		read: { type: "string", description: "URI of a resource to read; needs server" },
	},
} as unknown as TSchema;

const text = (value: string, details: Record<string, unknown>): GatewayResult => ({
	content: [{ type: "text", text: value }],
	details,
});

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function isEnabled(entry: McpServerEntry): boolean {
	return entry.config.enabled !== false;
}

function describeTransport(entry: McpServerEntry): "stdio" | "http" {
	return "url" in entry.config ? "http" : "stdio";
}

/** The name the REPL lists a tool under: `<server>_<tool>`, unique across servers. */
function displayName(server: string, tool: string): string {
	return `${server}_${tool}`;
}

/** What `/mcp` and `mcp.servers()` call a server's state. */
function statusOf(server: Server): string {
	if (!isEnabled(server.entry)) return "disabled";
	const connection = server.connection;
	if (!connection) return "not connected";
	if (connection.state === "needs-auth") return "needs sign-in";
	return connection.state;
}

function parseArgs(args: GatewayParams["args"]): Record<string, unknown> {
	if (args === undefined || args === "") return {};
	const parsed: unknown = typeof args === "string" ? JSON.parse(args) : args;
	if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
		throw new GatewayError("invalid_args", "Invalid args: expected a JSON object");
	}
	return parsed as Record<string, unknown>;
}

function gatewayDescription(names: readonly string[]): string {
	return [
		"MCP gateway: server status, tool listing, description and search, resources, and MCP tool calls.",
		// The worker reads the server names from this line for the runtime guide.
		...(names.length > 0 ? [`Servers: ${names.join(", ")}`] : []),
		'From Python use the mcp namespace: await mcp.servers(), mcp.tools("server"), mcp.describe("tool"), mcp.search("query"), mcp.call("tool", key=value).',
	].join("\n\n");
}

/** Score of `tool` for the query words: name matches count more than description matches. */
function searchScore(words: readonly string[], server: Server, tool: McpTool): number {
	const name = `${server.entry.name} ${tool.name}`.toLowerCase();
	const description =
		`${tool.title ?? ""} ${tool.description ?? ""} ${server.entry.config.description ?? ""}`.toLowerCase();
	let score = 0;
	for (const word of words) {
		if (name.includes(word)) score += 2;
		else if (description.includes(word)) score += 1;
		else return 0;
	}
	return score;
}

export function createMcpExtension(options: McpExtensionOptions = {}): ExtensionFactory {
	return (pi: ExtensionAPI) => {
		const agentDir = options.agentDir ?? getAgentDir();
		const createTransport = options.createTransport ?? createDefaultTransport;
		const openUrl = options.openUrl ?? openBrowser;
		let credentials = options.credentials;
		let cwd = options.cwd ?? process.cwd();
		let projectTrusted = options.projectTrusted ?? false;
		let modelRegistry: ModelRegistry | undefined;
		let log: McpServerLog | undefined;
		let servers = new Map<string, Server>();
		let configErrors: string[] = [];
		let closed = false;

		const load = (): LoadedMcpConfig =>
			options.servers ? { servers: options.servers, errors: [] } : loadMcpConfig({ agentDir, cwd, projectTrusted });

		/** Read the configuration again. Connections of servers whose entry did not change are kept. */
		const reload = async (): Promise<void> => {
			const loaded = load();
			configErrors = loaded.errors;
			const next = new Map<string, Server>();
			for (const entry of loaded.servers) {
				const previous = servers.get(entry.name);
				const unchanged =
					previous !== undefined && JSON.stringify(previous.entry.config) === JSON.stringify(entry.config);
				next.set(entry.name, unchanged ? { ...previous, entry } : { entry });
				if (unchanged) servers.delete(entry.name);
			}
			const dropped = [...servers.values()];
			servers = next;
			await Promise.all(dropped.map((server) => server.connection?.close().catch(() => undefined)));
			ensureGateway();
		};

		const connectionOf = (server: Server): McpServerConnection => {
			if (!isEnabled(server.entry)) {
				throw new GatewayError(
					"server_disabled",
					`MCP server "${server.entry.name}" is disabled. Enable it with /mcp.`,
				);
			}
			credentials ??= new McpOAuthCredentialStore();
			log ??= new McpServerLog(join(agentDir, "mcp.log"));
			server.connection ??= new McpServerConnection({
				entry: server.entry,
				cwd,
				createTransport,
				credentials,
				providerToken: async (provider) => modelRegistry?.getApiKeyForProvider(provider),
				log,
				onTools: () => {},
			});
			return server.connection;
		};

		/** The server's connection, connected. Failures are gateway errors that say what to do next. */
		const connect = async (server: Server): Promise<McpServerConnection> => {
			const connection = connectionOf(server);
			try {
				await connection.getClient();
			} catch (error) {
				if (connection.state === "needs-auth") {
					throw new GatewayError("auth_required", `${errorMessage(error)}`, { server: server.entry.name });
				}
				throw new GatewayError(
					"connect_failed",
					`MCP server "${server.entry.name}" could not be connected: ${errorMessage(error)}`,
					{ server: server.entry.name },
				);
			}
			return connection;
		};

		const serverNamed = (name: string): Server => {
			const server = servers.get(name);
			if (!server) {
				throw new GatewayError(
					"server_not_found",
					`No MCP server "${name}". Servers: ${[...servers.keys()].join(", ") || "(none configured)"}`,
				);
			}
			return server;
		};

		/** Tools the REPL can reach: `"exposure": "hidden"` (per server or per tool) removes them. */
		const visibleTools = (server: Server): McpTool[] =>
			(server.connection?.tools ?? []).filter(
				(tool) => getMcpToolExposure(server.entry.config, tool.name) !== "hidden",
			);

		const enabledServers = (only?: string): Server[] =>
			only === undefined ? [...servers.values()].filter((server) => isEnabled(server.entry)) : [serverNamed(only)];

		/**
		 * Find a tool by the name the REPL lists (`<server>_<tool>`), by Pi's `mcp__<server>__<tool>`, or by the
		 * server's own name for it. Servers are connected as needed: the named or prefixed one first, the rest only
		 * when the name is still unknown.
		 */
		const findTool = async (name: string, only?: string): Promise<{ server: Server; tool: McpTool }> => {
			const candidates = enabledServers(only);
			const matches = (server: Server, tool: McpTool): boolean =>
				tool.name === name ||
				displayName(server.entry.name, tool.name) === name ||
				`mcp__${server.entry.name}__${tool.name}`.replace(/[^A-Za-z0-9_]/g, "_") === name;
			const prefixed = candidates.filter(
				(server) =>
					name.startsWith(`${server.entry.name}_`) ||
					name.startsWith(`mcp__${server.entry.name.replace(/-/g, "_")}__`),
			);
			const ordered = [...prefixed, ...candidates.filter((server) => !prefixed.includes(server))];
			let failure: unknown;
			for (const server of ordered) {
				try {
					await connect(server);
				} catch (error) {
					// A server that is down does not hide a tool another server has; the named server's error is kept.
					if (only !== undefined || prefixed.includes(server)) failure ??= error;
					continue;
				}
				const tool = visibleTools(server).find((candidate) => matches(server, candidate));
				if (tool) return { server, tool };
			}
			if (failure) throw failure;
			throw new GatewayError(
				"tool_not_found",
				`Tool "${name}" not found${only ? ` on server "${only}"` : ""}. List tools with await mcp.tools("server") or await mcp.search("words").`,
				{ requestedTool: name },
			);
		};

		const status = (): GatewayResult => {
			const list = [...servers.values()].map((server) => ({
				name: server.entry.name,
				status: statusOf(server),
				toolCount: server.connection?.state === "connected" ? visibleTools(server).length : null,
				transport: describeTransport(server.entry),
				scope: server.entry.scope ?? "session",
				...(isEnabled(server.entry) ? {} : { disabled: true }),
				...(server.entry.config.description ? { description: server.entry.config.description } : {}),
				...(server.connection?.error ? { error: server.connection.error } : {}),
			}));
			const connected = list.filter((server) => server.status === "connected").length;
			const lines = list.map(
				(server) =>
					`${server.name}: ${server.status}${server.toolCount === null ? "" : ` (${server.toolCount} tools)`}${server.error ? `: ${server.error}` : ""}`,
			);
			return text([`MCP: ${connected}/${list.length} servers connected`, ...lines, ...configErrors].join("\n"), {
				mode: "status",
				servers: list,
				...(configErrors.length > 0 ? { configErrors } : {}),
			});
		};

		const gateway = async (params: GatewayParams, signal: AbortSignal | undefined): Promise<GatewayResult> => {
			if (params.tool) {
				const args = parseArgs(params.args);
				const { server, tool } = await findTool(params.tool, params.server);
				const connection = await connect(server);
				const result = await connection.callTool(tool.name, args, { signal, timeoutMs: connection.timeoutMs });
				const converted = await convertMcpResult(server.entry.name, tool.name, result, {
					readableResources: connection.resources.length > 0 || connection.resourceTemplates.length > 0,
				});
				return {
					content: converted.content,
					details: {
						mode: "call",
						server: server.entry.name,
						tool: tool.name,
						...(result.structuredContent === undefined ? {} : { structuredContent: result.structuredContent }),
						...(converted.isError ? { error: "tool_error" } : {}),
					},
				};
			}
			if (params.connect) {
				const server = serverNamed(params.connect);
				const connection = connectionOf(server);
				if (connection.state === "connected") await connection.reconnect();
				else await connect(server);
				return text(`Connected to ${server.entry.name} (${visibleTools(server).length} tools).`, {
					mode: "connect",
					server: server.entry.name,
				});
			}
			if (params.describe) {
				const { server, tool } = await findTool(params.describe, params.server);
				const described = {
					name: displayName(server.entry.name, tool.name),
					originalName: tool.name,
					description: tool.description ?? "",
					inputSchema: tool.inputSchema,
					...(tool.outputSchema === undefined ? {} : { outputSchema: tool.outputSchema }),
				};
				return text(
					`${described.name}\nServer: ${server.entry.name}\n\n${described.description}\n\nParameters (JSON schema):\n${JSON.stringify(tool.inputSchema, null, 2)}`,
					{ mode: "describe", tool: described, server: server.entry.name },
				);
			}
			if (params.search !== undefined) {
				const words = params.search.toLowerCase().split(/\s+/).filter(Boolean);
				const matches: { server: string; tool: string; score: number; description: string }[] = [];
				for (const server of enabledServers(params.server)) {
					try {
						await connect(server);
					} catch {
						continue;
					}
					for (const tool of visibleTools(server)) {
						const score = words.length === 0 ? 1 : searchScore(words, server, tool);
						if (score > 0) {
							matches.push({
								server: server.entry.name,
								tool: displayName(server.entry.name, tool.name),
								score,
								description: (tool.description ?? "").split("\n", 1)[0] ?? "",
							});
						}
					}
				}
				matches.sort((a, b) => b.score - a.score || a.tool.localeCompare(b.tool));
				const limited = matches.slice(0, typeof params.limit === "number" && params.limit > 0 ? params.limit : 20);
				return text(
					[`Found ${matches.length} tools`, ...limited.map((match) => `${match.tool}: ${match.description}`)].join(
						"\n",
					),
					{ mode: "search", matches: limited, count: matches.length },
				);
			}
			if (params.instructions) {
				const server = serverNamed(params.instructions);
				const connection = await connect(server);
				return text(connection.instructions ?? `MCP server "${server.entry.name}" has no instructions.`, {
					mode: "instructions",
					server: server.entry.name,
				});
			}
			if (params.read) {
				if (!params.server) throw new GatewayError("invalid_args", "Reading a resource needs the server name");
				const server = serverNamed(params.server);
				const connection = await connect(server);
				const result = await connection.readResource(params.read, { signal, timeoutMs: connection.timeoutMs });
				const content = await toModelContent(
					server.entry.name,
					result.contents.map((resource) => ({ type: "resource" as const, resource })),
				);
				return { content, details: { mode: "read", server: server.entry.name, uri: params.read } };
			}
			if (params.resources) {
				const only = typeof params.resources === "string" ? params.resources : params.server;
				const resources: Record<string, unknown>[] = [];
				const resourceTemplates: Record<string, unknown>[] = [];
				const errors: { server: string; error: string }[] = [];
				for (const server of enabledServers(only)) {
					try {
						const connection = await connect(server);
						const request = { signal, timeoutMs: connection.timeoutMs };
						const [found, templates] = await Promise.all([
							connection.allResources(request),
							connection.allResourceTemplates(request),
						]);
						for (const item of found)
							if (!isMcpAppResource(item)) resources.push(listed(server.entry.name, item));
						for (const item of templates) {
							if (!isMcpAppResource(item)) resourceTemplates.push(listed(server.entry.name, item));
						}
					} catch (error) {
						errors.push({ server: server.entry.name, error: errorMessage(error) });
					}
				}
				return text(`${resources.length} resources, ${resourceTemplates.length} resource templates`, {
					mode: "resources",
					resources,
					resourceTemplates,
					...(errors.length > 0 ? { errors } : {}),
				});
			}
			if (params.server) {
				const server = serverNamed(params.server);
				await connect(server);
				const tools = visibleTools(server).map((tool) => displayName(server.entry.name, tool.name));
				return text(`${server.entry.name} (${tools.length} tools)\n${tools.join("\n")}`, {
					mode: "list",
					server: server.entry.name,
					tools,
					count: tools.length,
				});
			}
			return status();
		};

		/**
		 * The gateway is registered once there is a server to reach. A profile without MCP servers gets no `mcp`
		 * tool: nothing for the runtime guide to describe, and nothing for `ULTRON_EXTENSION_TOOLS=native` to hand
		 * the model.
		 */
		let gatewayRegistered = false;
		const ensureGateway = (): void => {
			if (gatewayRegistered || servers.size === 0) return;
			gatewayRegistered = true;
			pi.registerTool({
				name: MCP_GATEWAY_TOOL,
				label: "MCP",
				description: gatewayDescription(
					load()
						.servers.filter(isEnabled)
						.map((entry) => entry.name),
				),
				parameters: GATEWAY_PARAMETERS,
				async execute(_toolCallId, params, signal) {
					try {
						return await gateway(params as GatewayParams, signal);
					} catch (error) {
						if (signal?.aborted) throw error;
						const code = error instanceof GatewayError ? error.code : "error";
						const details = error instanceof GatewayError ? error.details : {};
						// A gateway error says what to do next; anything else is reported as the error it is.
						const message = error instanceof GatewayError ? error.message : `Error: ${errorMessage(error)}`;
						return text(message, { ...details, error: code });
					}
				},
			});
		};

		// ------------------------------------------------------------------ /mcp

		const signInPrompt = (ctx: ExtensionContext, name: string): McpSignInPrompt => ({
			showAuthorizationUrl: (url) => {
				openUrl(url.href);
				ctx.ui.notify(`Sign in to MCP server "${name}" in your browser:\n${url.href}`, "info");
			},
			// As in /login: a stray Enter on the empty prompt does not end the sign-in; Esc does.
			promptForRedirectUrl: async (signal) => {
				while (!signal.aborted) {
					const answer = await ctx.ui.input(
						`MCP sign-in for ${name}: paste the redirect URL, or wait for the browser. ${unreachableCallbackHint(browserReach())}`,
						"http://127.0.0.1/callback?code=...",
						{ signal },
					);
					if (answer === undefined) return undefined;
					if (answer.trim()) return answer;
				}
				return undefined;
			},
		});

		const signIn = async (ctx: ExtensionContext, server: Server): Promise<string> => {
			const connection = connectionOf(server);
			const url = connection.oauthUrl;
			if (!url) {
				const provider = "url" in server.entry.config ? server.entry.config.auth?.provider : undefined;
				return provider
					? `MCP server "${server.entry.name}" uses the ${provider} login: run /login ${provider}.`
					: `MCP server "${server.entry.name}" does not use OAuth sign-in.`;
			}
			// A first connection attempt finds out what the server asks for (the challenge), or that it needs nothing.
			await connection.getClient().catch(() => undefined);
			credentials ??= new McpOAuthCredentialStore();
			try {
				await signInMcpServer({
					serverUrl: url,
					store: credentials.forServer(server.entry.name, url),
					settings: connection.oauthSettings(),
					challenge: connection.challenge,
					prompt: signInPrompt(ctx, server.entry.name),
				});
			} catch (error) {
				if (error instanceof McpSignInCancelledError) return `Sign-in to "${server.entry.name}" cancelled.`;
				throw error;
			}
			await connection.reconnect();
			return `Signed in to "${server.entry.name}" (${visibleTools(server).length} tools).`;
		};

		const signOut = async (server: Server): Promise<string> => {
			const url = connectionOf(server).oauthUrl;
			credentials ??= new McpOAuthCredentialStore();
			const removed = url ? credentials.remove(server.entry.name, url) : false;
			await server.connection?.signOut();
			return removed ? `Signed out of "${server.entry.name}".` : `No stored credentials for "${server.entry.name}".`;
		};

		const disconnect = async (server: Server): Promise<string> => {
			await server.connection?.close().catch(() => undefined);
			server.connection = undefined;
			return `Disconnected "${server.entry.name}". The next call connects it again.`;
		};

		const setEnabled = async (server: Server, enabled: boolean): Promise<string> => {
			const { entry } = server;
			if (entry.scope) updateMcpServerConfig(entry.source, entry.name, { enabled });
			if (!enabled) await disconnect(server);
			if (enabled) delete entry.config.enabled;
			else entry.config.enabled = false;
			return `${enabled ? "Enabled" : "Disabled"} "${entry.name}"${entry.scope ? ` in ${entry.source}` : " for this session"}.`;
		};

		const showTools = async (server: Server): Promise<string> => {
			await connect(server);
			const tools = visibleTools(server);
			return [
				`${server.entry.name}: ${tools.length} tools`,
				...tools.map(
					(tool) =>
						`  ${displayName(server.entry.name, tool.name)}: ${(tool.description ?? "").split("\n", 1)[0]}`,
				),
			].join("\n");
		};

		const ACTIONS: Record<string, (ctx: ExtensionContext, server: Server) => Promise<string>> = {
			connect: async (_ctx, server) => textOf((await gateway({ connect: server.entry.name }, undefined)).content),
			disconnect: (_ctx, server) => disconnect(server),
			login: signIn,
			logout: (_ctx, server) => signOut(server),
			enable: (_ctx, server) => setEnabled(server, true),
			disable: (_ctx, server) => setEnabled(server, false),
			tools: (_ctx, server) => showTools(server),
		};
		const USAGE = `Usage: /mcp [status | reload | ${Object.keys(ACTIONS).join(" | ")} <server>]`;

		/** `/mcp` without arguments in a client with dialogs: pick a server, then what to do with it. */
		const menu = async (ctx: ExtensionCommandContext): Promise<string | undefined> => {
			if (servers.size === 0) return undefined;
			const labels = new Map(
				[...servers.values()].map((server) => [`${server.entry.name} — ${statusOf(server)}`, server]),
			);
			const picked = await ctx.ui.select("MCP servers", [...labels.keys()]);
			const server = picked === undefined ? undefined : labels.get(picked);
			if (!server) return undefined;
			const state = statusOf(server);
			const choices: [string, string][] = [
				...(state === "disabled" ? ([["Enable", "enable"]] as [string, string][]) : []),
				...(state !== "disabled" && state !== "connected" ? ([["Connect", "connect"]] as [string, string][]) : []),
				...(state === "connected"
					? ([
							["Show tools", "tools"],
							["Reconnect", "connect"],
							["Disconnect", "disconnect"],
						] as [string, string][])
					: []),
				...(state !== "disabled" && "url" in server.entry.config
					? ([
							["Sign in", "login"],
							["Sign out", "logout"],
						] as [string, string][])
					: []),
				...(state === "disabled" ? [] : ([["Disable", "disable"]] as [string, string][])),
			];
			const action = await ctx.ui.select(
				`${server.entry.name} (${state})`,
				choices.map(([label]) => label),
			);
			const name = choices.find(([label]) => label === action)?.[1];
			return name ? ACTIONS[name]?.(ctx, server) : undefined;
		};

		pi.registerCommand("mcp", {
			description: "MCP servers: status, connect or disconnect, sign in, enable or disable",
			getArgumentCompletions: (prefix) => {
				const [first, second, ...rest] = prefix.split(/\s+/);
				if (rest.length > 0) return null;
				const values =
					second === undefined
						? ["status", "reload", ...Object.keys(ACTIONS)]
						: [...servers.keys()].map((name) => `${first} ${name}`);
				const typed = second === undefined ? (first ?? "") : prefix;
				return values.filter((value) => value.startsWith(typed)).map((value) => ({ value, label: value }));
			},
			handler: async (args, ctx) => {
				const [action, name, ...extra] = args.trim().split(/\s+/).filter(Boolean);
				try {
					if (action === undefined) {
						const outcome = ctx.hasUI ? await menu(ctx) : undefined;
						ctx.ui.notify(outcome ?? textOf(status().content), "info");
						return;
					}
					if (action === "status" && name === undefined) return ctx.ui.notify(textOf(status().content), "info");
					if (action === "reload" && name === undefined) {
						await reload();
						return ctx.ui.notify(textOf(status().content), "info");
					}
					const run = ACTIONS[action];
					if (!run || name === undefined || extra.length > 0) return ctx.ui.notify(USAGE, "warning");
					ctx.ui.notify(await run(ctx, serverNamed(name)), "info");
				} catch (error) {
					ctx.ui.notify(`MCP: ${errorMessage(error)}`, "error");
				}
			},
		});

		// ------------------------------------------------------------- lifecycle

		pi.on("session_start", async (_event, ctx) => {
			modelRegistry = ctx.modelRegistry;
			cwd = options.cwd ?? ctx.cwd;
			projectTrusted = options.projectTrusted ?? ctx.isProjectTrusted();
			closed = false;
			await reload();
			const notice = options.startupNotice?.();
			if (notice) ctx.ui.notify(notice, "info");
			if (configErrors.length > 0) ctx.ui.notify(`MCP configuration:\n${configErrors.join("\n")}`, "warning");
			// Nothing here waits for a server: eager ones connect in the background, the rest on first use.
			for (const server of servers.values()) {
				const lifecycle = server.entry.config.lifecycle;
				if (!isEnabled(server.entry) || (lifecycle !== "eager" && lifecycle !== "keep-alive")) continue;
				void connect(server).catch((error: unknown) => {
					if (!closed && error instanceof GatewayError && error.code === "auth_required") {
						ctx.ui.notify(`${error.message}`, "warning");
					}
				});
			}
		});

		pi.on("session_shutdown", async () => {
			closed = true;
			await Promise.all([...servers.values()].map((server) => server.connection?.close().catch(() => undefined)));
		});

		// Servers are known before the session starts too: a tool call may arrive first (tests, RPC clients).
		for (const entry of load().servers) servers.set(entry.name, { entry });
		ensureGateway();
	};
}

export default createMcpExtension();
