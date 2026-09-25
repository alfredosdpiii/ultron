/**
 * Pi parity of the native runtime, driven through the real CLI with a scripted provider:
 * - RPC extension UI: worker extensions' ctx.ui dialogs reach Pi's RpcClient as extension_ui_request lines and the
 *   client's extension_ui_response answers flow back; with no client serving (print mode) dialogs get Pi's defaults.
 * - `-e`/`--no-extensions` load extensions as Pi does.
 * - RPC get_tree/get_entries cover every branch in Pi's entry shape; models are full Pi `Model` objects; stats and
 *   compaction results have Pi's shapes.
 * - `-p` print mode: SIGINT aborts the running turn before the process exits.
 */
import { type ChildProcess, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import type { SessionTreeNode } from "../src/core/session-manager.ts";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import type { RpcExtensionUIRequest } from "../src/modes/rpc/rpc-types.ts";
import { importPiSession } from "../src/ultron/migration.ts";
import { ScriptedProvider, type ScriptedReply, type ScriptedRequest } from "./support/scripted-provider.ts";

const cliPath = resolve(__dirname, "../src/cli.ts");
const sourceResolverPath = resolve(__dirname, "../src/experimental/source-resolver.ts");

const ASK_EXTENSION = `export default function (pi) {
	pi.registerTool({
		name: "ask",
		label: "ask",
		description: "Ask the user some questions",
		parameters: { type: "object", properties: {} },
		async execute(_id, _params, _signal, _onUpdate, ctx) {
			ctx.ui.notify("asking now", "info");
			ctx.ui.setStatus("ask", "busy");
			const picked = await ctx.ui.select("Pick a letter", ["a", "b"]);
			const confirmed = await ctx.ui.confirm("Sure?", "Really sure?");
			const name = await ctx.ui.input("Name", "your name");
			const timedOut = await ctx.ui.select("Nobody answers", ["x"], { timeout: 300 });
			return {
				content: [{ type: "text", text: "picked=" + picked + " confirmed=" + confirmed + " name=" + name + " timedOut=" + timedOut + " hasUI=" + ctx.hasUI }],
				details: {},
			};
		},
	});
	pi.registerCommand("from-e", { description: "Loaded with -e", handler: async () => {} });
}
`;

/** Logs Pi's session lifecycle events with ctx.hasUI and ctx.mode to lifecycle.jsonl in the cwd; blocks forks on request. */
const LIFECYCLE_EXTENSION = `import { appendFileSync, existsSync } from "node:fs";
import { join } from "node:path";
export default function (pi) {
	const log = (record) => appendFileSync(join(process.cwd(), "lifecycle.jsonl"), JSON.stringify(record) + "\\n");
	pi.on("session_start", async (event, ctx) => {
		log({ event: "session_start", reason: event.reason, previousSessionFile: event.previousSessionFile, hasUI: ctx.hasUI, mode: ctx.mode });
		if (ctx.hasUI) ctx.ui.notify("started in " + ctx.mode, "info");
	});
	pi.on("session_before_fork", async (event) => {
		log({ event: "session_before_fork", entryId: event.entryId, position: event.position });
		if (existsSync(join(process.cwd(), "block-fork"))) return { cancel: true };
	});
	pi.on("session_shutdown", async (event) => {
		log({ event: "session_shutdown", reason: event.reason, targetSessionFile: event.targetSessionFile });
	});
	pi.registerTool({
		name: "probe",
		label: "probe",
		description: "Report hasUI and mode",
		parameters: { type: "object", properties: {} },
		async execute(_id, _params, _signal, _onUpdate, ctx) {
			return { content: [{ type: "text", text: "hasUI=" + ctx.hasUI + " mode=" + ctx.mode }], details: {} };
		},
	});
}
`;

const DISCOVERED_EXTENSION = `export default function (pi) {
	pi.registerCommand("discovered", { description: "Found in the agent extensions dir", handler: async () => {} });
}
`;

function modelsJson(baseUrl: string): string {
	const model = (id: string) => ({
		id,
		name: id,
		reasoning: false,
		input: ["text"],
		contextWindow: 128000,
		maxTokens: 4096,
		cost: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0 },
	});
	return JSON.stringify({
		providers: {
			scripted: {
				baseUrl,
				api: "openai-completions",
				apiKey: "scripted-key",
				models: [model("scripted"), model("scripted-2")],
			},
		},
	});
}

function script(request: ScriptedRequest): ScriptedReply {
	if (request.lastUser.includes("use the ask tool")) {
		return request.lastToolResult === undefined ? { tool: "ask", args: {} } : { text: request.lastToolResult };
	}
	if (request.lastUser.includes("use the probe tool")) {
		return request.lastToolResult === undefined ? { tool: "probe", args: {} } : { text: request.lastToolResult };
	}
	if (request.lastUser.includes("use the bash tool")) {
		return request.lastToolResult === undefined
			? { tool: "bash", args: { command: "echo first-chunk; sleep 0.5; echo second-chunk" } }
			: { text: "bash done" };
	}
	if (request.lastUser.includes("slow turn")) return { text: "SHOULD_NOT_APPEAR", delayMs: 4_000 };
	return { text: `reply:${request.lastUser.slice(0, 60)}` };
}

function entryText(message: unknown): string {
	const content = (message as { content: unknown }).content;
	return typeof content === "string"
		? content
		: (content as Array<{ type: string; text?: string }>).map((part) => part.text ?? "").join("");
}

/** Message texts of a Pi tree, depth first. */
function texts(tree: SessionTreeNode[]): string[] {
	const out: string[] = [];
	const visit = (node: SessionTreeNode): void => {
		expect(typeof node.entry.timestamp).toBe("string");
		if (node.entry.type === "message") out.push(entryText(node.entry.message));
		node.children.forEach(visit);
	};
	tree.forEach(visit);
	return out;
}

/** A Pi session with two branches after "reply:first" and labels on "first" and on the abandoned "second". */
function writeBranchedPiSession(path: string, cwd: string, sessionId: string): void {
	const at = (seconds: number) => new Date(1_760_000_000_000 + seconds * 1000).toISOString();
	const usage = {
		input: 1,
		output: 1,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens: 2,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
	};
	const user = (id: string, parentId: string | null, text: string, seconds: number) => ({
		type: "message",
		id,
		parentId,
		timestamp: at(seconds),
		message: { role: "user", content: [{ type: "text", text }], timestamp: 1_760_000_000_000 + seconds * 1000 },
	});
	const reply = (id: string, parentId: string, text: string, seconds: number) => ({
		type: "message",
		id,
		parentId,
		timestamp: at(seconds),
		message: {
			role: "assistant",
			content: [{ type: "text", text }],
			api: "openai-completions",
			provider: "scripted",
			model: "scripted",
			usage,
			stopReason: "stop",
			timestamp: 1_760_000_000_000 + seconds * 1000,
		},
	});
	const lines = [
		{ type: "session", version: 3, id: sessionId, timestamp: at(0), cwd },
		user("u1", null, "first", 1),
		reply("a1", "u1", "reply:first", 2),
		user("u2", "a1", "second", 3),
		reply("a2", "u2", "reply:second", 4),
		{ type: "label", id: "l1", parentId: "a2", timestamp: at(5), targetId: "u1", label: "start" },
		{ type: "label", id: "l2", parentId: "l1", timestamp: at(6), targetId: "u2", label: "abandoned" },
		user("u3", "a1", "other", 7),
		reply("a3", "u3", "reply:other", 8),
	];
	writeFileSync(path, `${lines.map((line) => JSON.stringify(line)).join("\n")}\n`);
}

function childOf(client: RpcClient): ChildProcess {
	return (client as unknown as { process: ChildProcess }).process;
}

function awaitExit(child: ChildProcess, timeoutMs: number): Promise<number | null> {
	if (child.exitCode !== null) return Promise.resolve(child.exitCode);
	return new Promise((resolveExit, rejectExit) => {
		const timer = setTimeout(() => rejectExit(new Error("Process did not exit")), timeoutMs);
		child.once("exit", (code) => {
			clearTimeout(timer);
			resolveExit(code);
		});
	});
}

describe("Pi parity through the real CLI", () => {
	let root: string;
	let agentDir: string;
	let projectDir: string;
	let askExtension: string;
	let lifecycleExtension: string;
	let provider: ScriptedProvider;
	const clients: RpcClient[] = [];

	beforeEach(async () => {
		root = mkdtempSync(join(tmpdir(), "ultron-parity-"));
		agentDir = join(root, "agent");
		projectDir = join(root, "project");
		mkdirSync(join(agentDir, "extensions"), { recursive: true });
		mkdirSync(join(projectDir, "exts"), { recursive: true });
		writeFileSync(join(agentDir, "extensions", "discovered.ts"), DISCOVERED_EXTENSION);
		askExtension = join(projectDir, "exts", "ask.ts");
		writeFileSync(askExtension, ASK_EXTENSION);
		lifecycleExtension = join(projectDir, "exts", "lifecycle.ts");
		writeFileSync(lifecycleExtension, LIFECYCLE_EXTENSION);
		provider = new ScriptedProvider(script);
		await provider.start();
		writeFileSync(join(agentDir, "models.json"), modelsJson(provider.baseUrl));
	});

	afterEach(async () => {
		for (const client of clients.splice(0)) await client.stop().catch(() => {});
		await provider.stop();
		rmSync(root, { recursive: true, force: true });
	});

	function env(): Record<string, string> {
		return {
			NODE_OPTIONS: `--import ${sourceResolverPath}`,
			ULTRON_CODING_AGENT_DIR: agentDir,
			ULTRON_HINDSIGHT_URL: "off",
			ULTRON_SERVER_DIR: mkdtempSync(join("/tmp", "u-parity-")),
			PI_OFFLINE: "1",
		};
	}

	function startClient(args: string[]): RpcClient {
		const client = new RpcClient({
			cliPath,
			cwd: projectDir,
			provider: "scripted",
			model: "scripted",
			args,
			env: env(),
		});
		clients.push(client);
		return client;
	}

	test("extension UI requests round-trip over RPC, and -e / --no-extensions load extensions as in Pi", async () => {
		// A relative -e path resolves against the client's cwd, as in Pi.
		const client = startClient(["--no-session", "-e", "exts/ask.ts"]);
		await client.start();
		const listed = await client.getCommands();
		expect(listed.map((command) => command.name)).toEqual(expect.arrayContaining(["from-e", "discovered"]));
		expect(listed.find((command) => command.name === "from-e")?.sourceInfo).toMatchObject({ path: askExtension });

		const requests: RpcExtensionUIRequest[] = [];
		const stdin = childOf(client).stdin!;
		const respond = (response: object) => stdin.write(`${JSON.stringify(response)}\n`);
		client.onEvent((event) => {
			const request = event as unknown as RpcExtensionUIRequest;
			if (request.type !== "extension_ui_request") return;
			requests.push(request);
			if (request.method === "select" && request.title === "Pick a letter") {
				respond({ type: "extension_ui_response", id: request.id, value: "b" });
			} else if (request.method === "confirm") {
				respond({ type: "extension_ui_response", id: request.id, confirmed: true });
			} else if (request.method === "input") {
				respond({ type: "extension_ui_response", id: request.id, value: "Ultron" });
			}
			// "Nobody answers" is left unanswered: its own timeout gives Pi's default.
		});

		await client.promptAndWait("use the ask tool", undefined, 60_000);
		expect(await client.getLastAssistantText()).toBe(
			"picked=b confirmed=true name=Ultron timedOut=undefined hasUI=true",
		);
		expect(requests).toEqual([
			expect.objectContaining({ method: "notify", message: "asking now", notifyType: "info" }),
			expect.objectContaining({ method: "setStatus", statusKey: "ask", statusText: "busy" }),
			expect.objectContaining({ method: "select", title: "Pick a letter", options: ["a", "b"] }),
			expect.objectContaining({ method: "confirm", title: "Sure?", message: "Really sure?" }),
			expect.objectContaining({ method: "input", title: "Name", placeholder: "your name" }),
			expect.objectContaining({ method: "select", title: "Nobody answers", options: ["x"], timeout: 300 }),
		]);

		// --no-extensions skips discovery; explicit -e paths still load.
		const bare = startClient(["--no-session", "--no-extensions", "-e", askExtension]);
		await bare.start();
		const bareCommands = (await bare.getCommands()).map((command) => command.name);
		expect(bareCommands).toContain("from-e");
		expect(bareCommands).not.toContain("discovered");
	}, 120_000);

	test("the system prompt has Pi's tool snippets and guidelines, and the profile's SYSTEM.md replaces it as in Pi", async () => {
		const plain = startClient(["--no-session"]);
		await plain.start();
		await plain.promptAndWait("hello", undefined, 60_000);
		const system = provider.requests.at(-1)!.system;
		expect(system).toContain("Use read to examine files instead of cat or sed.");
		expect(system).toContain("Use write only for new files or complete rewrites.");
		expect(system).toContain("including multiple disjoint edits in one call");
		expect(system).toContain("- rlm: ");

		// Tools the model cannot call are not advertised, as in Pi.
		const without = startClient(["--no-session", "--exclude-tools", "rlm,edit"]);
		await without.start();
		await without.promptAndWait("hello without", undefined, 60_000);
		const narrowed = provider.requests.at(-1)!;
		expect(narrowed.system).not.toContain("- rlm: ");
		expect(narrowed.system).not.toContain("- edit: ");
		expect(narrowed.system).toContain("- bash: ");
		expect(narrowed.raw).not.toContain('"name":"rlm"');

		writeFileSync(join(agentDir, "SYSTEM.md"), "You are the parity test prompt.");
		const custom = startClient(["--no-session"]);
		await custom.start();
		await custom.promptAndWait("hello again", undefined, 60_000);
		expect(provider.requests.at(-1)!.system.startsWith("You are the parity test prompt.")).toBe(true);
	}, 120_000);

	test("get_tree and get_entries cover every branch; models, stats, and compaction have Pi's shapes", async () => {
		const client = startClient(["--no-session"]);
		await client.start();

		// Full Pi Model objects.
		const models = (await client.getAvailableModels()).filter((model) => model.provider === "scripted");
		expect(models.map((model) => model.id)).toEqual(["scripted", "scripted-2"]);
		for (const model of models) {
			expect(model).toMatchObject({
				provider: "scripted",
				api: "openai-completions",
				baseUrl: provider.baseUrl,
				contextWindow: 128000,
				maxTokens: 4096,
				input: ["text"],
				cost: { input: 1, output: 2 },
			});
		}
		expect((await client.getState()).model).toMatchObject({ id: "scripted", api: "openai-completions" });
		expect(await client.setModel("scripted", "scripted-2")).toMatchObject({
			id: "scripted-2",
			baseUrl: provider.baseUrl,
		});
		const cycled = await client.cycleModel();
		expect(cycled).toMatchObject({ isScoped: false });
		expect(cycled?.model).toEqual(expect.objectContaining({ api: expect.any(String), baseUrl: expect.any(String) }));
		await client.setModel("scripted", "scripted");

		await client.promptAndWait("first", undefined, 60_000);
		await client.promptAndWait("second", undefined, 60_000);
		const sourceSession = (await client.getState()).sessionId;
		const second = (await client.getForkMessages()).find((message) => message.text === "second");
		expect(second).toBeDefined();
		// As in Pi, forking at "second" makes a new session with the path to just before it.
		expect(await client.fork(second!.entryId)).toEqual({ text: "second", cancelled: false });
		expect((await client.getState()).sessionId).not.toBe(sourceSession);
		await client.promptAndWait("alternative", undefined, 60_000);

		const { tree, leafId } = await client.getTree();
		expect(texts(tree)).toEqual(["first", "reply:first", "alternative", "reply:alternative"]);

		const { entries, leafId: entriesLeaf } = await client.getEntries();
		expect(entriesLeaf).toBe(leafId);
		const since = await client.getEntries(entries[0]!.id);
		expect(since.entries).toHaveLength(entries.length - 1);
		expect((await client.getForkMessages()).map((message) => message.text)).toEqual(["first", "alternative"]);

		// Pi's context is the active branch only.
		const messages = await client.getMessages();
		expect(messages.map((message) => message.role)).toEqual(["user", "assistant", "user", "assistant"]);

		const stats = await client.getSessionStats();
		expect(stats).toMatchObject({
			userMessages: 2,
			assistantMessages: 2,
			toolCalls: 0,
			toolResults: 0,
			totalMessages: 4,
			contextUsage: { contextWindow: 128000 },
		});
		expect(stats.tokens.total).toBeGreaterThan(0);
		expect(stats.sessionFile).toMatch(/\.jsonl$/);
		expect((await client.getState()).sessionFile).toBe(stats.sessionFile);

		const compacted = await client.compact();
		expect(typeof compacted.summary).toBe("string");
		expect(typeof compacted.firstKeptEntryId).toBe("string");
		expect(typeof compacted.tokensBefore).toBe("number");
	}, 120_000);

	test("fork and clone copy only the path to the entry, from any branch, keeping its labels, as in Pi", async () => {
		const piPath = join(root, "branched.jsonl");
		writeBranchedPiSession(piPath, projectDir, "0192a0b0-0000-7000-8000-00000000f0a1");
		const imported = await importPiSession({
			piSessionPath: piPath,
			sessionsRoot: join(agentDir, "experimental", "sessions"),
		});
		const client = startClient(["--session-id", imported.sessionId]);
		await client.start();
		const whole = await client.getTree();
		expect(texts(whole.tree)).toEqual(["first", "reply:first", "second", "reply:second", "other", "reply:other"]);

		// "second" is on the abandoned branch; the fork holds only root → just before it, with the label on "first".
		const forkMessages = await client.getForkMessages();
		const second = forkMessages.find((message) => message.text === "second")!;
		expect(await client.fork(second.entryId)).toEqual({ text: "second", cancelled: false });
		const forked = await client.getState();
		expect(forked.sessionId).not.toBe(imported.sessionId);
		const forkTree = await client.getTree();
		expect(texts(forkTree.tree)).toEqual(["first", "reply:first"]);
		expect(forkTree.tree[0]!.label).toBe("start");
		expect(forkTree.leafId).toBe(forkTree.tree[0]!.children[0]!.entry.id);
		expect((await client.getMessages()).map((message) => message.role)).toEqual(["user", "assistant"]);

		// Clone copies the path to the current leaf into another new session.
		await client.promptAndWait("third", undefined, 60_000);
		expect(await client.clone()).toEqual({ cancelled: false });
		expect((await client.getState()).sessionId).not.toBe(forked.sessionId);
		expect(texts((await client.getTree()).tree)).toEqual(["first", "reply:first", "third", "reply:third"]);

		// Forking at the first message gives an empty session, as in Pi.
		const first = (await client.getForkMessages()).find((message) => message.text === "first")!;
		expect(await client.fork(first.entryId)).toEqual({ text: "first", cancelled: false });
		expect((await client.getTree()).tree).toEqual([]);
		expect(await client.getMessages()).toEqual([]);
		await client.promptAndWait("fresh", undefined, 60_000);
		expect(texts((await client.getTree()).tree)).toEqual(["fresh", "reply:fresh"]);
	}, 120_000);

	test("RPC streams Pi's tool execution lifecycle: start, updates, end", async () => {
		const client = startClient(["--no-session"]);
		await client.start();
		const events: Array<Record<string, unknown>> = [];
		client.onEvent((event) => {
			const record = event as unknown as Record<string, unknown>;
			if (typeof record.type === "string" && record.type.startsWith("tool_execution_")) events.push(record);
		});
		await client.promptAndWait("use the bash tool", undefined, 60_000);
		expect(await client.getLastAssistantText()).toBe("bash done");
		expect(events[0]).toMatchObject({
			type: "tool_execution_start",
			toolName: "bash",
			args: { command: "echo first-chunk; sleep 0.5; echo second-chunk" },
		});
		const toolCallId = events[0]!.toolCallId;
		expect(typeof toolCallId).toBe("string");
		const updates = events.slice(1, -1);
		expect(updates.length).toBeGreaterThan(0);
		for (const update of updates) {
			expect(update).toMatchObject({
				type: "tool_execution_update",
				toolCallId,
				toolName: "bash",
				args: { command: "echo first-chunk; sleep 0.5; echo second-chunk" },
				partialResult: { content: expect.any(Array) },
			});
		}
		expect(JSON.stringify(updates.at(-1)!.partialResult)).toContain("first-chunk");
		expect(events.at(-1)).toMatchObject({
			type: "tool_execution_end",
			toolCallId,
			toolName: "bash",
			isError: false,
			result: { content: [expect.objectContaining({ type: "text" })] },
		});
		expect(JSON.stringify(events.at(-1)!.result)).toContain("second-chunk");
	}, 90_000);

	function lifecycle(): Array<Record<string, unknown>> {
		const path = join(projectDir, "lifecycle.jsonl");
		if (!existsSync(path)) return [];
		return readFileSync(path, "utf8")
			.split("\n")
			.filter(Boolean)
			.map((line) => JSON.parse(line) as Record<string, unknown>);
	}

	async function waitFor<T>(read: () => T | undefined, timeoutMs = 20_000): Promise<T> {
		const deadline = Date.now() + timeoutMs;
		while (true) {
			const value = read();
			if (value !== undefined) return value;
			if (Date.now() > deadline) throw new Error("Timed out waiting for a condition");
			await new Promise((resolveWait) => setTimeout(resolveWait, 100));
		}
	}

	test("session_start sees Pi's hasUI and mode per client; its UI calls reach the client that is coming", async () => {
		const notices: string[] = [];
		const client = startClient(["--no-session", "-e", lifecycleExtension]);
		client.onEvent((event) => {
			const request = event as unknown as RpcExtensionUIRequest;
			if (request.type === "extension_ui_request" && request.method === "notify") notices.push(request.message);
		});
		await client.start();
		await client.promptAndWait("use the probe tool", undefined, 60_000);
		expect(await client.getLastAssistantText()).toBe("hasUI=true mode=rpc");
		expect(lifecycle()[0]).toMatchObject({ event: "session_start", reason: "startup", hasUI: true, mode: "rpc" });
		// The notify from session_start, before the client attached, was queued for it.
		expect(notices).toContain("started in rpc");

		for (const [args, mode] of [
			[["-p"], "print"],
			[["--mode", "json", "-p"], "json"],
		] as const) {
			rmSync(join(projectDir, "lifecycle.jsonl"), { force: true });
			const child = spawn(
				"node",
				[
					cliPath,
					"--provider",
					"scripted",
					"--model",
					"scripted",
					"--no-session",
					"-e",
					lifecycleExtension,
					...args,
					"use the probe tool",
				],
				{ cwd: projectDir, env: { ...process.env, ...env() }, stdio: ["ignore", "pipe", "pipe"] },
			);
			let stdout = "";
			child.stdout!.on("data", (data: Buffer) => {
				stdout += data.toString();
			});
			expect(await awaitExit(child, 60_000)).toBe(0);
			expect(stdout).toContain(`hasUI=false mode=${mode}`);
			expect(lifecycle()[0]).toMatchObject({ event: "session_start", reason: "startup", hasUI: false, mode });
		}
	}, 150_000);

	test("forks emit Pi's session_before_fork (cancellable), session_start reason fork, and session_shutdown", async () => {
		const client = startClient(["--no-session", "-e", lifecycleExtension]);
		await client.start();
		await client.promptAndWait("first", undefined, 60_000);
		await client.promptAndWait("second", undefined, 60_000);
		const source = await client.getState();
		const second = (await client.getForkMessages()).find((message) => message.text === "second")!;

		// An extension cancels: no new session, the client stays where it was.
		writeFileSync(join(projectDir, "block-fork"), "");
		expect(await client.fork(second.entryId)).toEqual({ cancelled: true });
		expect(await client.clone()).toEqual({ cancelled: true });
		expect((await client.getState()).sessionId).toBe(source.sessionId);
		rmSync(join(projectDir, "block-fork"));

		expect(await client.fork(second.entryId)).toEqual({ text: "second", cancelled: false });
		const forked = await client.getState();
		expect(forked.sessionId).not.toBe(source.sessionId);
		const leafBeforeFork = source.sessionFile;
		const events = lifecycle();
		expect(events.filter((event) => event.event === "session_before_fork")).toEqual([
			{ event: "session_before_fork", entryId: second.entryId, position: "before" },
			expect.objectContaining({ event: "session_before_fork", position: "at" }),
			{ event: "session_before_fork", entryId: second.entryId, position: "before" },
		]);
		// The fork's extensions start with reason "fork" and the source file; the source retires with reason "fork".
		expect(
			await waitFor(() => lifecycle().find((event) => event.event === "session_start" && event.reason === "fork")),
		).toMatchObject({ previousSessionFile: leafBeforeFork, hasUI: true, mode: "rpc" });
		expect(await waitFor(() => lifecycle().find((event) => event.event === "session_shutdown"))).toMatchObject({
			reason: "fork",
			targetSessionFile: forked.sessionFile,
		});
	}, 150_000);

	test("print mode: with no client serving, extension dialogs get Pi's defaults", async () => {
		const child = spawn(
			"node",
			[
				cliPath,
				"--provider",
				"scripted",
				"--model",
				"scripted",
				"--no-session",
				"-e",
				askExtension,
				"-p",
				"use the ask tool",
			],
			{ cwd: projectDir, env: { ...process.env, ...env() }, stdio: ["ignore", "pipe", "pipe"] },
		);
		let stdout = "";
		child.stdout!.on("data", (data: Buffer) => {
			stdout += data.toString();
		});
		expect(await awaitExit(child, 60_000)).toBe(0);
		expect(stdout).toContain("picked=undefined confirmed=false name=undefined timedOut=undefined hasUI=false");
	}, 90_000);

	test("print mode: provider requests carry Pi's request timeout, from settings when set", async () => {
		const run = async (prompt: string) => {
			const child = spawn(
				"node",
				[cliPath, "--provider", "scripted", "--model", "scripted", "--no-session", "-p", prompt],
				{ cwd: projectDir, env: { ...process.env, ...env() }, stdio: ["ignore", "pipe", "pipe"] },
			);
			expect(await awaitExit(child, 60_000)).toBe(0);
			return provider.requests.find((request) => request.lastUser === prompt);
		};
		// Pi sends its HTTP idle timeout (5 min) as the SDK request timeout; the OpenAI SDK reports it in seconds.
		expect((await run("default timeout"))?.headers["x-stainless-timeout"]).toBe("300");
		writeFileSync(join(agentDir, "settings.json"), JSON.stringify({ retry: { provider: { timeoutMs: 45_000 } } }));
		expect((await run("configured timeout"))?.headers["x-stainless-timeout"]).toBe("45");
	}, 120_000);

	test("print mode: SIGINT aborts the running turn before exiting", async () => {
		const sessionId = randomUUID();
		const child = spawn(
			"node",
			[cliPath, "--provider", "scripted", "--model", "scripted", "--session-id", sessionId, "-p", "a slow turn"],
			{ cwd: projectDir, env: { ...process.env, ...env() }, stdio: ["ignore", "pipe", "pipe"] },
		);
		const deadline = Date.now() + 30_000;
		while (!provider.requests.some((request) => request.lastUser.includes("slow turn"))) {
			if (Date.now() > deadline) throw new Error("The turn never reached the provider");
			await new Promise((resolveWait) => setTimeout(resolveWait, 50));
		}
		const signalledAt = Date.now();
		child.kill("SIGINT");
		expect(await awaitExit(child, 10_000)).toBe(130);
		expect(Date.now() - signalledAt).toBeLessThan(4_000);

		// The turn was aborted, not left running: after the slow reply would have arrived, it is not in the session.
		await new Promise((resolveWait) => setTimeout(resolveWait, 4_500));
		const later = startClient(["--session-id", sessionId]);
		await later.start();
		const messages = await later.getMessages();
		expect(JSON.stringify(messages)).not.toContain("SHOULD_NOT_APPEAR");
		const assistant = messages.find((message) => message.role === "assistant");
		expect(assistant).toMatchObject({ stopReason: "aborted" });
	}, 90_000);
});
