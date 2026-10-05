import {
	AgentHarness,
	type AgentMessage,
	applyContextEdits,
	CONTEXT_EDIT_CUSTOM_TYPE,
	CONTEXT_OMITTED_CUSTOM_TYPE,
	CONTEXT_OMITTED_TOOL_RESULT_TEXT,
	type Entry,
	MemorySessionRepo,
} from "@ultron/agent-core";
import { createModels, fauxAssistantMessage, fauxProvider, fauxToolCall } from "@ultron/ai";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { Type } from "typebox";
import { describe, expect, test } from "vitest";
import { renderRlmDock } from "../src/experimental/rlm-graph.ts";
import { PLAIN_STYLE, parseContextState } from "../src/experimental/rlm-visualizer.ts";
import {
	CONTEXT_ENTRY_PROJECTORS,
	CONTEXT_PROMPT,
	ContextControl,
	type ContextEditEvent,
	collapsedCellText,
	compactionTrimEnabled,
	messageText,
	taskLine,
	trimmedResultText,
} from "../src/ultron/context-control.ts";
import type { NativeHostApi } from "../src/ultron/rlm/host-module.ts";

let seq = 0;
function message(id: string, message: AgentMessage): Entry {
	seq += 1;
	return { id, parentId: null, seq, timestamp: seq, type: "message", message };
}
function edit(id: string, data: Record<string, unknown>): Entry {
	seq += 1;
	return {
		id,
		parentId: null,
		seq,
		timestamp: seq,
		type: "custom",
		customType: CONTEXT_EDIT_CUSTOM_TYPE,
		data: data as never,
	};
}
const user = (text: string): AgentMessage => ({ role: "user", content: text, timestamp: 0 });
const assistant = (text: string, calls: string[] = []): AgentMessage =>
	({
		role: "assistant",
		content: [
			{ type: "text", text },
			...calls.map((id) => ({ type: "toolCall", id, name: "rlm", arguments: { code: "x" } })),
		],
		api: "openai-completions",
		provider: "faux",
		model: "faux",
		usage: {
			input: 0,
			output: 0,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 0,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason: calls.length === 0 ? "stop" : "toolUse",
		timestamp: 0,
	}) as AgentMessage;
const result = (callId: string, text: string): AgentMessage =>
	({
		role: "toolResult",
		toolCallId: callId,
		toolName: "rlm",
		content: [{ type: "text", text }],
		isError: false,
		timestamp: 0,
	}) as AgentMessage;

describe("native context edits", () => {
	const base = (): Entry[] => [
		message("u1", user("first")),
		message("a1", assistant("calling", ["c1", "c2"])),
		message("r1", result("c1", "BIG ONE")),
		message("r2", result("c2", "BIG TWO")),
		message("a2", assistant("answer")),
	];
	const visible = (entries: Entry[]) =>
		entries.filter((entry) => !(entry.type === "custom" && entry.customType === CONTEXT_OMITTED_CUSTOM_TYPE));

	test("no edits leave entries untouched", () => {
		const entries = base();
		const applied = applyContextEdits(entries);
		expect(applied).toEqual(entries);
		for (const [index, entry] of applied.entries()) expect(entry).toBe(entries[index]);
	});

	test("omitting an assistant message omits the results of its calls", () => {
		const applied = visible(
			applyContextEdits([...base(), edit("e1", { edits: [{ targetId: "a1", replacement: null }] })]),
		);
		expect(applied.map((entry) => entry.id)).toEqual(["u1", "a2", "e1"]);
	});

	test("an omitted result whose call stays visible becomes a stub, keeping the pair", () => {
		const applied = applyContextEdits([...base(), edit("e1", { targetId: "r1", replacement: null })]);
		const r1 = applied.find((entry) => entry.id === "r1")!;
		expect(r1.type).toBe("message");
		expect(messageText((r1 as Extract<Entry, { type: "message" }>).message)).toBe(CONTEXT_OMITTED_TOOL_RESULT_TEXT);
	});

	test("a replacement that drops tool calls drops their results; later edits win", () => {
		const applied = visible(
			applyContextEdits([
				...base(),
				edit("e1", { edits: [{ targetId: "u1", replacement: null }] }),
				edit("e2", {
					edits: [
						{ targetId: "a1", replacement: { content: "SUMMARY" } },
						{ targetId: "u1", replacement: { content: "kept after all" } },
					],
				}),
			]),
		);
		expect(applied.map((entry) => entry.id)).toEqual(["u1", "a1", "a2", "e1", "e2"]);
		const text = applied.flatMap((entry) => (entry.type === "message" ? [messageText(entry.message)] : []));
		expect(text).toEqual(["kept after all", "SUMMARY", "answer"]);
	});
});

describe("collapse lines", () => {
	test("one line per task with definition, key, status, cost and handle; random keys are hidden", () => {
		expect(taskLine({ id: "t1", definition: "rlm-child@1", key: "fetch-1", status: "succeeded", cost: 0.0123 })).toBe(
			'↳ task t1 rlm-child@1 key=fetch-1 succeeded cost=$0.0123 · full result: await agents.result("t1")',
		);
		expect(
			taskLine({
				id: "t2",
				definition: "x@1",
				key: "0f8fad5b-d9cb-469f-a165-70867728950e",
				status: "failed",
				cost: null,
			}),
		).toBe('↳ task t2 x@1 failed cost=? · full result: await agents.result("t2")');
		const lines = Array.from({ length: 10 }, (_, index) => ({
			id: `t${index}`,
			definition: "d@1",
			key: `k${index}`,
			status: "succeeded",
			cost: 0,
		}));
		const text = collapsedCellText("entry-1", "x".repeat(5000), lines);
		expect(text).toContain('ctx.get("entry-1")');
		expect(text).toContain("+2 more finished tasks");
		expect(text.length).toBeLessThan(1500);
		expect(CONTEXT_PROMPT).toContain("ctx.forget");
	});
});

describe("ctx host module on a real lane", () => {
	test("history, summarize, guards, pin, note, and state follow the branch", async () => {
		const repo = new MemorySessionRepo();
		const session = await repo.create({ id: "ctx" }, BACKGROUND_CONTEXT);
		const faux = fauxProvider();
		const seen: string[][] = [];
		faux.setResponses(
			Array.from({ length: 10 }, (_, index) => (context: { messages: AgentMessage[] }) => {
				seen.push(context.messages.map((item) => messageText(item)));
				return fauxAssistantMessage(index === 0 ? "first answer" : `answer ${index}`);
			}) as never,
		);
		const models = createModels();
		models.setProvider(faux.provider);
		const { harness } = await AgentHarness.create(
			{ session, models, model: faux.getModel(), activeToolNames: [], entryProjectors: CONTEXT_ENTRY_PROJECTORS },
			BACKGROUND_CONTEXT,
		);
		const lane = await harness.lane("main", BACKGROUND_CONTEXT);
		const events: ContextEditEvent[] = [];
		const control = new ContextControl({ harness, rootLane: lane, observe: (event) => events.push(event) });
		const uninstall = control.install();
		const call = (type: string, payload: Record<string, unknown> = {}) =>
			control.module.handle(
				{ type, payload, caller: { lane: "main" }, context: BACKGROUND_CONTEXT },
				{} as NativeHostApi,
			);
		type Item = { id: string; kind: string; preview: string; state: string; pinned: boolean; protected: boolean };
		const history = async () => ((await call("ctx.history", { limit: 50 })) as { items: Item[] }).items;
		try {
			await lane.prompt("first question SECRET_ONE", undefined, BACKGROUND_CONTEXT);
			await lane.prompt("second question", undefined, BACKGROUND_CONTEXT);
			let items = await history();
			expect(items.map((item) => item.kind)).toEqual(["user", "assistant", "user", "assistant"]);
			const [firstQ, firstA, secondQ, secondA] = items;
			expect(secondQ!.protected).toBe(true);
			await expect(call("ctx.history", { kinds: ["bogus"] })).rejects.toThrow(/kinds/);

			// The current user message is refused; unknown ids too, all or nothing.
			await expect(call("ctx.forget", { ids: [firstQ!.id, secondQ!.id], reason: "x" })).rejects.toThrow(
				/current user turn/,
			);
			await expect(call("ctx.forget", { ids: ["nope"], reason: "x" })).rejects.toThrow(/not in the model's context/);
			expect((await history()).every((item) => item.state === "visible")).toBe(true);

			// Pin the second answer: summarize and forget refuse it.
			expect(await call("ctx.pin", { id: secondA!.id })).toMatchObject({ pinned: true, changed: true });
			await expect(call("ctx.summarize", { ids: [secondA!.id], text: "x" })).rejects.toThrow(/pinned/);

			const summary = (await call("ctx.summarize", {
				ids: [firstQ!.id, firstA!.id],
				text: "They asked one thing.",
			})) as {
				summarized: string[];
			};
			expect(summary.summarized).toEqual([firstQ!.id, firstA!.id]);
			await call("ctx.note", { text: "REMEMBER_THIS" });
			items = await history();
			expect(items.find((item) => item.id === firstQ!.id)!.state).toBe("summarized");
			expect(items.find((item) => item.id === firstA!.id)!.state).toBe("summarized");
			expect(items.at(-1)).toMatchObject({ kind: "note", preview: "REMEMBER_THIS" });
			const got = (await call("ctx.get", { id: firstQ!.id })) as { text: string; visible_text: string };
			expect(got.text).toContain("SECRET_ONE");
			expect(got.visible_text).toContain("They asked one thing.");

			await lane.prompt("third question", undefined, BACKGROUND_CONTEXT);
			const third = seen.at(-1)!;
			expect(third.join("\n")).not.toContain("SECRET_ONE");
			expect(third.join("\n")).not.toContain("first answer");
			expect(third[0]).toContain("They asked one thing.");
			expect(third).toContain("Note (ctx.note): REMEMBER_THIS");
			expect(third).toContain("answer 1");
			expect(events.map((event) => event.source)).toEqual(["ctx.summarize"]);
			expect(events[0]!.edits).toEqual([
				{ targetId: firstQ!.id, action: "replace" },
				{ targetId: firstA!.id, action: "omit" },
			]);

			const state = parseContextState(await call("ctx.state"));
			expect(state).toMatchObject({ forgottenCount: 2, pinnedCount: 1, notes: 1 });
			const panel = renderRlmDock({ now: 0, tasks: [], context: state }, 120, { style: PLAIN_STYLE }).join("\n");
			expect(panel).toContain("context: 2 forgotten · 0 collapsed · 1 pinned · 1 notes");
			expect(panel).toContain("pinned");

			// Unpinning makes it editable again.
			expect(await call("ctx.unpin", { id: secondA!.id })).toMatchObject({ pinned: false, changed: true });
			expect(await call("ctx.forget", { ids: [secondA!.id], reason: "done" })).toMatchObject({
				forgotten: [secondA!.id],
			});
		} finally {
			uninstall();
			await harness.close(BACKGROUND_CONTEXT);
		}
	});
});

describe("trim before compaction", () => {
	const BIG = `HEAD_MARK${"x".repeat(70_000)}MIDDLE_MARK${"y".repeat(10_000)}TAIL_MARK`;
	/**
	 * Two prompts on a 40,000-token window (compaction threshold 30,000). The first asks `question` and the model
	 * calls rlm, which returns `output`; the faux provider reports usage from the prompt's size.
	 */
	async function scenario(question: string, output: string) {
		const repo = new MemorySessionRepo();
		const session = await repo.create({ id: "trim" }, BACKGROUND_CONTEXT);
		const faux = fauxProvider({ models: [{ id: "small", contextWindow: 40_000 }] });
		const seen: string[] = [];
		const record = (context: { messages: AgentMessage[] }) =>
			seen.push(context.messages.map((item) => messageText(item)).join("\n"));
		faux.setResponses([
			((context: { messages: AgentMessage[] }) => {
				record(context);
				return fauxAssistantMessage(fauxToolCall("rlm", {}, { id: "c1" }), { stopReason: "toolUse" });
			}) as never,
			((context: { messages: AgentMessage[] }) => {
				record(context);
				return fauxAssistantMessage("read it");
			}) as never,
			...Array.from(
				{ length: 4 },
				() =>
					((context: { messages: AgentMessage[] }) => {
						record(context);
						return fauxAssistantMessage("SUMMARY_TEXT");
					}) as never,
			),
		]);
		const models = createModels();
		models.setProvider(faux.provider);
		const { harness } = await AgentHarness.create(
			{
				session,
				models,
				model: faux.getModel("small")!,
				activeToolNames: ["rlm"],
				compaction: { enabled: true, reserveTokens: 10_000, keepRecentTokens: 100 },
				tools: [
					{
						name: "rlm",
						label: "rlm",
						description: "runs a cell",
						parameters: Type.Object({}),
						execute: async () => ({ content: [{ type: "text" as const, text: output }], details: {} }),
					},
				],
				entryProjectors: CONTEXT_ENTRY_PROJECTORS,
			},
			BACKGROUND_CONTEXT,
		);
		const lane = await harness.lane("main", BACKGROUND_CONTEXT);
		const events: ContextEditEvent[] = [];
		const compactions: string[] = [];
		harness.events.on("compaction_end", (event) => {
			compactions.push(event.status);
		});
		const control = new ContextControl({
			harness,
			rootLane: lane,
			observe: (event) => events.push(event),
			trimBeforeCompaction: true,
		});
		const uninstall = control.install();
		try {
			await lane.prompt(question, undefined, BACKGROUND_CONTEXT);
			await lane.prompt("next question", undefined, BACKGROUND_CONTEXT);
			const entries = await lane.findEntries({ order: "oldestFirst" }, BACKGROUND_CONTEXT);
			return { seen, events, compactions, entries };
		} finally {
			uninstall();
			await harness.close(BACKGROUND_CONTEXT);
		}
	}

	test("an answered large result is trimmed and the threshold compaction declined", async () => {
		// About 40,000 tokens are over the threshold; trimming 75,000 characters frees about 18,700.
		const { seen, events, compactions, entries } = await scenario("look at the data", BIG);
		// Declined after the first run, and before the second prompt, whose estimate still uses the usage reported
		// before the trim.
		expect(compactions).toEqual(["declined", "declined"]);
		expect(entries.some((entry) => entry.type === "compaction")).toBe(false);
		expect(events.map((event) => event.source)).toEqual(["trim"]);
		const next = seen.at(-1)!;
		expect(next).toContain("HEAD_MARK");
		expect(next).toContain("TAIL_MARK");
		expect(next).not.toContain("MIDDLE_MARK");
		expect(next).toMatch(/characters trimmed before compaction; ctx\.get\("[^"]+"\) shows them/);
		// The durable transcript keeps the whole result.
		expect(JSON.stringify(entries)).toContain("MIDDLE_MARK");
	});

	test("when trimming cannot get under the threshold, nothing is edited and the compaction runs", async () => {
		// The bulk is the question itself; trimming the 9,000-character result frees about 1,000 tokens.
		const { events, compactions, entries } = await scenario(`question ${"q".repeat(70_000)}`, "r".repeat(9000));
		expect(events).toEqual([]);
		expect(compactions.length).toBeGreaterThan(0);
		expect(compactions.every((status) => status === "completed")).toBe(true);
		expect(entries.some((entry) => entry.type === "compaction")).toBe(true);
	});

	test("trimmed text and the off switch", () => {
		const text = trimmedResultText("e1", "a".repeat(5000) + "b".repeat(5000));
		expect(text.startsWith("a".repeat(4096))).toBe(true);
		expect(text.endsWith("b".repeat(1024))).toBe(true);
		expect(text).toContain("[... 4880 characters trimmed before compaction");
		expect(compactionTrimEnabled({})).toBe(true);
		expect(compactionTrimEnabled({ ULTRON_COMPACTION_TRIM: "off" })).toBe(false);
	});
});
