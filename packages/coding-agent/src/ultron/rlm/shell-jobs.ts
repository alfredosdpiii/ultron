/**
 * Host-owned shell jobs (nano-rlm's `rlm.shell.run`, adapted): `await bash(cmd, yield_after=s)` starts a command in
 * the session worker, not in the kernel, and returns either the finished result or a running handle after `s`
 * seconds. A job belongs to the lane that started it and to that lane's root turn: it outlives the cell, a kernel
 * restart and kernel eviction, and stops only on its own timeout, `job.cancel()`, an Esc abort of its root turn, or
 * worker shutdown. Output goes to a spill file with bounded retention (the head up to a cap, plus a rolling tail in
 * memory), and the job list is journaled as the session value `ultron.module/jobs`.
 */
import { randomBytes } from "node:crypto";
import { closeSync, mkdirSync, openSync, readSync, rmSync, writeSync } from "node:fs";
import { join } from "node:path";
import type { JsonValue } from "@ultron/chord";
import type { BashOperations } from "../../core/tools/bash.ts";
import { stripAnsi } from "../../utils/ansi.ts";
import { sanitizeBinaryOutput } from "../../utils/shell.ts";
import { readVersioned } from "../format-version.ts";
import { bashTimeoutSeconds } from "./host-bash.ts";
import type { HostCaller, HostModuleStore, NativeHostApi, NativeHostModule } from "./host-module.ts";

export type ShellJobStatus = "running" | "completed" | "failed" | "cancelled" | "timed_out" | "interrupted";

/** One journaled job (the durable part; the process and waiters live in memory). */
export interface ShellJobRecord {
	id: string;
	lane: string;
	/** Usage root (root turn) the job was started under; its Esc abort cancels the job. */
	rootId: string | null;
	command: string;
	cwd: string;
	timeout: number | null;
	status: ShellJobStatus;
	exitCode: number | null;
	startedAt: number;
	endedAt: number | null;
	outputPath: string;
	/** Bytes the command wrote (sanitized text). */
	outputBytes: number;
	/** Bytes kept in the spill file (the head, up to the retention cap). */
	retainedBytes: number;
	/** Last bytes of output, kept whole for `.text` and completion summaries. */
	tail: string;
	error?: string;
}

/** A job's end, as reported to the completion-event dispatcher. */
export interface ShellJobEnd {
	readonly job: ShellJobRecord;
	/** A cell was waiting on this job when it ended (the model already has the result). */
	readonly awaited: boolean;
	/** Cancelled because its root turn was aborted: no event. */
	readonly rootAborted: boolean;
}

/** Wire form returned to the kernel's ShellJob. */
export type ShellJobSnapshot = {
	id: string;
	lane: string;
	command: string;
	status: ShellJobStatus;
	running: boolean;
	exit_code: number | null;
	ok: boolean;
	timed_out: boolean;
	cancelled: boolean;
	text: string | null;
	truncated: boolean;
	output_path: string;
	output_bytes: number;
	elapsed_seconds: number;
	error: string | null;
};

export const JOB_TEXT_BYTES = 16 * 1024;
export const JOB_RETAINED_BYTES = 4 * 1024 * 1024;
export const JOB_MAX_ACTIVE = 32;
export const JOB_MAX_RETAINED = 64;
/** Longest wait a single `yield_after=` or `job.result(wait=)` call may ask for. */
export const JOB_MAX_WAIT_SECONDS = 3600;
const JOURNAL_VERSION = 1;

type Live = {
	record: ShellJobRecord;
	controller: AbortController;
	done: Promise<void>;
	waiters: number;
	cancelReason?: "cancelled" | "root_aborted" | "closed";
	release?: () => void;
};

export interface ShellJobsOptions {
	cwd: string;
	/** Directory for spill files (one `<job id>.log` per job). */
	dir: string;
	operations: () => BashOperations;
	store: HostModuleStore;
	/** Called once per job that ends while this worker runs. */
	onEnd?: (end: ShellJobEnd) => void;
	/** Keeps the worker alive while a job runs; returns the release. */
	holdActivity?: () => () => void;
	now?: () => number;
	retainedBytes?: number;
	maxRetained?: number;
}

export class ShellJobs {
	readonly #options: ShellJobsOptions;
	readonly #live = new Map<string, Live>();
	#records: ShellJobRecord[] = [];
	#loaded?: Promise<void>;
	#writes: Promise<void> = Promise.resolve();
	#closed = false;

	constructor(options: ShellJobsOptions) {
		this.#options = options;
		mkdirSync(options.dir, { recursive: true, mode: 0o700 });
	}

	get #now(): number {
		return (this.#options.now ?? Date.now)();
	}

	/** Host module for `shell.*` requests; identity comes from the calling lane. */
	readonly module: NativeHostModule = {
		prefixes: ["shell."],
		start: () => this.#load(),
		handle: (request, host) => this.#handle(request.type, request.payload, request.caller, host, request.context),
		close: () => this.close(),
	};

	#load(): Promise<void> {
		this.#loaded ??= (async () => {
			const saved = readVersioned("ultron.module/jobs", await this.#options.store.read());
			const jobs =
				saved && typeof saved === "object" && !Array.isArray(saved) && Array.isArray(saved.jobs)
					? (saved.jobs as unknown as ShellJobRecord[])
					: [];
			// Processes do not outlive the worker that owned them: a job still running in the journal was interrupted.
			this.#records = jobs.map((saved) => {
				// The journal leaves the output tail out (it would be rewritten with every job); read it back.
				const job = { ...saved, tail: saved.tail || this.#fileTail(saved) };
				return job.status === "running"
					? { ...job, status: "interrupted", endedAt: job.endedAt ?? this.#now, error: "worker restarted" }
					: job;
			});
			if (this.#records.some((job, index) => job.status !== jobs[index]?.status)) this.#persist();
		})();
		return this.#loaded;
	}

	#persist(): void {
		// Session values are append-only writes of the whole document: keep each record small.
		const jobs = this.#records.map((record) => ({ ...record, tail: "" }));
		const snapshot = structuredClone({ version: JOURNAL_VERSION, jobs } as unknown as JsonValue);
		this.#writes = this.#writes.then(() => this.#options.store.write(snapshot)).catch(() => {});
	}

	/** Wait for journal writes (tests and shutdown). */
	settled(): Promise<void> {
		return this.#writes;
	}

	async #handle(
		type: string,
		payload: Record<string, unknown>,
		caller: HostCaller,
		host: NativeHostApi,
		context: { abortSignal?: AbortSignal },
	): Promise<unknown> {
		await this.#load();
		const signal = context.abortSignal;
		switch (type) {
			case "shell.run": {
				fields(payload, ["command", "timeout", "yield_after"]);
				const command = payload.command;
				if (typeof command !== "string" || !command.trim())
					throw new Error("bash command must be a non-empty string");
				const timeout = bashTimeoutSeconds(payload.timeout);
				const yieldAfter = waitSeconds(payload.yield_after, "yield_after") ?? 0;
				const live = this.start(caller.lane, host.rootOf?.(caller) ?? null, command, timeout);
				await this.#wait(live, yieldAfter, signal);
				return this.snapshot(live.record);
			}
			case "shell.bash": {
				// Plain `await bash(cmd)`: wait up to `yield_after` like the blocking bash, then leave a slow command
				// running as a job (its end is announced as an event) instead of holding the cell.
				fields(payload, ["command", "timeout", "yield_after"]);
				const command = payload.command;
				if (typeof command !== "string" || !command.trim())
					throw new Error("bash command must be a non-empty string");
				const timeout = bashTimeoutSeconds(payload.timeout);
				const yieldAfter = waitSeconds(payload.yield_after, "yield_after");
				const live = this.start(caller.lane, host.rootOf?.(caller) ?? null, command, timeout);
				await this.#wait(live, yieldAfter, signal);
				if (live.record.status === "running" && signal?.aborted) {
					// The cell was stopped while it waited: stop the command as the blocking bash would, and count the
					// cell as its waiter so no completion is announced.
					live.waiters += 1;
					try {
						await this.cancel(live.record.id, "cancelled");
					} finally {
						live.waiters -= 1;
					}
				}
				return this.bashResult(live.record);
			}
			case "shell.result": {
				fields(payload, ["id", "wait"]);
				const record = this.#visible(payload.id, caller);
				const live = this.#live.get(record.id);
				if (live) await this.#wait(live, waitSeconds(payload.wait, "wait"), signal);
				return this.snapshot(this.#record(record.id));
			}
			case "shell.get": {
				fields(payload, ["id"]);
				return this.snapshot(this.#visible(payload.id, caller));
			}
			case "shell.cancel": {
				fields(payload, ["id"]);
				const record = this.#visible(payload.id, caller);
				await this.cancel(record.id, "cancelled");
				return this.snapshot(this.#record(record.id));
			}
			case "shell.read": {
				fields(payload, ["id", "cursor", "max_bytes"]);
				const record = this.#visible(payload.id, caller);
				return this.read(record, payload.cursor, payload.max_bytes);
			}
			case "shell.list": {
				fields(payload, []);
				return [...this.#records]
					.reverse()
					.filter((record) => caller.lane === "main" || record.lane === caller.lane)
					.map((record) => ({ ...this.snapshot(record), text: null }));
			}
			default:
				throw new Error(`Ultron RLM host request is not wired: ${type}`);
		}
	}

	#record(id: string): ShellJobRecord {
		const record = this.#records.find((candidate) => candidate.id === id);
		if (!record) throw new Error(`Unknown shell job ${id}`);
		return record;
	}

	/** The root lane sees every job; a child lane sees its own. */
	#visible(id: unknown, caller: HostCaller): ShellJobRecord {
		if (typeof id !== "string" || !id) throw new Error("job id must be a nonempty string");
		const record = this.#records.find((candidate) => candidate.id === id);
		if (!record || (caller.lane !== "main" && record.lane !== caller.lane))
			throw new Error(`Unknown shell job ${id}`);
		return record;
	}

	/** Start a job on `lane` under `rootId`. */
	start(lane: string, rootId: string | null, command: string, timeout: number | undefined): Live {
		if (this.#closed) throw new Error("Shell jobs are closed");
		if ([...this.#live.values()].length >= JOB_MAX_ACTIVE)
			throw new Error(`At most ${JOB_MAX_ACTIVE} shell jobs can run at once`);
		const id = `job-${randomBytes(4).toString("hex")}`;
		const outputPath = join(this.#options.dir, `${id}.log`);
		const record: ShellJobRecord = {
			id,
			lane,
			rootId,
			command,
			cwd: this.#options.cwd,
			timeout: timeout ?? null,
			status: "running",
			exitCode: null,
			startedAt: this.#now,
			endedAt: null,
			outputPath,
			outputBytes: 0,
			retainedBytes: 0,
			tail: "",
		};
		this.#records.push(record);
		this.#prune();
		this.#persist();
		const controller = new AbortController();
		const live: Live = { record, controller, done: Promise.resolve(), waiters: 0 };
		live.release = this.#options.holdActivity?.();
		this.#live.set(id, live);
		live.done = this.#run(live);
		return live;
	}

	async #run(live: Live): Promise<void> {
		const { record, controller } = live;
		const cap = this.#options.retainedBytes ?? JOB_RETAINED_BYTES;
		const fd = openSync(record.outputPath, "w", 0o600);
		const decoder = new TextDecoder();
		let timedOut = false;
		const timer =
			record.timeout === null
				? undefined
				: setTimeout(() => {
						timedOut = true;
						controller.abort();
					}, record.timeout * 1000);
		timer?.unref();
		const write = (text: string) => {
			if (!text) return;
			const bytes = Buffer.from(text);
			record.outputBytes += bytes.length;
			const room = cap - record.retainedBytes;
			if (room > 0) {
				const kept = bytes.subarray(0, room);
				writeSync(fd, kept);
				record.retainedBytes += kept.length;
			}
			record.tail = keepTail(record.tail + text, JOB_TEXT_BYTES / 2);
		};
		let exitCode: number | null = null;
		let error: string | undefined;
		try {
			const result = await this.#options.operations().exec(record.command, record.cwd, {
				onData: (data) =>
					write(sanitizeBinaryOutput(stripAnsi(decoder.decode(data, { stream: true }))).replace(/\r/g, "")),
				signal: controller.signal,
			});
			exitCode = result.exitCode;
		} catch (caught) {
			if (!controller.signal.aborted) error = caught instanceof Error ? caught.message : String(caught);
		} finally {
			if (timer) clearTimeout(timer);
			write(decoder.decode());
			closeSync(fd);
		}
		record.endedAt = this.#now;
		record.exitCode = controller.signal.aborted ? null : exitCode;
		record.status = timedOut
			? "timed_out"
			: controller.signal.aborted
				? "cancelled"
				: error !== undefined
					? "failed"
					: "completed";
		if (timedOut) record.error = `timed out after ${record.timeout}s; process tree killed`;
		else if (error !== undefined) record.error = error;
		this.#live.delete(record.id);
		this.#persist();
		live.release?.();
		if (live.cancelReason === "closed") return;
		try {
			this.#options.onEnd?.({
				job: structuredClone(record),
				awaited: live.waiters > 0,
				rootAborted: live.cancelReason === "root_aborted",
			});
		} catch {
			// Observers never affect a job.
		}
	}

	/** Wait for a job for at most `seconds` (undefined: until it ends), or until the calling cell ends. */
	async #wait(live: Live, seconds: number | undefined, signal: AbortSignal | undefined): Promise<void> {
		// A waiter marks the job as awaited: if it ends now, the caller gets the result inline and no event is sent.
		live.waiters += 1;
		// Even `yield_after=0` gives a command that ends at once (echo, a failed spawn) the chance to report inline.
		const ms = seconds === undefined ? undefined : Math.max(20, seconds * 1000);
		let timer: NodeJS.Timeout | undefined;
		let onAbort: (() => void) | undefined;
		const waits: Promise<unknown>[] = [live.done];
		if (ms !== undefined)
			waits.push(
				new Promise((resolve) => {
					timer = setTimeout(resolve, ms);
				}),
			);
		if (signal)
			waits.push(
				new Promise((resolve) => {
					onAbort = () => resolve(undefined);
					if (signal.aborted) onAbort();
					else signal.addEventListener("abort", onAbort, { once: true });
				}),
			);
		try {
			await Promise.race(waits);
		} finally {
			if (timer) clearTimeout(timer);
			if (onAbort) signal?.removeEventListener("abort", onAbort);
			live.waiters -= 1;
		}
	}

	async cancel(id: string, reason: "cancelled" | "root_aborted" | "closed"): Promise<void> {
		const live = this.#live.get(id);
		if (!live) return;
		live.cancelReason ??= reason;
		live.controller.abort();
		await live.done;
	}

	/** Esc aborted a root turn: stop every job started under it. */
	cancelRoot(rootId: string): Promise<void> {
		return Promise.all(
			[...this.#live.values()]
				.filter((live) => live.record.rootId === rootId)
				.map((live) => this.cancel(live.record.id, "root_aborted")),
		).then(() => {});
	}

	/** Running jobs of `lane` (roots in `excluded` do not count). */
	running(lane: string, excluded: (rootId: string | undefined) => boolean = () => false): number {
		return [...this.#live.values()].filter(
			(live) => live.record.lane === lane && !excluded(live.record.rootId ?? undefined),
		).length;
	}

	/** Running and recent jobs for inspection (`agents.status`, `/rlm`), newest first. */
	list(): ShellJobRecord[] {
		return [...this.#records].reverse().map((record) => structuredClone(record));
	}

	snapshot(record: ShellJobRecord): ShellJobSnapshot {
		const running = record.status === "running";
		const { text, truncated } = this.#text(record);
		return {
			id: record.id,
			lane: record.lane,
			command: record.command,
			status: record.status,
			running,
			exit_code: record.exitCode,
			ok: record.status === "completed" && record.exitCode === 0,
			timed_out: record.status === "timed_out",
			cancelled: record.status === "cancelled",
			text,
			truncated,
			output_path: record.outputPath,
			output_bytes: record.outputBytes,
			elapsed_seconds: Math.round(((record.endedAt ?? this.#now) - record.startedAt) / 100) / 10,
			error: record.error ?? null,
		};
	}

	/**
	 * The reply to a plain `bash` that ran as a job: the blocking bash's shape (the kernel makes a BashOutput of
	 * it), plus `running` and the job's snapshot while the command is still running.
	 */
	bashResult(record: ShellJobRecord): JsonValue {
		if (record.status === "failed") throw new Error(record.error ?? "bash command failed");
		const snapshot = this.snapshot(record);
		const running = record.status === "running";
		return {
			output: snapshot.text ?? "",
			exit_code: record.exitCode,
			cancelled: record.status === "cancelled" || record.status === "interrupted",
			timed_out: record.status === "timed_out",
			truncated: snapshot.truncated,
			// The spill file holds the whole output when retention kept all of it; else only its head.
			full_output_path: snapshot.truncated ? record.outputPath : null,
			partial_file: record.retainedBytes < record.outputBytes,
			running,
			job: running ? snapshot : null,
		};
	}

	/** Whole output when it fits, else the first and last halves around a marker naming how to read the rest. */
	#text(record: ShellJobRecord): { text: string; truncated: boolean } {
		const half = JOB_TEXT_BYTES / 2;
		if (record.outputBytes <= JOB_TEXT_BYTES && record.retainedBytes === record.outputBytes)
			return { text: this.#readText(record, 0, JOB_TEXT_BYTES), truncated: false };
		const head = this.#readText(record, 0, half);
		const omitted = Math.max(0, record.outputBytes - half - Buffer.byteLength(record.tail));
		const marker = `\n[... ${omitted} bytes omitted; ${record.retainedBytes} bytes kept in ${record.outputPath}: await job.read(cursor=${half}, max_bytes=65536) ...]\n`;
		return { text: head + marker + record.tail, truncated: true };
	}

	/** The last bytes of a job's spill file, when the file holds all of its output. */
	#fileTail(record: ShellJobRecord): string {
		if (record.retainedBytes !== record.outputBytes || record.retainedBytes === 0) return "";
		const half = JOB_TEXT_BYTES / 2;
		return keepTail(this.#readText(record, Math.max(0, record.retainedBytes - half - 4), half + 4), half);
	}

	#readText(record: ShellJobRecord, cursor: number, maxBytes: number): string {
		try {
			const fd = openSync(record.outputPath, "r");
			try {
				const buffer = Buffer.alloc(maxBytes);
				const read = readSync(fd, buffer, 0, maxBytes, cursor);
				return buffer.subarray(0, read).toString("utf8");
			} finally {
				closeSync(fd);
			}
		} catch {
			return "";
		}
	}

	read(record: ShellJobRecord, cursorValue: unknown, maxValue: unknown): JsonValue {
		const cursor = cursorValue === undefined ? 0 : cursorValue;
		const maxBytes = maxValue === undefined ? 65_536 : maxValue;
		if (typeof cursor !== "number" || !Number.isSafeInteger(cursor) || cursor < 0)
			throw new Error("cursor must be a non-negative integer");
		if (typeof maxBytes !== "number" || !Number.isSafeInteger(maxBytes) || maxBytes < 1 || maxBytes > 1_048_576)
			throw new Error("max_bytes must be an integer between 1 and 1048576");
		const start = Math.min(cursor, record.retainedBytes);
		const text = this.#readText(record, start, Math.min(maxBytes, record.retainedBytes - start));
		const next = start + Buffer.byteLength(text);
		return {
			text,
			next_cursor: next,
			done: record.status !== "running" && next >= record.retainedBytes,
			truncated: record.retainedBytes < record.outputBytes,
		};
	}

	/** Bounded retention: keep the newest finished jobs and their spill files. */
	#prune(): void {
		const max = this.#options.maxRetained ?? JOB_MAX_RETAINED;
		const finished = this.#records.filter((record) => record.status !== "running");
		const excess = this.#records.length - max;
		if (excess <= 0) return;
		const drop = new Set(finished.slice(0, excess).map((record) => record.id));
		for (const record of this.#records) if (drop.has(record.id)) rmSync(record.outputPath, { force: true });
		this.#records = this.#records.filter((record) => !drop.has(record.id));
	}

	async close(): Promise<void> {
		this.#closed = true;
		await Promise.all([...this.#live.keys()].map((id) => this.cancel(id, "closed")));
		await this.#writes;
	}
}

/** One-line completion summary: the last non-empty output lines, bounded. */
export function jobSummary(job: ShellJobRecord, maxChars = 240): string {
	const lines = job.tail
		.split("\n")
		.map((line) => line.trim())
		.filter(Boolean)
		.slice(-3);
	const text = lines.join(" | ");
	const status =
		job.status === "completed" ? `exit ${job.exitCode}` : job.error ? `${job.status}: ${job.error}` : job.status;
	const body = text.length > maxChars ? `…${text.slice(-(maxChars - 1))}` : text;
	return body ? `${status}; ${body}` : status;
}

function keepTail(text: string, maxBytes: number): string {
	if (Buffer.byteLength(text) <= maxBytes) return text;
	const bytes = Buffer.from(text);
	// Cut at a character boundary so the tail never starts inside a multi-byte character.
	let start = bytes.length - maxBytes;
	while (start < bytes.length && (bytes[start]! & 0xc0) === 0x80) start += 1;
	return bytes.subarray(start).toString("utf8");
}

function waitSeconds(value: unknown, name: string): number | undefined {
	if (value === undefined || value === null) return undefined;
	if (typeof value !== "number" || !Number.isFinite(value) || value < 0)
		throw new Error(`${name} must be a non-negative number of seconds`);
	return Math.min(value, JOB_MAX_WAIT_SECONDS);
}

function fields(payload: Record<string, unknown>, allowed: readonly string[]): void {
	for (const key of Object.keys(payload)) if (!allowed.includes(key)) throw new Error(`Unknown payload field: ${key}`);
}
