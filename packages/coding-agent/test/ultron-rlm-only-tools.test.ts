/**
 * The RLM REPL as the root agent's only built-in tool (nano-rlm's design):
 * - the default tool set is [rlm] (extension tools are Python skills in the REPL); ULTRON_TOOLS=native restores Pi's
 *   read/edit/write/bash and the extension tools as native tools;
 * - `bash` and `edit` are pre-imported async skills in the kernel, with nano-rlm's edit semantics;
 * - tool results are middle-truncated at a byte budget and a large last value is shown by reference;
 * - the system prompt carries the runtime guide, and a scripted model edits a file through `rlm` + `edit`.
 */
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { createLocalBashOperations } from "../src/core/tools/bash.ts";
import { createUltronRlmTool } from "../src/experimental/session-worker.ts";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import { runHostBash } from "../src/ultron/rlm/host-bash.ts";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { DEFAULT_RLM_OUTPUT_BYTES, rlmOutputBudget, truncateToolOutput } from "../src/ultron/rlm/output-truncation.ts";
import {
	defaultBuiltinToolNames,
	RLM_TOOL_DESCRIPTION,
	rlmRuntimePrompt,
	rlmToolGuidelines,
} from "../src/ultron/rlm/prompt.ts";
import {
	ScriptedProvider,
	type ScriptedReply,
	type ScriptedRequest,
	scriptedModelsJson,
} from "./support/scripted-provider.ts";
import { tempServerDir } from "./support/server-dir.ts";

const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));

/** A kernel whose only host service is the real bash executor, as the worker wires it. */
function skillKernel(cwd: string): RlmKernel {
	return new RlmKernel({ cwd, runtimePath }, async (type, payload, signal) => {
		if (type === "bash") return runHostBash(payload, cwd, createLocalBashOperations({}), signal);
		throw new Error(`unexpected host request ${type}`);
	});
}

describe("default tool set", () => {
	test("the REPL is the only built-in tool by default and ULTRON_TOOLS=native restores Pi's set", () => {
		expect(defaultBuiltinToolNames({})).toEqual(["rlm"]);
		expect(defaultBuiltinToolNames({ ULTRON_TOOLS: "rlm" })).toEqual(["rlm"]);
		expect(defaultBuiltinToolNames({ ULTRON_TOOLS: "native" })).toEqual(["read", "edit", "write", "bash", "rlm"]);
		expect(defaultBuiltinToolNames({ ULTRON_TOOLS: " Native " })).toEqual(["read", "edit", "write", "bash", "rlm"]);
	});

	test("the runtime guide follows the active tools", () => {
		const replOnly = rlmRuntimePrompt(["rlm"]);
		expect(replOnly).toContain("## Runtime");
		expect(replOnly).toContain("## Skills");
		expect(replOnly).toContain("## Delegation");
		expect(replOnly).toContain("await rlm.collect(");
		expect(replOnly).toContain("await edit(path=..., old_str=..., new_str=...)` replaces exactly one occurrence");
		expect(replOnly).toContain("through `bash` with the project's own interpreter and toolchain");
		expect(replOnly).toContain("`preview(x)`");
		expect(replOnly).not.toContain("also run shell");
		const withNative = rlmRuntimePrompt(["read", "edit", "write", "bash", "rlm"]);
		expect(withNative).toContain("also run shell");
		expect(withNative).toContain("The native edit tool and the `edit` skill");
		expect(rlmRuntimePrompt(["read", "bash"])).toBeUndefined();
		expect(rlmToolGuidelines(["rlm"])[0]).toContain("Do all work through the rlm tool");
		expect(rlmToolGuidelines(["bash", "rlm"])[0]).not.toContain("Do all work");
		// The tool description names the pre-imported APIs; how to use them is said once, in the guide.
		expect(RLM_TOOL_DESCRIPTION).toContain("Pre-imported: `bash`, `read`, `edit`");
		expect(RLM_TOOL_DESCRIPTION).not.toContain("raises ValueError");
		expect(replOnly).toContain("await bash('''cmd''')");
	});

	test("the guide stays small, byte-stable and keeps the rules that matter", () => {
		const guide = rlmRuntimePrompt(["rlm"])!;
		// 13,976 characters and a 2,185-character tool description before the cost pass (2026-09-27); 9,462 and
		// 1,117 before the compact guide (5,084 and 439 after, same day); 5,318 before the wait-for-children wording
		// replaced "keep working" (5,493 after, 2026-09-28); 5,485 with the subagent verdict lines (2026-09-29); 5,486
		// with the worktree line, after trimming the search example and two phrasings (2026-09-30); 5,486 with the
		// spill-file note, after trimming two phrasings (2026-10-05).
		expect(guide.length).toBeLessThan(5_500);
		expect(RLM_TOOL_DESCRIPTION.length).toBeLessThan(600);
		expect(rlmRuntimePrompt(["rlm"])).toBe(guide);
		for (const rule of [
			"## Search before delegating",
			"Do not spawn subagents to read or classify documents",
			"narrow with code",
			"Never end a turn with a promise",
			"A turn ends when you reply without calling rlm",
			"Output over ~20 KB is cut in the middle",
			"run all project code (tests, repros, builds, imports) through `bash`",
			"replaces exactly one occurrence and raises ValueError",
			"never sleep, poll or loop",
			"`help(obj)`",
			"If you are a subagent, do the brief yourself",
			"rlm.map",
			"help(ctx)",
			"help(skills)",
			"help(agent)",
			"never check on children's files, logs or progress",
			"With nothing of your own left, `await rlm.collect(hs)` (free",
			"or end your turn: each end arrives as a `child_done` event",
			"`depth=N` lets a child delegate too (≤3 levels)",
			"spawn only if given depth",
			// Subagents finish with a checked verdict; the root trusts only verified ones.
			"`await rlm.finish(status, summary, evidence=[...], changed_files=[...])`",
			'Trust only verdicts whose `check.outcome` is "verified"',
			// Children that edit files get private worktrees; coupled work stays in one child; the root merges.
			"Children editing files: `worktree=True` each (coupled work in one brief), then `await rlm.merge(hs)`",
		])
			expect(guide).toContain(rule);
		expect(guide).not.toContain("keep working");
		// Without completion events nothing wakes an idle root, so waiting means rlm.collect alone.
		const quiet = rlmRuntimePrompt(["rlm"], { asyncEvents: false })!;
		const delegation = quiet.slice(quiet.indexOf("## Delegation"), quiet.indexOf("## Other APIs"));
		expect(delegation).toContain("With nothing of your own left, `await rlm.collect(hs)` (free");
		expect(delegation).not.toContain("child_done");
		expect(delegation).not.toContain("end your turn");
		expect(quiet).toContain("ULTRON_ASYNC_EVENTS=off");
		// Rarely needed detail lives in docstrings, not in every request.
		expect(guide).not.toContain("Triage.generations()");
		expect(guide).not.toContain("code_history");
	});
});

describe("tool output truncation", () => {
	test("keeps small output whole and cuts the middle of large output with a byte count", () => {
		expect(truncateToolOutput("short", 1024)).toBe("short");
		const lines = Array.from({ length: 400 }, (_, index) => `line ${index} ${"x".repeat(20)}`);
		const text = lines.join("\n");
		const cut = truncateToolOutput(text, 2000);
		expect(Buffer.byteLength(cut)).toBeLessThan(2400);
		expect(cut).toContain("line 0 ");
		expect(cut).toContain("line 399 ");
		expect(cut).not.toContain("line 200 ");
		const elided = Buffer.byteLength(text) - 2 * 1000;
		expect(cut).toContain(`[... ${elided} bytes truncated ...]`);
		expect(cut).toContain(`Warning: truncated output (${Buffer.byteLength(text)} bytes, 400 lines`);
	});

	test("never splits a multi-byte character at the cut", () => {
		const text = "é".repeat(3000);
		const cut = truncateToolOutput(text, 1024);
		expect(cut).not.toContain("�");
		expect(cut).toContain("bytes truncated");
	});

	test("the budget defaults to 20 KB and clamps the override", () => {
		expect(rlmOutputBudget({})).toBe(DEFAULT_RLM_OUTPUT_BYTES);
		expect(rlmOutputBudget({ ULTRON_RLM_OUTPUT_BYTES: "abc" })).toBe(DEFAULT_RLM_OUTPUT_BYTES);
		expect(rlmOutputBudget({ ULTRON_RLM_OUTPUT_BYTES: "10" })).toBe(1024);
		expect(rlmOutputBudget({ ULTRON_RLM_OUTPUT_BYTES: "50000" })).toBe(50000);
		expect(rlmOutputBudget({ ULTRON_RLM_OUTPUT_BYTES: "99999999" })).toBe(128 * 1024);
	});
});

describe("kernel skills", () => {
	let cwd: string;
	let kernel: RlmKernel;
	const previousBudget = process.env.ULTRON_RLM_OUTPUT_BYTES;

	beforeEach(() => {
		cwd = mkdtempSync(join(tmpdir(), "ultron-rlm-skills-"));
		// The kernel reads the budget from its environment at start: 1 KB makes truncation cheap to trigger.
		process.env.ULTRON_RLM_OUTPUT_BYTES = "1024";
		kernel = skillKernel(cwd);
	});

	afterEach(async () => {
		if (previousBudget === undefined) delete process.env.ULTRON_RLM_OUTPUT_BYTES;
		else process.env.ULTRON_RLM_OUTPUT_BYTES = previousBudget;
		await kernel.shutdown();
		rmSync(cwd, { recursive: true, force: true });
	});

	test("bash returns the output as a string with the exit code, plus dict compatibility and timeouts", async () => {
		expect(
			await kernel.execute(
				"out = await bash('''echo hi; echo err >&2; exit 3'''); (str(out), out.exit_code, out.ok)",
			),
		).toMatchObject({ status: "ok", result: "('hi\\nerr\\n[exit code 3]', 3, False)" });
		expect(await kernel.execute("(out['exit_code'], out.get('output'), 'hi' in out)")).toMatchObject({
			status: "ok",
			result: "(3, 'hi\\nerr\\n', True)",
		});
		expect(await kernel.execute("ok = await bash('''printf ok'''); (str(ok), ok.ok, ok.exit_code)")).toMatchObject({
			status: "ok",
			result: "('ok', True, 0)",
		});
		expect(
			await kernel.execute("slow = await bash('''sleep 5''', timeout=0.3); (str(slow), slow.timed_out, slow.ok)"),
		).toMatchObject({
			status: "ok",
			result: "('[timed out after 0.3s]', True, False)",
		});
		expect(await kernel.execute("await bash('')")).toMatchObject({
			status: "error",
			error: { ename: "ValueError", evalue: "bash command must be a non-empty string" },
		});
	});

	test("edit replaces exactly one occurrence and raises on a missing or ambiguous old_str", async () => {
		writeFileSync(join(cwd, "mod.py"), "x = 1\ny = 2\nx = 1\n");
		expect(await kernel.execute("await edit(path='mod.py', old_str='y = 2', new_str='y = 3')")).toMatchObject({
			status: "ok",
			result: "'Edited mod.py'",
		});
		expect(readFileSync(join(cwd, "mod.py"), "utf8")).toBe("x = 1\ny = 3\nx = 1\n");
		expect(await kernel.execute("await edit(path='mod.py', old_str='x = 1', new_str='x = 9')")).toMatchObject({
			status: "error",
			error: { ename: "ValueError", evalue: "old_str must appear exactly once in mod.py (found 2)" },
		});
		expect(await kernel.execute("await edit(path='mod.py', old_str='absent', new_str='x')")).toMatchObject({
			status: "error",
			error: { ename: "ValueError", evalue: "old_str must appear exactly once in mod.py (found 0)" },
		});
		expect(await kernel.execute("await edit(path='missing.py', old_str='a', new_str='b')")).toMatchObject({
			status: "error",
			error: { ename: "FileNotFoundError", evalue: "missing.py not found" },
		});
		// Nothing changed on the failed edits, and an absolute path works too.
		expect(readFileSync(join(cwd, "mod.py"), "utf8")).toBe("x = 1\ny = 3\nx = 1\n");
		expect(
			await kernel.execute(
				`await edit(path=${JSON.stringify(join(cwd, "mod.py"))}, old_str='y = 3', new_str='y = 4')`,
			),
		).toMatchObject({ status: "ok" });
		expect(readFileSync(join(cwd, "mod.py"), "utf8")).toBe("x = 1\ny = 4\nx = 1\n");
	});

	test("a large last value is shown by reference, stays in the kernel as `_`, and preview() bounds it", async () => {
		const big = await kernel.execute("'ab' * 5000");
		expect(big.status).toBe("ok");
		expect(big.result).toMatch(/^<str: 10,000 chars, 1 lines, sha256 [0-9a-f]{12}>\nhead: 'abab/);
		expect(big.result).toContain("\ntail: '");
		expect(big.result).toContain("kept in the kernel as `_`");
		expect(Buffer.byteLength(big.result ?? "")).toBeLessThan(1500);
		expect(await kernel.execute("len(_)")).toMatchObject({ status: "ok", result: "10000" });
		const list = await kernel.execute("list(range(1000))");
		expect(list.result).toContain("<list: 1,000 items; item types int>");
		expect(list.result).toContain("head: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]");
		expect(list.result).toContain("tail: [995, 996, 997, 998, 999]");
		const dict = await kernel.execute("{f'k{i}': i for i in range(500)}");
		expect(dict.result).toContain("<dict: 500 keys>");
		expect(dict.result).toContain("first keys: ['k0', 'k1'");
		expect(dict.result).toContain("first item: ['k0', 0]");
		// Small values are shown whole, a None expression shows nothing, and preview() describes any value.
		expect(await kernel.execute("[1, 2, 3]")).toMatchObject({ status: "ok", result: "[1, 2, 3]" });
		expect(await kernel.execute("print('only')")).toMatchObject({ status: "ok", stdout: "only\n", result: "" });
		expect(await kernel.execute("p = preview(_); (p['type'], p['length'], p['truncated'])")).toMatchObject({
			status: "ok",
			result: "('builtins.list', 3, False)",
		});
	});

	test("printed output keeps its head and tail within the budget and says how much was cut", async () => {
		const result = await kernel.execute("for i in range(300): print(f'row {i:04d} ' + 'x' * 40)");
		expect(result.status).toBe("ok");
		expect(result.stdout).toContain("row 0000 ");
		expect(result.stdout).toContain("row 0299 ");
		expect(result.stdout).not.toContain("row 0150 ");
		expect(result.stdout).toMatch(/\[\.\.\. \d+ bytes truncated \.\.\.\]/);
		expect(Buffer.byteLength(result.stdout)).toBeLessThan(1200);
	});
});

describe("the rlm tool's result", () => {
	test("is middle-truncated and a failed cell shows its output before the traceback", async () => {
		const cwd = mkdtempSync(join(tmpdir(), "ultron-rlm-tool-output-"));
		const previous = process.env.ULTRON_RLM_OUTPUT_BYTES;
		process.env.ULTRON_RLM_OUTPUT_BYTES = "1024";
		const tool = createUltronRlmTool(cwd, async (type) => {
			throw new Error(`unexpected host request ${type}`);
		});
		const invocation = {
			invocationId: "rlm-output",
			operationId: "operation-rlm-output",
			turnId: "turn-rlm-output",
			getMemo: async () => undefined,
			setMemo: async () => undefined,
		};
		const run = (code: string) =>
			tool.execute(
				"call",
				{ code },
				() => {},
				{ env: new NodeExecutionEnv({ cwd }) },
				invocation,
				BACKGROUND_CONTEXT,
			);
		try {
			const result = await run("print('start'); print('y' * 900); 'z' * 3000");
			const text = (result.content[0] as { text: string }).text;
			expect(text).toContain("Warning: truncated output");
			expect(text).toContain("start");
			expect(text).toContain("bytes truncated");
			expect(Buffer.byteLength(text)).toBeLessThan(1400);
			await expect(run("print('before the error')\nraise RuntimeError('boom')")).rejects.toThrow(
				/before the error[\s\S]*Traceback[\s\S]*RuntimeError: boom$/,
			);
		} finally {
			if (previous === undefined) delete process.env.ULTRON_RLM_OUTPUT_BYTES;
			else process.env.ULTRON_RLM_OUTPUT_BYTES = previous;
			await tool.close();
			rmSync(cwd, { recursive: true, force: true });
		}
	});
});

describe("cut cell output is saved, not discarded", () => {
	const previous = process.env.ULTRON_RLM_OUTPUT_BYTES;
	let root: string;

	beforeEach(() => {
		root = mkdtempSync(join(tmpdir(), "ultron-rlm-spill-"));
		process.env.ULTRON_RLM_OUTPUT_BYTES = "1024";
	});

	afterEach(() => {
		if (previous === undefined) delete process.env.ULTRON_RLM_OUTPUT_BYTES;
		else process.env.ULTRON_RLM_OUTPUT_BYTES = previous;
		rmSync(root, { recursive: true, force: true });
	});

	test("a stream over budget is written whole to a private file named in the marker", async () => {
		const spill = join(root, "spill");
		const kernel = new RlmKernel({ cwd: root, runtimePath, env: { ULTRON_RLM_SPILL_DIR: spill } }, async (type) => {
			throw new Error(`unexpected host request ${type}`);
		});
		try {
			const result = await kernel.execute(
				"for i in range(300): print(f'row {i:04d} ' + 'x' * 20)\nprint('big ' + 'q' * 50_000)",
			);
			const path = result.stdout.match(/full stdout in (\S+) \.\.\.\]/)?.[1];
			expect(path).toBeDefined();
			expect(result.stdout).not.toContain("row 0150 ");
			const full = readFileSync(path!, "utf8");
			expect(full).toContain("row 0150 ");
			expect(full.split("\n").length).toBe(302);
			expect(full).toContain("q".repeat(50_000));
			expect(statSync(path!).mode & 0o777).toBe(0o600);
			expect(statSync(spill).mode & 0o777).toBe(0o700);
			// Output within budget writes nothing.
			await kernel.execute("print('small')");
			expect(readdirSync(spill)).toHaveLength(1);
			// The directory keeps the newest 20 files.
			for (let i = 0; i < 22; i++) await kernel.execute("print('z' * 5000)");
			expect(readdirSync(spill)).toHaveLength(20);
		} finally {
			await kernel.shutdown();
		}
	});

	test("without a spill directory the marker is unchanged", async () => {
		const kernel = new RlmKernel({ cwd: root, runtimePath }, async (type) => {
			throw new Error(`unexpected host request ${type}`);
		});
		try {
			const result = await kernel.execute("print('w' * 5000)");
			expect(result.stdout).toMatch(/\[\.\.\. \d+ bytes truncated \.\.\.\]/);
		} finally {
			await kernel.shutdown();
		}
	});

	test("the rlm tool saves a cut combined result under its lane's output directory", async () => {
		const outputDir = join(root, "output");
		const tool = createUltronRlmTool(
			root,
			async (type) => {
				throw new Error(`unexpected host request ${type}`);
			},
			undefined,
			{ outputDir },
		);
		try {
			// Neither stream alone is over budget, together they are.
			const result = await tool.execute(
				"call",
				{ code: "import sys\nprint('o' * 900)\nprint('e' * 900, file=sys.stderr)" },
				() => {},
				{ env: new NodeExecutionEnv({ cwd: root }) },
				{
					invocationId: "rlm-spill",
					operationId: "operation-rlm-spill",
					turnId: "turn-rlm-spill",
					getMemo: async () => undefined,
					setMemo: async () => undefined,
				},
				BACKGROUND_CONTEXT,
			);
			const text = (result.content[0] as { text: string }).text;
			const path = text.match(/full output in (\S+) \.\.\.\]/)?.[1];
			expect(path?.startsWith(join(outputDir, "main"))).toBe(true);
			expect(readFileSync(path!, "utf8")).toBe(`${"o".repeat(900)}\n${"e".repeat(900)}`);
		} finally {
			await tool.close();
		}
	});
});

const PROBE_EXTENSION = `export default function (pi) {
	pi.registerTool({
		name: "probe",
		label: "probe",
		description: "An extension tool",
		parameters: { type: "object", properties: {} },
		async execute() {
			return { content: [{ type: "text", text: "probed" }], details: {} };
		},
	});
}
`;

describe("CLI with the RLM-only tool set", () => {
	const cliPath = resolve(__dirname, "../src/cli.ts");
	const sourceResolverPath = resolve(__dirname, "../src/experimental/source-resolver.ts");
	let root: string;
	let agentDir: string;
	let projectDir: string;
	let provider: ScriptedProvider;
	const clients: RpcClient[] = [];

	/** The model reads greet.py, fixes it with the edit skill, runs it through bash, and reports the output. */
	function script(request: ScriptedRequest): ScriptedReply {
		if (request.lastUser.includes("use the probe tool")) {
			return request.lastToolResult === undefined
				? { tool: "rlm", args: { code: "print(await tools.probe())" } }
				: { text: request.lastToolResult };
		}
		if (request.lastUser.includes("fix greet.py")) {
			if (request.lastToolResult === undefined)
				return {
					tool: "rlm",
					args: {
						code: [
							"from pathlib import Path",
							"source = Path('greet.py').read_text()",
							"print(await edit(path='greet.py', old_str='return \"helo \" + name', new_str='return \"hello \" + name'))",
							"out = await bash('''python3 greet.py''')",
							"(str(out), out.ok)",
						].join("\n"),
					},
				};
			return { text: `result: ${request.lastToolResult ?? "(none)"}` };
		}
		return { text: `reply:${request.lastUser.slice(0, 40)}` };
	}

	function toolNames(request: ScriptedRequest): string[] {
		const body = JSON.parse(request.raw) as { tools?: Array<{ function: { name: string } }> };
		return (body.tools ?? []).map((tool) => tool.function.name);
	}

	beforeEach(async () => {
		root = mkdtempSync(join(tmpdir(), "ultron-rlm-only-cli-"));
		agentDir = join(root, "agent");
		projectDir = join(root, "project");
		mkdirSync(join(agentDir, "extensions"), { recursive: true });
		mkdirSync(projectDir, { recursive: true });
		writeFileSync(join(agentDir, "extensions", "probe.ts"), PROBE_EXTENSION);
		writeFileSync(
			join(projectDir, "greet.py"),
			'def greet(name):\n    return "helo " + name\n\nprint(greet("world"))\n',
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

	function startClient(args: string[], env: Record<string, string> = {}): RpcClient {
		const client = new RpcClient({
			cliPath,
			cwd: projectDir,
			provider: "scripted",
			model: "scripted",
			args,
			env: {
				NODE_OPTIONS: `--import ${sourceResolverPath}`,
				ULTRON_CODING_AGENT_DIR: agentDir,
				ULTRON_HINDSIGHT_URL: "off",
				// Short on purpose: the server socket name is long and Unix socket paths are capped at 108 bytes.
				ULTRON_SERVER_DIR: tempServerDir("u-rlm-"),
				PI_OFFLINE: "1",
				...env,
			},
		});
		clients.push(client);
		return client;
	}

	test("the model gets only rlm, calls an extension tool from Python, and edits and runs a file through the skills", async () => {
		const client = startClient(["--no-session"]);
		await client.start();
		await client.promptAndWait("use the probe tool", undefined, 60_000);
		expect(await client.getLastAssistantText()).toBe("probed");
		expect(toolNames(provider.requests[0]!)).toEqual(["rlm"]);
		const system = provider.requests[0]!.system;
		expect(system).toContain("- rlm: Python REPL for files");
		// REPL-only mode leaves out Pi's own docs pointers.
		expect(system).not.toContain("Pi documentation");
		expect(system).not.toContain("- bash:");
		expect(system).toContain("<runtime>");
		expect(system).toContain("## Delegation");
		expect(system).toContain("Do all work through the rlm tool");
		expect(system).toContain("- probe: An extension tool");

		const events = await client.promptAndWait("fix greet.py", undefined, 120_000);
		const started = events.filter((event) => event.type === "tool_execution_start");
		expect(started.map((event) => (event as { toolName: string }).toolName)).toEqual(["rlm"]);
		expect(readFileSync(join(projectDir, "greet.py"), "utf8")).toContain('return "hello " + name');
		expect(await client.getLastAssistantText()).toBe("result: Edited greet.py\n('hello world', True)");
	}, 180_000);

	test("ULTRON_TOOLS=native restores Pi's tools next to rlm, and --tools still selects explicitly", async () => {
		const native = startClient(["--no-session"], { ULTRON_TOOLS: "native" });
		await native.start();
		await native.promptAndWait("hello", undefined, 60_000);
		expect(toolNames(provider.requests.at(-1)!)).toEqual(["read", "edit", "write", "bash", "rlm", "probe"]);
		expect(provider.requests.at(-1)!.system).toContain("- bash:");
		expect(provider.requests.at(-1)!.system).toContain("also run shell");
		expect(provider.requests.at(-1)!.system).toContain("Pi documentation");
		await native.stop();

		const explicit = startClient(["--no-session", "--tools", "bash,rlm"]);
		await explicit.start();
		await explicit.promptAndWait("hello again", undefined, 60_000);
		expect(toolNames(provider.requests.at(-1)!)).toEqual(["bash", "rlm"]);
	}, 180_000);
});
