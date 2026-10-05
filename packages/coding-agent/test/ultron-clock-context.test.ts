/**
 * Clock context (clock-context.ts): a `[clock]` line with the local time before a user message that opens a lane's
 * conversation or comes 10 minutes or more after the previous message; derived from timestamps, so it is the same
 * on every request; root and subagent lanes only; ULTRON_CLOCK_CONTEXT=off disables it.
 */
import { AgentHarness, type AgentMessage, MemorySessionRepo } from "@ultron/agent-core";
import { createModels, fauxAssistantMessage, fauxProvider } from "@ultron/ai";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { describe, expect, test } from "vitest";
import {
	CLOCK_MESSAGE_TYPE,
	clockContextEnabled,
	formatClock,
	formatGap,
	installClockContext,
	withClockLines,
} from "../src/ultron/clock-context.ts";
import { messageText } from "../src/ultron/context-control.ts";

const T0 = Date.UTC(2026, 9, 5, 12, 0);
const user = (text: string, timestamp: number) => ({ role: "user", content: text, timestamp }) as AgentMessage;
const answer = (timestamp: number) => ({ ...fauxAssistantMessage("ok"), timestamp }) as AgentMessage;

describe("clock lines", () => {
	test("before the first user message and after a gap of 10 minutes or more, never otherwise", () => {
		const messages = [
			user("a", T0),
			answer(T0 + 1000),
			user("b", T0 + 5 * 60_000),
			answer(T0 + 6 * 60_000),
			user("c", T0 + 3 * 3600_000),
		];
		const out = withClockLines(messages)!;
		const lines = out.filter((message) => message.role === "custom");
		expect(lines).toHaveLength(2);
		expect((lines[0] as { customType: string }).customType).toBe(CLOCK_MESSAGE_TYPE);
		expect(messageText(lines[0]!)).toBe(`[clock] ${formatClock(T0)}`);
		expect(messageText(lines[1]!)).toBe(`[clock] ${formatClock(T0 + 3 * 3600_000)}, 2 h after the previous message`);
		expect(out.indexOf(lines[1]!)).toBe(out.indexOf(messages[4]!) - 1);
		// Deterministic: the same history gives the same lines.
		expect(withClockLines(messages)).toEqual(out);
		// Messages without timestamps get none.
		expect(withClockLines([user("x", 0)])).toBeUndefined();
	});

	test("formats", () => {
		expect(formatClock(T0)).toMatch(/^2026-10-0[45] \d\d:\d\d UTC[+-]\d\d:\d\d$/);
		expect(formatGap(12 * 60_000)).toBe("12 min");
		expect(formatGap(3 * 3600_000)).toBe("3 h");
		expect(formatGap(50 * 3600_000)).toBe("2 d");
		expect(clockContextEnabled({})).toBe(true);
		expect(clockContextEnabled({ ULTRON_CLOCK_CONTEXT: "off" })).toBe(false);
	});

	test("root and subagent requests carry it; other lanes do not", async () => {
		const repo = new MemorySessionRepo();
		const session = await repo.create({ id: "clock" }, BACKGROUND_CONTEXT);
		const faux = fauxProvider();
		const seen: Array<{ lane: string; text: string }> = [];
		faux.setResponses(
			Array.from({ length: 3 }, () => (context: { messages: AgentMessage[] }, options?: { sessionId?: string }) => {
				seen.push({
					lane: (options?.sessionId ?? "").slice("clock:".length),
					text: context.messages.map((message) => JSON.stringify(message)).join("\n"),
				});
				return fauxAssistantMessage("ok");
			}) as never,
		);
		const models = createModels();
		models.setProvider(faux.provider);
		const { harness } = await AgentHarness.create(
			{ session, models, model: faux.getModel(), activeToolNames: [] },
			BACKGROUND_CONTEXT,
		);
		const remove = installClockContext(harness);
		try {
			for (const name of ["main", "ultron.rlm-child.t1", "ultron.rlm-frame.f1"]) {
				const lane = await harness.lane(name, BACKGROUND_CONTEXT);
				await lane.prompt("hello", undefined, BACKGROUND_CONTEXT);
			}
			expect(seen.map((request) => [request.lane, request.text.includes("[clock] ")])).toEqual([
				["main", true],
				["ultron.rlm-child.t1", true],
				["ultron.rlm-frame.f1", false],
			]);
		} finally {
			remove();
			await harness.close(BACKGROUND_CONTEXT);
		}
	});
});
