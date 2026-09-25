/**
 * A38: nested context excludes unauthorized and contaminating history.
 *
 * Uses a real AgentHarness over an in-memory Session and a scripted faux provider that
 * captures every provider request with the lane (provider session id) that sent it. The
 * root lane holds a secret marker in its conversation; reviewer and RLM child tasks
 * started by NativeRlmHost must send only their own prompt, never the root history.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AgentHarness, MemorySessionRepo } from "@ultron/agent-core";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { createModels, type FauxResponseFactory, fauxAssistantMessage, fauxProvider } from "@ultron/ai";
import type { JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { afterEach, describe, expect, test } from "vitest";
import { createUltronRlmTool } from "../src/experimental/session-worker.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";

const SECRET = "ROOT_ONLY_SECRET_9c41";
const SPECULATION = "IMPLEMENTER_SPECULATION_b77e";
const SESSION_ID = "a38";

type Captured = { lane: string; messages: string; count: number };

function memoryStore(): NativeHostStore {
	let value: JsonValue | undefined;
	return {
		read: async () => structuredClone(value) as never,
		write: async (next) => {
			value = structuredClone(next) as JsonValue;
		},
	};
}

async function setup() {
	const repo = new MemorySessionRepo();
	const session = await repo.create({ id: SESSION_ID }, BACKGROUND_CONTEXT);
	const faux = fauxProvider();
	const captured: Captured[] = [];
	const respond: FauxResponseFactory = (context, options) => {
		const lane = (options?.sessionId ?? "").slice(SESSION_ID.length + 1);
		const { messages } = context;
		captured.push({
			lane,
			messages: JSON.stringify(messages),
			count: messages.length,
		});
		if (lane.startsWith("ultron.security-reviewer."))
			return fauxAssistantMessage('{"outcome":"no_findings","findings":[]}');
		if (lane.startsWith("ultron.rlm-child.")) return fauxAssistantMessage("child finished");
		return fauxAssistantMessage("root acknowledged");
	};
	faux.setResponses(Array.from({ length: 20 }, () => respond));
	const models = createModels();
	models.setProvider(faux.provider);
	const { harness } = await AgentHarness.create(
		{ session, models, model: faux.getModel(), activeToolNames: [] },
		BACKGROUND_CONTEXT,
	);
	const main = await harness.lane("main", BACKGROUND_CONTEXT);
	const host = new NativeRlmHost(harness, main, { store: memoryStore() });
	return {
		harness,
		main,
		host,
		captured,
		close: async () => {
			await host.close();
			await harness.close(BACKGROUND_CONTEXT);
			await session.close(BACKGROUND_CONTEXT);
			await repo.close(BACKGROUND_CONTEXT);
		},
	};
}

let cleanup: (() => Promise<void>) | undefined;
afterEach(async () => {
	await cleanup?.();
	cleanup = undefined;
});

describe("A38 nested context isolation", () => {
	test("reviewer and RLM child lanes receive only their own prompt, not root history", async () => {
		const fixture = await setup();
		cleanup = fixture.close;
		const { main, host, captured } = fixture;

		expect(
			await main.prompt(`Remember ${SECRET}. My guess: ${SPECULATION}.`, undefined, BACKGROUND_CONTEXT),
		).toMatchObject({ ok: true });
		expect(captured.at(-1)).toMatchObject({ lane: "main", count: 1 });
		expect(captured.at(-1)!.messages).toContain(SECRET);

		const review = await host.handle(
			"agents.invoke",
			{ definition: "security-reviewer@1", input: { request: "Review diff REVIEW_INPUT_31" } },
			BACKGROUND_CONTEXT,
		);
		expect(review).toMatchObject({ status: "succeeded", value: { outcome: "no_findings", findings: [] } });

		const child = (await host.handle(
			"rlm.spawn",
			{ prompt: "Child task CHILD_INPUT_58", kwargs: { name: "helper" } },
			BACKGROUND_CONTEXT,
		)) as { rlm_child_id: string };
		const collected = (await host.handle("rlm.collect", { selectors: [child.rlm_child_id] }, BACKGROUND_CONTEXT)) as {
			results: Array<{ result: unknown }>;
		};
		expect(collected.results[0]!.result).toMatchObject({ status: "succeeded", value: "child finished" });

		// The root lane keeps its own history on its next turn (the capture really sees history).
		await main.prompt("Second root turn", undefined, BACKGROUND_CONTEXT);
		const rootSecond = captured.at(-1)!;
		expect(rootSecond).toMatchObject({ lane: "main" });
		expect(rootSecond.messages).toContain(SECRET);
		expect(rootSecond.messages).not.toContain("REVIEW_INPUT_31");
		expect(rootSecond.messages).not.toContain("CHILD_INPUT_58");

		const reviewer = captured.filter((request) => request.lane.startsWith("ultron.security-reviewer."));
		const rlmChild = captured.filter((request) => request.lane.startsWith("ultron.rlm-child."));
		expect(reviewer).toHaveLength(1);
		expect(rlmChild).toHaveLength(1);
		for (const [request, input] of [
			[reviewer[0]!, "REVIEW_INPUT_31"],
			[rlmChild[0]!, "CHILD_INPUT_58"],
		] as const) {
			// Exactly one message: the task prompt built from its own explicit input.
			expect(request.count).toBe(1);
			expect(request.messages).toContain(input);
			expect(request.messages).not.toContain(SECRET);
			expect(request.messages).not.toContain(SPECULATION);
			expect(request.messages).not.toContain("root acknowledged");
		}
		expect(reviewer[0]!.messages).toContain("Review the supplied change for security issues");
		expect(reviewer[0]!.messages).not.toContain("CHILD_INPUT_58");
		expect(rlmChild[0]!.messages).not.toContain("REVIEW_INPUT_31");

		// Every task lane was created without a parent entry to inherit from.
		const lanes = await fixture.harness.lanes(BACKGROUND_CONTEXT);
		expect(lanes.map((lane) => lane.name).sort()).toEqual(
			["main", ...captured.filter((r) => r.lane !== "main").map((r) => r.lane)].sort(),
		);
	});

	test("each lane gets its own Python kernel, so root variables do not leak into a child", async () => {
		const cwd = mkdtempSync(join(tmpdir(), "ultron-a38-"));
		const lanes = new Map([
			["op-root", "main"],
			["op-child", "ultron.security-reviewer.task-1"],
		]);
		const tool = createUltronRlmTool(
			cwd,
			async () => null,
			async (invocation) => lanes.get(invocation.operationId)!,
		);
		const env = new NodeExecutionEnv({ cwd });
		const run = async (operationId: string, code: string) => {
			const invocation = {
				invocationId: `${operationId}-call`,
				operationId,
				turnId: "turn",
				getMemo: async () => undefined,
				setMemo: async () => undefined,
			};
			const result = await tool.execute("call", { code }, () => {}, { env }, invocation, BACKGROUND_CONTEXT);
			return (result.content[0] as { text: string }).text;
		};
		try {
			await run("op-root", `secret = ${JSON.stringify(SECRET)}`);
			expect(await run("op-root", "secret")).toBe(`'${SECRET}'`);
			expect(await run("op-child", "'secret' in globals()")).toBe("False");
			expect(await run("op-child", "import os\nos.getpid()")).not.toBe(
				await run("op-root", "import os\nos.getpid()"),
			);
		} finally {
			await tool.close();
			rmSync(cwd, { recursive: true, force: true });
		}
	});
});
