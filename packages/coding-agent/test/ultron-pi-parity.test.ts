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
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import type { SessionTreeNode } from "../src/core/session-manager.ts";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import type { RpcExtensionUIRequest } from "../src/modes/rpc/rpc-types.ts";
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
				content: [{ type: "text", text: "picked=" + picked + " confirmed=" + confirmed + " name=" + name + " timedOut=" + timedOut }],
				details: {},
			};
		},
	});
	pi.registerCommand("from-e", { description: "Loaded with -e", handler: async () => {} });
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
	if (request.lastUser.includes("slow turn")) return { text: "SHOULD_NOT_APPEAR", delayMs: 4_000 };
	return { text: `reply:${request.lastUser.slice(0, 60)}` };
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
		expect(await client.getLastAssistantText()).toBe("picked=b confirmed=true name=Ultron timedOut=undefined");
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
		const second = (await client.getForkMessages()).find((message) => message.text === "second");
		expect(second).toBeDefined();
		// Forking at "second" continues from just before it; the next prompt starts a sibling branch.
		expect(await client.fork(second!.entryId)).toEqual({ text: "second", cancelled: false });
		await client.promptAndWait("alternative", undefined, 60_000);

		const { tree, leafId } = await client.getTree();
		expect(tree).toHaveLength(1);
		const byText = new Map<string, SessionTreeNode>();
		const visit = (node: SessionTreeNode): void => {
			expect(typeof node.entry.timestamp).toBe("string");
			if (node.entry.type === "message") {
				const content = (node.entry.message as { content: unknown }).content;
				const text =
					typeof content === "string"
						? content
						: (content as Array<{ type: string; text?: string }>).map((part) => part.text ?? "").join("");
				byText.set(text, node);
			}
			node.children.forEach(visit);
		};
		tree.forEach(visit);
		const firstReply = byText.get("reply:first");
		expect(firstReply?.children.map((child) => child.entry.id).sort()).toEqual(
			[byText.get("second")!.entry.id, byText.get("alternative")!.entry.id].sort(),
		);
		expect(leafId).toBe(byText.get("reply:alternative")!.entry.id);

		const { entries, leafId: entriesLeaf } = await client.getEntries();
		expect(entriesLeaf).toBe(leafId);
		const userTexts = entries.flatMap((entry) =>
			entry.type === "message" && entry.message.role === "user" ? [entry.message.content] : [],
		);
		expect(JSON.stringify(userTexts)).toContain("second");
		expect(JSON.stringify(userTexts)).toContain("alternative");
		const since = await client.getEntries(entries[0]!.id);
		expect(since.entries).toHaveLength(entries.length - 1);
		expect((await client.getForkMessages()).map((message) => message.text)).toEqual(
			expect.arrayContaining(["first", "second", "alternative"]),
		);

		// Pi's context is the active branch only.
		const messages = await client.getMessages();
		expect(messages.map((message) => message.role)).toEqual(["user", "assistant", "user", "assistant"]);

		const stats = await client.getSessionStats();
		expect(stats).toMatchObject({
			userMessages: 3,
			assistantMessages: 3,
			toolCalls: 0,
			toolResults: 0,
			totalMessages: 6,
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
		expect(stdout).toContain("picked=undefined confirmed=false name=undefined timedOut=undefined");
	}, 90_000);

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
