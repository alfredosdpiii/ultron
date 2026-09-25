import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { BACKGROUND_CONTEXT, withAbortSignal } from "@ultron/chord/context";
import { describe, expect, test } from "vitest";
import { createUltronRlmTool } from "../src/experimental/session-worker.ts";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";

// A27: RLM handles a self-contained task without graph/delegation/memory/refinement.
const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));

const REGIONS = ["north", "south", "east", "west"];
const ROWS = 5_000;

/** Self-contained task: build CSV text, parse it, and aggregate, all in Python. */
const TASK_CELLS = [
	[
		"import csv, io, json, statistics",
		"lines = ['id,region,amount,status']",
		`regions = ${JSON.stringify(REGIONS)}`,
		`for i in range(${ROWS}):`,
		"    lines.append(f\"{i},{regions[i % 4]},{(i * 37) % 101},{'void' if i % 7 == 0 else 'ok'}\")",
		"raw = '\\n'.join(lines)",
		"len(raw.splitlines())",
	].join("\n"),
	[
		"rows = list(csv.DictReader(io.StringIO(raw)))",
		"totals = {}",
		"for row in rows:",
		"    if row['status'] == 'ok':",
		"        totals[row['region']] = totals.get(row['region'], 0) + int(row['amount'])",
		"voided = sum(1 for row in rows if row['status'] == 'void')",
		"top = max(totals, key=totals.get)",
		"median = statistics.median(int(row['amount']) for row in rows)",
	].join("\n"),
	"print(json.dumps({'totals': totals, 'voided': voided, 'top': top, 'median': median}, sort_keys=True))",
];

/** Independent expected answer computed in TypeScript. */
function expectedAnswer(): string {
	const totals: Record<string, number> = {};
	let voided = 0;
	const amounts: number[] = [];
	for (let i = 0; i < ROWS; i += 1) {
		const amount = (i * 37) % 101;
		amounts.push(amount);
		if (i % 7 === 0) voided += 1;
		else totals[REGIONS[i % 4]] = (totals[REGIONS[i % 4]] ?? 0) + amount;
	}
	amounts.sort((left, right) => left - right);
	const median = (amounts[ROWS / 2 - 1] + amounts[ROWS / 2]) / 2;
	const top = Object.entries(totals).reduce((best, entry) => (entry[1] > best[1] ? entry : best))[0];
	const sorted = Object.fromEntries(Object.entries(totals).sort(([left], [right]) => left.localeCompare(right)));
	// Python's json.dumps(sort_keys=True) formatting, including float median.
	const totalsText = Object.entries(sorted)
		.map(([key, value]) => `"${key}": ${value}`)
		.join(", ");
	return `{"median": ${Number.isInteger(median) ? `${median}.0` : median}, "top": "${top}", "totals": {${totalsText}}, "voided": ${voided}}\n`;
}

function recordingHandler(mode: "record" | "reject") {
	const requests: Array<{ type: string; payload: Record<string, unknown> }> = [];
	return {
		requests,
		handler: async (type: string, payload: Record<string, unknown>) => {
			requests.push({ type, payload });
			if (mode === "reject") throw new Error(`optional service unavailable: ${type}`);
			return { recorded: true };
		},
	};
}

describe("A27 self-contained RLM task", () => {
	test("a real kernel parses and aggregates data with zero host dispatches", async () => {
		const recorder = recordingHandler("record");
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, recorder.handler);
		try {
			expect(await kernel.execute(TASK_CELLS[0])).toMatchObject({ status: "ok", result: String(ROWS + 1) });
			expect(await kernel.execute(TASK_CELLS[1])).toMatchObject({ status: "ok" });
			expect(await kernel.execute(TASK_CELLS[2])).toMatchObject({ status: "ok", stdout: expectedAnswer() });
			// No graph, delegation, memory, refinement, or any other host request was made.
			expect(recorder.requests).toEqual([]);
		} finally {
			await kernel.shutdown();
		}
	});

	test("the task completes when every optional service rejects, and a service call fails explicitly", async () => {
		const recorder = recordingHandler("reject");
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, recorder.handler);
		try {
			for (const cell of TASK_CELLS.slice(0, 2)) expect(await kernel.execute(cell)).toMatchObject({ status: "ok" });
			expect(await kernel.execute(TASK_CELLS[2])).toMatchObject({ stdout: expectedAnswer() });
			expect(recorder.requests).toEqual([]);

			// Absent services surface as explicit errors, never as fabricated results.
			expect(await kernel.execute("await memory.prepare('regional totals')")).toMatchObject({
				status: "error",
				error: { ename: "RuntimeError", evalue: "optional service unavailable: memory.prepare" },
			});
			expect(recorder.requests.map((request) => request.type)).toEqual(["memory.prepare"]);
			// The computed state survives the failed optional call.
			expect(await kernel.execute("top, voided")).toMatchObject({
				status: "ok",
				result: expect.stringMatching(/^\('/),
			});
		} finally {
			await kernel.shutdown();
		}
	});

	test("the rlm tool over a native host without optional services completes with no dispatches and no tasks", async () => {
		const cwd = mkdtempSync(join(tmpdir(), "ultron-a27-"));
		let taskDocument: unknown;
		const store: NativeHostStore = {
			read: async () => taskDocument as never,
			write: async (document) => {
				taskDocument = structuredClone(document);
			},
		};
		const lanes: string[] = [];
		const host = new NativeRlmHost(
			{
				lane: async (name: string) => {
					lanes.push(name);
					throw new Error("no lanes in a self-contained task");
				},
			} as never,
			{} as never,
			{ store },
		);
		const dispatched: string[] = [];
		const tool = createUltronRlmTool(cwd, (type, payload, signal) => {
			dispatched.push(type);
			return host.handle(type, payload, signal ? withAbortSignal(signal, BACKGROUND_CONTEXT) : BACKGROUND_CONTEXT);
		});
		const invocation = {
			invocationId: "a27",
			operationId: "operation-a27",
			turnId: "turn-a27",
			getMemo: async () => undefined,
			setMemo: async () => undefined,
		};
		try {
			let last: unknown;
			for (const [index, code] of TASK_CELLS.entries()) {
				last = await tool.execute(
					`call-${index}`,
					{ code },
					() => {},
					{ env: new NodeExecutionEnv({ cwd }) },
					invocation,
					BACKGROUND_CONTEXT,
				);
			}
			expect((last as { content: Array<{ text: string }> }).content[0].text.split("\n")[0]).toBe(
				expectedAnswer().trimEnd(),
			);
			expect(dispatched).toEqual([]);
			expect(lanes).toEqual([]);
			expect(await host.handle("agents.tasks", {}, BACKGROUND_CONTEXT)).toMatchObject({ tasks: [] });

			// Optional services are genuinely absent here: asking for one fails explicitly.
			await expect(
				tool.execute(
					"call-refinement",
					{ code: "await refinements.list()" },
					() => {},
					{ env: new NodeExecutionEnv({ cwd }) },
					invocation,
					BACKGROUND_CONTEXT,
				),
			).rejects.toThrow("local services are not connected");
		} finally {
			await tool.close();
			await host.close();
			rmSync(cwd, { recursive: true, force: true });
		}
	});
});
