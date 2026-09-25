/**
 * A47 (and A48 end to end): the real CLI in RPC mode against a scripted provider whose model declares a small
 * context window. The root loads an input several times larger than that window, slices it in the kernel, and
 * fans bounded inference frames over the slices. The provider records every request body, so the test can
 * assert that no request exceeds the window and that the root's own requests never carry the input.
 */
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, describe, expect, test } from "vitest";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";
import { ScriptedProvider, type ScriptedReply, type ScriptedRequest } from "./support/scripted-provider.ts";

const cliPath = resolve(__dirname, "../src/cli.ts");
const sourceResolverPath = resolve(__dirname, "../src/experimental/source-resolver.ts");

/**
 * The scripted model's declared context window, in the provider's tokens (request bytes / 4). It stays above
 * Pi's compaction reserve (16k), so the root turn is not compacted before its first request.
 */
const WINDOW_TOKENS = 48_000;
const LINES = 16_000;

function logFile(): { text: string; errors: number } {
	const lines: string[] = [];
	let errors = 0;
	for (let index = 0; index < LINES; index += 1) {
		const error = index % 13 === 0 || index % 29 === 0;
		if (error) errors += 1;
		lines.push(
			`2026-09-26T03:${String(index % 60).padStart(2, "0")}:00Z id=L${String(index).padStart(6, "0")} ${error ? "ERROR upstream timeout" : "INFO request served"}`,
		);
	}
	return { text: `${lines.join("\n")}\n`, errors };
}

const COUNT_CELL = `import json
h = await rlm.load('big.log')
counts = await rlm.map('Count the lines that contain ERROR. Reply with the integer.', h.chunks(24000), contract=int, budget=Budget(calls=200))
bad = [c for c in counts if not isinstance(c, int)]
print('RESULT ' + json.dumps({'errors': sum(c for c in counts if isinstance(c, int)), 'frames': len(counts), 'bad': len(bad), 'handle': repr(h)}))`;

const REPAIR_CELL = `import json
h = await rlm.load('big.log')
v = await rlm.infer('REPAIR: how many lines contain ERROR?', h.lines(0, 50), contract=int)
x = await rlm.infer('NEVER: how many lines contain ERROR?', [h.lines(0, 50)], contract=int, max_repairs=1)
print('RESULT ' + json.dumps({'value': v, 'expected': h.lines(0, 50).text.count('ERROR'), 'incomplete': isinstance(x, Incomplete), 'falsy': not x, 'status': x.status, 'trace': x.trace_id, 'last': x.last_outputs, 'spent': x.spent}))`;

const SLOW_CELL = `h = await rlm.load('big.log')
await rlm.map('SLOW: count the ERROR lines.', h.chunks(24000)[:6], contract=int)`;

const errorLines = (text: string) => text.split("\n").filter((line) => line.includes("ERROR")).length;
const viewOf = (message: string) => /--- view 1: [^\n]*---\n([\s\S]*?)\n--- end of view 1 ---/.exec(message)?.[1] ?? "";
const isFrame = (request: ScriptedRequest) => request.system.includes("inference frame");

function script(request: ScriptedRequest): ScriptedReply {
	if (isFrame(request)) {
		const task = request.firstUser;
		if (task.includes("NEVER:")) return { text: "I cannot tell." };
		if (task.includes("SLOW:")) return { text: "0", delayMs: 60_000 };
		if (task.includes("REPAIR:") && request.turn === 0) return { text: "There are a few, maybe four?" };
		return { text: String(errorLines(viewOf(request.firstUser))) };
	}
	if (request.firstUser.startsWith("A47"))
		return request.turn === 0 ? { tool: "rlm", args: { code: COUNT_CELL } } : { text: "DONE" };
	if (request.firstUser.startsWith("A48"))
		return request.turn === 0 ? { tool: "rlm", args: { code: REPAIR_CELL } } : { text: "DONE" };
	if (request.firstUser.startsWith("A50"))
		return request.turn === 0 ? { tool: "rlm", args: { code: SLOW_CELL } } : { text: "DONE" };
	throw new Error(`Unscripted request: ${request.firstUser.slice(0, 80)}`);
}

type Harness = { provider: ScriptedProvider; client: RpcClient; root: string; input: { text: string; errors: number } };

async function start(): Promise<Harness> {
	const root = mkdtempSync(join(tmpdir(), "ultron-a47-"));
	const agentDir = join(root, "agent");
	const projectDir = join(root, "project");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(projectDir, { recursive: true });
	const input = logFile();
	writeFileSync(join(projectDir, "big.log"), input.text);
	const provider = new ScriptedProvider(script);
	await provider.start();
	writeFileSync(
		join(agentDir, "models.json"),
		JSON.stringify({
			providers: {
				scripted: {
					baseUrl: provider.baseUrl,
					api: "openai-completions",
					apiKey: "scripted-key",
					models: [
						{
							id: "scripted",
							name: "scripted",
							reasoning: false,
							input: ["text"],
							contextWindow: WINDOW_TOKENS,
							maxTokens: 4096,
						},
					],
				},
			},
		}),
	);
	const client = new RpcClient({
		cliPath,
		cwd: projectDir,
		provider: "scripted",
		model: "scripted",
		args: ["--no-session"],
		env: {
			NODE_OPTIONS: `--import ${sourceResolverPath}`,
			ULTRON_CODING_AGENT_DIR: agentDir,
			ULTRON_SERVER_DIR: mkdtempSync(join("/tmp", "u-a47-")),
			ULTRON_HINDSIGHT_URL: "off",
			PI_OFFLINE: "1",
		},
	});
	await client.start();
	return { provider, client, root, input };
}

async function results(client: RpcClient): Promise<Array<Record<string, any>>> {
	const messages = await client.getMessages();
	return messages
		.filter((message) => message.role === "toolResult")
		.flatMap((message) =>
			(message.content as Array<{ type: string; text?: string }>)
				.filter((part) => part.type === "text")
				.flatMap((part) => (part.text ?? "").split("\n"))
				.filter((line) => line.startsWith("RESULT "))
				.map((line) => JSON.parse(line.slice("RESULT ".length)) as Record<string, any>),
		);
}

async function inspect(client: RpcClient, request: string, payload: object = {}): Promise<unknown> {
	const response = (await (
		client as unknown as { send(command: object): Promise<{ success: boolean; data?: unknown; error?: string }> }
	).send({ type: "inspect", request, payload })) as { success: boolean; data?: unknown; error?: string };
	if (!response.success) throw new Error(response.error ?? `inspect ${request} failed`);
	return response.data;
}

describe("A47 an input larger than the window is processed exactly through bounded frames", () => {
	let harness: Harness | undefined;

	afterEach(async () => {
		if (!harness) return;
		await harness.client.stop().catch(() => {});
		await harness.provider.stop();
		rmSync(harness.root, { recursive: true, force: true });
		harness = undefined;
	});

	test("exact answer, no request above the window, and the root never sees the input", async () => {
		harness = await start();
		const { client, provider, input } = harness;
		expect(input.text.length / 4).toBeGreaterThan(4 * WINDOW_TOKENS);
		await client.promptAndWait("A47: count the ERROR lines in big.log", undefined, 240_000);
		expect(await client.getLastAssistantText()).toBe("DONE");
		const [result] = await results(client);
		expect(result).toMatchObject({ errors: input.errors, bad: 0 });
		expect(result!.frames).toBeGreaterThan(4);
		// Printing a handle shows its label, size and digest only.
		expect(result!.handle).toMatch(/^ContextHandle\(label='big.log', chars=\d+, size=\d+, digest='sha256:/);

		const frames = provider.requests.filter(isFrame);
		const roots = provider.requests.filter((request) => !isFrame(request));
		expect(frames).toHaveLength(result!.frames);
		expect(roots).toHaveLength(2);
		// No single request exceeds the declared window (the provider counts request bytes / 4 as input tokens).
		for (const request of provider.requests)
			expect(Math.ceil(request.raw.length / 4)).toBeLessThanOrEqual(WINDOW_TOKENS);
		// The root transcript never contains the input: not one log line reaches a root request.
		for (const request of roots) {
			expect(request.raw).not.toMatch(/id=L\d{6}/);
			expect(request.raw).not.toContain("upstream timeout");
		}
		// Frames are private: no root system prompt, no tools, no parent transcript.
		for (const request of frames) {
			expect(request.system).not.toContain("rlm");
			expect((request.body as { tools?: unknown[] }).tools ?? []).toHaveLength(0);
			expect(request.body.messages.filter((message) => message.role === "user")).toHaveLength(1);
			expect(request.raw).not.toContain("A47: count");
		}
		// Every frame is journaled as an rlm-frame task and traced.
		const status = (await inspect(client, "agents.status")) as {
			tasks: Array<{ definition: string; state: string }>;
		};
		const frameTasks = status.tasks.filter((task) => task.definition === "rlm-frame@1");
		expect(frameTasks).toHaveLength(result!.frames);
		expect(frameTasks.every((task) => task.state === "completed")).toBe(true);
		const listed = (await inspect(client, "rlm.frames", { limit: 5 })) as { frames: Array<{ status: string }> };
		expect(listed.frames).toHaveLength(5);
		expect(listed.frames.every((frame) => frame.status === "complete")).toBe(true);
	}, 300_000);

	test("A48 end to end: repair returns a valid value, exhaustion is an Incomplete, and the root turn continues", async () => {
		harness = await start();
		const { client, provider } = harness;
		await client.promptAndWait("A48: repair and exhaustion", undefined, 240_000);
		// The root turn continued after the Incomplete and finished normally.
		expect(await client.getLastAssistantText()).toBe("DONE");
		const [result] = await results(client);
		expect(result!.value).toBe(result!.expected);
		expect(result).toMatchObject({ incomplete: true, falsy: true, status: "contract_unmet", spent: { calls: 2 } });
		expect(result!.last).toEqual(["I cannot tell.", "I cannot tell."]);
		const repaired = provider.requests.filter((request) => isFrame(request) && request.firstUser.includes("REPAIR:"));
		expect(repaired).toHaveLength(2);
		expect(repaired[1]!.lastUser).toMatch(/does not satisfy the contract/);
		const trace = (await inspect(client, "rlm.frames", { id: result!.trace })) as Record<string, unknown>;
		expect(trace).toMatchObject({ id: result!.trace, status: "incomplete", reason: "contract_unmet" });
	}, 300_000);

	test("A50 end to end: aborting the root turn cancels every running frame within 2 s", async () => {
		harness = await start();
		const { client, provider } = harness;
		const frames = 6;
		await client.prompt("A50: slow frames");
		const deadline = Date.now() + 60_000;
		while (provider.requests.filter((request) => request.firstUser.includes("SLOW:")).length < frames) {
			if (Date.now() > deadline) throw new Error("frames did not start");
			await new Promise((resolve) => setTimeout(resolve, 20));
		}
		const aborted = Date.now();
		await client.abort();
		let states: string[] = [];
		while (Date.now() - aborted < 2_000) {
			const status = (await inspect(client, "agents.status")) as {
				tasks: Array<{ definition: string; state: string }>;
			};
			states = status.tasks.filter((task) => task.definition === "rlm-frame@1").map((task) => task.state);
			if (states.length === frames && states.every((state) => state === "cancelled")) break;
			await new Promise((resolve) => setTimeout(resolve, 25));
		}
		expect(states).toEqual(Array(frames).fill("cancelled"));
		expect(Date.now() - aborted).toBeLessThan(2_000);
		// The session keeps working after the abort.
		await new Promise((resolve) => setTimeout(resolve, 500));
		await client.promptAndWait("A50: continue", undefined, 60_000);
		expect(await client.getLastAssistantText()).toBe("DONE");
	}, 300_000);
});
