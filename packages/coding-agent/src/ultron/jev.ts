import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { getAgentDir } from "../config.ts";

export type JevRoute = "fast" | "powerful" | "architecture" | "designer";
export type JevTriage = {
	route: JevRoute;
	routeConfidence: number;
	complexity: number;
	complexityConfidence: number;
	urgency: "low" | "normal" | "high";
	category: "lookup" | "extraction" | "localized_change" | "debugging" | "architecture" | "design" | "other";
};
export type JevMemoryGate = { retrieve: boolean; probability: number };
export type JevMemoryPolicy = { action: "keep" | "skip" | "sensitive"; confidence: number };

type Answer = { noul?: unknown; choice?: unknown; score?: unknown; confidence?: unknown };
type SystemOneResponse = { answers?: Record<string, Answer> };

type JevOptions = {
	apiKey: string;
	baseUrl?: string;
	model?: string;
	fetch?: typeof globalThis.fetch;
	timeoutMs?: number;
};

const maxResponseBytes = 1_048_576;
/** Recall gate cut-off: a probability at or above it retrieves memory. */
export const JEV_RECALL_THRESHOLD = 0.65;
const memoryRecallThreshold = JEV_RECALL_THRESHOLD;

const sensitivePatterns = [
	/\b(?:sk|ghp|github_pat|xox[baprs]|AIza|AKIA)[A-Za-z0-9_-]{8,}\b/i,
	/-----BEGIN [A-Z ]*PRIVATE KEY-----/i,
	/\b(?:api[_ -]?key|access[_ -]?token|auth[_ -]?token|password|passwd|secret|private[_ -]?key)\s*[:=]\s*\S+/i,
	/\bbearer\s+[A-Za-z0-9._~+/=-]{16,}\b/i,
	/\b\d{3}-\d{2}-\d{4}\b/,
];
const ephemeralPrompt =
	/^(?:please\s+)?(?:explain|define|describe|what\s+is|how\s+(?:does|do)|calculate|solve|summarize)\b/i;
const durableIntent =
	/\b(?:remember|preference|prefer|project|decision|commitment|my|our|past|history|before|later|save|keep)\b/i;
const routes = new Set<JevRoute>(["fast", "powerful", "architecture", "designer"]);
const categories = new Set<JevTriage["category"]>([
	"lookup",
	"extraction",
	"localized_change",
	"debugging",
	"architecture",
	"design",
	"other",
]);

function nonempty(value: unknown): value is string {
	return typeof value === "string" && value.trim().length > 0;
}
function probability(value: unknown): number {
	if (typeof value !== "number" || !Number.isFinite(value) || value < 0 || value > 1)
		throw new Error("Invalid Jev probability");
	return value;
}
function errorCode(error: unknown): string {
	return error instanceof Error && error.name === "AbortError" ? "ABORTED" : "UNAVAILABLE";
}
/**
 * Secrets in source code. Narrower than the memory patterns, which would flag ordinary identifiers such as
 * `skill_version`: token prefixes are case-sensitive with their separator, and a credential name counts only
 * when it is assigned a string literal.
 */
const codeSecretPatterns = [
	/\b(?:sk-(?:proj-|live-|test-)?|sk_live_|sk_test_|ghp_|gho_|github_pat_|xox[baprs]-|AIza|AKIA|ASIA)[A-Za-z0-9_-]{12,}/,
	/-----BEGIN [A-Z ]*PRIVATE KEY-----/,
	/\b(?:api[_-]?key|access[_-]?token|auth[_-]?token|password|passwd|secret|private[_-]?key)\s*[:=]\s*["'][^"'\s]{8,}["']/i,
	/\bbearer\s+[A-Za-z0-9._~+/=-]{20,}/i,
	/\b\d{3}-\d{2}-\d{4}\b/,
];

/** Deterministic secret check for code proposals; runs whether or not Jev is configured. */
export function containsCodeSecret(text: string): boolean {
	return codeSecretPatterns.some((pattern) => pattern.test(text));
}
function deterministicPolicy(prompt: string, text: string): JevMemoryPolicy | undefined {
	const content = `${prompt}\n${text}`;
	if (sensitivePatterns.some((pattern) => pattern.test(content))) return { action: "sensitive", confidence: 1 };
	if (ephemeralPrompt.test(prompt.trim()) && !durableIntent.test(prompt)) return { action: "skip", confidence: 1 };
	return undefined;
}

export class NativeJevClient {
	private readonly apiKey: string;
	private readonly baseUrl: string;
	private readonly model: string;
	private readonly fetcher: typeof globalThis.fetch;
	private readonly timeoutMs: number;

	constructor(options: JevOptions) {
		if (!nonempty(options.apiKey)) throw new Error("Jev API key is required");
		let base: URL;
		try {
			base = new URL(options.baseUrl ?? "https://api.typesafe.ai");
		} catch {
			throw new Error("Invalid Jev base URL");
		}
		if (!["http:", "https:"].includes(base.protocol) || base.username || base.password || base.search || base.hash) {
			throw new Error("Invalid Jev base URL");
		}
		if (!Number.isSafeInteger(options.timeoutMs ?? 5000) || (options.timeoutMs ?? 5000) < 1)
			throw new Error("Invalid Jev timeout");
		this.apiKey = options.apiKey;
		this.baseUrl = base.href.replace(/\/$/, "");
		this.model = options.model ?? "jev-latest";
		this.fetcher = options.fetch ?? globalThis.fetch;
		this.timeoutMs = options.timeoutMs ?? 5000;
	}

	private async systemOne(
		state: Record<string, unknown>,
		questions: Record<string, unknown>,
		signal?: AbortSignal,
	): Promise<SystemOneResponse> {
		if (!this.fetcher) throw new Error("Jev fetch is unavailable");
		const controller = new AbortController();
		const timer = setTimeout(() => controller.abort(), this.timeoutMs);
		// Do not forward caller-supplied abort reasons, which may contain secrets.
		const onAbort = () => controller.abort();
		signal?.addEventListener("abort", onAbort, { once: true });
		let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
		let onRequestAbort: (() => void) | undefined;
		try {
			if (signal?.aborted) onAbort();
			controller.signal.throwIfAborted();
			const aborted = new Promise<never>((_resolve, reject) => {
				onRequestAbort = () => reject(controller.signal.reason);
				controller.signal.addEventListener("abort", onRequestAbort, { once: true });
			});
			const request = async (): Promise<SystemOneResponse> => {
				const response = await this.fetcher(`${this.baseUrl}/v1/systemone`, {
					method: "POST",
					redirect: "error",
					headers: {
						authorization: `Bearer ${this.apiKey}`,
						"content-type": "application/json",
						accept: "application/json",
					},
					body: JSON.stringify({ model: this.model, state, questions }),
					signal: controller.signal,
				});
				// An injected fetch may settle after cancellation without honoring its signal.
				if (controller.signal.aborted) {
					void response.body?.cancel().catch(() => {});
					controller.signal.throwIfAborted();
				}
				reader = response.body?.getReader();
				if (!response.ok || !reader) throw new Error("Invalid Jev response");
				if (Number(response.headers.get("content-length")) > maxResponseBytes)
					throw new Error("Jev response is too large");
				// Bound retained bytes even for absent/false Content-Length or many tiny chunks.
				const chunks: Uint8Array[] = [];
				let size = 0;
				for (;;) {
					const { done, value } = await reader.read();
					controller.signal.throwIfAborted();
					if (done) break;
					if (value.byteLength > maxResponseBytes - size) {
						await reader.cancel();
						reader.releaseLock();
						reader = undefined;
						await response.body?.cancel();
						throw new Error("Jev response is too large");
					}
					if (value.byteLength === 0) continue;
					chunks.push(value);
					size += value.byteLength;
				}
				const bytes = new Uint8Array(size);
				let offset = 0;
				for (const chunk of chunks) {
					bytes.set(chunk, offset);
					offset += chunk.byteLength;
				}
				const value: unknown = JSON.parse(new TextDecoder().decode(bytes));
				if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("Invalid Jev response");
				return value as SystemOneResponse;
			};
			// One race covers fetch and the complete body, even if either ignores abort.
			return await Promise.race([request(), aborted]);
		} catch (error) {
			const code = controller.signal.aborted ? "ABORTED" : errorCode(error);
			const wrapped = new Error(`Jev ${code}`);
			wrapped.name = code;
			throw wrapped;
		} finally {
			clearTimeout(timer);
			signal?.removeEventListener("abort", onAbort);
			if (onRequestAbort) controller.signal.removeEventListener("abort", onRequestAbort);
			if (reader) void reader.cancel().catch(() => {});
		}
	}

	async triage(prompt: string, signal?: AbortSignal): Promise<JevTriage> {
		if (!nonempty(prompt)) throw new Error("Jev prompt is empty");
		const response = await this.systemOne(
			{ request: prompt, task: "Classify this coding-agent request before execution." },
			{
				route: {
					type: "choice",
					instructions: "Which model class is sufficient for this request?",
					criteria: {
						fast: "Routine work.",
						powerful: "Difficult reasoning or debugging.",
						architecture: "Architecture or broad refactoring.",
						designer: "Design or frontend work.",
					},
				},
				complexity: {
					type: "score",
					instructions: "How complex is this request?",
					criteria: ["Small", "Moderate", "Broad or difficult"],
				},
				urgency: {
					type: "choice",
					instructions: "How urgent is this request?",
					criteria: { low: "No time pressure.", normal: "Current session.", high: "Immediate incident." },
				},
				category: {
					type: "choice",
					instructions: "What is the primary request category?",
					criteria: {
						lookup: "Lookup.",
						extraction: "Extraction.",
						localized_change: "Small edit.",
						debugging: "Debugging.",
						architecture: "Architecture.",
						design: "Design.",
						other: "Other.",
					},
				},
			},
			signal,
		);
		const answers = response.answers ?? {};
		const route = answers.route?.choice;
		const urgency = answers.urgency?.choice;
		const category = answers.category?.choice;
		if (
			!routes.has(route as JevRoute) ||
			!["low", "normal", "high"].includes(String(urgency)) ||
			!categories.has(category as JevTriage["category"])
		)
			throw new Error("Invalid Jev triage result");
		const complexity = answers.complexity?.score;
		return {
			route: route as JevRoute,
			routeConfidence: probability(answers.route?.confidence),
			complexity:
				typeof complexity === "number" && Number.isFinite(complexity) ? Math.max(0, Math.min(2, complexity)) : 1,
			complexityConfidence: probability(answers.complexity?.confidence),
			urgency: urgency as JevTriage["urgency"],
			category: category as JevTriage["category"],
		};
	}

	async memoryGate(prompt: string, signal?: AbortSignal): Promise<JevMemoryGate> {
		const response = await this.systemOne(
			{ request: prompt, task: "Decide whether stored personal memory is needed to answer this request." },
			{
				retrieve: {
					type: "noul",
					instructions:
						"Would stored facts about the user's projects, preferences, people, or past actions improve this answer?",
				},
			},
			signal,
		);
		const value = probability(response.answers?.retrieve?.noul);
		return { retrieve: value >= memoryRecallThreshold, probability: value };
	}

	async memoryPolicy(prompt: string, text: string, signal?: AbortSignal): Promise<JevMemoryPolicy> {
		const deterministic = deterministicPolicy(prompt, text);
		if (deterministic) return deterministic;
		const response = await this.systemOne(
			{
				user_request: prompt,
				assistant_response: text,
				task: "Decide whether this belongs in durable personal memory.",
			},
			{
				action: {
					type: "choice",
					instructions: "What should happen to this interaction?",
					criteria: {
						keep: "Keep durable facts, preferences, decisions, commitments, or reusable context.",
						skip: "Do not keep ephemeral or task-local details.",
						sensitive: "Do not keep secrets or unusually sensitive data.",
					},
				},
			},
			signal,
		);
		const action = response.answers?.action?.choice;
		if (action !== "keep" && action !== "skip" && action !== "sensitive")
			throw new Error("Invalid Jev memory policy result");
		return { action, confidence: probability(response.answers?.action?.confidence) };
	}

	/**
	 * Relevance and sensitivity of a proposed code skill, judged like a memory retention: keep (a reusable
	 * procedure), skip (a one-off), or sensitive. Secrets in the code are caught before any call.
	 */
	async skillPolicy(name: string, evidence: string, source: string, signal?: AbortSignal): Promise<JevMemoryPolicy> {
		if (containsCodeSecret(`${name}\n${evidence}\n${source}`)) return { action: "sensitive", confidence: 1 };
		const response = await this.systemOne(
			{
				skill_name: name,
				evidence,
				source,
				task: "Decide whether this Python procedure should be kept as a reusable, tested skill for later tasks.",
			},
			{
				action: {
					type: "choice",
					instructions: "What should happen to this proposed skill?",
					criteria: {
						keep: "Keep a reusable procedure likely to recur in later tasks.",
						skip: "A one-off, task-specific script with little reuse.",
						sensitive: "Do not keep: it embeds secrets, credentials, or unusually sensitive data.",
					},
				},
			},
			signal,
		);
		const action = response.answers?.action?.choice;
		if (action !== "keep" && action !== "skip" && action !== "sensitive")
			throw new Error("Invalid Jev memory policy result");
		return { action, confidence: probability(response.answers?.action?.confidence) };
	}

	async memoryRecall(prompt: string, signal?: AbortSignal): Promise<JevMemoryGate> {
		return this.memoryGate(prompt, signal);
	}
}

/**
 * Jev is configured by TYPESAFE_API_KEY or, as with the Pi Jev extension, a `jev-api-key` file in the
 * agent directory. Pass `keyFile: undefined` explicitly to consult only the environment.
 */
export function createNativeJevClient(
	options: { keyFile?: string } = { keyFile: join(getAgentDir(), "jev-api-key") },
): NativeJevClient | undefined {
	let apiKey = process.env.TYPESAFE_API_KEY?.trim();
	if (!apiKey && options.keyFile) {
		try {
			apiKey = readFileSync(options.keyFile, "utf8").trim();
		} catch {
			return undefined;
		}
	}
	if (!apiKey) return undefined;
	return new NativeJevClient({
		apiKey,
		baseUrl: process.env.TYPESAFE_BASE_URL,
		model: process.env.ULTRON_JEV_MODEL ?? process.env.PI_JEV_MODEL,
		timeoutMs: Number(process.env.PI_JEV_TIMEOUT_MS ?? 5000),
	});
}

export function projectIdentity(value: string): string {
	return createHash("sha256").update(value).digest("hex").slice(0, 16);
}
