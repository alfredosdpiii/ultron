/**
 * Clock context: the root prompt carries no date, so a lane cannot tell that a resumed session sat idle for a day.
 * Before a user message that starts a lane's conversation, or that comes CLOCK_GAP_MS or more after the message
 * before it, the model sees one line with the local time and the gap ("[clock] 2026-10-05 14:03 UTC+02:00, 3 h
 * after the previous message").
 *
 * The line is derived from the messages' own timestamps on every request, never stored, so the same history always
 * renders the same text and the prompt cache holds. Root and subagent lanes get it; inference frames and typed
 * agents do not. ULTRON_CLOCK_CONTEXT=off disables it.
 */
import { type AgentHarness, type AgentMessage, createCustomMessage } from "@ultron/agent-core";

export const CLOCK_MESSAGE_TYPE = "ultron-clock";
export const CLOCK_GAP_MS = 10 * 60 * 1000;

export function clockContextEnabled(env: NodeJS.ProcessEnv = process.env): boolean {
	const raw = env.ULTRON_CLOCK_CONTEXT?.trim().toLowerCase();
	return !(raw === "off" || raw === "0" || raw === "false");
}

/** Lanes whose conversation is a person's (the root) or a subagent's. */
export function clockLane(lane: string): boolean {
	return lane === "main" || lane.startsWith("ultron.rlm-child.");
}

/** Local time with its UTC offset, to the minute: "2026-10-05 14:03 UTC+02:00". */
export function formatClock(timestamp: number): string {
	const date = new Date(timestamp);
	const pad = (value: number) => String(value).padStart(2, "0");
	const offset = -date.getTimezoneOffset();
	const sign = offset >= 0 ? "+" : "-";
	const zone = `UTC${sign}${pad(Math.floor(Math.abs(offset) / 60))}:${pad(Math.abs(offset) % 60)}`;
	return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())} ${zone}`;
}

/** A gap in the largest whole unit: "12 min", "3 h", "2 d". */
export function formatGap(ms: number): string {
	const minutes = Math.floor(ms / 60_000);
	if (minutes < 120) return `${minutes} min`;
	const hours = Math.floor(minutes / 60);
	return hours < 48 ? `${hours} h` : `${Math.floor(hours / 24)} d`;
}

function timestampOf(message: AgentMessage): number | undefined {
	const value = (message as { timestamp?: unknown }).timestamp;
	return typeof value === "number" && Number.isFinite(value) && value > 0 ? value : undefined;
}

/** `messages` with a clock line before each user message that opens the conversation or follows a gap; undefined when none. */
export function withClockLines(messages: readonly AgentMessage[], gapMs = CLOCK_GAP_MS): AgentMessage[] | undefined {
	const out: AgentMessage[] = [];
	let previous: number | undefined;
	let added = false;
	for (const message of messages) {
		const at = timestampOf(message);
		if (message.role === "user" && at !== undefined && (previous === undefined || at - previous >= gapMs)) {
			const text = `[clock] ${formatClock(at)}${previous === undefined ? "" : `, ${formatGap(at - previous)} after the previous message`}`;
			out.push(createCustomMessage(CLOCK_MESSAGE_TYPE, text, false, undefined, at));
			added = true;
		}
		out.push(message);
		if (at !== undefined) previous = at;
	}
	return added ? out : undefined;
}

/** Add clock lines to root and subagent requests; returns the uninstaller. */
export function installClockContext(harness: AgentHarness): () => void {
	return harness.hooks.on("transform_context", (event) => {
		if (!clockLane(event.lane)) return undefined;
		const messages = withClockLines(event.messages);
		return messages === undefined ? undefined : { messages };
	});
}
