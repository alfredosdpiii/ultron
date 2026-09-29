import { type ChildProcessWithoutNullStreams, spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { PassThrough } from "node:stream";
import { fileURLToPath } from "node:url";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { ENV_AGENT_DIR } from "../src/config.ts";
import { KeybindingsManager } from "../src/core/keybindings.ts";
import { RlmPane } from "../src/experimental/rlm-pane.ts";
import { PLAIN_STYLE, RlmClock } from "../src/experimental/rlm-visualizer.ts";
import {
	connectControl,
	controlDir,
	findServer,
	listServers,
	type ServerRecord,
	startControlServer,
} from "../src/ultron/claude/control-socket.ts";
import { runHook } from "../src/ultron/claude/hooks.ts";
import { type McpServerHandlers, serveMcp } from "../src/ultron/claude/mcp-protocol.ts";
import { parseMcpArgs } from "../src/ultron/claude/mcp-server.ts";
import { parseWatchArgs, pollSnapshot, renderWatchFrame } from "../src/ultron/claude/watch.ts";

/**
 * `ultron mcp`: Ultron's REPL as an MCP server for Claude Code. The protocol layer, the control socket (hooks, the
 * viewer, subagent servers), and the server end to end: a real kernel whose state persists across calls, completion
 * events delivered with the next result, hooks, frames charged to the usage ledger, and the watch view.
 */

const here = dirname(fileURLToPath(import.meta.url));
const cliPath = resolve(here, "../src/cli.ts");
const sourceResolverPath = resolve(here, "../src/experimental/source-resolver.ts");

describe("MCP protocol", () => {
	function session(handlers: Partial<McpServerHandlers> = {}) {
		const input = new PassThrough();
		const lines: Array<Record<string, unknown>> = [];
		const waiters: Array<() => void> = [];
		const output = {
			write: (text: string) => {
				for (const line of text.split("\n").filter(Boolean))
					lines.push(JSON.parse(line) as Record<string, unknown>);
				for (const waiter of waiters.splice(0)) waiter();
			},
		};
		const done = serveMcp(
			{
				name: "ultron",
				version: "1",
				instructions: () => "GUIDE",
				tools: () => [{ name: "rlm", description: "d", inputSchema: { type: "object" } }],
				callTool: async (_name, args, signal) => {
					if (args.code === "wait")
						await new Promise((_, reject) =>
							signal.addEventListener("abort", () => reject(new Error("cancelled"))),
						);
					return { content: [{ type: "text", text: `ran ${String(args.code)}` }] };
				},
				prompts: () => [{ name: "ultron-guide", description: "g", text: () => "GUIDE" }],
				resources: () => [
					{
						uri: "ultron://guide",
						name: "guide",
						description: "g",
						mimeType: "text/markdown",
						text: () => "GUIDE",
					},
				],
				...handlers,
			},
			input,
			output,
		);
		const request = async (id: number, method: string, params: Record<string, unknown> = {}) => {
			input.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
			for (;;) {
				const found = lines.find((line) => line.id === id);
				if (found) return found;
				await new Promise<void>((wake) => waiters.push(wake));
			}
		};
		return { input, request, done, lines };
	}

	test("initialize, tools, prompts and resources", async () => {
		const { input, request, done } = session();
		const init = await request(1, "initialize", {
			protocolVersion: "2025-06-18",
			clientInfo: { name: "claude-code" },
		});
		expect(init.result).toMatchObject({
			protocolVersion: "2025-06-18",
			serverInfo: { name: "ultron" },
			instructions: "GUIDE",
			capabilities: { tools: {}, prompts: {}, resources: {} },
		});
		// An unknown version gets the newest one the server speaks.
		expect((await request(2, "initialize", { protocolVersion: "1999-01-01" })).result).toMatchObject({
			protocolVersion: "2025-11-25",
		});
		expect((await request(3, "tools/list")).result).toMatchObject({ tools: [{ name: "rlm" }] });
		expect((await request(4, "tools/call", { name: "rlm", arguments: { code: "1" } })).result).toEqual({
			content: [{ type: "text", text: "ran 1" }],
		});
		expect((await request(5, "tools/call", { name: "bash", arguments: {} })).error).toMatchObject({ code: -32602 });
		expect((await request(6, "prompts/get", { name: "ultron-guide" })).result).toMatchObject({
			messages: [{ role: "user", content: { type: "text", text: "GUIDE" } }],
		});
		expect((await request(7, "resources/read", { uri: "ultron://guide" })).result).toMatchObject({
			contents: [{ uri: "ultron://guide", text: "GUIDE" }],
		});
		expect((await request(8, "no/such")).error).toMatchObject({ code: -32601 });
		input.end();
		await done;
	});

	test("notifications/cancelled aborts the call", async () => {
		const { input, request, done, lines } = session();
		const pending = request(1, "tools/call", { name: "rlm", arguments: { code: "wait" } });
		await new Promise((wake) => setTimeout(wake, 20));
		input.write(
			`${JSON.stringify({ jsonrpc: "2.0", method: "notifications/cancelled", params: { requestId: 1 } })}\n`,
		);
		expect((await pending).error).toMatchObject({ message: "cancelled" });
		input.write("not json\n");
		await new Promise((wake) => setTimeout(wake, 20));
		expect(lines.at(-1)).toMatchObject({ id: null, error: { code: -32700 } });
		input.end();
		await done;
	});

	test("server flags are checked", () => {
		expect(parseMcpArgs(["--no-instructions", "--frame-model", "claude-code/haiku"])).toMatchObject({
			instructions: false,
			frameModel: "claude-code/haiku",
		});
		expect(() => parseMcpArgs(["--bogus"])).toThrow(/unknown option/);
		expect(() => parseMcpArgs(["--child"])).toThrow(/--parent-socket/);
		expect(() => parseMcpArgs(["--children", "gpt"])).toThrow(/claude or ultron/);
		expect(() => parseWatchArgs(["--nope"])).toThrow(/unknown option/);
	});
});

describe("control socket", () => {
	let runtimeDir: string;
	beforeAll(() => {
		runtimeDir = mkdtempSync(join(tmpdir(), "ultron-claude-rt-"));
	});
	afterAll(() => rmSync(runtimeDir, { recursive: true, force: true }));

	test("owner-only socket and registry; requests and errors; found by Claude Code session", async () => {
		const dir = controlDir({ XDG_RUNTIME_DIR: runtimeDir });
		expect(statSync(dir).mode & 0o777).toBe(0o700);
		const record: ServerRecord = {
			name: "sess-a",
			pid: process.pid,
			socket: join(dir, "sess-a.sock"),
			cwd: "/work",
			startedAt: Date.now(),
			claudeSessionId: "sess-a",
		};
		const server = await startControlServer(
			record,
			async (request) => {
				if (request.op === "fail") throw new Error("nope");
				return { echo: request.op };
			},
			dir,
		);
		try {
			expect(statSync(record.socket).mode & 0o777).toBe(0o600);
			expect(statSync(join(dir, "sess-a.json")).mode & 0o777).toBe(0o600);
			expect(findServer({ claudeSessionId: "sess-a" }, dir)?.socket).toBe(record.socket);
			expect(findServer({ cwd: "/work" }, dir)?.name).toBe("sess-a");
			const client = await connectControl(record.socket);
			expect(await client.request("status")).toEqual({ echo: "status" });
			await expect(client.request("fail")).rejects.toThrow("nope");
			client.close();
			// A hook finds the server by the session id in its input and prints the server's answer.
			const output = await runHook("stop", {
				socket: record.socket,
				input: JSON.stringify({ session_id: "sess-a" }),
			});
			expect(JSON.parse(output)).toEqual({ echo: "hook" });
		} finally {
			await server.close();
		}
		expect(listServers(dir)).toEqual([]);
		// Without a server, hooks are quiet.
		expect(await runHook("stop", { socket: record.socket, input: "{}" })).toBe("");
		expect(await runHook("bogus", { input: "{}" })).toBe("");
	});
});

describe("ultron mcp end to end", () => {
	let work: string;
	let project: string;
	let runtimeDir: string;
	let provider: Server;
	let child: ChildProcessWithoutNullStreams;
	let stderr = "";
	const frameRequests: string[] = [];
	const lines: Array<Record<string, unknown>> = [];
	const waiters: Array<() => void> = [];
	let nextId = 1;

	const request = async (method: string, params: Record<string, unknown> = {}) => {
		const id = nextId++;
		child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
		for (;;) {
			const found = lines.find((line) => line.id === id);
			if (found) return found;
			await new Promise<void>((wake) => waiters.push(wake));
		}
	};
	const cell = async (code: string) => {
		const response = await request("tools/call", { name: "rlm", arguments: { code } });
		const result = response.result as { content: Array<{ type: string; text?: string }>; isError?: boolean };
		return { text: result.content[0]?.text ?? "", isError: result.isError === true };
	};

	beforeAll(async () => {
		work = mkdtempSync(join(tmpdir(), "ultron-claude-mcp-"));
		project = join(work, "project");
		runtimeDir = join(work, "run");
		const agentDir = join(work, "agent");
		for (const dir of [project, runtimeDir, agentDir]) mkdirSync(dir, { recursive: true, mode: 0o700 });
		// An OpenAI-compatible stub serves the frames' model, so rlm.infer runs and is charged to the ledger.
		provider = createServer(async (incoming, response) => {
			const chunks: Buffer[] = [];
			for await (const chunk of incoming) chunks.push(chunk as Buffer);
			frameRequests.push(Buffer.concat(chunks).toString("utf8"));
			response.writeHead(200, { "content-type": "text/event-stream" });
			const chunk = (delta: object, finish: string | null, usage?: object) =>
				`data: ${JSON.stringify({ id: "x", object: "chat.completion.chunk", created: 0, model: "stub", choices: [{ index: 0, delta, finish_reason: finish }], ...(usage ? { usage } : {}) })}\n\n`;
			response.write(chunk({ role: "assistant", content: '"blue"' }, null));
			response.end(
				`${chunk({}, "stop", { prompt_tokens: 10, completion_tokens: 2, total_tokens: 12 })}data: [DONE]\n\n`,
			);
		});
		await new Promise<void>((done) => provider.listen(0, "127.0.0.1", done));
		const port = (provider.address() as AddressInfo).port;
		writeFileSync(
			join(agentDir, "models.json"),
			JSON.stringify({
				providers: {
					stub: {
						baseUrl: `http://127.0.0.1:${port}/v1`,
						apiKey: "stub-key",
						api: "openai-completions",
						models: [{ id: "frames", cost: { input: 1, output: 1, cacheRead: 0, cacheWrite: 0 } }],
					},
				},
			}),
		);
		writeFileSync(
			join(agentDir, "settings.json"),
			JSON.stringify({ defaultProvider: "stub", defaultModel: "frames", hindsightUrl: "off" }),
		);
		child = spawn(
			process.execPath,
			["--import", sourceResolverPath, cliPath, "mcp", "--frame-model", "no-such-provider/frames"],
			{
				cwd: project,
				env: {
					...process.env,
					[ENV_AGENT_DIR]: agentDir,
					XDG_RUNTIME_DIR: runtimeDir,
					CLAUDE_CODE_SESSION_ID: "e2e-session",
					ULTRON_LOKI: "off",
					ULTRON_HINDSIGHT_URL: "off",
					ULTRON_CLAUDE_CHILDREN: "ultron",
				},
				stdio: ["pipe", "pipe", "pipe"],
			},
		);
		let buffered = "";
		child.stdout.setEncoding("utf8");
		child.stdout.on("data", (chunk: string) => {
			buffered += chunk;
			let newline = buffered.indexOf("\n");
			while (newline !== -1) {
				lines.push(JSON.parse(buffered.slice(0, newline)) as Record<string, unknown>);
				buffered = buffered.slice(newline + 1);
				newline = buffered.indexOf("\n");
			}
			for (const waiter of waiters.splice(0)) waiter();
		});
		child.stderr.setEncoding("utf8");
		child.stderr.on("data", (chunk: string) => {
			stderr += chunk;
		});
		const init = await request("initialize", {
			protocolVersion: "2025-11-25",
			capabilities: {},
			clientInfo: { name: "test", version: "1" },
		});
		expect(init.result).toMatchObject({ serverInfo: { name: "ultron" } });
		expect(String((init.result as { instructions: string }).instructions)).toContain("top of your next rlm result");
	}, 60_000);

	afterAll(async () => {
		child?.stdin.end();
		await new Promise((done) => child?.once("close", done));
		provider?.close();
		rmSync(work, { recursive: true, force: true });
	});

	test("one tool; state persists across calls; errors and secrets are handled like Ultron's own tool", async () => {
		expect((await request("tools/list")).result).toMatchObject({ tools: [{ name: "rlm" }] });
		expect(await cell("x = 41\nprint('hi')")).toEqual({ text: "hi", isError: false });
		expect(await cell("x + 1")).toEqual({ text: "42", isError: false });
		const failed = await cell("1/0");
		expect(failed.isError).toBe(true);
		expect(failed.text).toContain("ZeroDivisionError");
		expect((await cell("print('ghp_' + 'a' * 36)")).text).toBe("[REDACTED:github_token]");
		expect(stderr).not.toMatch(/runtime failed/);
	}, 60_000);

	test("a detached job's end leads the next result; hooks open and close turns", async () => {
		const socket = join(runtimeDir, "ultron-claude", "e2e-session.sock");
		const hook = (event: string, input: Record<string, unknown>) =>
			runHook(event, { socket, input: JSON.stringify({ session_id: "e2e-session", ...input }) });
		expect(await hook("user-prompt", { prompt: "start" })).toBe("");
		const started = await cell("job = await bash('''sleep 0.3; echo finished-job''', yield_after=0)\nprint(job)");
		expect(started.text).toContain("running");
		await new Promise((wake) => setTimeout(wake, 1500));
		const next = await cell("print('after')");
		expect(next.text).toMatch(
			/^<runtime_event kind="job_done" id="job-[^"]+" status="completed" summary="exit 0; finished-job"/,
		);
		expect(next.text.endsWith("after")).toBe(true);
		// An event arriving after the turn goes out with the next prompt instead.
		await cell("job2 = await bash('''sleep 0.3; echo second-job''', yield_after=0)");
		await hook("stop", { last_assistant_message: "done" });
		await new Promise((wake) => setTimeout(wake, 1500));
		const context = JSON.parse(await hook("user-prompt", { prompt: "and now?" }));
		expect(context.hookSpecificOutput.hookEventName).toBe("UserPromptSubmit");
		expect(context.hookSpecificOutput.additionalContext).toContain("second-job");
		expect((await cell("print('clean')")).text).toBe("clean");
		const client = await connectControl(socket);
		try {
			const status = (await client.request("status")) as Record<string, unknown>;
			// claude-code is not installed here: frames fall back to the default model.
			expect(status).toMatchObject({ ready: true, claudeSessionId: "e2e-session", frameModel: "stub/frames" });
			await expect(client.request("inspect", { request: "bash", payload: {} })).rejects.toThrow(/read-only/);
		} finally {
			client.close();
		}
	}, 60_000);

	test("frames run on the frame model and are charged to the usage ledger", async () => {
		const answer = await cell(
			"r = await rlm.infer('What color is the sky? Answer with one word.', contract=str)\nprint(r)",
		);
		expect(answer.text).toContain("blue");
		expect(frameRequests.length).toBeGreaterThan(0);
		const usage = await cell(
			"import json\ns = await agents.status()\nprint(json.dumps({'cost': s['usage']['cost']['spentUsd'] > 0, 'turns': s['usage']['turns']['turns']}))",
		);
		expect(JSON.parse(usage.text)).toEqual({ cost: true, turns: expect.any(Number) });
	}, 60_000);

	test("the watch view renders the session from its socket", async () => {
		const socket = join(runtimeDir, "ultron-claude", "e2e-session.sock");
		const client = await connectControl(socket);
		try {
			const { snapshot, status } = await pollSnapshot(client, new RlmClock());
			expect(status.claudeSessionId).toBe("e2e-session");
			expect(snapshot.cells?.length).toBeGreaterThan(0);
			const pane = new RlmPane({
				snapshot: () => snapshot,
				height: () => 30,
				keybindings: KeybindingsManager.create(),
				style: PLAIN_STYLE,
				focused: () => false,
				onClose: () => {},
				requestRender: () => {},
			});
			const text = renderWatchFrame(pane, snapshot, status, 200, 40, PLAIN_STYLE).join("\n");
			expect(text).toContain("Ultron");
			expect(text).toContain("frames stub/frames");
			expect(text).toMatch(/RLM/);
		} finally {
			client.close();
		}
	}, 30_000);

	test("the session is remembered by Claude Code's session id", () => {
		const remembered = JSON.parse(
			readFileSync(join(work, "agent", "claude-code", "sessions", "e2e-session.json"), "utf8"),
		) as { cwd: string };
		expect(remembered.cwd).toBe(project);
	});
});
