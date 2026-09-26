/**
 * Persistent prompt history for the native TUI.
 *
 * Every prompt, steer, follow-up, `!` command and slash command the user submits is appended to
 * `<agentDir>/prompt-history.jsonl`, one JSON object per line with the text, the working directory and a
 * timestamp. On start the most recent entries are loaded into the editor's up/down history: the current
 * project's prompts first (newest first), then other projects'. A resumed Session's own user messages join the
 * current project's by time, so Up recalls what was typed in that Session even if no history file recorded it.
 */

import { appendFileSync, closeSync, fstatSync, mkdirSync, openSync, readSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import type { AgentMessage, Entry } from "@ultron/agent-core";

export const PROMPT_HISTORY_FILE = "prompt-history.jsonl";
/** How many entries the editor keeps for up/down navigation and reverse search. */
export const PROMPT_HISTORY_LIMIT = 500;
/** Only the tail of the file is read on start; older lines are never needed for the most recent entries. */
const PROMPT_HISTORY_TAIL_BYTES = 4 * 1024 * 1024;

export interface PromptHistoryEntry {
	readonly text: string;
	readonly cwd: string;
	/** Milliseconds since the epoch. */
	readonly timestamp: number;
}

/** A prompt and when it was submitted (milliseconds since the epoch). */
export interface TimedPrompt {
	readonly text: string;
	readonly timestamp: number;
}

export function promptHistoryPath(agentDir: string): string {
	return join(agentDir, PROMPT_HISTORY_FILE);
}

/** Append-only prompt history file. A `null` path keeps the history in memory only. */
export class PromptHistoryStore {
	readonly path: string | null;
	readonly #cwd: string;
	readonly #limit: number;
	#last: string | undefined;
	/** This project's prompts and other projects', most recent first, including what this process appended. */
	#current: TimedPrompt[] = [];
	#others: TimedPrompt[] = [];

	constructor(options: { readonly path: string | null; readonly cwd: string; readonly limit?: number }) {
		this.path = options.path;
		this.#cwd = resolve(options.cwd);
		this.#limit = options.limit ?? PROMPT_HISTORY_LIMIT;
	}

	/**
	 * The most recent entries, most recent first: the current project's, then other projects'. Duplicates keep
	 * their most recent occurrence within each group, and a text is listed once overall.
	 */
	load(): string[] {
		const entries = this.#read();
		this.#last = entries.at(-1)?.text;
		this.#current = [];
		this.#others = [];
		for (let index = entries.length - 1; index >= 0; index--) {
			const entry = entries[index]!;
			(resolve(entry.cwd) === this.#cwd ? this.#current : this.#others).push(entry);
		}
		return this.history();
	}

	/**
	 * The history for the editor, most recent first: this project's prompts (from the file, from this process, and
	 * the given Session prompts, ordered by time), then other projects'. Each text is listed once.
	 */
	history(sessionPrompts: readonly TimedPrompt[] = []): string[] {
		const current = [...this.#current, ...sessionPrompts].sort((left, right) => right.timestamp - left.timestamp);
		return mergePromptHistory(
			[current.map((prompt) => prompt.text), this.#others.map((prompt) => prompt.text)],
			this.#limit,
		);
	}

	/**
	 * Record a submitted text. Whitespace-only text and a repeat of the previous entry are skipped. Returns the
	 * trimmed text when it was recorded. Write failures never reach the user: history is a convenience.
	 */
	append(text: string, now: number = Date.now()): string | undefined {
		const trimmed = text.trim();
		if (trimmed.length === 0 || trimmed === this.#last) return undefined;
		this.#last = trimmed;
		const entry: PromptHistoryEntry = { text: trimmed, cwd: this.#cwd, timestamp: now };
		this.#current.unshift(entry);
		if (this.path === null) return trimmed;
		try {
			mkdirSync(dirname(this.path), { recursive: true });
			// A torn last line (a crash mid-write) must not swallow this entry.
			const separator = endsWithNewline(this.path) ? "" : "\n";
			appendFileSync(this.path, `${separator}${JSON.stringify(entry)}\n`, { encoding: "utf8", mode: 0o600 });
		} catch {
			// A read-only or full agent dir must not break prompting.
		}
		return trimmed;
	}

	#read(): PromptHistoryEntry[] {
		if (this.path === null) return [];
		let text: string;
		try {
			text = readTail(this.path, PROMPT_HISTORY_TAIL_BYTES);
		} catch {
			return [];
		}
		const entries: PromptHistoryEntry[] = [];
		for (const line of text.split("\n")) {
			if (line.trim().length === 0) continue;
			try {
				const value = JSON.parse(line) as Partial<PromptHistoryEntry>;
				if (typeof value.text !== "string" || value.text.trim().length === 0) continue;
				entries.push({
					text: value.text.trim(),
					cwd: typeof value.cwd === "string" ? value.cwd : "",
					timestamp: typeof value.timestamp === "number" ? value.timestamp : 0,
				});
			} catch {
				// A torn final line from a crash, or a hand edit: skip it.
			}
		}
		return entries;
	}
}

/** Concatenate history groups (each most recent first), keeping the first occurrence of each text. */
export function mergePromptHistory(groups: readonly (readonly string[])[], limit = PROMPT_HISTORY_LIMIT): string[] {
	const seen = new Set<string>();
	const merged: string[] = [];
	for (const group of groups) {
		for (const raw of group) {
			const text = raw.trim();
			if (text.length === 0 || seen.has(text)) continue;
			seen.add(text);
			merged.push(text);
			if (merged.length >= limit) return merged;
		}
	}
	return merged;
}

/** The user-typed prompts of a Session transcript, most recent first, with when they were sent. */
export function sessionPromptHistory(transcript: readonly Entry[]): TimedPrompt[] {
	const prompts: TimedPrompt[] = [];
	for (let index = transcript.length - 1; index >= 0; index--) {
		const entry = transcript[index]!;
		if (entry.type !== "message") continue;
		const text = userText(entry.message);
		if (text !== undefined) prompts.push({ text, timestamp: entry.timestamp });
	}
	return prompts;
}

/**
 * Entries matching a reverse-search query, most recent first. Every whitespace-separated term must occur
 * (case-insensitive); an empty query matches everything.
 */
export function searchPromptHistory(history: readonly string[], query: string): string[] {
	const terms = query.toLowerCase().split(/\s+/u).filter(Boolean);
	if (terms.length === 0) return [...history];
	return history.filter((entry) => {
		const lower = entry.toLowerCase();
		return terms.every((term) => lower.includes(term));
	});
}

function userText(message: AgentMessage): string | undefined {
	if (message.role !== "user") return undefined;
	const text =
		typeof message.content === "string"
			? message.content
			: message.content.flatMap((part) => (part.type === "text" ? [part.text] : [])).join("");
	const trimmed = text.trim();
	return trimmed.length === 0 ? undefined : trimmed;
}

/** True for a missing or empty file, or one whose last byte is a newline. */
function endsWithNewline(path: string): boolean {
	let fd: number;
	try {
		fd = openSync(path, "r");
	} catch {
		return true;
	}
	try {
		const size = fstatSync(fd).size;
		if (size === 0) return true;
		const last = Buffer.alloc(1);
		readSync(fd, last, 0, 1, size - 1);
		return last[0] === 0x0a;
	} finally {
		closeSync(fd);
	}
}

function readTail(path: string, maxBytes: number): string {
	const fd = openSync(path, "r");
	try {
		const size = fstatSync(fd).size;
		const length = Math.min(size, maxBytes);
		const buffer = Buffer.alloc(length);
		let offset = 0;
		while (offset < length) {
			const read = readSync(fd, buffer, offset, length - offset, size - length + offset);
			if (read === 0) break;
			offset += read;
		}
		const text = buffer.subarray(0, offset).toString("utf8");
		// A partial first line from cutting the file mid-record is dropped.
		return length < size ? text.slice(text.indexOf("\n") + 1) : text;
	} finally {
		closeSync(fd);
	}
}
