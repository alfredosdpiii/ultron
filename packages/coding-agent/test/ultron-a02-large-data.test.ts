/**
 * A02: large input stays outside active context; full data remains usable.
 *
 * The dataset is a CSV file of several MB written outside the model. Python in the persistent
 * RLM kernel computes exact aggregates over every row, while the text returned to the model
 * (the `rlm` tool result) stays bounded even when a cell prints or returns the whole dataset.
 *
 * Model-visible bound: runtime.py caps stdout, stderr and the result preview at 8 KiB each
 * (plus a 16-byte truncation marker), and createUltronRlmTool joins those three with newlines,
 * so one `rlm` tool result is at most 3 * (8192 + 16) + 2 = 24,626 bytes.
 */
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type Server } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { BACKGROUND_CONTEXT } from "@earendil-works/chord/context";
import { NodeExecutionEnv } from "@earendil-works/pi-agent-core/node";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { createUltronRlmTool } from "../src/experimental/session-worker.ts";
import { RpcClient } from "../src/modes/rpc/rpc-client.ts";

const PREVIEW_BYTES = 8192;
const MARKER = "\n... [truncated]";
const TOOL_RESULT_BOUND = 3 * (PREVIEW_BYTES + Buffer.byteLength(MARKER)) + 2;
const ROWS = 600_000;
const TAIL_SENTINEL = "TAIL_SENTINEL_ROW_7f3a";

type Dataset = { path: string; bytes: number; sum: number; max: number; tailCount: number };

/** Deterministic rows: id,value,tag. The last row carries a sentinel that must never reach the model. */
function writeDataset(dir: string): Dataset {
	const lines: string[] = ["id,value,tag"];
	let sum = 0;
	let max = 0;
	for (let id = 0; id < ROWS; id += 1) {
		const value = (id * 7919) % 100_003;
		sum += value;
		if (value > max) max = value;
		const tag = id === ROWS - 1 ? TAIL_SENTINEL : `t${id % 97}`;
		lines.push(`${id},${value},${tag}`);
	}
	const text = `${lines.join("\n")}\n`;
	const path = join(dir, "large.csv");
	writeFileSync(path, text);
	return { path, bytes: Buffer.byteLength(text), sum, max, tailCount: 1 };
}

function toolText(result: { content: Array<{ type: string; text?: string }> }): string {
	const part = result.content[0];
	if (part?.type !== "text" || typeof part.text !== "string") throw new Error("Expected text tool result");
	return part.text;
}

describe("A02 large data stays outside the model context", () => {
	let dir: string;
	let data: Dataset;

	beforeAll(() => {
		dir = mkdtempSync(join(tmpdir(), "ultron-a02-"));
		data = writeDataset(dir);
	});

	afterAll(() => {
		rmSync(dir, { recursive: true, force: true });
	});

	test("the rlm tool computes exact aggregates over all rows while its result stays bounded", async () => {
		expect(data.bytes).toBeGreaterThan(8 * 1024 * 1024);
		const tool = createUltronRlmTool(dir, async () => {
			throw new Error("No host requests expected");
		});
		const env = new NodeExecutionEnv({ cwd: dir });
		let call = 0;
		const run = async (code: string) => {
			call += 1;
			const invocation = {
				invocationId: `a02-${call}`,
				operationId: `a02-op-${call}`,
				turnId: "a02-turn",
				getMemo: async () => undefined,
				setMemo: async () => undefined,
			};
			const result = await tool.execute(`a02-${call}`, { code }, () => {}, { env }, invocation, BACKGROUND_CONTEXT);
			const text = toolText(result as never);
			expect(Buffer.byteLength(text)).toBeLessThanOrEqual(TOOL_RESULT_BOUND);
			expect(text).not.toContain(TAIL_SENTINEL);
			return text;
		};
		try {
			// Load the whole file into retained Python state; print all of it and return all of it.
			const loaded = await run(
				[
					"import sys",
					`raw = open(${JSON.stringify(data.path)}).read()`,
					"rows = [line.split(',') for line in raw.splitlines()[1:]]",
					"print(raw)",
					"print(raw, file=sys.stderr)",
					"rows",
				].join("\n"),
			);
			expect(loaded).toContain("id,value,tag\n0,0,t0\n");
			expect(loaded.split("[truncated]").length - 1).toBe(3);

			// Later cells still see every row: the preview bound did not discard data.
			const aggregate = await run(
				[
					"values = [int(row[1]) for row in rows]",
					`(len(rows), sum(values), max(values), sum(1 for row in rows if row[2] == ${JSON.stringify(TAIL_SENTINEL)}))`,
				].join("\n"),
			);
			expect(aggregate).toBe(`(${ROWS}, ${data.sum}, ${data.max}, ${data.tailCount})`);

			// A huge single value (the raw text) and a huge int are also previewed, not returned whole.
			const huge = await run("raw");
			expect(huge.endsWith(MARKER)).toBe(true);
			const bigInt = await run("10 ** 200000");
			expect(bigInt).toMatch(/^<int with \d+ bits>/);
		} finally {
			await tool.close();
		}
	}, 60_000);

	describe("end to end through the native CLI worker", () => {
		const cliPath = resolve(__dirname, "../src/cli.ts");
		const sourceResolverPath = resolve(__dirname, "../src/experimental/source-resolver.ts");
		let provider: Server;
		let client: RpcClient | undefined;
		const bodies: string[] = [];

		const code = [
			"import sys",
			"raw = open('large.csv').read()",
			"rows = [line.split(',') for line in raw.splitlines()[1:]]",
			"print(raw)",
			"total = sum(int(row[1]) for row in rows)",
			"f'ROWS={len(rows)} SUM={total}'",
		].join("\n");

		beforeAll(async () => {
			provider = createServer(async (request, response) => {
				const chunks: Buffer[] = [];
				for await (const chunk of request) chunks.push(chunk as Buffer);
				const body = Buffer.concat(chunks).toString("utf8");
				bodies.push(body);
				const parsed = JSON.parse(body) as { messages: Array<{ role: string }> };
				const hasToolResult = parsed.messages.some((message) => message.role === "tool");
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
				const usage = { prompt_tokens: 3, completion_tokens: 1, total_tokens: 4 };
				if (hasToolResult) {
					response.write(chunk({ role: "assistant", content: "A02_DONE" }, null));
					response.write(chunk({}, "stop", usage));
				} else {
					response.write(
						chunk(
							{
								role: "assistant",
								tool_calls: [
									{
										index: 0,
										id: "call_a02",
										type: "function",
										function: { name: "rlm", arguments: JSON.stringify({ code }) },
									},
								],
							},
							null,
						),
					);
					response.write(chunk({}, "tool_calls", usage));
				}
				response.end("data: [DONE]\n\n");
			});
			await new Promise<void>((resolveListen) => provider.listen(0, "127.0.0.1", resolveListen));
		});

		afterAll(async () => {
			await client?.stop().catch(() => {});
			await new Promise<void>((resolveClose) => provider.close(() => resolveClose()));
		});

		test("the request after the rlm call carries the aggregate, not the dataset", async () => {
			const address = provider.address();
			if (address === null || typeof address === "string") throw new Error("Provider is not listening");
			const agentDir = join(dir, "agent");
			const projectDir = join(dir, "project");
			mkdirSync(agentDir, { recursive: true });
			mkdirSync(projectDir, { recursive: true });
			writeDataset(projectDir);
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
			client = new RpcClient({
				cliPath,
				cwd: projectDir,
				provider: "mock",
				model: "mock",
				args: ["--no-session"],
				env: {
					NODE_OPTIONS: `--import ${sourceResolverPath}`,
					ULTRON_CODING_AGENT_DIR: agentDir,
					ULTRON_HINDSIGHT_URL: "off",
					ULTRON_SERVER_DIR: mkdtempSync(join("/tmp", "u-a02-")),
					PI_OFFLINE: "1",
				},
			});
			await client.start();
			await client.promptAndWait("Sum the value column of large.csv", undefined, 90_000);
			expect(await client.getLastAssistantText()).toBe("A02_DONE");

			expect(bodies).toHaveLength(2);
			const [first, second] = bodies.map((body) => Buffer.byteLength(body));
			// The follow-up request grows only by the tool call and the bounded tool result.
			expect(second! - first!).toBeLessThanOrEqual(TOOL_RESULT_BOUND + 4096);
			expect(second!).toBeLessThan(data.bytes / 100);
			expect(bodies[1]).toContain(`ROWS=${ROWS} SUM=${data.sum}`);
			expect(bodies[1]).toContain("[truncated]");
			expect(bodies[1]).not.toContain(TAIL_SENTINEL);
		}, 120_000);
	});
});
