import { createHash, randomUUID } from "node:crypto";
import { isJsonValue, type JsonValue } from "@ultron/chord";
import { readVersioned } from "./format-version.ts";
import type { JevMemoryGate, JevMemoryPolicy, JevTriage, NativeJevClient } from "./jev.ts";

/**
 * Bounded, durable log of Jev decisions for the read-only `jev.decisions` inspector.
 *
 * Records never hold prompts or responses: only a short SHA-256 prefix and the length of the input,
 * the decision fields Jev returned, and an error code. The newest `capacity` records are kept.
 */
export type JevDecisionKind = "triage" | "recall" | "retain";

export type JevDecision = {
	id: string;
	at: number;
	kind: JevDecisionKind;
	status: "ok" | "error" | "unavailable";
	durationMs: number;
	/** First 12 hex chars of SHA-256 of the input; never the input itself. */
	inputSha256: string;
	inputChars: number;
	route?: JevTriage["route"];
	routeConfidence?: number;
	complexity?: number;
	urgency?: JevTriage["urgency"];
	category?: JevTriage["category"];
	retrieve?: boolean;
	probability?: number;
	action?: JevMemoryPolicy["action"];
	confidence?: number;
	/** Error code (`Jev UNAVAILABLE`, `Jev ABORTED`, …) or the reason Jev was unavailable. */
	reason?: string;
};

export interface JevDecisionStore {
	read(): Promise<JsonValue | undefined>;
	write(document: JsonValue): Promise<void>;
}

export type JevAvailability = {
	jev: boolean;
	hindsight: boolean;
};

export const JEV_DECISION_CAPACITY = 200;

function digest(input: string): string {
	return createHash("sha256").update(input).digest("hex").slice(0, 12);
}

function reasonOf(error: unknown): string {
	// Jev client errors are already code-only ("Jev UNAVAILABLE"); anything else is reduced to its name.
	if (error instanceof Error && /^Jev [A-Z_]+$/.test(error.message)) return error.message;
	if (error instanceof Error && error.message === "Invalid Jev triage result") return error.message;
	if (error instanceof Error && error.message === "Invalid Jev memory policy result") return error.message;
	if (error instanceof Error && error.message === "Invalid Jev probability") return error.message;
	return error instanceof Error ? error.name : "Error";
}

export class JevDecisionLog {
	readonly #store: JevDecisionStore;
	readonly #capacity: number;
	readonly #now: () => number;
	#decisions: JevDecision[] | undefined;
	#tail: Promise<void> = Promise.resolve();

	constructor(store: JevDecisionStore, options: { capacity?: number; now?: () => number } = {}) {
		this.#store = store;
		this.#capacity = Math.max(1, options.capacity ?? JEV_DECISION_CAPACITY);
		this.#now = options.now ?? Date.now;
	}

	now(): number {
		return this.#now();
	}

	async #load(): Promise<JevDecision[]> {
		if (this.#decisions !== undefined) return this.#decisions;
		// An unreadable store starts an empty log, but a document in a newer format fails explicitly: it must never be
		// replaced by this build's shorter log.
		const read = await this.#store.read().catch(() => undefined);
		const stored = isJsonValue(read) ? readVersioned("ultron.jev.decisions/root", read) : read;
		const list =
			stored !== null &&
			typeof stored === "object" &&
			!Array.isArray(stored) &&
			Array.isArray((stored as { decisions?: unknown }).decisions)
				? ((stored as { decisions: unknown[] }).decisions.filter(
						(item) => item !== null && typeof item === "object" && typeof (item as JevDecision).kind === "string",
					) as JevDecision[])
				: [];
		this.#decisions ??= list.slice(-this.#capacity);
		return this.#decisions;
	}

	/** Append a decision. Persistence failures never fail the Jev call being recorded. */
	record(input: string, decision: Omit<JevDecision, "id" | "inputSha256" | "inputChars">): Promise<void> {
		const entry: JevDecision = {
			id: `jev-${randomUUID()}`,
			inputSha256: digest(input),
			inputChars: input.length,
			...decision,
		};
		const operation = this.#tail.then(async () => {
			const decisions = await this.#load();
			decisions.push(entry);
			if (decisions.length > this.#capacity) decisions.splice(0, decisions.length - this.#capacity);
			const document = { version: 1, decisions } as unknown;
			if (isJsonValue(document)) await this.#store.write(structuredClone(document));
		});
		this.#tail = operation.catch(() => {});
		return this.#tail;
	}

	async list(): Promise<JevDecision[]> {
		await this.#tail;
		return structuredClone(await this.#load());
	}
}

type RecordableJev = Pick<NativeJevClient, "triage" | "memoryGate" | "memoryPolicy" | "memoryRecall">;

/** Wrap a Jev client so every triage, recall gate, and retention policy call lands in `log`. */
export function recordingJevClient(client: RecordableJev, log: JevDecisionLog): RecordableJev {
	const timed = async <T>(
		kind: JevDecisionKind,
		input: string,
		call: () => Promise<T>,
		fields: (value: T) => Partial<JevDecision>,
	): Promise<T> => {
		const started = log.now();
		try {
			const value = await call();
			void log.record(input, {
				at: started,
				kind,
				status: "ok",
				durationMs: log.now() - started,
				...fields(value),
			});
			return value;
		} catch (error) {
			void log.record(input, {
				at: started,
				kind,
				status: "error",
				durationMs: log.now() - started,
				reason: reasonOf(error),
			});
			throw error;
		}
	};
	const triageFields = (value: JevTriage): Partial<JevDecision> => ({
		route: value.route,
		routeConfidence: value.routeConfidence,
		complexity: value.complexity,
		urgency: value.urgency,
		category: value.category,
	});
	const gateFields = (value: JevMemoryGate): Partial<JevDecision> => ({
		retrieve: value.retrieve,
		probability: value.probability,
	});
	return {
		triage: (prompt, signal) => timed("triage", prompt, () => client.triage(prompt, signal), triageFields),
		memoryGate: (prompt, signal) => timed("recall", prompt, () => client.memoryGate(prompt, signal), gateFields),
		memoryRecall: (prompt, signal) => timed("recall", prompt, () => client.memoryRecall(prompt, signal), gateFields),
		memoryPolicy: (prompt, text, signal) =>
			timed(
				"retain",
				`${prompt}\n${text}`,
				() => client.memoryPolicy(prompt, text, signal),
				(value: JevMemoryPolicy) => ({ action: value.action, confidence: value.confidence }),
			),
	};
}
