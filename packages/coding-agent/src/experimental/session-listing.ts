/**
 * What Pi's session selector shows for a native Session (name, first user message, message count), read from its
 * JSONL file without opening it: a running worker may own the file, and a read never writes.
 */

import { readFile } from "node:fs/promises";

export interface NativeSessionFileDetails {
	name: string | null;
	firstMessage: string;
	messageCount: number;
}

interface Mutation {
	kind?: unknown;
	type?: unknown;
	op?: unknown;
	namespace?: unknown;
	key?: unknown;
	value?: unknown;
	message?: unknown;
	name?: unknown;
}

const SESSION_NAME_NAMESPACE = "pi.session.name";

export async function readNativeSessionDetails(path: string): Promise<NativeSessionFileDetails> {
	let text: string;
	try {
		text = await readFile(path, "utf8");
	} catch {
		return { name: null, firstMessage: "", messageCount: 0 };
	}
	return nativeSessionDetails(text);
}

/** Parse a native (or legacy Pi-format) session file's lines; unreadable lines, like a torn last line, are skipped. */
export function nativeSessionDetails(text: string): NativeSessionFileDetails {
	let name: string | null = null;
	let firstMessage = "";
	let messageCount = 0;
	const visit = (mutation: Mutation): void => {
		if (mutation.kind === "value" && mutation.namespace === SESSION_NAME_NAMESPACE) {
			name = mutation.op === "set" && typeof mutation.value === "string" ? mutation.value : null;
			return;
		}
		// Pi's own format (a legacy file the repo still reads).
		if (mutation.type === "session_info") {
			name = typeof mutation.name === "string" && mutation.name.trim() ? mutation.name.trim() : null;
			return;
		}
		if ((mutation.kind === "entry" || mutation.kind === undefined) && mutation.type === "message") {
			const message = mutation.message as { role?: unknown; content?: unknown } | undefined;
			if (message?.role !== "user" && message?.role !== "assistant") return;
			messageCount += 1;
			if (message.role === "user" && !firstMessage) firstMessage = contentText(message.content);
		}
	};
	for (const line of text.split("\n")) {
		if (!line.trim()) continue;
		let parsed: unknown;
		try {
			parsed = JSON.parse(line);
		} catch {
			continue;
		}
		for (const mutation of Array.isArray(parsed) ? parsed : [parsed]) {
			if (typeof mutation === "object" && mutation !== null) visit(mutation as Mutation);
		}
	}
	return { name, firstMessage, messageCount };
}

function contentText(content: unknown): string {
	if (typeof content === "string") return content;
	if (!Array.isArray(content)) return "";
	return content
		.flatMap((part) =>
			typeof part === "object" && part !== null && (part as { type?: unknown }).type === "text"
				? [String((part as { text?: unknown }).text ?? "")]
				: [],
		)
		.join(" ");
}
