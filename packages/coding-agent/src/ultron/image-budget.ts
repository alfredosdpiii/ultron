/**
 * Image budget: every request re-sends every image still in the conversation, and `view_image` results pile up in
 * a long session. When a lane's context holds more than `max` tool-result images, the oldest are replaced by a short
 * text note before the request; the durable transcript keeps them.
 *
 * Images are dropped in batches of half the budget, so between max/2 and max stay, and an earlier message changes
 * (losing the prompt cache from there) only once every max/2 new images rather than with each one. The result is a
 * function of the messages alone, the same on every request. ULTRON_CONTEXT_IMAGES sets `max` (default 8; 0 keeps all).
 */
import type { AgentHarness, AgentMessage } from "@ultron/agent-core";

export const DEFAULT_CONTEXT_IMAGES = 8;
export const OFFLOADED_IMAGE_TEXT =
	"[image omitted from context to stay within the image budget; call view_image again to see it]";

/** Tool-result images kept in a request: ULTRON_CONTEXT_IMAGES, default 8; 0 disables the budget. */
export function contextImageBudget(env: NodeJS.ProcessEnv = process.env): number {
	const raw = env.ULTRON_CONTEXT_IMAGES?.trim();
	const value = raw ? Number(raw) : Number.NaN;
	return Number.isInteger(value) && value >= 0 ? value : DEFAULT_CONTEXT_IMAGES;
}

/** How many of `total` images (oldest first) to drop under budget `max`. */
export function imagesToDrop(total: number, max: number): number {
	if (max <= 0 || total <= max) return 0;
	const step = Math.max(1, Math.floor(max / 2));
	return Math.min(total, Math.floor((total - step) / step) * step);
}

/** `messages` with the oldest tool-result images replaced by a note; undefined when nothing changes. */
export function withImageBudget(messages: readonly AgentMessage[], max: number): AgentMessage[] | undefined {
	let total = 0;
	for (const message of messages)
		if (message.role === "toolResult") for (const part of message.content) if (part.type === "image") total += 1;
	let drop = imagesToDrop(total, max);
	if (drop === 0) return undefined;
	return messages.map((message) => {
		if (drop === 0 || message.role !== "toolResult" || !message.content.some((part) => part.type === "image"))
			return message;
		const content = message.content.map((part) => {
			if (part.type !== "image" || drop === 0) return part;
			drop -= 1;
			return { type: "text" as const, text: OFFLOADED_IMAGE_TEXT };
		});
		return { ...message, content };
	});
}

/** Apply the image budget to every request; returns the uninstaller. */
export function installImageBudget(harness: AgentHarness, max: number = contextImageBudget()): () => void {
	if (max <= 0) return () => {};
	return harness.hooks.on("transform_context", (event) => {
		const messages = withImageBudget(event.messages, max);
		return messages === undefined ? undefined : { messages };
	});
}
