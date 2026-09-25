import type { AgentHarness, AgentMessage, CustomMessage } from "@earendil-works/pi-agent-core";
import type { MemoryPrepared, MemoryScope, NativeMemoryService } from "./memory.ts";

/**
 * Automatic per-turn memory for the root agent, replacing the Pi Jev extension's hooks.
 *
 * Before each root run the user's request goes through Jev's recall gate (the automatic, non-explicit
 * path: a low score means no Hindsight call at all); recalled evidence is injected as an untrusted
 * `ultron-memory` custom message, which is kept in the transcript. After a completed root run, Jev's
 * retention policy judges the request and the answer; only a confident keep is stored. Every gate
 * decision lands in the Jev decision log and the memory journal (`memory.why("auto:<runId>")`).
 * Memory failures never block or fail a turn.
 */
export type AutoMemoryMode = "off" | "recall" | "on";

export const AUTO_MEMORY_MESSAGE_TYPE = "ultron-memory";
const MAX_EXCHANGE_CHARS = 6000;
const DEFAULT_RECALL_TIMEOUT_MS = 20_000;

/** ULTRON_AUTO_MEMORY: `on` (default) recalls and retains, `recall` only recalls, `off` does neither. */
export function autoMemoryModeFromEnv(value: string | undefined): AutoMemoryMode {
	const normalized = value?.trim().toLowerCase() ?? "";
	if (["off", "0", "false", "no", "none"].includes(normalized)) return "off";
	if (normalized === "recall") return "recall";
	return "on";
}

/** ULTRON_AUTO_MEMORY_SCOPE: `project` (default), `session`, or `global`. */
export function autoMemoryScopeFromEnv(value: string | undefined): MemoryScope {
	const normalized = value?.trim().toLowerCase();
	return normalized === "session" || normalized === "global" ? normalized : "project";
}

type RunStatus = "completed" | "aborted" | "failed";

export type AutoMemoryOptions = {
	mode: AutoMemoryMode;
	scope?: MemoryScope;
	memory: Pick<NativeMemoryService, "prepare" | "propose">;
	sessionId: string;
	/** Lane whose runs get automatic memory; child task lanes never do. */
	lane?: string;
	recallTimeoutMs?: number;
	/** Keeps the worker alive while a retention is in flight after the run ended. */
	holdActivity?: () => () => void;
	/** Called with every swallowed failure (never rethrown). */
	onError?: (phase: "recall" | "retain", error: unknown) => void;
};

function textOf(content: unknown): string {
	if (typeof content === "string") return content;
	if (!Array.isArray(content)) return "";
	return content
		.filter(
			(part): part is { type: "text"; text: string } =>
				typeof part === "object" && part !== null && part.type === "text" && typeof part.text === "string",
		)
		.map((part) => part.text)
		.join("\n");
}

export function promptText(prompt: readonly AgentMessage[]): string {
	return prompt
		.filter((message) => message.role === "user")
		.map((message) => textOf((message as { content?: unknown }).content))
		.filter((text) => text.trim())
		.join("\n")
		.trim();
}

export class AutoMemory {
	readonly #options: AutoMemoryOptions;
	readonly #lane: string;
	readonly #scope: MemoryScope;
	/** Per root run: the request and the latest assistant answer text. */
	readonly #runs = new Map<string, { prompt: string; answer: string }>();
	readonly #pending = new Set<Promise<void>>();

	constructor(options: AutoMemoryOptions) {
		this.#options = options;
		this.#lane = options.lane ?? "main";
		this.#scope = options.scope ?? "project";
	}

	/** Register the hooks. Returns a function that removes them. */
	install(harness: Pick<AgentHarness, "hooks" | "events">): () => void {
		if (this.#options.mode === "off") return () => {};
		const removers = [
			harness.hooks.on("before_run", async (event) => {
				if (event.lane !== this.#lane) return undefined;
				const message = await this.beforeRun(event.runId, event.prompt);
				return message ? { messages: [message] } : undefined;
			}),
			harness.events.on("turn_end", (event) => {
				if (event.lane === this.#lane) this.turnEnded(event.runId, textOf(event.message.content));
			}),
			harness.events.on("run_end", (event) => {
				if (event.lane === this.#lane) this.runEnded(event.runId, event.status);
			}),
		];
		return () => {
			for (const remove of removers) remove();
		};
	}

	/** Recall for a root run. Resolves to the message to inject, or undefined; never rejects. */
	async beforeRun(runId: string, prompt: readonly AgentMessage[]): Promise<CustomMessage | undefined> {
		const text = promptText(prompt);
		if (!text) return undefined;
		this.#runs.set(runId, { prompt: text, answer: "" });
		const taskId = `auto:${runId}`;
		let prepared: MemoryPrepared;
		try {
			prepared = await this.#options.memory.prepare(
				{ query: text.slice(0, MAX_EXCHANGE_CHARS), scope: this.#scope, taskId },
				AbortSignal.timeout(this.#options.recallTimeoutMs ?? DEFAULT_RECALL_TIMEOUT_MS),
			);
		} catch (error) {
			this.#options.onError?.("recall", error);
			return undefined;
		}
		if (!prepared.context) return undefined;
		return {
			role: "custom",
			customType: AUTO_MEMORY_MESSAGE_TYPE,
			content: prepared.context,
			display: true,
			details: { taskId, operationId: prepared.operation.id, scope: this.#scope, count: prepared.results.length },
			timestamp: Date.now(),
		};
	}

	turnEnded(runId: string, answer: string): void {
		const run = this.#runs.get(runId);
		if (run && answer.trim()) run.answer = answer;
	}

	/** Start retention for a completed root run. The returned promise never rejects. */
	runEnded(runId: string, status: RunStatus): Promise<void> {
		const run = this.#runs.get(runId);
		this.#runs.delete(runId);
		if (!run || status !== "completed" || this.#options.mode !== "on" || !run.answer.trim()) return Promise.resolve();
		const release = this.#options.holdActivity?.() ?? (() => {});
		const prompt = run.prompt.slice(0, MAX_EXCHANGE_CHARS);
		const response = run.answer.slice(0, MAX_EXCHANGE_CHARS);
		const task = this.#options.memory
			.propose({
				text: `[User]\n${prompt}\n\n[Assistant]\n${response}`,
				evidence: [{ ref: `session:${this.#options.sessionId}#run:${runId}` }],
				scope: this.#scope,
				source: { prompt, response },
			})
			.then(
				() => {},
				(error: unknown) => this.#options.onError?.("retain", error),
			)
			.finally(() => {
				release();
				this.#pending.delete(task);
			});
		this.#pending.add(task);
		return task;
	}

	/** Wait for retentions still in flight (for shutdown and tests). */
	async settle(): Promise<void> {
		await Promise.all([...this.#pending]);
	}
}
