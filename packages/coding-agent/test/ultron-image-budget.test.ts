/**
 * Image budget (image-budget.ts): past ULTRON_CONTEXT_IMAGES (default 8) tool-result images, the oldest are replaced
 * by a note in batches of half the budget, so 4 to 8 stay and earlier messages change only every 4 new images.
 */
import type { AgentMessage } from "@ultron/agent-core";
import { describe, expect, test } from "vitest";
import { contextImageBudget, imagesToDrop, OFFLOADED_IMAGE_TEXT, withImageBudget } from "../src/ultron/image-budget.ts";

const shot = (id: string, images: number): AgentMessage =>
	({
		role: "toolResult",
		toolCallId: id,
		toolName: "rlm",
		content: [
			{ type: "text", text: `cell ${id}` },
			...Array.from({ length: images }, (_, index) => ({
				type: "image",
				data: `${id}-${index}`,
				mimeType: "image/png",
			})),
		],
		isError: false,
		timestamp: 1,
	}) as AgentMessage;

const images = (messages: readonly AgentMessage[]) =>
	messages.flatMap((message) =>
		message.role === "toolResult"
			? message.content.flatMap((part) => (part.type === "image" ? [part.data] : []))
			: [],
	);

describe("image budget", () => {
	test("drops in batches of half the budget, keeping between half and all of it", () => {
		const kept = (total: number) => total - imagesToDrop(total, 8);
		expect([1, 8, 9, 11, 12, 15, 16, 20].map(kept)).toEqual([1, 8, 5, 7, 4, 7, 4, 4]);
		expect(imagesToDrop(100, 0)).toBe(0);
	});

	test("the oldest images become a note; text and newer images stay", () => {
		const messages = [shot("a", 3), shot("b", 3), shot("c", 3)];
		const out = withImageBudget(messages, 8)!;
		expect(images(out)).toEqual(["b-1", "b-2", "c-0", "c-1", "c-2"]);
		const first = out[0] as Extract<AgentMessage, { role: "toolResult" }>;
		expect(first.content).toEqual([
			{ type: "text", text: "cell a" },
			{ type: "text", text: OFFLOADED_IMAGE_TEXT },
			{ type: "text", text: OFFLOADED_IMAGE_TEXT },
			{ type: "text", text: OFFLOADED_IMAGE_TEXT },
		]);
		expect(images(messages)).toHaveLength(9);
		expect(withImageBudget([shot("a", 8)], 8)).toBeUndefined();
	});

	test("ULTRON_CONTEXT_IMAGES", () => {
		expect(contextImageBudget({})).toBe(8);
		expect(contextImageBudget({ ULTRON_CONTEXT_IMAGES: "0" })).toBe(0);
		expect(contextImageBudget({ ULTRON_CONTEXT_IMAGES: "x" })).toBe(8);
	});
});
