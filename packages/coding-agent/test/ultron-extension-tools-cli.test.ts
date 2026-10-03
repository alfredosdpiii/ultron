/**
 * Extension tools in the real CLI (scripted provider, RPC mode), with a fake extension that registers the `mcp` gateway
 * (over in-memory fake MCP servers) plus plain tools:
 * - the model's tool list is [rlm] by default; the runtime guide lists the extension tools and MCP servers;
 * - the model calls the fake MCP tool from a cell with `asyncio.gather` over three calls, and the graph shows three
 *   tool nodes under that cell;
 * - a wait that outlives ULTRON_TOOL_YIELD_AFTER detaches, and its `tool_done` event re-invokes the model;
 * - ULTRON_EXTENSION_TOOLS=native, ULTRON_NATIVE_EXTENSION_TOOLS and ULTRON_TOOLS=native restore native tools, and a
 *   native call is a graph node under the turn.
 */
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { buildRlmGraph } from "../src/experimental/rlm-graph.ts";
import { extractTurn, parseAgentsStatus, RlmClock } from "../src/experimental/rlm-visualizer.ts";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import {
	ScriptedProvider,
	type ScriptedReply,
	type ScriptedRequest,
	scriptedModelsJson,
} from "./support/scripted-provider.ts";
import { tempServerDir } from "./support/server-dir.ts";

const FAKE_EXTENSION = resolve(__dirname, "support/fake-mcp-extension.ts");
const TOOL_EVENT =
	/<runtime_event kind="tool_done" id="(call-[0-9a-f]+)" status="([a-z_]+)" summary="([^"]*)" fetch="([^"]*)" \/>/;

const GATHER = "RESEARCH: three questions in parallel";
const DETACH = "DETACH: start a run and tell me when it is done";
const NATIVE = "NATIVE: use the probe tool";

function script(request: ScriptedRequest): ScriptedReply {
	if (request.firstUser === GATHER) {
		if (request.lastToolResult === undefined)
			return {
				tool: "rlm",
				args: {
					code: [
						"qs = ['solana tps', 'eth gas', 'btc fees']",
						"runs = await asyncio.gather(*(mcp.call('exa-agent_exa_agent_create_run', query=q, effort='low') for q in qs))",
						"ids = [r.json()['id'] for r in runs]",
						"print('IDS', ids)",
					].join("\n"),
				},
			};
		return { text: `STARTED ${request.lastToolResult}` };
	}
	if (request.firstUser === DETACH) {
		const event = TOOL_EVENT.exec(request.lastUser);
		if (event && request.lastToolResult === undefined)
			return {
				tool: "rlm",
				args: { code: `r = await tools.result("${event[1]}")\nprint("RESULT", r.json()["output"])` },
			};
		if (request.lastToolResult?.startsWith("RESULT")) return { text: `DONE: ${request.lastToolResult}` };
		if (request.turn === 0)
			return {
				tool: "rlm",
				args: {
					code: [
						"run = await mcp.call('exa-agent_exa_agent_create_run', query='solana tps')",
						"w = await mcp.call('exa-agent_exa_agent_wait_run', run_id=run.json()['id'])",
						"print('WAITING', w.running)",
					].join("\n"),
				},
			};
		return { text: "THE RUN IS GOING; I will report when it finishes." };
	}
	if (request.firstUser === NATIVE)
		return request.lastToolResult === undefined
			? { tool: "probe", args: { text: "native-probe" } }
			: { text: `probe said ${request.lastToolResult}` };
	return { text: "ok" };
}

function toolNames(request: ScriptedRequest): string[] {
	const body = JSON.parse(request.raw) as { tools?: Array<{ function: { name: string } }> };
	return (body.tools ?? []).map((tool) => tool.function.name);
}

async function inspect(client: RpcClient, request: string, payload: unknown): Promise<unknown> {
	const response = (await (client as unknown as { send(command: object): Promise<{ data?: unknown }> }).send({
		type: "inspect",
		request,
		payload,
	})) as { data?: unknown };
	return response.data;
}

describe("extension tools in the real CLI", () => {
	const cliPath = resolve(__dirname, "../src/cli.ts");
	const sourceResolverPath = resolve(__dirname, "../src/experimental/source-resolver.ts");
	let root: string;
	let agentDir: string;
	let projectDir: string;
	let provider: ScriptedProvider;
	const clients: RpcClient[] = [];

	beforeEach(async () => {
		root = mkdtempSync(join(tmpdir(), "ultron-ext-tools-cli-"));
		agentDir = join(root, "agent");
		projectDir = join(root, "project");
		mkdirSync(join(agentDir, "extensions"), { recursive: true });
		mkdirSync(projectDir, { recursive: true });
		writeFileSync(
			join(agentDir, "extensions", "fake-mcp.ts"),
			`export { default } from ${JSON.stringify(FAKE_EXTENSION)};\n`,
		);
		provider = new ScriptedProvider(script);
		await provider.start();
		writeFileSync(join(agentDir, "models.json"), scriptedModelsJson(provider.baseUrl));
	});

	afterEach(async () => {
		for (const client of clients.splice(0)) await client.stop().catch(() => {});
		await provider.stop();
		rmSync(root, { recursive: true, force: true });
	});

	function startClient(env: Record<string, string> = {}): RpcClient {
		const client = new RpcClient({
			cliPath,
			cwd: projectDir,
			provider: "scripted",
			model: "scripted",
			args: ["--no-session"],
			env: {
				NODE_OPTIONS: `--import ${sourceResolverPath}`,
				ULTRON_CODING_AGENT_DIR: agentDir,
				ULTRON_HINDSIGHT_URL: "off",
				ULTRON_SERVER_DIR: tempServerDir("u-ext-"),
				ULTRON_TOOL_ROUNDS_NUDGE: "0",
				ULTRON_SKILL_NUDGE: "0",
				PI_OFFLINE: "1",
				...env,
			},
		});
		clients.push(client);
		return client;
	}

	test("the model gets only rlm, calls MCP from a cell with asyncio.gather, and the graph shows three nodes", async () => {
		const client = startClient();
		await client.start();
		const events = await client.promptAndWait(GATHER, undefined, 60_000);
		expect(toolNames(provider.requests[0]!)).toEqual(["rlm"]);
		const system = provider.requests[0]!.system;
		expect(system).toContain("## Extension tools");
		expect(system).toContain("- probe: An extension tool that echoes its arguments.");
		expect(system).toContain("MCP servers (exa-agent, docs) are reached through the `mcp` namespace");
		const started = events.filter((event) => event.type === "tool_execution_start");
		expect(started.map((event) => (event as { toolName: string }).toolName)).toEqual(["rlm"]);
		expect(await client.getLastAssistantText()).toBe("STARTED IDS ['run-1', 'run-2', 'run-3']");

		const status = parseAgentsStatus(await inspect(client, "agents.status", { graph: true }));
		expect(status.toolCalls.map((call) => [call.label, call.status, call.source])).toEqual([
			["mcp exa-agent_exa_agent_create_run", "completed", "repl"],
			["mcp exa-agent_exa_agent_create_run", "completed", "repl"],
			["mcp exa-agent_exa_agent_create_run", "completed", "repl"],
		]);
		const { entries } = await client.getEntries();
		const { turn, cells } = extractTurn({ transcript: entries }, new RlmClock(), Date.now());
		const graph = buildRlmGraph({ now: Date.now(), tasks: [], toolCalls: status.toolCalls, turn, cells });
		const cellNodes = graph.children.filter((node) => node.kind === "cell");
		expect(cellNodes).toHaveLength(1);
		expect(cellNodes[0]!.children.map((node) => [node.kind, node.label, node.status])).toEqual([
			["tool", "mcp exa-agent_exa_agent_create_run", "done"],
			["tool", "mcp exa-agent_exa_agent_create_run", "done"],
			["tool", "mcp exa-agent_exa_agent_create_run", "done"],
		]);
		expect(cellNodes[0]!.children.every((node) => node.startedAt !== undefined && node.endedAt !== undefined)).toBe(
			true,
		);
	}, 120_000);

	test("a wait that outlives ULTRON_TOOL_YIELD_AFTER detaches, and its tool_done event re-invokes the model", async () => {
		const client = startClient({ ULTRON_TOOL_YIELD_AFTER: "0.5", FAKE_MCP_WAIT_MS: "3000" });
		let ends = 0;
		client.onEvent((event) => {
			if (event.type === "agent_end") ends += 1;
		});
		await client.start();
		await client.promptAndWait(DETACH, undefined, 60_000);
		expect(await client.getLastAssistantText()).toContain("THE RUN IS GOING");
		expect(provider.requests[1]!.lastToolResult).toContain("WAITING True");
		await expect.poll(() => ends, { timeout: 30_000, interval: 100 }).toBe(2);
		expect(await client.getLastAssistantText()).toBe("DONE: RESULT answer for solana tps");
		const event = provider.requests.map((request) => TOOL_EVENT.exec(request.lastUser)).find(Boolean)!;
		expect(event[2]).toBe("completed");
		expect(event[3]).toContain("mcp exa-agent_exa_agent_wait_run: ok;");
		expect(event[4]).toBe(`await tools.result(&quot;${event[1]}&quot;)`);
	}, 120_000);

	test("ULTRON_EXTENSION_TOOLS=native restores native tools, and a native call is a graph node under the turn", async () => {
		const native = startClient({ ULTRON_EXTENSION_TOOLS: "native" });
		await native.start();
		await native.promptAndWait(NATIVE, undefined, 60_000);
		expect(toolNames(provider.requests[0]!)).toEqual(["rlm", "mcp", "probe", "slow", "boom"]);
		expect(provider.requests[0]!.system).not.toContain("## Extension tools");
		expect(await native.getLastAssistantText()).toBe("probe said native-probe");
		const status = parseAgentsStatus(await inspect(native, "agents.status", { graph: true }));
		expect(status.toolCalls.map((call) => [call.label, call.status, call.source])).toEqual([
			["probe", "completed", "native"],
		]);
		const graph = buildRlmGraph({ now: Date.now(), tasks: [], toolCalls: status.toolCalls, cells: [] });
		expect(graph.children.map((node) => [node.kind, node.label, node.status])).toEqual([["tool", "probe", "done"]]);
		await native.stop();

		const allowlisted = startClient({ ULTRON_NATIVE_EXTENSION_TOOLS: "probe" });
		await allowlisted.start();
		await allowlisted.promptAndWait("hello", undefined, 60_000);
		expect(toolNames(provider.requests.at(-1)!)).toEqual(["rlm", "probe"]);
		// The guide still lists the tools that live only in the REPL.
		expect(provider.requests.at(-1)!.system).toContain("- mcp:");
		expect(provider.requests.at(-1)!.system).not.toContain("- probe:");
		await allowlisted.stop();

		const allNative = startClient({ ULTRON_TOOLS: "native" });
		await allNative.start();
		await allNative.promptAndWait("hello again", undefined, 60_000);
		expect(toolNames(provider.requests.at(-1)!)).toEqual([
			"read",
			"edit",
			"write",
			"bash",
			"rlm",
			"mcp",
			"probe",
			"slow",
			"boom",
		]);
	}, 180_000);
});
