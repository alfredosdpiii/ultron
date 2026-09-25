/**
 * A22: detached work and retained children survive UI loss.
 *
 * Real CLI processes drive a real foreground server, coordinator, and Session worker. The first client starts a
 * background job and leaves while the child lane's model request is still in flight; the child must finish, and a
 * second client must reattach to the same Session and read the durable result. Afterwards, and in the idle case,
 * no Ultron process may remain.
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
		provider = createServer((request: IncomingMessage, response: ServerResponse) => {
			void (async () => {
				let body = "";
				for await (const part of request) body += String(part);
				const messages = (JSON.parse(body) as { messages: ChatMessage[] }).messages;
				const last = messages.at(-1);
				const lastText = textOf(last?.content);
				if (last?.role === "tool") {
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

	function startClient(sessionId: string): RpcClient {
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
		const client = new RpcClient({
			cliPath,
			cwd: projectDir,
			provider: "mock",
			model: "mock",
			args: ["--session-id", sessionId],
			env: {
				NODE_OPTIONS: `--import ${sourceResolverPath}`,
				ULTRON_CODING_AGENT_DIR: agentDir,
				ULTRON_SERVER_DIR: serverDir,
				PI_OFFLINE: "1",
			},
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
	async function startJobAndLoseClient(sessionId: string): Promise<{ job: StartedJob; workerPid: number }> {
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

		// The UI goes away (SIGTERM, as from a closed terminal or a supervisor) while the child is in flight.
		const firstProcess = childProcessOf(first);
		firstProcess.kill("SIGTERM");
		const firstExit = await awaitExit(firstProcess, 10_000);
		expect(firstExit.code).toBe(143);
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
});
