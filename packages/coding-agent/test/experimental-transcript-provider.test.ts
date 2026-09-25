import type { AgentLane, EventListener, HarnessEvent, LaneSnapshot, WatchHandle } from "@ultron/agent-core";
import { replicatedState } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { describe, expect, test, vi } from "vitest";
import type { TranscriptState } from "../src/experimental/services/transcript.ts";
import { createTranscriptService } from "../src/experimental/services/transcript-provider.ts";

function laneSnapshot(tipId: string | null = null): LaneSnapshot {
	return {
		lane: "main",
		transcript: [],
		tipId,
		configuration: {
			model: { provider: "test", modelId: "model" },
			thinkingLevel: "off",
			activeToolNames: [],
		},
		stats: {
			messageCount: 0,
			usage: {
				input: 0,
				output: 0,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 0,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
		},
		operation: null,
		queues: [],
		faulted: false,
	};
}

describe("Transcript service", () => {
	test("publishes coherent state and rebases after navigation", async () => {
		let listener: EventListener | undefined;
		const replacement = laneSnapshot("replacement-tip");
		const resnapshot = vi.fn(async () => replacement);
		const unsubscribe = vi.fn();
		const handle: WatchHandle<LaneSnapshot> = {
			snapshot: laneSnapshot(),
			start(next) {
				listener = next;
			},
			resnapshot,
			unsubscribe,
		};
		const lane = { watch: async () => handle } as unknown as AgentLane;
		const runtime = createTranscriptService(lane, replicatedState);
		const states: TranscriptState[] = [];
		runtime.service.state.subscribe((value) => {
			if (value.snapshot !== null) states.push(value);
		});
		await runtime.activate();

		expect(states).toHaveLength(1);
		expect(states[0]).toMatchObject({ snapshot: { lane: "main", operation: null }, event: null });
		await listener?.({ type: "run_start", lane: "main", runId: "run-1", startedAt: 1 }, BACKGROUND_CONTEXT);
		expect(states).toHaveLength(2);
		expect(states[1]).toMatchObject({
			snapshot: { operation: { id: "run-1" } },
			event: { type: "run_start", runId: "run-1" },
		});

		await listener?.(
			{
				type: "entry_added",
				lane: "main",
				entry: {
					id: "entry-1",
					parentId: null,
					seq: 1,
					timestamp: 2,
					type: "message",
					message: { role: "user", content: "hello", timestamp: 2 },
				},
			},
			BACKGROUND_CONTEXT,
		);
		expect(states.at(-1)).toMatchObject({
			snapshot: { tipId: "entry-1", transcript: [{ id: "entry-1" }] },
			event: { type: "entry_added" },
		});

		const navigation: HarnessEvent = {
			type: "navigation_end",
			lane: "main",
			runId: "navigation-1",
			status: "completed",
			fromTipId: "entry-1",
			tipId: "replacement-tip",
			endedAt: 3,
		};
		await listener?.(navigation, BACKGROUND_CONTEXT);
		await vi.waitFor(() => expect(states.at(-1)?.event).toBeNull());
		expect(states.at(-2)).toMatchObject({
			snapshot: { tipId: "entry-1" },
			event: { type: "navigation_end" },
		});
		expect(states.at(-1)).toMatchObject({ snapshot: { tipId: "replacement-tip" }, event: null });
		expect(runtime.service.state.value).toMatchObject({ snapshot: { tipId: "replacement-tip" }, event: null });
		expect(resnapshot).toHaveBeenCalledOnce();

		await runtime.dispose();
		expect(unsubscribe).toHaveBeenCalledOnce();
	});

	test("tool events with undefined fields still reach subscribers, as JSON", async () => {
		let listener: EventListener | undefined;
		const handle: WatchHandle<LaneSnapshot> = {
			snapshot: laneSnapshot(),
			start(next) {
				listener = next;
			},
			resnapshot: async () => laneSnapshot(),
			unsubscribe() {},
		};
		const runtime = createTranscriptService({ watch: async () => handle } as unknown as AgentLane, replicatedState);
		const events: unknown[] = [];
		runtime.service.state.subscribe((value, _context, delivery) => {
			if (delivery.kind === "update" && value.event !== null) events.push(value.event);
		});
		await runtime.activate();
		const base = { lane: "main", runId: "run-1", turnId: "turn-1", toolCallId: "call-1", toolName: "bash" };
		await listener?.({ type: "run_start", lane: "main", runId: "run-1", startedAt: 1 }, BACKGROUND_CONTEXT);
		await listener?.({ type: "tool_start", ...base, args: { command: "ls" } } as HarnessEvent, BACKGROUND_CONTEXT);
		await listener?.(
			{ type: "tool_update", ...base, partialResult: { content: [], details: undefined } } as HarnessEvent,
			BACKGROUND_CONTEXT,
		);
		await listener?.(
			{
				type: "tool_end",
				...base,
				result: { content: [{ type: "text", text: "a.txt" }], details: undefined },
				isError: false,
				terminate: false,
			} as HarnessEvent,
			BACKGROUND_CONTEXT,
		);
		expect(events.map((event) => (event as { type: string }).type)).toEqual([
			"run_start",
			"tool_start",
			"tool_update",
			"tool_end",
		]);
		expect(events[2]).toMatchObject({ partialResult: { content: [] } });
		expect(events[2]).not.toHaveProperty("partialResult.details");
		expect(events[3]).toMatchObject({ result: { content: [{ type: "text", text: "a.txt" }] }, isError: false });
		await runtime.dispose();
	});
});
