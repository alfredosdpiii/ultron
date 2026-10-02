import { mkdirSync, utimesSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

/** 2026-09-01T10:00:00Z: the fixtures' clock starts here. */
export const FIXTURE_START = Date.UTC(2026, 8, 1, 10, 0, 0);

export interface FixtureUsage {
	input?: number;
	output?: number;
	cacheRead?: number;
	cacheWrite?: number;
	/** Provider-reported cost; omit for a response that reported none. */
	cost?: number;
}

export interface FixtureResponse {
	/** Cells the response runs: each is an `rlm` tool call with a result. */
	cells?: Array<{ code: string; output?: string; error?: boolean }>;
	/** Other model tools called (`mcp`, Pi's `bash`). */
	tools?: Array<{ name: string; error?: boolean }>;
	usage?: FixtureUsage;
}

export interface FixtureRun {
	lane: string;
	prompt?: string;
	provider?: string;
	model?: string;
	responses: FixtureResponse[];
	status?: "completed" | "aborted" | "failed";
	/** Wall time of the run in ms (default 60 s). */
	ms?: number;
	/** User messages steered in during the run (`[Ultron] ...` nudges). */
	steers?: string[];
}

/**
 * Writes a native session file (JSONL storage v4) line by line, in the shapes the worker writes: transactions of
 * entries, usage rows and session values. Times advance from {@link FIXTURE_START}.
 */
export class SessionFixture {
	readonly id: string;
	readonly cwd: string;
	readonly #lines: string[];
	#seq = 0;
	#entry = 0;
	#operation = 0;
	#now = FIXTURE_START;
	readonly #tips = new Map<string, string | null>();

	constructor(id: string, cwd: string, options: { createdAt?: number } = {}) {
		this.id = id;
		this.cwd = cwd;
		this.#now = options.createdAt ?? FIXTURE_START;
		this.#lines = [JSON.stringify({ v: 4, kind: "header", id, storageVersion: 1, createdAt: this.#now, cwd })];
	}

	#commit(writes: Array<Record<string, unknown>>): void {
		const stamped = writes.map((write) => ({ ...write, seq: ++this.#seq }));
		this.#lines.push(JSON.stringify(stamped.length === 1 ? stamped[0] : stamped));
	}

	value(namespace: string, key: string, value: unknown): this {
		this.#commit([{ kind: "value", op: "set", namespace, key, value }]);
		return this;
	}

	delete(namespace: string, key: string): this {
		this.#commit([{ kind: "value", op: "delete", namespace, key }]);
		return this;
	}

	/** One transaction of writes exactly as given (each gets its sequence number). */
	raw(...writes: Array<Record<string, unknown>>): this {
		this.#commit(writes);
		return this;
	}

	/** The newest entry of a lane's branch. */
	tip(lane: string): string | null {
		return this.#tips.get(lane) ?? null;
	}

	/** A streamed-frame list write, which a reader must skip. */
	pendingFrame(): this {
		this.#commit([{ kind: "list", op: "append", namespace: "pi.pending.assistant_frame", key: "x:y", value: {} }]);
		return this;
	}

	laneModel(lane: string, provider: string, modelId: string): this {
		return this.value("pi.lane.config", lane, {
			model: { provider, modelId },
			thinkingLevel: "medium",
			activeToolNames: ["rlm"],
		});
	}

	advance(ms: number): this {
		this.#now += ms;
		return this;
	}

	#message(lane: string, message: Record<string, unknown>): { id: string; write: Record<string, unknown> } {
		const id = `entry-${String(++this.#entry).padStart(4, "0")}`;
		const parentId = this.#tips.get(lane) ?? null;
		this.#tips.set(lane, id);
		this.#now += 1000;
		return {
			id,
			write: {
				kind: "entry",
				id,
				parentId,
				type: "message",
				message: { ...message, timestamp: this.#now },
				timestamp: this.#now,
			},
		};
	}

	/** One run of a lane: its prompt, the responses with their cells, the run record and the lane's tip. */
	run(run: FixtureRun): this {
		const operationId = `op-${String(++this.#operation).padStart(4, "0")}`;
		const startedAt = this.#now;
		const fromTipId = this.#tips.get(run.lane) ?? null;
		const provider = run.provider ?? "priced";
		const model = run.model ?? "model-a";
		this.value("pi.op.meta", operationId, {
			operationId,
			lane: run.lane,
			sourceTipId: fromTipId,
			startedAt,
			intent: { kind: "run", promptEntryIds: [] },
		});
		this.#commit([
			this.#message(run.lane, { role: "user", content: [{ type: "text", text: run.prompt ?? "do it" }] }).write,
		]);
		let call = 0;
		for (const response of run.responses) {
			const usage = response.usage ?? {};
			const total = (usage.input ?? 0) + (usage.output ?? 0) + (usage.cacheRead ?? 0) + (usage.cacheWrite ?? 0);
			const reported = {
				input: usage.input ?? 0,
				output: usage.output ?? 0,
				cacheRead: usage.cacheRead ?? 0,
				cacheWrite: usage.cacheWrite ?? 0,
				totalTokens: total,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: usage.cost ?? 0 },
			};
			const calls = [
				...(response.cells ?? []).map((cell) => ({
					id: `call-${operationId}-${++call}`,
					name: "rlm",
					arguments: { code: cell.code } as Record<string, unknown>,
					output: cell.output ?? "ok",
					error: cell.error === true,
				})),
				...(response.tools ?? []).map((tool) => ({
					id: `call-${operationId}-${++call}`,
					name: tool.name,
					arguments: {} as Record<string, unknown>,
					output: "ok",
					error: tool.error === true,
				})),
			];
			const assistant = this.#message(run.lane, {
				role: "assistant",
				content:
					calls.length === 0
						? [{ type: "text", text: "answer" }]
						: calls.map((item) => ({
								type: "toolCall",
								id: item.id,
								name: item.name,
								arguments: item.arguments,
							})),
				api: "test",
				provider,
				model,
				usage: reported,
				stopReason: calls.length === 0 ? "stop" : "toolUse",
			});
			this.#commit([
				assistant.write,
				{ kind: "usage", id: `usage-${assistant.id}`, usage: reported, entryId: assistant.id, adjustment: false },
			]);
			for (const item of calls)
				this.#commit([
					this.#message(run.lane, {
						role: "toolResult",
						toolCallId: item.id,
						toolName: item.name,
						content: [{ type: "text", text: item.output }],
						isError: item.error,
					}).write,
				]);
			for (const steer of run.steers?.splice(0) ?? [])
				this.#commit([this.#message(run.lane, { role: "user", content: [{ type: "text", text: steer }] }).write]);
		}
		this.#now = Math.max(this.#now, startedAt + (run.ms ?? 60_000));
		const tipId = this.#tips.get(run.lane) ?? null;
		this.#commit([
			{
				kind: "value",
				op: "set",
				namespace: "pi.result",
				key: operationId,
				value: {
					operationId,
					kind: "run",
					status: run.status ?? "completed",
					fromTipId,
					tipId,
					startedAt,
					endedAt: this.#now,
				},
			},
			{ kind: "value", op: "set", namespace: "pi.branch.tip", key: run.lane, value: tipId },
			{ kind: "value", op: "delete", namespace: "pi.op.meta", key: operationId },
		]);
		return this;
	}

	tasks(tasks: Array<Record<string, unknown>>): this {
		return this.value("ultron.tasks", "root", {
			version: 1,
			tasks: tasks.map((task, index) => ({
				fingerprint: "0".repeat(64),
				key: `key-${index}`,
				...task,
			})),
		});
	}

	frame(id: string, trace: Record<string, unknown>): this {
		return this.value("ultron.rlm.frames", id, {
			version: 1,
			id,
			status: "complete",
			task: "label this",
			views: [],
			contract: null,
			maxRepairs: 2,
			depth: 1,
			budget: null,
			spent: { calls: 1, tokens: 100 },
			taskId: null,
			lane: null,
			callerTaskId: null,
			parentFrame: null,
			model: null,
			startedAt: this.#now,
			endedAt: this.#now + 1000,
			attempts: [],
			requests: [],
			...trace,
		});
	}

	/** The file's text; `torn` leaves an unfinished last line, as a write in progress does. */
	text(options: { torn?: boolean } = {}): string {
		return `${this.#lines.join("\n")}\n${options.torn ? '{"kind":"value","op":"set","seq":99999,"namesp' : ""}`;
	}

	/** Write the file as the repo names it (`<root>/--<cwd>--/<created>_<id>.jsonl`), last modified at `modifiedAt`. */
	write(sessionsRoot: string, options: { modifiedAt?: number; torn?: boolean } = {}): string {
		const directory = join(sessionsRoot, `--${this.cwd.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-")}--`);
		const path = join(directory, `2026-09-01T10-00-00-000Z_${this.id}.jsonl`);
		mkdirSync(dirname(path), { recursive: true });
		writeFileSync(path, this.text(options));
		const at = new Date(options.modifiedAt ?? this.#now);
		utimesSync(path, at, at);
		return path;
	}
}

/** A stats document (`ultron.module/stats`) with the given fields over zeroed counters. */
export function fixtureStats(fields: Record<string, unknown> = {}): Record<string, unknown> {
	return {
		version: 1,
		root: "lane",
		since: FIXTURE_START,
		updatedAt: FIXTURE_START,
		cells: {
			root: { count: 0, failed: 0, apis: {} },
			subagents: { count: 0, failed: 0, apis: {} },
			other: { count: 0, failed: 0, apis: {} },
		},
		hostCalls: {},
		secretsMasked: 0,
		guards: {},
		nudges: { toolRounds: 0, wait: 0, skill: 0 },
		usageLimitBlocks: 0,
		merges: {},
		childModels: {},
		externalTurns: { count: 0, wallMs: 0 },
		...fields,
	};
}
