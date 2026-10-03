/**
 * Native MCP in the real CLI (scripted provider, RPC mode), against a real stdio MCP server (the MCP package's
 * fixture: a local Node process, no network) configured in the profile's `mcp.json` as pi-mcp-adapter wrote it:
 * - the model's tool list is still [rlm]: MCP tools are reached from Python, never declared to the model;
 * - the runtime guide names the server, and a cell calls its tool with `mcp.call`;
 * - starting the session does not start the server: it is connected by the first call;
 * - `ultron mcp list --json` reports the server, and the profile's `mcp.json` is left as written.
 */
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import {
	ScriptedProvider,
	type ScriptedReply,
	type ScriptedRequest,
	scriptedModelsJson,
} from "./support/scripted-provider.ts";
import { tempServerDir } from "./support/server-dir.ts";

const STDIO_SERVER = resolve(__dirname, "../../mcp/test/fixtures/stdio-server.mjs");
const ASK = "ECHO: say hello through the MCP server";

function script(request: ScriptedRequest): ScriptedReply {
	if (request.firstUser !== ASK) return { text: "ok" };
	if (request.lastToolResult === undefined)
		return {
			tool: "rlm",
			args: {
				code: [
					"before = [(s['name'], s['status']) for s in await mcp.servers()]",
					"names = await mcp.tools('fixture')",
					"r = await mcp.call('fixture_echo', text='hello from the cell')",
					"after = [(s['name'], s['status'], s['toolCount']) for s in await mcp.servers()]",
					"print(before, names, str(r), after)",
				].join("\n"),
			},
		};
	return { text: `DONE ${request.lastToolResult}` };
}

function toolNames(request: ScriptedRequest): string[] {
	const body = JSON.parse(request.raw) as { tools?: Array<{ function: { name: string } }> };
	return (body.tools ?? []).map((tool) => tool.function.name);
}

describe("native MCP in the real CLI", () => {
	const cliPath = resolve(__dirname, "../src/cli.ts");
	const sourceResolverPath = resolve(__dirname, "../src/experimental/source-resolver.ts");
	let root: string;
	let agentDir: string;
	let projectDir: string;
	let mcpJson: string;
	let provider: ScriptedProvider;
	const clients: RpcClient[] = [];

	beforeEach(async () => {
		root = mkdtempSync(join(tmpdir(), "ultron-native-mcp-"));
		agentDir = join(root, "agent");
		projectDir = join(root, "project");
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(projectDir, { recursive: true });
		mcpJson = JSON.stringify(
			{
				mcpServers: {
					fixture: {
						command: process.execPath,
						args: [STDIO_SERVER],
						inheritEnv: false,
						env: { FIXTURE_TOKEN: "fixture-token-not-a-real-secret" },
						lifecycle: "lazy",
						requestTimeoutMs: 30000,
					},
				},
			},
			null,
			2,
		);
		writeFileSync(join(agentDir, "mcp.json"), mcpJson);
		provider = new ScriptedProvider(script);
		await provider.start();
		writeFileSync(join(agentDir, "models.json"), scriptedModelsJson(provider.baseUrl));
	});

	afterEach(async () => {
		for (const client of clients.splice(0)) await client.stop().catch(() => {});
		await provider.stop();
		rmSync(root, { recursive: true, force: true });
	});

	const env = () => ({
		NODE_OPTIONS: `--import ${sourceResolverPath}`,
		ULTRON_CODING_AGENT_DIR: agentDir,
		ULTRON_HINDSIGHT_URL: "off",
		ULTRON_SERVER_DIR: tempServerDir("u-mcp-"),
		ULTRON_TOOL_ROUNDS_NUDGE: "0",
		ULTRON_SKILL_NUDGE: "0",
		PI_OFFLINE: "1",
	});

	test("the model gets only rlm; a cell lists and calls the server's tool; the server starts on first use", async () => {
		const client = new RpcClient({
			cliPath,
			cwd: projectDir,
			provider: "scripted",
			model: "scripted",
			args: ["--no-session"],
			env: env(),
		});
		clients.push(client);
		await client.start();
		await client.promptAndWait(ASK, undefined, 60_000);
		expect(toolNames(provider.requests[0]!)).toEqual(["rlm"]);
		expect(provider.requests[0]!.system).toContain("MCP servers (fixture) are reached through the `mcp` namespace");
		expect(await client.getLastAssistantText()).toBe(
			"DONE [('fixture', 'not connected')] ['fixture_echo'] hello from the cell [('fixture', 'connected', 1)]",
		);
		// Every later request still declares one tool.
		expect(provider.requests.map(toolNames)).toEqual(provider.requests.map(() => ["rlm"]));
		expect(readFileSync(join(agentDir, "mcp.json"), "utf8")).toBe(mcpJson);
	}, 120_000);

	test("`ultron mcp list --json` connects the server and reports its tools; `ultron mcp --help` explains both roles", () => {
		const run = (...args: string[]) =>
			spawnSync(process.execPath, ["--import", sourceResolverPath, cliPath, "mcp", ...args], {
				cwd: projectDir,
				env: { PATH: process.env.PATH ?? "", HOME: root, ...env() },
				encoding: "utf8",
				timeout: 60_000,
			});
		const listed = run("list", "--json");
		expect(listed.status).toBe(0);
		expect(JSON.parse(listed.stdout)).toMatchObject({
			servers: [{ name: "fixture", scope: "global", state: "connected", tools: ["echo"] }],
		});
		expect(listed.stdout + listed.stderr).not.toContain("fixture-token-not-a-real-secret");
		const help = run("--help");
		expect(help.status).toBe(0);
		expect(help.stdout).toContain("mcp add <server>");
		expect(help.stdout).toContain("stdio MCP");
	}, 120_000);
});
