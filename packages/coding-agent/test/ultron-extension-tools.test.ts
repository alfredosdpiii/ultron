/**
 * Extension tools inside the REPL (Prime Intellect's RLM harness: MCP tools as pre-imported IPython skills):
 * - `tools.list/describe/call` and `tools.<name>(...)` run a registered extension tool in the host, with the cell's
 *   abort signal; results are the text (bounded by the output budget) plus `.details`; failures raise ToolError;
 * - `mcp.*` maps onto the pi-mcp-adapter gateway's parameters (keyword arguments become the `args` object, a JSON
 *   string is accepted, `mcp.<server>.<tool>` passes the server), and gateway errors raise McpError;
 * - a slow call detaches after ULTRON_TOOL_YIELD_AFTER and its end is reported for a `tool_done` event;
 * - extension tools are not model tools by default; ULTRON_EXTENSION_TOOLS=native, ULTRON_TOOLS=native, the setting
 *   and the allowlist restore them;
 * - REPL calls and tools the model calls directly appear as graph nodes.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { BACKGROUND_CONTEXT, withAbortSignal } from "@ultron/chord/context";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { buildRlmGraph, layoutGraph, renderRlmFooter } from "../src/experimental/rlm-graph.ts";
import { parseAgentsStatus, type RlmSnapshot } from "../src/experimental/rlm-visualizer.ts";
import {
	ExtensionToolCalls,
	type ExtensionToolRunner,
	extensionToolMode,
	modelExtensionToolNames,
	nativeExtensionToolAllowlist,
	prepareToolArguments,
	type ToolCallEnd,
	toolCallLabel,
	toolCallSummary,
	toolResultBudget,
} from "../src/ultron/rlm/extension-tools.ts";
import { createMemoryModuleStore, type NativeHostApi } from "../src/ultron/rlm/host-module.ts";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { extensionToolsPrompt, rlmRuntimePrompt } from "../src/ultron/rlm/prompt.ts";
import fakeExtension, { type FakeCall } from "./support/fake-mcp-extension.ts";

const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));

type Definition = {
	name: string;
	label?: string;
	description: string;
	parameters: unknown;
	execute(id: string, params: unknown, signal: AbortSignal, onUpdate: unknown, ctx: unknown): Promise<unknown>;
};

function fakeRunner(): ExtensionToolRunner {
	const definitions = new Map<string, Definition>();
	fakeExtension({ registerTool: (tool) => definitions.set(String(tool.name), tool as unknown as Definition) });
	return {
		tools: () =>
			[...definitions.values()].map((definition) => ({
				name: definition.name,
				...(definition.label === undefined ? {} : { label: definition.label }),
				description: definition.description,
				parameters: definition.parameters,
			})),
		execute: async (name, toolCallId, params, signal, onUpdate) => {
			const definition = definitions.get(name);
			if (!definition) throw new Error(`Unknown extension tool "${name}"`);
			const args = prepareToolArguments(definition, params);
			return (await definition.execute(toolCallId, args, signal, onUpdate, {})) as never;
		},
	};
}

function fakeCalls(): FakeCall[] {
	const global = globalThis as { __fakeExtensionCalls?: FakeCall[] };
	global.__fakeExtensionCalls ??= [];
	return global.__fakeExtensionCalls;
}

async function until(check: () => boolean, timeoutMs = 10_000): Promise<void> {
	const deadline = Date.now() + timeoutMs;
	while (!check()) {
		if (Date.now() > deadline) throw new Error("timed out waiting");
		await new Promise((resolve) => setTimeout(resolve, 20));
	}
}

describe("extension tools from the kernel", () => {
	let cwd: string;
	let calls: ExtensionToolCalls;
	let ends: ToolCallEnd[];
	const kernels: RlmKernel[] = [];
	const host = { rootOf: () => "turn:run-1" } as unknown as NativeHostApi;

	const kernel = (env: Record<string, string> = {}, outputBytes?: number): RlmKernel => {
		if (outputBytes !== undefined) {
			calls = new ExtensionToolCalls({
				runner: fakeRunner,
				store: createMemoryModuleStore(),
				outputBytes,
				onEnd: (end) => ends.push(end),
			});
		}
		const created = new RlmKernel({ cwd, runtimePath, env }, async (type, payload, signal) => {
			if (!type.startsWith("tools.")) throw new Error(`unexpected host request ${type}`);
			return calls.module.handle(
				{
					type,
					payload,
					caller: { lane: "main" },
					context: signal ? withAbortSignal(signal, BACKGROUND_CONTEXT) : BACKGROUND_CONTEXT,
				},
				host,
			);
		});
		kernels.push(created);
		return created;
	};

	beforeEach(() => {
		cwd = mkdtempSync(join(tmpdir(), "ultron-ext-tools-"));
		fakeCalls().splice(0);
		ends = [];
		calls = new ExtensionToolCalls({
			runner: fakeRunner,
			store: createMemoryModuleStore(),
			onEnd: (end) => ends.push(end),
		});
	});

	afterEach(async () => {
		for (const created of kernels.splice(0)) await created.shutdown();
		await calls.close();
		rmSync(cwd, { recursive: true, force: true });
	});

	test("tools.list, tools.describe, tools.call and attribute access", async () => {
		const k = kernel();
		const listed = await k.execute("[(t['name'], t['description']) for t in await tools.list()]");
		expect(listed.status).toBe("ok");
		expect(listed.result).toContain("('probe', 'An extension tool that echoes its arguments.')");
		expect(listed.result).toContain("'mcp'");
		expect(listed.result).not.toContain("Second paragraph");

		const described = await k.execute("d = await tools.describe('slow')\n(d['name'], d['parameters']['required'])");
		expect(described.result).toBe("('slow', ['ms'])");

		const called = await k.execute(
			[
				"r = await tools.call('probe', {'text': 'hi'}, repeat=2)",
				"r2 = await tools.probe(text='yo')",
				"(str(r), r.details['length'], r.ok, r.running, r.id.startswith('call-'), str(r2), isinstance(r, str))",
			].join("\n"),
		);
		expect(called.result).toBe("('hihi', 4, True, False, True, 'yo', True)");
		expect(
			fakeCalls()
				.filter((call) => call.tool === "probe")
				.map((call) => call.params),
		).toEqual([{ text: "hi", repeat: 2 }, { text: "yo" }]);
		// Every call is a record with a label and previews, for the graph.
		const records = calls.list();
		expect(records).toHaveLength(2);
		expect(records[0]).toMatchObject({ source: "repl", name: "probe", status: "completed", lane: "main" });
		expect(records[0]!.input).toContain('"text":"yo"');
		expect(records[0]!.preview).toBe("yo");
	});

	test("mcp maps keyword arguments, a JSON string and server attributes onto the gateway's parameters", async () => {
		const k = kernel();
		const discovered = await k.execute(
			[
				"s = [x['name'] for x in await mcp.servers()]",
				"t = await mcp.tools('exa-agent')",
				"everything = await mcp.tools()",
				"d = await mcp.describe('exa-agent_exa_agent_create_run')",
				"found = await mcp.search('wait')",
				"(s, t, len(everything), d['server'], d['parameters']['required'], [m['tool'] for m in found])",
			].join("\n"),
		);
		expect(discovered.status).toBe("ok");
		expect(discovered.result).toBe(
			"(['exa-agent', 'docs'], ['exa-agent_exa_agent_create_run', 'exa-agent_exa_agent_wait_run'], 4, 'exa-agent', ['query'], ['exa-agent_exa_agent_wait_run'])",
		);

		const ran = await k.execute(
			[
				"a = await mcp.call('exa-agent_exa_agent_create_run', query='solana tps', effort='high')",
				"b = await mcp.call('exa-agent_exa_agent_create_run', '{\"query\": \"from json\"}')",
				"c = await mcp.exa_agent.exa_agent_create_run(query='by attribute')",
				"w = await mcp.call('exa-agent_exa_agent_wait_run', run_id=a.json()['id'])",
				"(a.json()['effort'], b.json()['query'], c.json()['query'], w.json()['output'])",
			].join("\n"),
		);
		expect(ran.status).toBe("ok");
		expect(ran.result).toBe("('high', 'from json', 'by attribute', 'answer for solana tps')");
		const gatewayCalls = fakeCalls()
			.filter((call) => call.tool === "mcp" && call.params.tool !== undefined)
			.map((call) => call.params);
		expect(gatewayCalls).toEqual([
			{ tool: "exa-agent_exa_agent_create_run", args: { query: "solana tps", effort: "high" } },
			{ tool: "exa-agent_exa_agent_create_run", args: { query: "from json" } },
			{ tool: "exa_agent_create_run", args: { query: "by attribute" }, server: "exa-agent" },
			{ tool: "exa-agent_exa_agent_wait_run", args: { run_id: "run-1" } },
		]);
		// The graph labels a gateway call by the MCP tool it called.
		expect(calls.list()[0]!.label).toBe("mcp exa-agent_exa_agent_wait_run");
	});

	test("three MCP calls gathered in one cell run concurrently, each a record", async () => {
		const k = kernel();
		const started = Date.now();
		const gathered = await k.execute(
			[
				"qs = ['a', 'b', 'c']",
				"runs = await asyncio.gather(*(mcp.call('exa-agent_exa_agent_create_run', query=q) for q in qs))",
				"ids = [r.json()['id'] for r in runs]",
				"done = await asyncio.gather(*(mcp.call('exa-agent_exa_agent_wait_run', run_id=i) for i in ids))",
				"[d.json()['output'] for d in done]",
			].join("\n"),
		);
		expect(gathered.result).toBe("['answer for a', 'answer for b', 'answer for c']");
		expect(Date.now() - started).toBeLessThan(5_000);
		const waits = calls.list().filter((call) => call.label === "mcp exa-agent_exa_agent_wait_run");
		expect(waits).toHaveLength(3);
		expect(waits.every((call) => call.status === "completed" && call.endedAt !== null)).toBe(true);
	});

	test("errors propagate: a throwing tool, invalid arguments, an unknown tool, and gateway errors", async () => {
		const k = kernel();
		const boom = await k.execute(
			"try:\n    await tools.boom()\nexcept ToolError as e:\n    print('ERR', e.name, e.status, str(e))",
		);
		expect(boom.stdout).toContain("ERR boom failed boom failed: boom went the tool");
		const invalid = await k.execute("await tools.slow()");
		expect(invalid.status).toBe("error");
		expect(invalid.error?.ename).toBe("ToolError");
		expect(invalid.error?.evalue).toContain('Validation failed for tool "slow"');
		const unknown = await k.execute("await tools.call('nope')");
		expect(unknown.status).toBe("error");
		expect(unknown.error?.evalue).toContain('Unknown extension tool "nope"; available: mcp, probe, slow, boom');
		const gateway = await k.execute(
			[
				"out = []",
				"for name in ['missing_tool', 'docs_fail']:",
				"    try:",
				"        await mcp.call(name)",
				"    except McpError as e:",
				"        out.append((e.status, str(e)[:60]))",
				"out",
			].join("\n"),
		);
		expect(gateway.result).toContain("('tool_not_found', 'mcp missing_tool: Tool \"missing_tool\" not found.");
		expect(gateway.result).toContain("('tool_error', 'mcp docs_fail: Error: upstream exploded')");
		expect(calls.list().find((call) => call.name === "boom")).toMatchObject({
			status: "failed",
			error: "boom went the tool",
		});
	});

	test("a large result is middle-truncated to the output budget", async () => {
		const k = kernel({}, 2_000);
		const big = await k.execute(
			"r = await tools.probe(text='abcdefghij', repeat=5000)\n(len(r) < 3000, r.truncated, 'bytes truncated' in r, r.details['length'])",
		);
		expect(big.result).toBe("(True, True, True, 50000)");
	});

	test("aborting the cell cancels a call it is waiting on, and nothing is announced", async () => {
		const k = kernel();
		const controller = new AbortController();
		const running = k.execute("await tools.slow(ms=20000, yield_after=None)", controller.signal);
		await until(() => fakeCalls().some((call) => call.tool === "slow"));
		controller.abort();
		await running.catch(() => undefined);
		await until(() => calls.list()[0]?.status === "cancelled");
		expect(fakeCalls().find((call) => call.tool === "slow")?.aborted).toBe(true);
		await until(() => ends.length === 1);
		expect(ends[0]!.awaited).toBe(true);
	});

	test("a slow call detaches after the yield time; its end is reported for a completion event", async () => {
		const k = kernel({ ULTRON_TOOL_YIELD_AFTER: "0.3" });
		const detached = await k.execute(
			"r = await tools.slow(ms=1200)\n(r.running, r.ok, r.call.status, 'still running as call' in r, r.call.id)",
		);
		expect(detached.status).toBe("ok");
		expect(detached.result).toMatch(/^\(True, False, 'running', True, 'call-[0-9a-f]+'\)$/);
		const id = /'(call-[0-9a-f]+)'/.exec(detached.result ?? "")![1]!;
		// The cell ended; the call keeps running in the host, and its end is not awaited by anyone.
		await until(() => ends.length === 1);
		expect(ends[0]!.awaited).toBe(false);
		expect(ends[0]!.rootAborted).toBe(false);
		expect(ends[0]!.call).toMatchObject({ id, status: "completed", rootId: "turn:run-1", source: "repl" });
		expect(toolCallSummary(ends[0]!.call)).toBe("slow: ok; slept 1200");
		const fetched = await k.execute(`r = await tools.result("${id}")\n(str(r), r.details)`);
		expect(fetched.result).toBe("('slept 1200', {'ms': 1200})");
	});

	test("yield_after returns a handle: result() waits, cancel() stops, a root abort cancels", async () => {
		const k = kernel();
		const handle = await k.execute(
			[
				"h = await tools.slow(ms=300, yield_after=0)",
				"first = (type(h).__name__, h.running)",
				"r = await h.result()",
				"h2 = await mcp.call('exa-agent_exa_agent_wait_run', run_id='run-9', yield_after=0)",
				"h3 = await tools.slow(ms=20000, yield_after=0)",
				"await h3.cancel()",
				"(first, str(r), type(h2).__name__, h3.status)",
			].join("\n"),
		);
		expect(handle.result).toBe("(('ToolCall', True), 'slept 300', 'ToolCall', 'cancelled')");
		const root = await k.execute("h4 = await tools.slow(ms=20000, yield_after=0)\nh4.id");
		const id = (root.result ?? "").replace(/'/g, "");
		await calls.cancelRoot("turn:run-1");
		expect(calls.list().find((call) => call.id === id)?.status).toBe("cancelled");
		const end = ends.find((item) => item.call.id === id);
		expect(end?.rootAborted).toBe(true);
	});
});

describe("where extension tools live", () => {
	test("the REPL by default; ULTRON_EXTENSION_TOOLS, ULTRON_TOOLS=native and the setting restore native tools", () => {
		expect(extensionToolMode({})).toBe("repl");
		expect(extensionToolMode({ ULTRON_EXTENSION_TOOLS: "native" })).toBe("native");
		expect(extensionToolMode({ ULTRON_TOOLS: "native" })).toBe("native");
		expect(extensionToolMode({ ULTRON_TOOLS: "native", ULTRON_EXTENSION_TOOLS: "repl" })).toBe("repl");
		expect(extensionToolMode({}, { mode: "native" })).toBe("native");
		expect(extensionToolMode({ ULTRON_EXTENSION_TOOLS: "repl" }, { mode: "native" })).toBe("repl");
		const all = ["mcp", "probe", "slow"];
		expect(modelExtensionToolNames(all, { mode: "repl", allowlist: [] })).toEqual([]);
		expect(modelExtensionToolNames(all, { mode: "native", allowlist: [] })).toEqual(all);
		const allowlist = nativeExtensionToolAllowlist(
			{ ULTRON_NATIVE_EXTENSION_TOOLS: " probe, ,slow" },
			{ native: ["mcp"] },
		);
		expect(allowlist).toEqual(["probe", "slow", "mcp"]);
		expect(modelExtensionToolNames(all, { mode: "repl", allowlist: ["probe"] })).toEqual(["probe"]);
		expect(modelExtensionToolNames(all, { mode: "repl", allowlist: [], explicit: ["rlm", "slow"] })).toEqual([
			"slow",
		]);
	});

	test("a tool result held in Python is bounded by ULTRON_TOOL_RESULT_BYTES (default 256 KiB)", () => {
		expect(toolResultBudget({})).toBe(256 * 1024);
		expect(toolResultBudget({ ULTRON_TOOL_RESULT_BYTES: "100" })).toBe(1024);
		expect(toolResultBudget({ ULTRON_TOOL_RESULT_BYTES: "99999999" })).toBe(512 * 1024);
		expect(toolResultBudget({ ULTRON_TOOL_RESULT_BYTES: "x" })).toBe(256 * 1024);
	});

	test("the runtime guide lists the extension tools and MCP servers, bounded, and how to call them", () => {
		const tools = [
			{ name: "mcp", description: "MCP gateway — status and calls.\n\nServers: exa-agent" },
			...Array.from({ length: 20 }, (_, index) => ({ name: `tool${index}`, description: "x".repeat(500) })),
		];
		const guide = rlmRuntimePrompt(["rlm"], { extensionTools: tools, mcpServers: ["exa-agent"] })!;
		expect(guide).toContain("## Extension tools");
		expect(guide).toContain("- mcp: MCP gateway — status and calls. Servers: exa-agent");
		expect(guide).toContain("- … 9 more: `await tools.list()`");
		expect(guide).toContain("MCP servers (exa-agent) are reached through the `mcp` namespace");
		expect(guide).toContain('mcp.call("exa-agent_exa_agent_create_run", query=q, effort="medium")');
		expect(guide).toContain('<runtime_event kind="tool_done">');
		expect(guide).toContain("asyncio.gather");
		const section = extensionToolsPrompt(tools, { mcpServers: ["exa-agent"] })!;
		expect(section.length).toBeLessThan(4_500);
		// Nothing to say when every extension tool is a native tool, or there are none.
		expect(extensionToolsPrompt(tools, { native: tools.map((tool) => tool.name) })).toBeUndefined();
		expect(rlmRuntimePrompt(["rlm"])).not.toContain("## Extension tools");
	});

	test("gateway calls are labelled by mode", () => {
		expect(toolCallLabel("mcp", { tool: "exa-agent_exa_agent_run", args: {} })).toBe("mcp exa-agent_exa_agent_run");
		expect(toolCallLabel("mcp", { describe: "x" })).toBe("mcp describe x");
		expect(toolCallLabel("mcp", { search: "q" })).toBe('mcp search "q"');
		expect(toolCallLabel("mcp", { server: "docs" })).toBe("mcp list docs");
		expect(toolCallLabel("mcp", {})).toBe("mcp status");
		expect(toolCallLabel("probe", { text: "a" })).toBe("probe");
	});
});

describe("tool calls in the RLM graph", () => {
	test("REPL calls attach to their cell and native calls to the turn, with status and elapsed time", async () => {
		const calls = new ExtensionToolCalls({
			runner: () => undefined,
			store: createMemoryModuleStore(),
			now: () => 1_500,
		});
		calls.nativeStarted({ lane: "main", toolCallId: "tc-1", toolName: "mcp", args: { tool: "docs_search_docs" } });
		calls.nativeStarted({ lane: "main", toolCallId: "tc-2", toolName: "probe", args: {} });
		calls.nativeEnded({
			toolCallId: "tc-2",
			result: { content: [{ type: "text", text: "probed" }], details: {} },
			isError: false,
		});
		const native = calls.list();
		expect(native.map((call) => [call.label, call.status, call.source])).toEqual([
			["probe", "completed", "native"],
			["mcp docs_search_docs", "running", "native"],
		]);
		const status = parseAgentsStatus({
			tasks: [],
			toolCalls: [
				...native,
				{
					id: "call-1",
					lane: "main",
					source: "repl",
					name: "mcp",
					label: "mcp exa-agent_exa_agent_create_run",
					status: "completed",
					startedAt: 1_100,
					endedAt: 1_300,
					input: '{"tool":"exa-agent_exa_agent_create_run"}',
					preview: '{"id": "run-1"}',
				},
				{
					id: "call-2",
					lane: "main",
					source: "repl",
					name: "mcp",
					label: "mcp exa-agent_exa_agent_wait_run",
					status: "running",
					startedAt: 1_200,
				},
			],
		});
		expect(status.toolCalls).toHaveLength(4);
		const snapshot: RlmSnapshot = {
			now: 2_000,
			tasks: [],
			toolCalls: status.toolCalls,
			turn: { startedAt: 1_000, prompt: "research" },
			// A cell runs from 1000 to 1400; the native calls start at 1500, while the turn continues.
			cells: [{ toolCallId: "c1", code: "await mcp.call(...)", status: "ok", startedAt: 1_000, endedAt: 1_400 }],
		};
		const graph = buildRlmGraph(snapshot);
		const cell = graph.children.find((node) => node.kind === "cell")!;
		expect(cell.children.map((node) => [node.kind, node.label, node.status])).toEqual([
			["tool", "mcp exa-agent_exa_agent_create_run", "done"],
			["tool", "mcp exa-agent_exa_agent_wait_run", "running"],
		]);
		expect(cell.children[0]!.note).toBe('→ {"id": "run-1"}');
		expect(cell.children[0]!.details).toContainEqual(["fetch", 'await tools.result("call-1")']);
		const turnTools = graph.children.filter((node) => node.kind === "tool");
		expect(turnTools.map((node) => [node.label, node.status])).toEqual([
			["probe", "done"],
			["mcp docs_search_docs", "running"],
		]);
		expect(graph.status).toBe("running");
		const rows = layoutGraph(graph).map((row) => row.node.label);
		expect(rows).toContain("mcp exa-agent_exa_agent_wait_run");
		expect(renderRlmFooter(snapshot, 200)).toContain("2 tool calls");

		// A tool call made directly while no cell ran still makes the turn active, so the graph never looks idle.
		const onlyNative = buildRlmGraph({ now: 2_000, tasks: [], toolCalls: [status.toolCalls[1]!], cells: [] });
		expect(onlyNative.status).toBe("running");
		expect(onlyNative.children.map((node) => node.label)).toEqual(["mcp docs_search_docs"]);
		await calls.close();
	});

	test("a native run that ends without a tool result leaves no running node", async () => {
		const calls = new ExtensionToolCalls({ runner: () => undefined, store: createMemoryModuleStore() });
		calls.nativeStarted({ lane: "main", toolCallId: "tc-9", toolName: "bash", args: { command: "sleep 9" } });
		calls.nativeInterrupted("main");
		expect(calls.list()[0]).toMatchObject({ status: "cancelled", name: "bash" });
		await calls.close();
	});

	test("the journal survives a restart; a call running then is interrupted", async () => {
		const store = createMemoryModuleStore();
		const first = new ExtensionToolCalls({ runner: fakeRunner, store });
		await first.module.start?.({} as NativeHostApi);
		first.start("main", null, "slow", { ms: 20_000 });
		await first.settled();
		// Simulate a worker that died: a new instance loads the same journal.
		const second = new ExtensionToolCalls({ runner: fakeRunner, store });
		await second.module.start?.({} as NativeHostApi);
		expect(second.list()[0]).toMatchObject({ name: "slow", status: "interrupted", error: "worker restarted" });
		await first.close();
		await second.close();
	});
});
