/**
 * A native session file, read for the session report without opening the session: a running worker may own the
 * file, and a read here never writes (opening a session repairs a torn last line by rewriting the file).
 *
 * The file is an append-only log of transactions (see `@ultron/agent-core`'s JSONL storage): entries, usage rows,
 * and writes of session values. This reads it line by line and keeps a digest: per entry who wrote it and what it
 * used (never its text), the latest value of the namespaces the report reads, and every operation's lane and
 * result, including operations whose `pi.op.meta` was deleted when they ended.
 */
import { createReadStream } from "node:fs";
import { stat } from "node:fs/promises";
import { createInterface } from "node:readline";
import { type CellApi, cellApis, classifyNudge, type NudgeKind } from "./session-stats.ts";

export type LogUsage = {
	input: number;
	output: number;
	cacheRead: number;
	cacheWrite: number;
	totalTokens: number;
	/** Provider-reported cost; null when the response carried none. */
	cost: number | null;
};

export type LogToolCall = {
	name: string;
	/** For an `rlm` cell: the REPL APIs its code names. */
	apis?: CellApi[];
};

export type LogEntry = {
	id: string;
	parentId: string | null;
	timestamp: number;
	kind: "user" | "assistant" | "toolResult" | "custom" | "compaction" | "branch_summary" | "other";
	/** Assistant messages. */
	provider?: string;
	model?: string;
	usage?: LogUsage;
	toolCalls?: LogToolCall[];
	/** Tool results. */
	toolName?: string;
	isError?: boolean;
	/** `[REDACTED:<kind>]` markers in a tool result's text. */
	redactions?: number;
	/** A user message that is one of Ultron's steers. */
	nudge?: NudgeKind;
};

export type LogUsageRow = { usage: LogUsage; entryId: string | null; adjustment: boolean };

export type LogOperation = { id: string; lane: string; kind: string; startedAt: number };

export type LogResult = {
	operationId: string;
	kind: string;
	status: string;
	fromTipId: string | null;
	tipId: string | null;
	startedAt: number;
	endedAt: number;
};

export type SessionLogHeader = { id: string; cwd: string; createdAt: number; parentSessionId?: string };

export type SessionLog = {
	path: string;
	header: SessionLogHeader;
	/** File modification time and size when read. */
	modifiedAt: number;
	bytes: number;
	entries: LogEntry[];
	usageRows: LogUsageRow[];
	/** Latest value by `namespace/key`, for the namespaces in {@link KEPT_NAMESPACES}. */
	values: Map<string, unknown>;
	operations: Map<string, LogOperation>;
	results: LogResult[];
	/** Lines that were not valid JSON (a torn last line while a worker is writing). */
	unreadableLines: number;
};

/** Namespaces whose latest values the report reads. Everything else (pending frames, operation state) is skipped. */
const KEPT_NAMESPACES = new Set([
	"pi.session.name",
	"pi.lane.config",
	"pi.branch.tip",
	"ultron.tasks",
	"ultron.usage",
	"ultron.rlm.frames",
	"ultron.jev.decisions",
	"ultron.memory.state",
	"ultron.module",
	"ultron.claude-code.lanes",
]);

export class SessionLogError extends Error {}

function record(value: unknown): Record<string, unknown> | undefined {
	return value !== null && typeof value === "object" && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

function amount(value: unknown): number {
	return typeof value === "number" && Number.isFinite(value) && value > 0 ? value : 0;
}

export function logUsage(value: unknown): LogUsage | undefined {
	const usage = record(value);
	if (usage === undefined) return undefined;
	const cost = record(usage.cost)?.total;
	const input = amount(usage.input);
	const output = amount(usage.output);
	const cacheRead = amount(usage.cacheRead);
	const cacheWrite = amount(usage.cacheWrite);
	return {
		input,
		output,
		cacheRead,
		cacheWrite,
		totalTokens: amount(usage.totalTokens) || input + output + cacheRead + cacheWrite,
		cost: typeof cost === "number" && Number.isFinite(cost) && cost >= 0 ? cost : null,
	};
}

function textOf(content: unknown): string {
	if (typeof content === "string") return content;
	if (!Array.isArray(content)) return "";
	let text = "";
	for (const part of content) {
		const item = record(part);
		if (item?.type === "text" && typeof item.text === "string") text += item.text;
	}
	return text;
}

function timestampOf(value: unknown): number {
	return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function digestEntry(write: Record<string, unknown>): LogEntry | undefined {
	if (typeof write.id !== "string") return undefined;
	const entry: LogEntry = {
		id: write.id,
		parentId: typeof write.parentId === "string" ? write.parentId : null,
		timestamp: timestampOf(write.timestamp),
		kind: "other",
	};
	if (write.type === "compaction" || write.type === "branch_summary" || write.type === "custom") {
		entry.kind = write.type;
		const usage = logUsage(write.usage);
		if (usage !== undefined) entry.usage = usage;
		return entry;
	}
	const message = record(write.message);
	if (write.type !== "message" || message === undefined) return entry;
	if (message.role === "assistant") {
		entry.kind = "assistant";
		if (typeof message.provider === "string") entry.provider = message.provider;
		if (typeof message.model === "string") entry.model = message.model;
		const usage = logUsage(message.usage);
		if (usage !== undefined) entry.usage = usage;
		const calls: LogToolCall[] = [];
		for (const part of Array.isArray(message.content) ? message.content : []) {
			const item = record(part);
			if (item?.type !== "toolCall" || typeof item.name !== "string") continue;
			const code = record(item.arguments)?.code;
			calls.push(
				item.name === "rlm" && typeof code === "string"
					? { name: item.name, apis: cellApis(code) }
					: { name: item.name },
			);
		}
		if (calls.length > 0) entry.toolCalls = calls;
	} else if (message.role === "toolResult") {
		entry.kind = "toolResult";
		if (typeof message.toolName === "string") entry.toolName = message.toolName;
		entry.isError = message.isError === true;
		const redactions = textOf(message.content).split("[REDACTED:").length - 1;
		if (redactions > 0) entry.redactions = redactions;
	} else if (message.role === "user") {
		entry.kind = "user";
		const nudge = classifyNudge(textOf(message.content));
		if (nudge !== undefined) entry.nudge = nudge;
	} else if (message.role === "custom") {
		entry.kind = "custom";
	}
	return entry;
}

function parseHeader(line: string, path: string): SessionLogHeader {
	let value: unknown;
	try {
		value = JSON.parse(line);
	} catch {
		throw new SessionLogError(`${path} is not a session file (its first line is not JSON)`);
	}
	const header = record(value);
	if (header?.type === "session" && header.version !== undefined)
		throw new SessionLogError(
			`${path} is a Pi-format session (version ${String(header.version)}); only native Ultron sessions are reported`,
		);
	if (header?.kind !== "header" || typeof header.id !== "string" || typeof header.cwd !== "string")
		throw new SessionLogError(`${path} is not a native Ultron session file`);
	return {
		id: header.id,
		cwd: header.cwd,
		createdAt: timestampOf(header.createdAt),
		...(typeof header.parentSessionId === "string" ? { parentSessionId: header.parentSessionId } : {}),
	};
}

/** Read only a session file's header (its first line). */
export async function readSessionHeader(path: string): Promise<SessionLogHeader> {
	const lines = createInterface({ input: createReadStream(path, { encoding: "utf8" }), crlfDelay: Infinity });
	try {
		for await (const line of lines) return parseHeader(line, path);
	} finally {
		lines.close();
	}
	throw new SessionLogError(`${path} is empty`);
}

/** Read a native session file. Throws {@link SessionLogError} for a file that is not one. */
export async function readSessionLog(path: string): Promise<SessionLog> {
	const info = await stat(path);
	const lines = createInterface({ input: createReadStream(path, { encoding: "utf8" }), crlfDelay: Infinity });
	let header: SessionLogHeader | undefined;
	const log: Omit<SessionLog, "header"> = {
		path,
		modifiedAt: Math.round(info.mtimeMs),
		bytes: info.size,
		entries: [],
		usageRows: [],
		values: new Map(),
		operations: new Map(),
		results: [],
		unreadableLines: 0,
	};
	const apply = (write: Record<string, unknown>): void => {
		if (write.kind === "entry") {
			const entry = digestEntry(write);
			if (entry !== undefined) log.entries.push(entry);
			return;
		}
		if (write.kind === "usage") {
			const usage = logUsage(write.usage);
			if (usage !== undefined)
				log.usageRows.push({
					usage,
					entryId: typeof write.entryId === "string" ? write.entryId : null,
					adjustment: write.adjustment === true,
				});
			return;
		}
		if (write.kind !== "value" || typeof write.namespace !== "string" || typeof write.key !== "string") return;
		const value = record(write.value);
		if (write.namespace === "pi.op.meta") {
			// Deleted when the operation ends; the set is the only place its lane is written.
			if (write.op === "set" && typeof value?.lane === "string")
				log.operations.set(write.key, {
					id: write.key,
					lane: value.lane,
					kind: typeof record(value.intent)?.kind === "string" ? (record(value.intent)?.kind as string) : "run",
					startedAt: timestampOf(value.startedAt),
				});
			return;
		}
		if (write.namespace === "pi.result") {
			if (write.op === "set" && value !== undefined)
				log.results.push({
					operationId: typeof value.operationId === "string" ? value.operationId : write.key,
					kind: typeof value.kind === "string" ? value.kind : "run",
					status: typeof value.status === "string" ? value.status : "unknown",
					fromTipId: typeof value.fromTipId === "string" ? value.fromTipId : null,
					tipId: typeof value.tipId === "string" ? value.tipId : null,
					startedAt: timestampOf(value.startedAt),
					endedAt: timestampOf(value.endedAt),
				});
			return;
		}
		if (!KEPT_NAMESPACES.has(write.namespace)) return;
		const address = `${write.namespace}/${write.key}`;
		if (write.op === "set") log.values.set(address, write.value);
		else if (write.op === "delete") log.values.delete(address);
	};
	try {
		for await (const line of lines) {
			if (header === undefined) {
				header = parseHeader(line, path);
				continue;
			}
			// Streamed assistant frames are most of a file's lines and nothing here reads them.
			if (line.length === 0 || line.startsWith('{"kind":"list"')) continue;
			let parsed: unknown;
			try {
				parsed = JSON.parse(line);
			} catch {
				log.unreadableLines += 1;
				continue;
			}
			for (const write of Array.isArray(parsed) ? parsed : [parsed]) {
				const item = record(write);
				if (item !== undefined) apply(item);
			}
		}
	} finally {
		lines.close();
	}
	if (header === undefined) throw new SessionLogError(`${path} is empty`);
	return { header, ...log };
}

/** The latest value at `namespace/key`, or undefined. */
export function logValue(log: SessionLog, namespace: string, key: string): unknown {
	return log.values.get(`${namespace}/${key}`);
}

/** The latest values of a namespace, by key. */
export function logValues(log: SessionLog, namespace: string): Map<string, unknown> {
	const prefix = `${namespace}/`;
	const found = new Map<string, unknown>();
	for (const [address, value] of log.values)
		if (address.startsWith(prefix)) found.set(address.slice(prefix.length), value);
	return found;
}
