/**
 * Completion events (the dispatcher's delivery rules), against a fake lane that follows the harness's queue
 * semantics: a steer is consumed by a running run or left in the inbox once the run has ended, `cancelQueued`
 * reports which, and an abort drops queued steers.
 */
import type { AgentHarness, AgentMessage } from "@ultron/agent-core";
import { describe, expect, test } from "vitest";
import {
	AsyncEventDispatcher,
	type AsyncEventOptions,
	asyncEventsEnabled,
	maxEventRunsFromEnv,
	RUNTIME_EVENT_MESSAGE_TYPE,
	type RuntimeEvent,
	runtimeEventText,
} from "../src/ultron/async-events.ts";

type Queued = { kind: "steer" | "nextRun"; message: AgentMessage };

class FakeLane {
	current: { id: string; kind: "run" } | null = null;
	readonly inbox = new Map<string, Queued>();
	readonly consumed = new Set<string>();
	readonly runs: { operationId: string; message: AgentMessage }[] = [];
	#next = 0;
	readonly name: string;
	readonly harness: FakeHarness;
	constructor(name: string, harness: FakeHarness) {
		this.name = name;
		this.harness = harness;
	}

	async inspectExecution() {
		return { current: this.current };
	}
	async accept(request: { prompt: AgentMessage }) {
		if (this.current) return { ok: false as const, error: { _tag: "LaneBusy" } };
		const operationId = `run-${this.name}-${++this.#next}`;
		this.current = { id: operationId, kind: "run" };
		this.runs.push({ operationId, message: request.prompt });
		return { ok: true as const, value: { operationId, kind: "run", startedAt: 0 } };
	}
	async drive() {
		return { ok: true };
	}
	async steer(message: AgentMessage) {
		const entryId = `e${++this.#next}`;
		this.inbox.set(entryId, { kind: "steer", message });
		return { ok: true as const, value: { entryId } };
	}
	async nextRun(message: AgentMessage) {
		const entryId = `e${++this.#next}`;
		this.inbox.set(entryId, { kind: "nextRun", message });
		return { ok: true as const, value: { entryId } };
	}
	async cancelQueued(entryId: string) {
		if (this.inbox.delete(entryId)) return { ok: true as const, value: { kind: "cancelled" } };
		return { ok: true as const, value: { kind: this.consumed.has(entryId) ? "already_consumed" : "not_found" } };
	}
	/** The running run places every queued steer at its boundary. */
	boundary(): void {
		for (const [id, item] of this.inbox)
			if (item.kind === "steer") {
				this.inbox.delete(id);
				this.consumed.add(id);
			}
	}
	/** End the current run; an abort drops queued steers, as the harness does. */
	end(status: "completed" | "aborted" = "completed"): void {
		const runId = this.current?.id ?? "none";
		this.current = null;
		if (status === "aborted") for (const [id, item] of this.inbox) if (item.kind === "steer") this.inbox.delete(id);
		this.harness.emit({ type: "run_end", lane: this.name, runId, status });
	}
	queued(kind: Queued["kind"]): AgentMessage[] {
		return [...this.inbox.values()].filter((item) => item.kind === kind).map((item) => item.message);
	}
}

class FakeHarness {
	readonly lanes = new Map<string, FakeLane>();
	readonly listeners: Array<(event: Record<string, unknown>) => void> = [];
	lane(name: string): FakeLane {
		let lane = this.lanes.get(name);
		if (!lane) {
			lane = new FakeLane(name, this);
			this.lanes.set(name, lane);
		}
		return lane;
	}
	readonly events = {
		on: (_type: string, listener: (event: Record<string, unknown>) => void) => {
			this.listeners.push(listener);
			return () => {};
		},
	};
	emit(event: Record<string, unknown>): void {
		for (const listener of this.listeners) listener(event);
	}
}

function setup(options: Partial<AsyncEventOptions> = {}) {
	const harness = new FakeHarness();
	const aliases = new Map<string, string>();
	const aborted: string[] = [];
	const dispatcher = new AsyncEventDispatcher({
		harness: harness as unknown as Pick<AgentHarness, "lane" | "events">,
		host: {
			rootIdOfRun: (runId) => aliases.get(runId) ?? `turn:${runId}`,
			continueRootTurn: (runId, rootId) => aliases.set(runId, rootId),
		},
		coalesceMs: 30,
		onRootAborted: (rootId) => {
			aborted.push(rootId);
		},
		...options,
	});
	dispatcher.install();
	return { harness, main: harness.lane("main"), dispatcher, aliases, aborted };
}

const event = (id: string, extra: Partial<RuntimeEvent> = {}): RuntimeEvent => ({
	kind: "job_done",
	id,
	status: "completed",
	summary: `exit 0; ${id} passed`,
	fetch: `await rlm.job("${id}")`,
	lane: "main",
	rootId: "turn:user-1",
	...extra,
});

const text = (message: AgentMessage | undefined) => String((message as { content?: unknown })?.content ?? "");
const settle = (ms = 80) => new Promise((resolve) => setTimeout(resolve, ms));

describe("completion events", () => {
	test("an idle root is re-invoked once per coalesced batch, charged to the root that started the work", async () => {
		const { main, dispatcher, aliases } = setup();
		dispatcher.publish(event("job-a"));
		dispatcher.publish(event("job-b", { kind: "child_done", fetch: 'await rlm.collect(["t1"])' }));
		dispatcher.publish(event("job-c"));
		await settle();
		await dispatcher.drain();
		expect(main.runs).toHaveLength(1);
		const message = main.runs[0]!.message as { role: string; customType: string };
		expect(message).toMatchObject({ role: "custom", customType: RUNTIME_EVENT_MESSAGE_TYPE });
		expect(text(main.runs[0]!.message).split("\n")).toHaveLength(3);
		expect(text(main.runs[0]!.message)).toContain('<runtime_event kind="job_done" id="job-a" status="completed"');
		expect(aliases.get(main.runs[0]!.operationId)).toBe("turn:user-1");
		main.end();
		// A later completion is its own batch and its own run, still charged to the same root.
		dispatcher.publish(event("job-d"));
		await settle();
		await dispatcher.drain();
		expect(main.runs).toHaveLength(2);
		expect(aliases.get(main.runs[1]!.operationId)).toBe("turn:user-1");
	});

	test("a mid-turn event is steered to the next boundary and never starts a second run", async () => {
		const { main, dispatcher } = setup();
		await main.accept({ prompt: { role: "user", content: "work", timestamp: 0 } });
		dispatcher.publish(event("job-a"));
		await settle();
		await dispatcher.drain();
		expect(main.queued("steer")).toHaveLength(1);
		main.boundary();
		main.end();
		await settle(20);
		await dispatcher.drain();
		expect(main.runs).toHaveLength(1);
		expect(main.inbox.size).toBe(0);
	});

	test("a steer that the run did not consume (it ended first) is taken back and re-invokes the idle root", async () => {
		const { main, dispatcher } = setup();
		await main.accept({ prompt: { role: "user", content: "work", timestamp: 0 } });
		dispatcher.publish(event("job-a"));
		await settle();
		await dispatcher.drain();
		main.end();
		await settle(20);
		await dispatcher.drain();
		expect(main.inbox.size).toBe(0);
		expect(main.runs).toHaveLength(2);
		expect(text(main.runs[1]!.message)).toContain('id="job-a"');
	});

	test("the turn budget, cost cap and chain cap turn re-invocation into quiet delivery", async () => {
		const refused = setup({ refuse: async () => "Usage turn limit reached for root turn:user-1" });
		const deliveries: string[] = [];
		refused.dispatcher.publish(event("job-a"));
		await settle();
		await refused.dispatcher.drain();
		expect(refused.main.runs).toHaveLength(0);
		expect(refused.main.queued("nextRun")).toHaveLength(1);

		const capped = setup({
			maxRuns: 1,
			onDelivery: (delivery) => deliveries.push(`${delivery.mode}:${delivery.reason ?? ""}`),
		});
		capped.dispatcher.publish(event("job-a"));
		await settle();
		await capped.dispatcher.drain();
		capped.main.end();
		capped.dispatcher.publish(event("job-b"));
		await settle();
		await capped.dispatcher.drain();
		expect(capped.main.runs).toHaveLength(1);
		expect(capped.main.queued("nextRun")).toHaveLength(1);
		expect(deliveries).toEqual(["run:", "nextRun:1 of 1 automatic runs used for turn:user-1"]);
	});

	test("after an abort, the root never restarts: its jobs stop and later completions wait quietly", async () => {
		const { main, dispatcher, aborted } = setup();
		const admitted = await main.accept({ prompt: { role: "user", content: "work", timestamp: 0 } });
		const runId = admitted.ok ? admitted.value.operationId : "";
		// A child's completion is steered into the run, then the user presses Esc: the harness drops the steer.
		dispatcher.publish(event("t1", { kind: "child_done", rootId: `turn:${runId}` }));
		dispatcher.publish(event("job-x", { status: "cancelled", rootId: `turn:${runId}` }));
		await settle();
		await dispatcher.drain();
		expect(main.queued("steer")).toHaveLength(1);
		main.end("aborted");
		await settle(20);
		await dispatcher.drain();
		expect(aborted).toEqual([`turn:${runId}`]);
		expect(dispatcher.rootAborted(`turn:${runId}`)).toBe(true);
		// No new run; the child's result waits for the next user turn, the cancelled job is not announced.
		expect(main.runs).toHaveLength(1);
		expect(main.queued("nextRun").map(text)).toEqual([expect.stringContaining('id="t1"')]);
		expect(text(main.queued("nextRun")[0])).not.toContain("job-x");
		dispatcher.publish(event("t2", { kind: "task_done", rootId: `turn:${runId}` }));
		await settle();
		await dispatcher.drain();
		expect(main.runs).toHaveLength(1);
		expect(main.queued("nextRun")).toHaveLength(2);
	});

	test("children's events go to their own lane, which is steered when running and never re-invoked", async () => {
		const { harness, main, dispatcher } = setup();
		const child = harness.lane("ultron.rlm-child.t1");
		dispatcher.publish(event("job-c1", { lane: child.name, rootId: "turn:user-1" }));
		await settle();
		await dispatcher.drain();
		expect(child.runs).toHaveLength(0);
		expect(child.queued("nextRun")).toHaveLength(1);
		await child.accept({ prompt: { role: "user", content: "child work", timestamp: 0 } });
		dispatcher.publish(event("job-c2", { lane: child.name }));
		await settle();
		await dispatcher.drain();
		expect(child.queued("steer")).toHaveLength(1);
		expect(main.runs).toHaveLength(0);
		expect(main.inbox.size).toBe(0);
	});

	test("ULTRON_ASYNC_EVENTS=off disables events", async () => {
		const { main, dispatcher } = setup({ enabled: asyncEventsEnabled("off") });
		dispatcher.publish(event("job-a"));
		await settle();
		await dispatcher.drain();
		expect(main.runs).toHaveLength(0);
		expect(main.inbox.size).toBe(0);
		expect(asyncEventsEnabled(undefined)).toBe(true);
		expect(asyncEventsEnabled("on")).toBe(true);
		expect(maxEventRunsFromEnv("3")).toBe(3);
		expect(maxEventRunsFromEnv("x")).toBe(16);
	});

	test("the event text is one bounded, escaped line per completion", () => {
		const line = runtimeEventText([event("job-a", { summary: `a "quoted" <b>\n${"x".repeat(1000)}` })]);
		expect(line).toMatch(
			/^<runtime_event kind="job_done" id="job-a" status="completed" summary="a &quot;quoted&quot; &lt;b&gt; x+…" fetch="await rlm.job\(&quot;job-a&quot;\)" \/>$/,
		);
		expect(line.length).toBeLessThan(400);
	});
});
