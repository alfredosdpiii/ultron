/**
 * Completion events instead of polling (Unreal Agent's asynchronous execution). When detached work ends (a shell
 * job started with `yield_after`, a detached extension/MCP tool call, an `rlm.spawn` child, an `agents.spawn` task or a
 * background job), the host appends one short `ultron-runtime-event` custom message to the transcript of the lane
 * that owns the work:
 *
 *   <runtime_event kind="job_done" id="job-1a2b" status="completed" summary="exit 0; 12 passed" fetch="await rlm.job('job-1a2b')" />
 *
 * Delivery:
 * - Completions within `coalesceMs` (500 ms) of the first one become one message.
 * - A lane in the middle of a run gets the message as a steer: the harness places it at the next turn boundary, or
 *   continues the run with it if the run was about to end. A steer that a run did not consume (it ended first) is
 *   taken back and delivered again as below.
 * - An idle root lane is re-invoked: the message starts a run (as Unreal calls the model again), charged to the
 *   root that started the work, so the per-root turn, token, wall and cost limits keep counting across the chain.
 * - Otherwise (a child lane, or a root that must not be re-invoked) the message waits as `nextRun` input and is
 *   seen with the lane's next run. Nothing is ever inserted before earlier messages: events are appended, which
 *   keeps the provider's prompt-cache prefix valid.
 *
 * Loop guards: only root-owned work re-invokes the root; a root whose turn the user aborted (Esc) never restarts
 * work (its jobs are cancelled and their ends are not announced); a spent turn or token budget, an exhausted wall
 * budget or cost cap, or more than `maxRuns` automatic runs in one chain turns re-invocation into quiet delivery.
 * `ULTRON_ASYNC_EVENTS=off` disables events entirely.
 */
import type { AgentHarness, AgentLane, CustomMessage } from "@ultron/agent-core";
import type { Context } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { maskCellOutput } from "./rlm/output-secrets.ts";

export const RUNTIME_EVENT_MESSAGE_TYPE = "ultron-runtime-event";
export const DEFAULT_COALESCE_MS = 500;
export const DEFAULT_MAX_EVENT_RUNS = 16;
const SUMMARY_CHARS = 240;

export type RuntimeEventKind = "job_done" | "tool_done" | "child_done" | "task_done";

export interface RuntimeEvent {
	readonly kind: RuntimeEventKind;
	readonly id: string;
	readonly status: string;
	readonly summary: string;
	/** The kernel call that returns the full result. */
	readonly fetch: string;
	/** Lane that owns the work and receives the event. */
	readonly lane: string;
	/** Usage root the work was started under. */
	readonly rootId?: string;
}

export function asyncEventsEnabled(value: string | undefined): boolean {
	if (value === undefined || value.trim() === "") return true;
	return !["off", "0", "false", "no"].includes(value.trim().toLowerCase());
}

export function maxEventRunsFromEnv(value: string | undefined): number {
	const parsed = Number(value);
	return value !== undefined && value.trim() !== "" && Number.isInteger(parsed) && parsed >= 0
		? parsed
		: DEFAULT_MAX_EVENT_RUNS;
}

function attribute(value: string): string {
	return value
		.replace(/\s+/g, " ")
		.replace(/&/g, "&amp;")
		.replace(/"/g, "&quot;")
		.replace(/</g, "&lt;")
		.replace(/>/g, "&gt;")
		.trim();
}

export function boundedSummary(text: string, max = SUMMARY_CHARS): string {
	const line = text.replace(/\s+/g, " ").trim();
	return line.length > max ? `${line.slice(0, max - 1)}…` : line;
}

/** The text of one event message: one `<runtime_event>` line per completion. */
export function runtimeEventText(events: readonly RuntimeEvent[]): string {
	return events
		.map(
			(event) =>
				`<runtime_event kind="${event.kind}" id="${attribute(event.id)}" status="${attribute(event.status)}" summary="${attribute(boundedSummary(maskCellOutput(event.summary.slice(0, 4 * SUMMARY_CHARS))))}" fetch="${attribute(event.fetch)}" />`,
		)
		.join("\n");
}

export function runtimeEventMessage(events: readonly RuntimeEvent[], now = Date.now()): CustomMessage {
	return {
		role: "custom",
		customType: RUNTIME_EVENT_MESSAGE_TYPE,
		content: runtimeEventText(events),
		display: true,
		details: { events: events.map((event) => ({ kind: event.kind, id: event.id, status: event.status })) },
		timestamp: now,
	};
}

export interface AsyncEventHost {
	/** Usage root of a root-lane run (its own, or the root it continues). */
	rootIdOfRun(runId: string): string;
	/** Charge the root-lane run `runId` to `rootId`. */
	continueRootTurn(runId: string, rootId: string): void;
}

export interface AsyncEventOptions {
	harness: Pick<AgentHarness, "lane" | "events">;
	host: AsyncEventHost;
	enabled?: boolean;
	coalesceMs?: number;
	/** Automatic runs one chain (a root and the runs its events started) may start. */
	maxRuns?: number;
	/** Why the root may not be re-invoked (a spent budget), or undefined when it may. */
	refuse?: (rootId: string) => Promise<string | undefined>;
	/** A root turn was aborted (Esc): stop its jobs. */
	onRootAborted?: (rootId: string) => void | Promise<void>;
	/** Keeps the worker alive while an event waits for delivery; returns the release. */
	holdActivity?: () => () => void;
	/** Delivery decisions, for tests and diagnostics. */
	onDelivery?: (delivery: {
		lane: string;
		mode: "run" | "steer" | "nextRun";
		events: RuntimeEvent[];
		reason?: string;
	}) => void;
}

type Pending = { events: RuntimeEvent[]; timer?: NodeJS.Timeout; release?: () => void };
type Steered = { lane: string; entryId: string; events: RuntimeEvent[] };

export class AsyncEventDispatcher {
	readonly #options: AsyncEventOptions;
	readonly #pending = new Map<string, Pending>();
	readonly #steered = new Map<string, Steered>();
	readonly #abortedRoots = new Set<string>();
	readonly #chainRuns = new Map<string, number>();
	#flushing: Promise<void> = Promise.resolve();
	#inFlight = 0;
	#closed = false;
	readonly #removers: Array<() => void> = [];

	constructor(options: AsyncEventOptions) {
		this.#options = options;
	}

	get enabled(): boolean {
		return this.#options.enabled !== false;
	}

	install(): () => void {
		this.#removers.push(
			this.#options.harness.events.on("run_end", (event) => {
				if (event.lane === "main" && event.status === "aborted") {
					const rootId = this.#options.host.rootIdOfRun(event.runId);
					this.#abortedRoots.add(rootId);
					void Promise.resolve(this.#options.onRootAborted?.(rootId)).catch(() => {});
				}
				this.#afterRun(event.lane, event.status === "aborted");
			}),
		);
		return () => {
			for (const remove of this.#removers.splice(0)) remove();
		};
	}

	/** Events for `lane` waiting to be delivered, or steered into a run that has not consumed them yet. */
	pendingFor(lane: string): number {
		const waiting = this.#pending.get(lane)?.events.length ?? 0;
		const steered = [...this.#steered.values()].filter((item) => item.lane === lane).length;
		return waiting + steered + this.#inFlight;
	}

	/** Whether a root turn was aborted (its work never restarts the model). */
	rootAborted(rootId: string | undefined): boolean {
		return rootId !== undefined && this.#abortedRoots.has(rootId);
	}

	publish(event: RuntimeEvent): void {
		if (!this.enabled || this.#closed) return;
		let pending = this.#pending.get(event.lane);
		if (!pending) {
			pending = { events: [], release: this.#options.holdActivity?.() };
			this.#pending.set(event.lane, pending);
		}
		pending.events.push(event);
		pending.timer ??= setTimeout(() => this.#flush(event.lane), this.#options.coalesceMs ?? DEFAULT_COALESCE_MS);
	}

	#flush(laneName: string): void {
		const pending = this.#pending.get(laneName);
		if (!pending) return;
		this.#pending.delete(laneName);
		if (pending.timer) clearTimeout(pending.timer);
		// Deliveries are serialized so a flush never races another one on the same lane's state.
		this.#inFlight += 1;
		this.#flushing = this.#flushing
			.then(() => this.#deliver(laneName, pending.events))
			.catch(() => {})
			.finally(() => {
				this.#inFlight -= 1;
				pending.release?.();
			});
	}

	/** Deliver now, bypassing the coalescing window (tests, shutdown). */
	async drain(): Promise<void> {
		for (const lane of [...this.#pending.keys()]) this.#flush(lane);
		await this.#flushing;
	}

	async #deliver(laneName: string, events: RuntimeEvent[], context: Context = BACKGROUND_CONTEXT): Promise<void> {
		if (this.#closed || events.length === 0) return;
		const lane = await this.#options.harness.lane(laneName, context);
		const execution = await lane.inspectExecution(context);
		if (execution.current !== null) {
			if (execution.current.kind !== "run") {
				// Compaction or navigation: try again once it settles.
				this.#retryLater(events);
				return;
			}
			await this.#steer(lane, events, context);
			return;
		}
		const reason = await this.#refusal(laneName, events);
		if (reason === undefined) {
			const rootId = chainRoot(events)!;
			const admission = await lane.accept({ kind: "prompt", prompt: runtimeEventMessage(events) }, context);
			if (admission.ok) {
				this.#options.host.continueRootTurn(admission.value.operationId, rootId);
				this.#chainRuns.set(rootId, (this.#chainRuns.get(rootId) ?? 0) + 1);
				this.#options.onDelivery?.({ lane: laneName, mode: "run", events });
				const release = this.#options.holdActivity?.();
				void lane
					.drive({ operationId: admission.value.operationId, waitForRetry: true }, context)
					.catch(() => {})
					.finally(() => release?.());
				return;
			}
			if (admission.error._tag === "LaneBusy") {
				await this.#steer(lane, events, context);
				return;
			}
		}
		const queued = await lane.nextRun(runtimeEventMessage(events), undefined, context);
		if (queued.ok) this.#options.onDelivery?.({ lane: laneName, mode: "nextRun", events, reason });
	}

	/** Why an idle lane's events must not start a run; undefined when they may. */
	async #refusal(laneName: string, events: readonly RuntimeEvent[]): Promise<string | undefined> {
		if (laneName !== "main") return "child lanes are never re-invoked";
		const rootId = chainRoot(events);
		if (rootId === undefined) return "no root turn owns this work";
		if (this.#abortedRoots.has(rootId)) return "the root turn was aborted";
		const runs = this.#chainRuns.get(rootId) ?? 0;
		const max = this.#options.maxRuns ?? DEFAULT_MAX_EVENT_RUNS;
		if (runs >= max) return `${runs} of ${max} automatic runs used for ${rootId}`;
		return this.#options.refuse?.(rootId);
	}

	async #steer(lane: AgentLane, events: RuntimeEvent[], context: Context): Promise<void> {
		const queued = await lane.steer(runtimeEventMessage(events), undefined, context);
		if (!queued.ok) {
			this.#retryLater(events);
			return;
		}
		this.#steered.set(queued.value.entryId, { lane: lane.name, entryId: queued.value.entryId, events });
		this.#options.onDelivery?.({ lane: lane.name, mode: "steer", events });
		// The run may have ended between the check and the steer: then nothing will consume it.
		if ((await lane.inspectExecution(context)).current === null) await this.#reclaim(lane, false, context);
	}

	#retryLater(events: RuntimeEvent[]): void {
		const release = this.#options.holdActivity?.();
		setTimeout(() => {
			release?.();
			for (const event of events) this.publish(event);
		}, 1000).unref();
	}

	#afterRun(laneName: string, aborted: boolean): void {
		if (![...this.#steered.values()].some((item) => item.lane === laneName)) return;
		this.#flushing = this.#flushing
			.then(async () => {
				const lane = await this.#options.harness.lane(laneName, BACKGROUND_CONTEXT);
				await this.#reclaim(lane, aborted, BACKGROUND_CONTEXT);
			})
			.catch(() => {});
	}

	/**
	 * Steered events a finished run did not consume are taken back and delivered again. After an abort, steers are
	 * dropped by the harness with the queue; completions of tasks (not of the jobs the abort cancelled) are kept for
	 * the lane's next run.
	 */
	async #reclaim(lane: AgentLane, aborted: boolean, context: Context): Promise<void> {
		for (const item of [...this.#steered.values()]) {
			if (item.lane !== lane.name) continue;
			const cancelled = await lane.cancelQueued(item.entryId, context);
			if (!cancelled.ok || cancelled.value.kind === "already_consumed") {
				this.#steered.delete(item.entryId);
				continue;
			}
			if (cancelled.value.kind === "cancelled" || (aborted && cancelled.value.kind === "not_found")) {
				this.#steered.delete(item.entryId);
				const kept = item.events.filter(
					(event) => !((event.kind === "job_done" || event.kind === "tool_done") && event.status === "cancelled"),
				);
				if (kept.length > 0) await this.#deliver(lane.name, kept, context);
			}
		}
	}

	async close(): Promise<void> {
		this.#closed = true;
		for (const remove of this.#removers.splice(0)) remove();
		for (const pending of this.#pending.values()) {
			if (pending.timer) clearTimeout(pending.timer);
			pending.release?.();
		}
		this.#pending.clear();
		await this.#flushing;
	}
}

/** The root a batch belongs to: the newest event's (a batch for one lane shares its root in practice). */
function chainRoot(events: readonly RuntimeEvent[]): string | undefined {
	for (let index = events.length - 1; index >= 0; index -= 1) {
		const rootId = events[index]!.rootId;
		if (rootId !== undefined) return rootId;
	}
	return undefined;
}
