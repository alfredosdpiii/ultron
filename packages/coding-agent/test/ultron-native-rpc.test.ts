import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type Server } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { RpcEventTranslator } from "../src/experimental/rpc-events.ts";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";

const cliPath = resolve(__dirname, "../src/cli.ts");
const sourceResolverPath = resolve(__dirname, "../src/experimental/source-resolver.ts");

describe("RpcEventTranslator", () => {
	test("reconstructs Pi turn and agent boundaries from lane events", () => {
		const translator = new RpcEventTranslator();
		const user = { role: "user" as const, content: "hi", timestamp: 1 };
		const assistant = {
			role: "assistant" as const,
			content: [{ type: "text" as const, text: "ok" }],
			api: "openai-completions" as const,
			provider: "mock",
			model: "mock",
			usage: {
				input: 1,
				output: 1,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 2,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
			stopReason: "stop" as const,
			timestamp: 2,
		};
		const types = [
			{ type: "run_start", runId: "r", startedAt: 0 },
			{ type: "message_start", runId: "r", message: user },
			{ type: "message_end", runId: "r", message: user },
			{ type: "message_start", runId: "r", message: assistant },
			{
				type: "message_update",
				runId: "r",
				message: assistant,
				frame: { type: "text_delta", contentIndex: 0, delta: "ok" },
			},
			{ type: "message_end", runId: "r", message: assistant },
			{ type: "run_end", runId: "r", fromTipId: null, tipId: "t", endedAt: 1, status: "completed" },
		].flatMap((event) => translator.translate(event as never).map(({ type }) => type));

		expect(types).toEqual([
			"agent_start",
			"message_start",
			"message_end",
			"turn_start",
			"message_start",
			"message_update",
			"message_end",
			"turn_end",
			"agent_end",
			"agent_settled",
		]);
	});
});

describe("native RPC mode", () => {
	let root: string;
	let provider: Server;
	let client: RpcClient | undefined;

	beforeEach(async () => {
		root = mkdtempSync(join(tmpdir(), "ultron-rpc-"));
		provider = createServer(async (request, response) => {
			for await (const _chunk of request) {
				// Drain the request body.
			}
			response.writeHead(200, { "content-type": "text/event-stream" });
			const chunk = (delta: object, finishReason: string | null, usage?: object) =>
				`data: ${JSON.stringify({
					id: "mock",
					object: "chat.completion.chunk",
					created: 0,
					model: "mock",
					choices: [{ index: 0, delta, finish_reason: finishReason }],
					...(usage === undefined ? {} : { usage }),
				})}\n\n`;
			response.write(chunk({ role: "assistant", content: "NATIVE_RPC_OK" }, null));
			response.write(chunk({}, "stop", { prompt_tokens: 3, completion_tokens: 1, total_tokens: 4 }));
			response.end("data: [DONE]\n\n");
		});
		await new Promise<void>((resolveListen) => provider.listen(0, "127.0.0.1", resolveListen));
	});

	afterEach(async () => {
		await client?.stop().catch(() => {});
		client = undefined;
		await new Promise<void>((resolveClose) => provider.close(() => resolveClose()));
		rmSync(root, { recursive: true, force: true });
	});

	function startClient(): RpcClient {
		const address = provider.address();
		if (address === null || typeof address === "string") throw new Error("Provider is not listening");
		const agentDir = join(root, "agent");
		const projectDir = join(root, "project");
		mkdirSync(join(agentDir, "extensions"), { recursive: true });
		mkdirSync(projectDir, { recursive: true });
		writeFileSync(
			join(agentDir, "extensions", "hello.ts"),
			'export default function (pi) { pi.registerCommand("ultron-hello", { description: "Say hello", handler: async () => {} }); }\n',
		);
		writeFileSync(
			join(agentDir, "models.json"),
			JSON.stringify({
				providers: {
					mock: {
						baseUrl: `http://127.0.0.1:${address.port}/v1`,
						api: "openai-completions",
						apiKey: "mock-key",
						models: [
							{
								id: "mock",
								name: "mock",
								reasoning: false,
								input: ["text"],
								contextWindow: 32000,
								maxTokens: 256,
							},
						],
					},
				},
			}),
		);
		return new RpcClient({
			cliPath,
			cwd: projectDir,
			provider: "mock",
			model: "mock",
			args: ["--no-session"],
			env: {
				NODE_OPTIONS: `--import ${sourceResolverPath}`,
				ULTRON_CODING_AGENT_DIR: agentDir,
				ULTRON_SERVER_DIR: mkdtempSync(join("/tmp", "u-rpc-")),
				PI_OFFLINE: "1",
			},
		});
	}

	test("runs prompts, bash, and session commands through the native worker", async () => {
		client = startClient();
		await client.start();

		const state = await client.getState();
		expect(state.model).toMatchObject({ provider: "mock", id: "mock" });
		expect(state.isStreaming).toBe(false);

		const events = await client.promptAndWait("say the marker");
		expect(events.map(({ type }) => type)).toEqual(
			expect.arrayContaining(["agent_start", "turn_start", "message_end", "turn_end", "agent_end", "agent_settled"]),
		);
		expect(await client.getLastAssistantText()).toBe("NATIVE_RPC_OK");

		const status = (await (
			client as unknown as { send(command: object): Promise<{ success: boolean; data?: unknown }> }
		).send({
			type: "inspect",
			request: "agents.status",
		})) as { success: boolean; data?: { controls?: Record<string, boolean> } };
		expect(status.success).toBe(true);
		// Optional controls are reported, and all of them are off by default.
		expect(Object.values(status.data?.controls ?? {}).every((enabled) => enabled === false)).toBe(true);
		const refused = (await (client as unknown as { send(command: object): Promise<{ success: boolean }> }).send({
			type: "inspect",
			request: "agents.spawn",
		})) as { success: boolean };
		expect(refused.success).toBe(false);

		const commands = await client.getCommands();
		expect(commands).toContainEqual(expect.objectContaining({ name: "ultron-hello", source: "extension" }));

		const bash = await client.bash("echo native-bash");
		expect(bash).toMatchObject({ output: "native-bash\n", exitCode: 0, cancelled: false });

		await client.setSessionName("Native RPC");
		expect((await client.getState()).sessionName).toBe("Native RPC");

		const messages = await client.getMessages();
		expect(messages.map(({ role }) => role)).toEqual(["user", "assistant", "bashExecution"]);

		const exported = await client.exportHtml(join(root, "session.html"));
		const html = readFileSync(exported.path, "utf8");
		const encoded = /id="session-data"[^>]*>([^<]+)</.exec(html)?.[1];
		expect(encoded).toBeDefined();
		const sessionData = JSON.parse(Buffer.from(encoded!, "base64").toString("utf8")) as {
			entries: Array<{ type: string; message?: { role: string } }>;
		};
		expect(sessionData.entries.map((entry) => entry.message?.role)).toEqual(["user", "assistant", "bashExecution"]);
	}, 60_000);
});
