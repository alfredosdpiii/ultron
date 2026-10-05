/**
 * `rlm.spawn(brief, name=..., fork=True)`: the child's lane starts on its parent's branch, so its first request
 * carries the parent's conversation so far, without the tool call still running (the cell that spawned it), and
 * its brief says its kernel is new. Without fork the child sees only its brief (A38).
 */
import { AgentHarness, type AgentMessage, type Entry, MemorySessionRepo } from "@ultron/agent-core";
import { createModels, type FauxResponseFactory, fauxAssistantMessage, fauxProvider } from "@ultron/ai";
import type { JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { afterEach, describe, expect, test } from "vitest";
import { forkPoint, NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";

const PARENT_MARKER = "PARENT_CONTEXT_7d21";
const SESSION_ID = "fork";

function memoryStore(): NativeHostStore {
	let value: JsonValue | undefined;
	return {
		read: async () => structuredClone(value) as never,
		write: async (next) => {
			value = structuredClone(next) as JsonValue;
		},
	};
}

const assistantCalling = (id: string): AgentMessage =>
	({
		...fauxAssistantMessage("running a cell"),
		content: [
			{ type: "text", text: "running a cell" },
			{ type: "toolCall", id, name: "rlm", arguments: { code: "await rlm.spawn(...)" } },
		],
		stopReason: "toolUse",
	}) as AgentMessage;

async function setup() {
	const repo = new MemorySessionRepo();
	const session = await repo.create({ id: SESSION_ID }, BACKGROUND_CONTEXT);
	const faux = fauxProvider();
	const captured: Array<{ lane: string; messages: string }> = [];
	const respond: FauxResponseFactory = (context, options) => {
		const lane = (options?.sessionId ?? "").slice(SESSION_ID.length + 1);
		captured.push({ lane, messages: JSON.stringify(context.messages) });
		return fauxAssistantMessage(lane.startsWith("ultron.rlm-child.") ? "child finished" : "root acknowledged");
	};
	faux.setResponses(Array.from({ length: 10 }, () => respond));
	const models = createModels();
	models.setProvider(faux.provider);
	const { harness } = await AgentHarness.create(
		{ session, models, model: faux.getModel(), activeToolNames: [] },
		BACKGROUND_CONTEXT,
	);
	const main = await harness.lane("main", BACKGROUND_CONTEXT);
	const host = new NativeRlmHost(harness, main, { store: memoryStore() });
	return {
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

async function spawnAndCollect(host: NativeRlmHost, kwargs: Record<string, unknown>) {
	const child = (await host.handle(
		"rlm.spawn",
		{ prompt: "Child brief CHILD_INPUT_12", kwargs: { name: "helper", ...kwargs } },
		BACKGROUND_CONTEXT,
	)) as { rlm_child_id: string; parent_branch_anchor: string };
	const collected = (await host.handle("rlm.collect", { selectors: [child.rlm_child_id] }, BACKGROUND_CONTEXT)) as {
		results: Array<{ result: unknown }>;
	};
	expect(collected.results[0]!.result).toMatchObject({ status: "succeeded", value: "child finished" });
	return child;
}

describe("rlm.spawn fork", () => {
	test("a forked child starts on the parent's finished conversation", async () => {
		const fixture = await setup();
		cleanup = fixture.close;
		const { main, host, captured } = fixture;
		await main.prompt(`Remember ${PARENT_MARKER}.`, undefined, BACKGROUND_CONTEXT);
		const answered = await main.getTipId(BACKGROUND_CONTEXT);
		// The parent's cell that calls rlm.spawn is still running: its call has no result yet.
		await main.appendMessage(assistantCalling("call-running"), BACKGROUND_CONTEXT);

		const child = await spawnAndCollect(host, { fork: true });
		expect(child.parent_branch_anchor).toBe(answered);
		const request = captured.find((item) => item.lane.startsWith("ultron.rlm-child."))!;
		expect(request.messages).toContain(PARENT_MARKER);
		expect(request.messages).toContain("root acknowledged");
		expect(request.messages).toContain("CHILD_INPUT_12");
		expect(request.messages).toContain("Your REPL kernel is new");
		expect(request.messages).not.toContain("call-running");
	});

	test("without fork the child sees only its brief", async () => {
		const fixture = await setup();
		cleanup = fixture.close;
		const { main, host, captured } = fixture;
		await main.prompt(`Remember ${PARENT_MARKER}.`, undefined, BACKGROUND_CONTEXT);
		const child = await spawnAndCollect(host, {});
		expect(child.parent_branch_anchor).toBe("");
		const request = captured.find((item) => item.lane.startsWith("ultron.rlm-child."))!;
		expect(request.messages).not.toContain(PARENT_MARKER);
		expect(request.messages).not.toContain("Your REPL kernel is new");
	});

	test("fork is refused on an empty lane and must be a boolean", async () => {
		const fixture = await setup();
		cleanup = fixture.close;
		await expect(
			fixture.host.handle("rlm.spawn", { prompt: "x", kwargs: { name: "c", fork: true } }, BACKGROUND_CONTEXT),
		).rejects.toThrow(/no finished conversation to fork/);
		await expect(
			fixture.host.handle("rlm.spawn", { prompt: "x", kwargs: { name: "c", fork: "yes" } }, BACKGROUND_CONTEXT),
		).rejects.toThrow(/fork must be True or False/);
	});

	test("forkPoint stops before a call still waiting for its result", () => {
		let seq = 0;
		const entry = (id: string, message: AgentMessage): Entry => {
			seq += 1;
			return { id, parentId: null, seq, timestamp: seq, type: "message", message };
		};
		const user = (text: string) => ({ role: "user", content: text, timestamp: 0 }) as AgentMessage;
		const result = (id: string) =>
			({
				role: "toolResult",
				toolCallId: id,
				toolName: "rlm",
				content: [],
				isError: false,
				timestamp: 0,
			}) as AgentMessage;
		expect(forkPoint([])).toBeUndefined();
		const done = [entry("u", user("hi")), entry("a1", assistantCalling("c1")), entry("r1", result("c1"))];
		expect(forkPoint(done)).toBe("r1");
		expect(forkPoint([...done, entry("a2", assistantCalling("c2"))])).toBe("r1");
	});
});
