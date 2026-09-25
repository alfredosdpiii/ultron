/**
 * A22: detached work and retained children survive UI loss.
 *
 * Real CLI processes drive a real foreground server, coordinator, and Session worker. The first client starts a
 * background job and leaves while the child lane's model request is still in flight; the child must finish, and a
 * second client must reattach to the same Session and read the durable result. Afterwards, and in the idle case,
 * no Ultron process may remain.
 *
 * Leaving the app is not an abort: a root turn in flight when its client quits (SIGTERM or RPC stdin close) runs to
 * completion, tool calls included, and a later client sees it; an explicit abort still stops it. A hard-killed client
 * sends no release, yet idle work exits within seconds while busy work finishes as if it had been released.
 */
import type { ChildProcess } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { join, resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";

const cliPath = resolve(__dirname, "../src/cli.ts");
const sourceResolverPath = resolve(__dirname, "../src/experimental/source-resolver.ts");

const ROOT_PROMPT = "start the slow job";
const CHILD_PROMPT = "slow child work";
const CHILD_RESULT = "CHILD_RESULT_A22";
const LONG_PROMPT = "a long root turn";
const LONG_TOOL_OUTPUT = "LONG_TOOL_RAN";
const LONG_RESULT = "LONG_TURN_DONE";

interface ChatMessage {
	readonly role: string;
	readonly content?: unknown;
}

function textOf(content: unknown): string {
	if (typeof content === "string") return content;
	if (!Array.isArray(content)) return "";
	return content
		.map((part: unknown) =>
			typeof part === "object" && part !== null && "text" in part && typeof part.text === "string" ? part.text : "",
		)
		.join("");
}

function chunk(delta: object, finishReason: string | null, usage?: object): string {
	return `data: ${JSON.stringify({
		id: "mock",
		object: "chat.completion.chunk",
		created: 0,
		model: "mock",
		choices: [{ index: 0, delta, finish_reason: finishReason }],
		...(usage === undefined ? {} : { usage }),
	})}\n\n`;
}

const USAGE = { prompt_tokens: 3, completion_tokens: 1, total_tokens: 4 };

function respondText(response: ServerResponse, text: string): void {
	response.writeHead(200, { "content-type": "text/event-stream" });
	response.write(chunk({ role: "assistant", content: text }, null));
	response.write(chunk({}, "stop", USAGE));
	response.end("data: [DONE]\n\n");
}

function respondRlm(response: ServerResponse, code: string): void {
	response.writeHead(200, { "content-type": "text/event-stream" });
	response.write(
		chunk(
			{
				role: "assistant",
				tool_calls: [
					{
						index: 0,
						id: `call_${randomUUID().slice(0, 8)}`,
						type: "function",
						function: { name: "rlm", arguments: JSON.stringify({ code }) },
					},
				],
			},
			null,
		),
	);
	response.write(chunk({}, "tool_calls", USAGE));
	response.end("data: [DONE]\n\n");
}

/** Command lines of live processes that belong to this test's isolated directories. */
function ownedProcesses(root: string): { pid: number; cmdline: string }[] {
	const owned: { pid: number; cmdline: string }[] = [];
	for (const entry of readdirSync("/proc")) {
		if (!/^\d+$/.test(entry)) continue;
		try {
			const cmdline = readFileSync(`/proc/${entry}/cmdline`, "utf8").replaceAll("\0", " ");
			if (cmdline.includes(root)) owned.push({ pid: Number(entry), cmdline });
		} catch {
			// The process exited while scanning.
		}
	}
	return owned;
}

function workerPids(root: string): number[] {
	return ownedProcesses(root)
		.filter(({ cmdline }) => cmdline.includes("session-worker"))
		.map(({ pid }) => pid);
}

function sockets(directory: string): string[] {
	if (!existsSync(directory)) return [];
	return readdirSync(directory).filter((name) => name.endsWith(".sock"));
}

async function poll<T>(read: () => T, done: (value: T) => boolean, timeoutMs: number): Promise<T> {
	const deadline = Date.now() + timeoutMs;
	while (true) {
		const value = read();
		if (done(value) || Date.now() >= deadline) return value;
		await new Promise((resolveWait) => setTimeout(resolveWait, 50));
	}
}

function childProcessOf(client: RpcClient): ChildProcess {
	const child = (client as unknown as { process: ChildProcess | null }).process;
	if (!child) throw new Error("RPC client has no process");
	return child;
}

/** Wait for a client CLI to exit on its own and report how long that took. */
async function awaitExit(child: ChildProcess, timeoutMs: number): Promise<{ elapsedMs: number; code: number | null }> {
	const started = Date.now();
	if (child.exitCode !== null || child.signalCode !== null) return { elapsedMs: 0, code: child.exitCode };
	return new Promise((resolveExit, rejectExit) => {
		const timer = setTimeout(() => rejectExit(new Error("Client CLI did not exit")), timeoutMs);
		child.once("exit", (code) => {
			clearTimeout(timer);
			resolveExit({ elapsedMs: Date.now() - started, code });
		});
	});
}

function toolResultTexts(messages: readonly unknown[]): string[] {
	return messages.flatMap((message) =>
		typeof message === "object" && message !== null && "role" in message && message.role === "toolResult"
			? [textOf((message as { content?: unknown }).content)]
			: [],
	);
}

describe.skipIf(process.platform !== "linux")("A22 detached work survives UI loss", () => {
	let root: string;
	let serverDir: string;
	let provider: Server;
	let collectCode: string | undefined;
	let childRequests = 0;
	let childCompleted = false;
	let childRequested!: Promise<void>;
	let resolveChildRequested!: () => void;
	let releaseChild!: () => void;
	let childGate!: Promise<void>;
	let longRequests = 0;
	let longToolResults = 0;
	let longCompleted = false;
	let longRequested!: Promise<void>;
	let resolveLongRequested!: () => void;
	let releaseLong!: () => void;
	let longGate!: Promise<void>;
	const clients: RpcClient[] = [];

	beforeEach(async () => {
		// Short paths keep Unix socket names under the platform limit.
		root = mkdtempSync("/tmp/u-a22-");
		serverDir = join(root, "s");
		collectCode = undefined;
		childRequests = 0;
		childCompleted = false;
		childRequested = new Promise((resolveRequested) => {
			resolveChildRequested = resolveRequested;
		});
		childGate = new Promise((resolveGate) => {
			releaseChild = resolveGate;
		});
		longRequests = 0;
		longToolResults = 0;
		longCompleted = false;
		longRequested = new Promise((resolveRequested) => {
			resolveLongRequested = resolveRequested;
		});
		longGate = new Promise((resolveGate) => {
			releaseLong = resolveGate;
		});
		provider = createServer((request: IncomingMessage, response: ServerResponse) => {
			void (async () => {
				let body = "";
				for await (const part of request) body += String(part);
				const messages = (JSON.parse(body) as { messages: ChatMessage[] }).messages;
				const last = messages.at(-1);
				const lastText = textOf(last?.content);
				const longTurn = messages.some(
					(message) => message.role === "user" && textOf(message.content) === LONG_PROMPT,
				);
				if (longTurn && last?.role === "tool") {
					longToolResults += 1;
					respondText(response, `${LONG_RESULT}: ${textOf(last.content).trim()}`);
					longCompleted = true;
				} else if (longTurn && lastText === LONG_PROMPT) {
					// The root turn's model reply is held while its client goes away.
					longRequests += 1;
					resolveLongRequested();
					await longGate;
					respondRlm(response, `print(${JSON.stringify(LONG_TOOL_OUTPUT)})`);
				} else if (last?.role === "tool") {
					respondText(response, "turn done");
				} else if (lastText.includes(CHILD_PROMPT) && !lastText.includes(ROOT_PROMPT)) {
					childRequests += 1;
					resolveChildRequested();
					await childGate;
					respondText(response, CHILD_RESULT);
					childCompleted = true;
				} else if (lastText.includes(ROOT_PROMPT)) {
					respondRlm(
						response,
						`import json\njob = await background.start(${JSON.stringify(CHILD_PROMPT)})\nprint("JOB " + json.dumps(job))`,
					);
				} else if (lastText.includes("collect the job") && collectCode !== undefined) {
					respondRlm(response, collectCode);
				} else {
					respondText(response, "IDLE_OK");
				}
			})().catch((error: unknown) => {
				response.destroy(error instanceof Error ? error : new Error(String(error)));
			});
		});
		await new Promise<void>((resolveListen) => provider.listen(0, "127.0.0.1", resolveListen));
	});

	afterEach(async () => {
		releaseChild();
		releaseLong();
		for (const client of clients.splice(0)) await client.stop().catch(() => {});
		for (const { pid } of ownedProcesses(root)) {
			try {
				process.kill(pid, "SIGKILL");
			} catch {
				// Already gone.
			}
		}
		await new Promise<void>((resolveClose) => provider.close(() => resolveClose()));
		rmSync(root, { recursive: true, force: true });
	});

	function writeModels(): { agentDir: string; projectDir: string } {
		const address = provider.address();
		if (address === null || typeof address === "string") throw new Error("Provider is not listening");
		const agentDir = join(root, "agent");
		const projectDir = join(root, "project");
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(projectDir, { recursive: true });
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
		return { agentDir, projectDir };
	}

	function cliEnv(agentDir: string, extra: Record<string, string> = {}): Record<string, string> {
		return {
			NODE_OPTIONS: `--import ${sourceResolverPath}`,
			ULTRON_CODING_AGENT_DIR: agentDir,
			ULTRON_SERVER_DIR: serverDir,
			PI_OFFLINE: "1",
			...extra,
		};
	}

	function startClient(sessionId: string, env: Record<string, string> = {}): RpcClient {
		const { agentDir, projectDir } = writeModels();
		const client = new RpcClient({
			cliPath,
			cwd: projectDir,
			provider: "mock",
			model: "mock",
			args: ["--session-id", sessionId],
			env: cliEnv(agentDir, env),
		});
		clients.push(client);
		return client;
	}

	interface StartedJob {
		readonly id: string;
		readonly definition: string;
		readonly parentId?: string;
	}

	/** Start a background job from a real rlm tool call, then lose the UI while the child request is in flight. */
	async function startJobAndLoseClient(
		sessionId: string,
		signal: "SIGTERM" | "SIGKILL" = "SIGTERM",
	): Promise<{ job: StartedJob; workerPid: number }> {
		const first = startClient(sessionId);
		await first.start();
		await first.promptAndWait(ROOT_PROMPT, undefined, 60_000);
		await childRequested;

		const jobLine = toolResultTexts(await first.getMessages())
			.flatMap((text) => text.split("\n"))
			.find((line) => line.startsWith("JOB "));
		expect(jobLine).toBeDefined();
		const job = JSON.parse(jobLine!.slice(4)) as StartedJob;
		expect(job.definition).toBe("background-job@1");
		const [workerPid] = workerPids(root);
		expect(workerPid).toBeDefined();

		// The UI goes away while the child is in flight: SIGTERM, as from a closed terminal or a supervisor, releases
		// the worker; SIGKILL, as from a crash or the OOM killer, sends nothing.
		const firstProcess = childProcessOf(first);
		firstProcess.kill(signal);
		const firstExit = await awaitExit(firstProcess, 10_000);
		if (signal === "SIGTERM") expect(firstExit.code).toBe(143);
		else expect(firstProcess.signalCode).toBe("SIGKILL");
		expect(firstExit.elapsedMs).toBeLessThan(5_000);
		clients.splice(clients.indexOf(first), 1);

		// The busy worker was handed off, not stopped: it still runs the child whose request is unanswered.
		await new Promise((resolveWait) => setTimeout(resolveWait, 500));
		expect(workerPids(root)).toEqual([workerPid]);
		expect(childCompleted).toBe(false);
		return { job, workerPid: workerPid! };
	}

	/** Read the job's result and task record through a scripted rlm call in the given client. */
	async function collect(client: RpcClient, job: StartedJob): Promise<void> {
		collectCode = [
			"import json",
			`result = await background.result(${JSON.stringify(job.id)})`,
			`task = await background.inspect(${JSON.stringify(job.id)})`,
			'print("RESULT " + json.dumps(result))',
			'print("TASK " + json.dumps(task))',
		].join("\n");
		await client.promptAndWait("collect the job", undefined, 60_000);
		const lines = toolResultTexts(await client.getMessages()).flatMap((text) => text.split("\n"));
		const result = JSON.parse(lines.find((line) => line.startsWith("RESULT "))!.slice(7)) as unknown;
		const task = JSON.parse(lines.find((line) => line.startsWith("TASK "))!.slice(5)) as {
			parentId?: string;
		};
		expect(result).toMatchObject({ status: "succeeded", value: CHILD_RESULT });
		// The original task record is intact: same identity and lineage, completed exactly once.
		expect(task).toMatchObject({
			id: job.id,
			definition: job.definition,
			state: "completed",
			result: { status: "succeeded", value: CHILD_RESULT },
		});
		expect(task.parentId).toBe(job.parentId);
		expect(childRequests).toBe(1);
	}

	/** Quit a client normally and require every Ultron process and socket to be gone. */
	async function quitAndExpectClean(client: RpcClient, timeoutMs: number): Promise<void> {
		const child = childProcessOf(client);
		child.stdin?.end();
		const exit = await awaitExit(child, 15_000);
		expect(exit.code).toBe(0);
		clients.splice(clients.indexOf(client), 1);
		const remaining = await poll(
			() => ownedProcesses(root),
			(processes) => processes.length === 0,
			timeoutMs,
		);
		expect(remaining).toEqual([]);
		expect(
			await poll(
				() => sockets(serverDir),
				(names) => names.length === 0,
				2_000,
			),
		).toEqual([]);
	}

	test("a background child keeps running after its client quits and a new client reattaches to it", async () => {
		const sessionId = randomUUID();
		const { job, workerPid } = await startJobAndLoseClient(sessionId);

		// A second client reattaches to the same live worker and Session.
		const second = startClient(sessionId);
		await second.start();
		const reattached = await second.getMessages();
		expect(toolResultTexts(reattached).some((text) => text.startsWith("JOB ") && text.includes(job.id))).toBe(true);
		expect(workerPids(root)).toEqual([workerPid]);

		// The child finishes at the provider after the UI loss.
		releaseChild();
		expect(
			await poll(
				() => childCompleted,
				(done) => done,
				10_000,
			),
		).toBe(true);
		await collect(second, job);

		// With the work done and the last client gone, nothing lingers.
		await quitAndExpectClean(second, 15_000);
	}, 120_000);

	test("detached work finishes with no client attached, retires, and stays durable for a later client", async () => {
		const sessionId = randomUUID();
		const { job, workerPid } = await startJobAndLoseClient(sessionId);

		releaseChild();
		expect(
			await poll(
				() => childCompleted,
				(done) => done,
				10_000,
			),
		).toBe(true);
		// Nobody is attached: once the job is committed the worker retires and the coordinator follows it.
		expect(
			await poll(
				() => ownedProcesses(root),
				(processes) => processes.length === 0,
				15_000,
			),
		).toEqual([]);

		// A later client starts a fresh worker for the Session and reads the durable result.
		const later = startClient(sessionId);
		await later.start();
		await collect(later, job);
		expect(workerPids(root)).not.toContain(workerPid);
		await quitAndExpectClean(later, 15_000);
	}, 120_000);

	test("quitting with no active work leaves no processes behind", async () => {
		const client = startClient(randomUUID());
		await client.start();
		await client.promptAndWait("say hello", undefined, 60_000);
		expect(await client.getLastAssistantText()).toBe("IDLE_OK");
		expect(workerPids(root)).toHaveLength(1);
		// Idle cleanup is prompt: no worker waits out the orphan grace.
		await quitAndExpectClean(client, 3_000);
	}, 60_000);

	function messageTexts(messages: readonly unknown[], role: string): string[] {
		return messages.flatMap((message) =>
			typeof message === "object" && message !== null && "role" in message && message.role === role
				? [textOf((message as { content?: unknown }).content)]
				: [],
		);
	}

	/** The long turn ran exactly once, its tool call included, and its final answer is in the Session. */
	function expectLongTurnCompleted(messages: readonly unknown[]): void {
		expect(messageTexts(messages, "user").filter((text) => text === LONG_PROMPT)).toHaveLength(1);
		expect(toolResultTexts(messages).filter((text) => text.includes(LONG_TOOL_OUTPUT))).toHaveLength(1);
		// The final answer was produced after the tool call, from its result.
		expect(messageTexts(messages, "assistant").find((text) => text.startsWith(LONG_RESULT))).toContain(
			LONG_TOOL_OUTPUT,
		);
		expect(longRequests).toBe(1);
		expect(longToolResults).toBe(1);
	}

	async function messagesWhen(
		client: RpcClient,
		done: (messages: readonly unknown[]) => boolean,
		timeoutMs: number,
	): Promise<unknown[]> {
		const deadline = Date.now() + timeoutMs;
		while (true) {
			const messages = await client.getMessages();
			if (done(messages) || Date.now() >= deadline) return messages;
			await new Promise((resolveWait) => setTimeout(resolveWait, 100));
		}
	}

	function kill(client: RpcClient, signal: NodeJS.Signals): Promise<{ elapsedMs: number; code: number | null }> {
		const child = childProcessOf(client);
		child.kill(signal);
		clients.splice(clients.indexOf(client), 1);
		return awaitExit(child, 10_000);
	}

	test.each(["SIGTERM", "stdin close", "SIGKILL"] as const)(
		"a root turn in flight when its client quits (%s) completes and a later client sees it",
		async (quit) => {
			const sessionId = randomUUID();
			const first = startClient(sessionId);
			await first.start();
			await first.prompt(LONG_PROMPT);
			await longRequested;
			const [workerPid] = workerPids(root);
			expect(workerPid).toBeDefined();

			// Leaving the app is not an abort.
			const firstProcess = childProcessOf(first);
			if (quit === "stdin close") firstProcess.stdin?.end();
			else firstProcess.kill(quit);
			const exit = await awaitExit(firstProcess, 10_000);
			expect(exit.code).toBe({ SIGTERM: 143, "stdin close": 0, SIGKILL: null }[quit]);
			clients.splice(clients.indexOf(first), 1);
			await new Promise((resolveWait) => setTimeout(resolveWait, 500));
			expect(workerPids(root)).toEqual([workerPid]);
			expect(longCompleted).toBe(false);

			if (quit !== "stdin close") {
				// Reattach while the turn is still in flight, then watch it finish in the same worker.
				const second = startClient(sessionId);
				await second.start();
				expect(workerPids(root)).toEqual([workerPid]);
				releaseLong();
				const messages = await messagesWhen(
					second,
					(current) => messageTexts(current, "assistant").some((text) => text.startsWith(LONG_RESULT)),
					20_000,
				);
				expectLongTurnCompleted(messages);
				await quitAndExpectClean(second, 15_000);
				return;
			}

			// Nobody is attached: the turn finishes, its worker retires, and a later client reads the durable result.
			releaseLong();
			expect(
				await poll(
					() => longCompleted,
					(done) => done,
					20_000,
				),
			).toBe(true);
			expect(
				await poll(
					() => ownedProcesses(root),
					(processes) => processes.length === 0,
					15_000,
				),
			).toEqual([]);
			const later = startClient(sessionId);
			await later.start();
			expectLongTurnCompleted(await later.getMessages());
			await quitAndExpectClean(later, 15_000);
		},
		120_000,
	);

	test("an explicit abort still stops the root turn", async () => {
		const client = startClient(randomUUID());
		await client.start();
		await client.prompt(LONG_PROMPT);
		await longRequested;
		await client.abort();
		await client.waitForIdle(10_000);
		releaseLong();
		await new Promise((resolveWait) => setTimeout(resolveWait, 1_000));
		expect(longToolResults).toBe(0);
		expect(messageTexts(await client.getMessages(), "assistant").join("\n")).not.toContain(LONG_RESULT);
		await quitAndExpectClean(client, 5_000);
	}, 60_000);

	test("a hard-killed idle client leaves no process behind within seconds", async () => {
		const client = startClient(randomUUID());
		await client.start();
		await client.promptAndWait("say hello", undefined, 60_000);
		expect(workerPids(root)).toHaveLength(1);
		await kill(client, "SIGKILL");
		const killedAt = Date.now();
		expect(
			await poll(
				() => ownedProcesses(root),
				(processes) => processes.length === 0,
				10_000,
			),
		).toEqual([]);
		expect(Date.now() - killedAt).toBeLessThan(3_000);
	}, 60_000);

	test("a hard-killed client's background job completes, stays durable, and then everything exits", async () => {
		const sessionId = randomUUID();
		const { job } = await startJobAndLoseClient(sessionId, "SIGKILL");
		releaseChild();
		expect(
			await poll(
				() => childCompleted,
				(done) => done,
				10_000,
			),
		).toBe(true);
		expect(
			await poll(
				() => ownedProcesses(root),
				(processes) => processes.length === 0,
				10_000,
			),
		).toEqual([]);
		const later = startClient(sessionId);
		await later.start();
		await collect(later, job);
		await quitAndExpectClean(later, 15_000);
	}, 120_000);

	test("a client restarted right after a hard kill reattaches to the busy worker", async () => {
		const sessionId = randomUUID();
		const { job, workerPid } = await startJobAndLoseClient(sessionId, "SIGKILL");
		const second = startClient(sessionId);
		await second.start();
		expect(workerPids(root)).toEqual([workerPid]);
		releaseChild();
		expect(
			await poll(
				() => childCompleted,
				(done) => done,
				10_000,
			),
		).toBe(true);
		await collect(second, job);
		await quitAndExpectClean(second, 15_000);
	}, 120_000);

	test("a client restarted within the orphan grace reattaches to the idle worker", async () => {
		const sessionId = randomUUID();
		// A grace longer than CLI startup makes the restart land inside it deterministically.
		const env = { __PI_SESSION_WORKER_ORPHAN_DEMAND_GRACE_MS: "20000" };
		const first = startClient(sessionId, env);
		await first.start();
		await first.promptAndWait("say hello", undefined, 60_000);
		const [workerPid] = workerPids(root);
		await kill(first, "SIGKILL");
		const second = startClient(sessionId, env);
		await second.start();
		expect(workerPids(root)).toEqual([workerPid]);
		expect(await second.getLastAssistantText()).toBe("IDLE_OK");
		await quitAndExpectClean(second, 5_000);
	}, 60_000);
});
