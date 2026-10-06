import { type Session, value } from "@ultron/agent-core";
import type { Context, JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { readVersioned } from "./format-version.ts";

export type NativeUsageKind = "task" | "model" | "jev";
export type NativeUsageCallStatus = "succeeded" | "failed" | "cancelled" | "unknown";

export type NativeUsageLimits = {
	/** Unfinished tasks admitted at once under one root. */
	maxAdmittedTasks?: number;
	/** Wall budget of one root, counted from its first reservation. */
	maxWallMs?: number;
	/**
	 * Optional spend cap per root, from provider-reported model cost of every response in the root's tree (its own
	 * lane, sub-agents, frames, typed agents and background jobs). Unset means no cap.
	 */
	maxCostUsd?: number;
	/**
	 * nano-rlm's `max_total_tokens`: provider-reported tokens of every model turn of one root, its descendants
	 * included. Unset means no limit.
	 */
	maxTotalTokens?: number;
	/** nano-rlm's `max_total_turns`: model turns (responses) of one root, its descendants included. Unset means no limit. */
	maxTotalTurns?: number;
	/**
	 * The token cap a root gets when `maxTotalTokens` is not set: the terminator for a runaway loop (verifiers' RLM
	 * harness defaults to 10M too). Goal roots (`goal:`) are exempt from it; an explicit `maxTotalTokens` applies to
	 * every root. Unset means none.
	 */
	defaultMaxTotalTokens?: number;
};

/** Which limit a spent root reached: a turn or token cap still allows one final answer, a cost cap does not. */
export type NativeUsageExhaustion = { reason: string; kind: "turns" | "tokens" | "cost" };

/** What a root has left under its turn and token caps (null: no cap), for the budget line on cell results. */
export type NativeUsageRemaining = { turnsLeft: number | null; tokensLeft: number | null; tokenCap: number | null };

/** The per-root policy knobs as settings (`rootBudget` in settings.json); the environment overrides them. */
export type NativeRootBudgetSettings = { maxTotalTokens?: number; maxTotalTurns?: number };

/** Worker defaults: 24 unfinished tasks and a 30-minute wall budget per root turn; no cost cap. */
export const DEFAULT_NATIVE_USAGE_LIMITS = { maxAdmittedTasks: 24, maxWallMs: 30 * 60 * 1000 } as const;

/**
 * The budget line on a cell result (Autolith shows calls and tokens remaining in each result instead of steering):
 * turns left when a turn cap is set; tokens left when the token cap was set explicitly, or once half of the default
 * cap is used. Undefined when there is nothing to show.
 */
export function budgetLine(remaining: NativeUsageRemaining, explicitTokenCap: boolean): string | undefined {
	const parts: string[] = [];
	if (remaining.turnsLeft !== null) parts.push(`${remaining.turnsLeft} model turns`);
	if (
		remaining.tokensLeft !== null &&
		remaining.tokenCap !== null &&
		(explicitTokenCap || remaining.tokensLeft <= remaining.tokenCap / 2)
	)
		parts.push(`${remaining.tokensLeft.toLocaleString("en-US")} tokens`);
	return parts.length === 0 ? undefined : `[budget left for this request: ${parts.join(", ")}]`;
}

/** Tokens a root turn's tree may use when no token cap is set (ULTRON_MAX_TOTAL_TOKENS=off removes it). */
export const DEFAULT_MAX_TOTAL_TOKENS = 10_000_000;

/**
 * Usage roots of session goals (`/goal`, goal.ts). A goal runs until it is done, blocked, stuck or stopped by the
 * user, so its root has no wall deadline and no admitted-task limit; the turn, token and cost caps still apply.
 */
export const GOAL_USAGE_ROOT_PREFIX = "goal:";

/**
 * Limits from `ULTRON_MAX_WALL_MS`, `ULTRON_MAX_ADMITTED_TASKS` and `ULTRON_MAX_COST_USD`. A missing or
 * invalid value keeps the default; `none`, `off` or `unlimited` removes the wall or admission limit.
 * The cost cap is off unless set to a nonnegative number (an optional leading `$` is accepted).
 * `ULTRON_MAX_TOTAL_TOKENS` and `ULTRON_MAX_TOTAL_TURNS` (positive integers) override the `rootBudget` settings;
 * `none`, `off` or `unlimited` removes a limit the settings set. Turns are uncapped by default; tokens get
 * DEFAULT_MAX_TOTAL_TOKENS per root turn (goal roots exempt) unless set or turned off.
 */
export function nativeUsageLimitsFromEnv(
	env: Record<string, string | undefined> = process.env,
	settings: NativeRootBudgetSettings = {},
): NativeUsageLimits {
	const unlimited = (raw: string) => ["none", "off", "unlimited"].includes(raw.toLowerCase());
	const integer = (raw: string | undefined, fallback: number, minimum: number): number | undefined => {
		const text = raw?.trim();
		if (!text) return fallback;
		if (unlimited(text)) return undefined;
		const parsed = Number(text);
		return Number.isSafeInteger(parsed) && parsed >= minimum ? parsed : fallback;
	};
	const costText = env.ULTRON_MAX_COST_USD?.trim().replace(/^\$/, "");
	const cost = costText ? Number(costText) : Number.NaN;
	const limits: NativeUsageLimits = {};
	const maxAdmittedTasks = integer(env.ULTRON_MAX_ADMITTED_TASKS, DEFAULT_NATIVE_USAGE_LIMITS.maxAdmittedTasks, 0);
	const maxWallMs = integer(env.ULTRON_MAX_WALL_MS, DEFAULT_NATIVE_USAGE_LIMITS.maxWallMs, 1);
	if (maxAdmittedTasks !== undefined) limits.maxAdmittedTasks = maxAdmittedTasks;
	if (maxWallMs !== undefined) limits.maxWallMs = maxWallMs;
	if (Number.isFinite(cost) && cost >= 0) limits.maxCostUsd = cost;
	const setting = (candidate: number | undefined): number | undefined =>
		Number.isSafeInteger(candidate) && (candidate as number) >= 1 ? candidate : undefined;
	const maxTotalTokens = integer(env.ULTRON_MAX_TOTAL_TOKENS, setting(settings.maxTotalTokens) ?? 0, 1);
	const maxTotalTurns = integer(env.ULTRON_MAX_TOTAL_TURNS, setting(settings.maxTotalTurns) ?? 0, 1);
	if (maxTotalTokens) limits.maxTotalTokens = maxTotalTokens;
	// Without a token cap a root gets the default one, unless ULTRON_MAX_TOTAL_TOKENS turns limits off.
	else if (maxTotalTokens === 0) limits.defaultMaxTotalTokens = DEFAULT_MAX_TOTAL_TOKENS;
	if (maxTotalTurns) limits.maxTotalTurns = maxTotalTurns;
	return limits;
}

export type NativeUsageStore = {
	read(): Promise<JsonValue | undefined>;
	write(document: JsonValue): Promise<void>;
};

export type NativeUsageMeasurement = {
	inputTokens?: number | null;
	outputTokens?: number | null;
	totalTokens?: number | null;
	cost?: number | null;
	wallMs?: number;
};

export type NativeUsageReservationRequest = {
	kind: NativeUsageKind;
	rootId?: string;
	taskId?: string;
	parentTaskId?: string;
	requestKey?: string;
	/** Inference budget subtree (`rlm.infer`/`rlm.map`) this call is charged to, for tree accounting. */
	budgetId?: string;
	timeoutMs?: number;
	deadlineAt?: number;
	/** Whether admitted work calls a model, so the cost cap applies. Model reservations always do; tasks default to true. */
	modelBacked?: boolean;
	signal?: AbortSignal;
};

export type NativeUsageReservation = {
	id: string;
	rootId: string;
	kind: NativeUsageKind;
	taskId?: string;
	parentTaskId?: string;
	requestKey?: string;
	budgetId?: string;
	admittedAt: number;
	deadlineAt: number | null;
};

export type NativeUsageSettlement = {
	status?: NativeUsageCallStatus;
	usage?: NativeUsageMeasurement;
};

export type NativeUsageTotals = {
	calls: number;
	taskCalls: number;
	modelCalls: number;
	jevCalls: number;
	wallMs: number;
	inputTokens: number | null;
	outputTokens: number | null;
	totalTokens: number | null;
	cost: number | null;
	unknownCalls: number;
};

export type NativeUsageStatus = {
	rootId: string;
	limits: {
		maxAdmittedTasks: number | null;
		maxWallMs: number | null;
		maxCostUsd: number | null;
		maxTotalTokens: number | null;
		maxTotalTurns: number | null;
	};
	/** Model responses and tokens of this root's whole tree (every lane it admitted). */
	turns: NativeUsageTurns;
	/** Settled model spend of this root against the optional cap. */
	cost: NativeUsageCost;
	admittedTasks: number;
	activeReservations: number;
	startedAt: number | null;
	deadlineAt: number | null;
	remainingWallMs: number | null;
	usage: NativeUsageTotals;
	reservations: NativeUsageReservation[];
	/** The whole session: every detailed root plus the folded history of older roots. */
	session: NativeUsageSessionTotals;
};

export type NativeUsageSessionTotals = {
	/** Roots still kept call by call. */
	roots: number;
	/** Older roots folded into the bounded history summary. */
	foldedRoots: number;
	usage: NativeUsageTotals;
	/** Provider-reported model spend across the session, folded history included. */
	spentUsd: number;
	unknownPricedCalls: number;
};

/**
 * Bounded summary of roots that were folded out of the detailed document. Sums are exact over the folded calls;
 * a null token or cost field means at least one folded call did not report it, as for live totals.
 */
export type NativeUsageHistory = {
	roots: number;
	firstStartedAt: number | null;
	lastSettledAt: number | null;
	usage: NativeUsageTotals;
	byKind: Record<NativeUsageKind, NativeUsageTotals>;
	spentUsd: number;
	unknownPricedCalls: number;
	/** Folded task admissions by settlement status, so reconciliation can still account for their journal tasks. */
	taskAdmissions: Record<NativeUsageCallStatus, number>;
	/** Folded roots that came from an import (see `StoredRoot.imported`). */
	importedRoots: number;
};

export type NativeUsageTurns = {
	turns: number;
	tokens: number;
	maxTotalTurns: number | null;
	maxTotalTokens: number | null;
};

export type NativeUsageCost = {
	maxCostUsd: number | null;
	/** Sum of provider-reported cost over settled model calls whose cost is known. */
	spentUsd: number;
	/** Settled model calls without a reported cost; with a cap set, any of these stops new model-backed work. */
	unknownPricedCalls: number;
	remainingUsd: number | null;
};

export type NativeUsageLedgerLike = {
	ready?(): Promise<void>;
	reserve(request: NativeUsageReservationRequest): Promise<NativeUsageReservation>;
	settle(reservation: NativeUsageReservation, settlement?: NativeUsageSettlement): Promise<void>;
	status(rootId?: string): Promise<NativeUsageStatus>;
	reconcile?(activeTaskIds: readonly string[]): Promise<void>;
	/**
	 * Counts one model response of `rootId` (the default root when undefined), on any lane of its tree, against the
	 * turn, token and cost limits.
	 */
	recordTurn?(rootId: string | undefined, measurement?: NativeUsageTurnMeasurement): Promise<void>;
	/** Why `rootId` may start no further model work under the turn, token and cost limits, or undefined. */
	turnBudgetExhausted?(rootId: string | undefined): Promise<string | undefined>;
};

/** Provider-reported usage of one model response; a missing cost counts the response as unpriced. */
export type NativeUsageTurnMeasurement = { totalTokens?: number | null; cost?: number | null };

type StoredMeasurement = {
	inputTokens: number | null;
	outputTokens: number | null;
	totalTokens: number | null;
	cost: number | null;
	wallMs: number;
};

type StoredReservation = NativeUsageReservation;

type StoredCall = {
	id: string;
	reservationId: string;
	rootId: string;
	kind: NativeUsageKind;
	/** Copied from the reservation so a settled call stays attributable to its task. */
	taskId?: string;
	parentTaskId?: string;
	requestKey?: string;
	budgetId?: string;
	admittedAt: number;
	settledAt: number;
	status: NativeUsageCallStatus;
	usage: StoredMeasurement;
};

type StoredRoot = {
	rootId: string;
	startedAt: number | null;
	deadlineAt: number | null;
	reservations: StoredReservation[];
	calls: StoredCall[];
	/** Set on a historical root written by an import, e.g. Pi usage entries; never admits new work. */
	imported?: { source: string; entries: number };
	/**
	 * Model responses of the root and its whole tree (every lane it admitted): the tree's spend. `cost` and
	 * `unpriced` are absent in documents written before responses carried cost; such a root's spend falls back to
	 * its settled model calls.
	 */
	turns?: { count: number; tokens: number; lastAt: number; cost?: number; unpriced?: number };
};

type StoredDocument = {
	version: 1;
	roots: Record<string, StoredRoot>;
	/** Folded older roots; absent until the first fold. */
	history?: NativeUsageHistory;
};

/** Roots kept call by call; older idle roots are folded into `history`. */
export const DEFAULT_USAGE_DETAILED_ROOTS = 50;

const DEFAULT_ROOT_ID = "ultron-root";
const USAGE_DOCUMENT_VERSION = 1;
const USAGE_DURABILITY_ERROR = "Usage ledger durability is uncertain; reopen the owner";

function memoryStore(): NativeUsageStore {
	let document: JsonValue | undefined;
	return {
		read: async () => document,
		write: async (next) => {
			document = structuredClone(next);
		},
	};
}

function object(valueToCheck: unknown): Record<string, unknown> {
	if (valueToCheck === null || typeof valueToCheck !== "object" || Array.isArray(valueToCheck)) {
		throw new Error("Invalid usage ledger document");
	}
	return valueToCheck as Record<string, unknown>;
}

function finiteInteger(valueToCheck: unknown, name: string, minimum = 0): number {
	if (!Number.isSafeInteger(valueToCheck) || (valueToCheck as number) < minimum) {
		throw new Error(`Invalid usage ledger ${name}`);
	}
	return valueToCheck as number;
}

function finiteNumber(valueToCheck: unknown, name: string): number {
	if (typeof valueToCheck !== "number" || !Number.isFinite(valueToCheck)) {
		throw new Error(`Invalid usage ledger ${name}`);
	}
	return valueToCheck;
}

function optionalFiniteNumber(valueToCheck: unknown, name: string): number | null {
	if (valueToCheck === null) return null;
	return finiteNumber(valueToCheck, name);
}

function validateMeasurement(valueToCheck: unknown): asserts valueToCheck is StoredMeasurement {
	const item = object(valueToCheck);
	if (!Object.hasOwn(item, "inputTokens")) throw new Error("Invalid usage ledger measurement");
	if (!Object.hasOwn(item, "outputTokens")) throw new Error("Invalid usage ledger measurement");
	if (!Object.hasOwn(item, "totalTokens")) throw new Error("Invalid usage ledger measurement");
	if (!Object.hasOwn(item, "cost")) throw new Error("Invalid usage ledger measurement");
	if (!Object.hasOwn(item, "wallMs")) throw new Error("Invalid usage ledger measurement");
	optionalFiniteNumber(item.inputTokens, "inputTokens");
	optionalFiniteNumber(item.outputTokens, "outputTokens");
	optionalFiniteNumber(item.totalTokens, "totalTokens");
	optionalFiniteNumber(item.cost, "cost");
	const wallMs = finiteNumber(item.wallMs, "wallMs");
	if (wallMs < 0) throw new Error("Invalid usage ledger wallMs");
	valueToCheck = item;
}

function validateReservation(valueToCheck: unknown): asserts valueToCheck is StoredReservation {
	const item = object(valueToCheck);
	if (typeof item.id !== "string" || !item.id) throw new Error("Invalid usage ledger reservation");
	if (typeof item.rootId !== "string" || !item.rootId) throw new Error("Invalid usage ledger reservation");
	if (!(["task", "model", "jev"] as string[]).includes(String(item.kind)))
		throw new Error("Invalid usage ledger reservation kind");
	if (item.taskId !== undefined && (typeof item.taskId !== "string" || !item.taskId))
		throw new Error("Invalid usage ledger task ID");
	if (item.parentTaskId !== undefined && (typeof item.parentTaskId !== "string" || !item.parentTaskId))
		throw new Error("Invalid usage ledger parent task ID");
	if (item.requestKey !== undefined && (typeof item.requestKey !== "string" || !item.requestKey))
		throw new Error("Invalid usage ledger request key");
	if (item.budgetId !== undefined && (typeof item.budgetId !== "string" || !item.budgetId))
		throw new Error("Invalid usage ledger budget ID");
	const admittedAt = finiteNumber(item.admittedAt, "admittedAt");
	if (admittedAt < 0) throw new Error("Invalid usage ledger admittedAt");
	optionalFiniteNumber(item.deadlineAt, "deadlineAt");
}

function validateCall(valueToCheck: unknown): asserts valueToCheck is StoredCall {
	const item = object(valueToCheck);
	if (typeof item.id !== "string" || !item.id || typeof item.reservationId !== "string" || !item.reservationId)
		throw new Error("Invalid usage ledger call");
	if (typeof item.rootId !== "string" || !item.rootId) throw new Error("Invalid usage ledger call root ID");
	if (!(["task", "model", "jev"] as string[]).includes(String(item.kind)))
		throw new Error("Invalid usage ledger call kind");
	if (item.requestKey !== undefined && (typeof item.requestKey !== "string" || !item.requestKey))
		throw new Error("Invalid usage ledger call request key");
	if (item.budgetId !== undefined && (typeof item.budgetId !== "string" || !item.budgetId))
		throw new Error("Invalid usage ledger call budget ID");
	for (const name of ["taskId", "parentTaskId"] as const)
		if (item[name] !== undefined && (typeof item[name] !== "string" || !item[name]))
			throw new Error("Invalid usage ledger call task ID");
	finiteNumber(item.admittedAt, "admittedAt");
	finiteNumber(item.settledAt, "settledAt");
	if (!(["succeeded", "failed", "cancelled", "unknown"] as string[]).includes(String(item.status)))
		throw new Error("Invalid usage ledger call status");
	validateMeasurement(item.usage);
}

function validateTotals(valueToCheck: unknown): void {
	const item = object(valueToCheck);
	for (const name of ["calls", "taskCalls", "modelCalls", "jevCalls", "unknownCalls"])
		finiteInteger(item[name], `history ${name}`);
	if (finiteNumber(item.wallMs, "history wallMs") < 0) throw new Error("Invalid usage ledger history wallMs");
	for (const name of ["inputTokens", "outputTokens", "totalTokens", "cost"]) {
		if (!Object.hasOwn(item, name)) throw new Error("Invalid usage ledger history totals");
		optionalFiniteNumber(item[name], `history ${name}`);
	}
}

function validateHistory(valueToCheck: unknown): void {
	const item = object(valueToCheck);
	finiteInteger(item.roots, "history roots");
	finiteInteger(item.importedRoots, "history importedRoots");
	optionalFiniteNumber(item.firstStartedAt, "history firstStartedAt");
	optionalFiniteNumber(item.lastSettledAt, "history lastSettledAt");
	validateTotals(item.usage);
	const byKind = object(item.byKind);
	for (const kind of ["task", "model", "jev"]) validateTotals(byKind[kind]);
	finiteNumber(item.spentUsd, "history spentUsd");
	finiteInteger(item.unknownPricedCalls, "history unknownPricedCalls");
	const admissions = object(item.taskAdmissions);
	for (const status of ["succeeded", "failed", "cancelled", "unknown"])
		finiteInteger(admissions[status], "history taskAdmissions");
}

function validateDocument(valueToCheck: JsonValue): StoredDocument {
	const document = object(valueToCheck);
	if (document.version !== USAGE_DOCUMENT_VERSION) throw new Error("Unsupported usage ledger version");
	if (document.history !== undefined) validateHistory(document.history);
	const roots = object(document.roots);
	for (const [rootId, valueToValidate] of Object.entries(roots)) {
		const root = object(valueToValidate);
		if (root.rootId !== rootId || typeof root.rootId !== "string" || !root.rootId)
			throw new Error("Invalid usage ledger root");
		optionalFiniteNumber(root.startedAt, "startedAt");
		optionalFiniteNumber(root.deadlineAt, "deadlineAt");
		if (!Array.isArray(root.reservations) || !Array.isArray(root.calls))
			throw new Error("Invalid usage ledger root records");
		if (root.turns !== undefined) {
			const turns = object(root.turns);
			finiteInteger(turns.count, "turn count");
			if (finiteNumber(turns.tokens, "turn tokens") < 0) throw new Error("Invalid usage ledger turn tokens");
			finiteNumber(turns.lastAt, "turn lastAt");
			if (turns.cost !== undefined && finiteNumber(turns.cost, "turn cost") < 0)
				throw new Error("Invalid usage ledger turn cost");
			if (turns.unpriced !== undefined) finiteInteger(turns.unpriced, "turn unpriced");
		}
		if (root.imported !== undefined) {
			const imported = object(root.imported);
			if (typeof imported.source !== "string" || !imported.source) throw new Error("Invalid usage ledger import");
			finiteInteger(imported.entries, "import entries");
		}
		const reservationIds = new Set<string>();
		for (const reservation of root.reservations) {
			validateReservation(reservation);
			if (reservationIds.has(reservation.id)) throw new Error("Duplicate usage ledger reservation");
			reservationIds.add(reservation.id);
		}
		const callIds = new Set<string>();
		for (const call of root.calls) {
			validateCall(call);
			if (callIds.has(call.id)) throw new Error("Duplicate usage ledger call");
			callIds.add(call.id);
		}
	}
	return document as unknown as StoredDocument;
}

function normalizeLimits(limits: NativeUsageLimits | undefined): Required<NativeUsageLimits> {
	const maxAdmittedTasks = limits?.maxAdmittedTasks;
	const maxWallMs = limits?.maxWallMs;
	const maxCostUsd = limits?.maxCostUsd;
	const maxTotalTokens = limits?.maxTotalTokens;
	const maxTotalTurns = limits?.maxTotalTurns;
	const defaultMaxTotalTokens = limits?.defaultMaxTotalTokens;
	if (defaultMaxTotalTokens !== undefined) finiteInteger(defaultMaxTotalTokens, "defaultMaxTotalTokens", 1);
	if (maxTotalTokens !== undefined) finiteInteger(maxTotalTokens, "maxTotalTokens", 1);
	if (maxTotalTurns !== undefined) finiteInteger(maxTotalTurns, "maxTotalTurns", 1);
	if (maxAdmittedTasks !== undefined) finiteInteger(maxAdmittedTasks, "maxAdmittedTasks", 0);
	if (maxWallMs !== undefined) finiteInteger(maxWallMs, "maxWallMs", 1);
	if (maxCostUsd !== undefined && finiteNumber(maxCostUsd, "maxCostUsd") < 0)
		throw new Error("Invalid usage ledger maxCostUsd");
	return {
		maxAdmittedTasks: maxAdmittedTasks ?? Number.POSITIVE_INFINITY,
		maxWallMs: maxWallMs ?? Number.POSITIVE_INFINITY,
		maxCostUsd: maxCostUsd ?? Number.POSITIVE_INFINITY,
		maxTotalTokens: maxTotalTokens ?? Number.POSITIVE_INFINITY,
		maxTotalTurns: maxTotalTurns ?? Number.POSITIVE_INFINITY,
		defaultMaxTotalTokens: defaultMaxTotalTokens ?? Number.POSITIVE_INFINITY,
	};
}

/** The token cap of `root`: the explicit one, else the default unless it is a goal root. */
function tokenCap(root: StoredRoot, limits: Required<NativeUsageLimits>): number {
	if (Number.isFinite(limits.maxTotalTokens)) return limits.maxTotalTokens;
	return root.rootId.startsWith(GOAL_USAGE_ROOT_PREFIX) ? Number.POSITIVE_INFINITY : limits.defaultMaxTotalTokens;
}

function publicLimits(limits: Required<NativeUsageLimits>): NativeUsageStatus["limits"] {
	return {
		maxAdmittedTasks: Number.isFinite(limits.maxAdmittedTasks) ? limits.maxAdmittedTasks : null,
		maxWallMs: Number.isFinite(limits.maxWallMs) ? limits.maxWallMs : null,
		maxCostUsd: Number.isFinite(limits.maxCostUsd) ? limits.maxCostUsd : null,
		maxTotalTokens: Number.isFinite(limits.maxTotalTokens) ? limits.maxTotalTokens : null,
		maxTotalTurns: Number.isFinite(limits.maxTotalTurns) ? limits.maxTotalTurns : null,
	};
}

function turnUsage(root: StoredRoot, limits: Required<NativeUsageLimits>): NativeUsageTurns {
	return {
		turns: root.turns?.count ?? 0,
		tokens: root.turns?.tokens ?? 0,
		maxTotalTurns: Number.isFinite(limits.maxTotalTurns) ? limits.maxTotalTurns : null,
		maxTotalTokens: Number.isFinite(tokenCap(root, limits)) ? tokenCap(root, limits) : null,
	};
}

/**
 * The error text for a root whose tree has used up its turn, token or cost limit, or undefined. Every limit counts
 * the root's whole tree: its own lane and every lane it admitted (sub-agents, frames, typed agents, jobs).
 */
function rootExhaustion(root: StoredRoot, limits: Required<NativeUsageLimits>): NativeUsageExhaustion | undefined {
	const count = root.turns?.count ?? 0;
	const tokens = root.turns?.tokens ?? 0;
	const cap = tokenCap(root, limits);
	const advice = "No further model work is admitted for this root; answer with what you have.";
	if (count >= limits.maxTotalTurns)
		return {
			kind: "turns",
			reason: `Usage turn limit reached for root ${root.rootId}: ${count} of ${limits.maxTotalTurns} model turns used (max_total_turns, ULTRON_MAX_TOTAL_TURNS). ${advice}`,
		};
	if (tokens >= cap)
		return {
			kind: "tokens",
			reason: `Usage token limit reached for root ${root.rootId}: ${tokens} of ${cap} tokens used (max_total_tokens, ULTRON_MAX_TOTAL_TOKENS). ${advice}`,
		};
	if (Number.isFinite(limits.maxCostUsd)) {
		const cost = spend(root, limits);
		if (cost.unknownPricedCalls > 0)
			return {
				kind: "cost",
				reason: `Usage pricing unknown; cannot enforce cost cap of $${limits.maxCostUsd} for root ${root.rootId} (${cost.unknownPricedCalls} model call(s) reported no cost). ${advice}`,
			};
		if (cost.spentUsd >= limits.maxCostUsd)
			return {
				kind: "cost",
				reason: `Usage cost cap reached for root ${root.rootId}: spent $${roundUsd(cost.spentUsd)} of $${limits.maxCostUsd} (ULTRON_MAX_COST_USD). ${advice}`,
			};
	}
	return undefined;
}

function roundUsd(amount: number): number {
	return Math.round(amount * 1e6) / 1e6;
}

/**
 * Model spend of a root's tree. A root that records responses (the worker records every assistant response on
 * every lane) is charged from that per-response tally, so a call is never counted twice: a sub-agent's or frame's
 * settled model call describes the same responses. Roots without the tally (imports, older documents, hosts that
 * do not record responses) fall back to their settled model calls.
 */
function spend(root: StoredRoot, limits?: Required<NativeUsageLimits>): NativeUsageCost {
	let spentUsd: number;
	let unknownPricedCalls: number;
	if (root.turns?.cost !== undefined) {
		spentUsd = root.turns.cost;
		unknownPricedCalls = root.turns.unpriced ?? 0;
	} else {
		const models = root.calls.filter((call) => call.kind === "model");
		spentUsd = models.reduce((total, call) => total + (call.usage.cost ?? 0), 0);
		unknownPricedCalls = models.filter((call) => call.usage.cost === null).length;
	}
	const maxCostUsd = limits && Number.isFinite(limits.maxCostUsd) ? limits.maxCostUsd : null;
	return {
		maxCostUsd,
		spentUsd,
		unknownPricedCalls,
		remainingUsd: maxCostUsd === null ? null : Math.max(0, maxCostUsd - spentUsd),
	};
}

function emptyDocument(): StoredDocument {
	return { version: USAGE_DOCUMENT_VERSION, roots: {} };
}

function emptyRoot(rootId: string): StoredRoot {
	return { rootId, startedAt: null, deadlineAt: null, reservations: [], calls: [] };
}

function cloneReservation(reservation: StoredReservation): NativeUsageReservation {
	return structuredClone(reservation);
}

function normalizeMeasurement(measurement: NativeUsageMeasurement | undefined, elapsedMs: number): StoredMeasurement {
	const valueOrNull = (valueToNormalize: number | null | undefined): number | null => valueToNormalize ?? null;
	const wallMs = measurement?.wallMs ?? elapsedMs;
	if (!Number.isFinite(wallMs) || wallMs < 0) throw new Error("Usage wallMs must be a non-negative finite number");
	for (const [name, valueToValidate] of [
		["inputTokens", measurement?.inputTokens],
		["outputTokens", measurement?.outputTokens],
		["totalTokens", measurement?.totalTokens],
		["cost", measurement?.cost],
	] as const) {
		if (valueToValidate !== undefined && valueToValidate !== null) finiteNumber(valueToValidate, name);
	}
	return {
		inputTokens: valueOrNull(measurement?.inputTokens),
		outputTokens: valueOrNull(measurement?.outputTokens),
		totalTokens: valueOrNull(measurement?.totalTokens),
		cost: valueOrNull(measurement?.cost),
		wallMs,
	};
}

function sumKnown(values: readonly (number | null)[]): number | null {
	if (values.some((valueToSum) => valueToSum === null)) return null;
	return values.reduce((total, valueToSum) => (total as number) + (valueToSum as number), 0);
}

function totals(calls: readonly StoredCall[]): NativeUsageTotals {
	const count = (kind: NativeUsageKind): number => calls.filter((call) => call.kind === kind).length;
	return {
		calls: calls.length,
		taskCalls: count("task"),
		modelCalls: count("model"),
		jevCalls: count("jev"),
		wallMs: calls.reduce((total, call) => total + call.usage.wallMs, 0),
		inputTokens: sumKnown(calls.map((call) => call.usage.inputTokens)),
		outputTokens: sumKnown(calls.map((call) => call.usage.outputTokens)),
		totalTokens: sumKnown(calls.map((call) => call.usage.totalTokens)),
		cost: sumKnown(calls.map((call) => call.usage.cost)),
		unknownCalls: calls.filter((call) =>
			[call.usage.inputTokens, call.usage.outputTokens, call.usage.totalTokens, call.usage.cost].some(
				(valueToCheck) => valueToCheck === null,
			),
		).length,
	};
}

function emptyTotals(): NativeUsageTotals {
	return totals([]);
}

/** Adds `source` into `target` with the same null rule as `totals`: any unknown makes the sum unknown. */
function mergeTotals(target: NativeUsageTotals, source: NativeUsageTotals): NativeUsageTotals {
	const known = (left: number | null, right: number | null): number | null =>
		left === null || right === null ? null : left + right;
	return {
		calls: target.calls + source.calls,
		taskCalls: target.taskCalls + source.taskCalls,
		modelCalls: target.modelCalls + source.modelCalls,
		jevCalls: target.jevCalls + source.jevCalls,
		wallMs: target.wallMs + source.wallMs,
		inputTokens: known(target.inputTokens, source.inputTokens),
		outputTokens: known(target.outputTokens, source.outputTokens),
		totalTokens: known(target.totalTokens, source.totalTokens),
		cost: known(target.cost, source.cost),
		unknownCalls: target.unknownCalls + source.unknownCalls,
	};
}

function emptyHistory(): NativeUsageHistory {
	return {
		roots: 0,
		firstStartedAt: null,
		lastSettledAt: null,
		usage: emptyTotals(),
		byKind: { task: emptyTotals(), model: emptyTotals(), jev: emptyTotals() },
		spentUsd: 0,
		unknownPricedCalls: 0,
		taskAdmissions: { succeeded: 0, failed: 0, cancelled: 0, unknown: 0 },
		importedRoots: 0,
	};
}

function foldRoot(history: NativeUsageHistory, root: StoredRoot): void {
	history.roots += 1;
	if (root.imported) history.importedRoots += 1;
	const started =
		root.startedAt ??
		root.calls.reduce<number | null>((min, call) => Math.min(min ?? call.admittedAt, call.admittedAt), null);
	if (started !== null) history.firstStartedAt = Math.min(history.firstStartedAt ?? started, started);
	for (const call of root.calls)
		history.lastSettledAt = Math.max(history.lastSettledAt ?? call.settledAt, call.settledAt);
	history.usage = mergeTotals(history.usage, totals(root.calls));
	for (const kind of ["task", "model", "jev"] as const)
		history.byKind[kind] = mergeTotals(history.byKind[kind], totals(root.calls.filter((call) => call.kind === kind)));
	const cost = spend(root);
	history.spentUsd += cost.spentUsd;
	history.unknownPricedCalls += cost.unknownPricedCalls;
	for (const call of root.calls) if (call.kind === "task") history.taskAdmissions[call.status] += 1;
}

function lastActivity(root: StoredRoot): number {
	let latest = root.startedAt ?? Number.NEGATIVE_INFINITY;
	for (const call of root.calls) latest = Math.max(latest, call.settledAt, call.admittedAt);
	for (const reservation of root.reservations) latest = Math.max(latest, reservation.admittedAt);
	if (root.turns) latest = Math.max(latest, root.turns.lastAt);
	return latest;
}

export class NativeUsageLedger implements NativeUsageLedgerLike {
	private readonly store: NativeUsageStore;
	private readonly limits: Required<NativeUsageLimits>;
	private readonly defaultRootId: string;
	private readonly now: () => number;
	private readonly detailedRoots: number;
	private readonly onRefused: NativeUsageLedgerOptions["onRefused"];
	private document: StoredDocument = emptyDocument();
	private loading?: Promise<void>;
	private tail: Promise<void> = Promise.resolve();
	private broken = false;

	constructor(store: NativeUsageStore = memoryStore(), options: NativeUsageLedgerOptions = {}) {
		if (options.detailedRoots !== undefined) finiteInteger(options.detailedRoots, "detailedRoots", 1);
		this.detailedRoots = options.detailedRoots ?? DEFAULT_USAGE_DETAILED_ROOTS;
		if (options.rootId !== undefined && (!options.rootId.trim() || options.rootId.includes("\0")))
			throw new Error("Usage rootId must be a nonempty string without NUL");
		this.store = store;
		this.limits = normalizeLimits(options.limits);
		this.defaultRootId = options.rootId ?? DEFAULT_ROOT_ID;
		this.now = options.now ?? Date.now;
		this.onRefused = options.onRefused;
	}

	/** A limit refusal, reported to `onRefused` before it is thrown. */
	private refused(message: string): Error {
		try {
			this.onRefused?.(message);
		} catch {
			// Observers never change the refusal.
		}
		return new Error(message);
	}

	private assertHealthy(): void {
		if (this.broken) throw new Error(USAGE_DURABILITY_ERROR);
	}

	private async load(): Promise<void> {
		let saved: JsonValue | undefined;
		try {
			saved = await this.store.read();
		} catch {
			throw new Error("Usage ledger could not be read");
		}
		this.document =
			saved === undefined ? emptyDocument() : validateDocument(readVersioned("ultron.usage/root", saved));
	}

	private ensureLoaded(): Promise<void> {
		this.loading ??= this.load();
		return this.loading;
	}

	/**
	 * Folds idle old roots into the history summary: a root is folded only when it has no active reservation, is
	 * not among the `detailedRoots` most recently active roots, and cannot admit again (its wall deadline passed,
	 * it has none, or it is an import). The default root is never folded, since callers without a root reuse it.
	 */
	private fold(document: StoredDocument): boolean {
		const roots = Object.values(document.roots);
		if (roots.length <= this.detailedRoots) return false;
		let folded = false;
		const now = this.now();
		const recent = new Set(
			roots
				.map((root) => ({ root, at: lastActivity(root) }))
				.sort((left, right) => right.at - left.at)
				.slice(0, this.detailedRoots)
				.map(({ root }) => root.rootId),
		);
		for (const root of roots) {
			if (recent.has(root.rootId) || root.rootId === this.defaultRootId || root.reservations.length > 0) continue;
			if (!root.imported && root.deadlineAt !== null && now < root.deadlineAt) continue;
			document.history ??= emptyHistory();
			foldRoot(document.history, root);
			delete document.roots[root.rootId];
			folded = true;
		}
		return folded;
	}

	private async write(document: StoredDocument): Promise<void> {
		this.fold(document);
		try {
			await this.store.write(structuredClone(document) as unknown as JsonValue);
		} catch {
			this.broken = true;
			throw new Error(USAGE_DURABILITY_ERROR);
		}
		this.document = document;
	}

	private enqueue<T>(change: () => Promise<T>): Promise<T> {
		const pending = this.tail.then(async () => {
			this.assertHealthy();
			await this.ensureLoaded();
			this.assertHealthy();
			return change();
		});
		this.tail = pending.then(
			() => {},
			() => {},
		);
		return pending;
	}

	async ready(): Promise<void> {
		await this.ensureLoaded();
		await this.tail;
		this.assertHealthy();
	}

	reserve(request: NativeUsageReservationRequest): Promise<NativeUsageReservation> {
		return this.enqueue(async () => {
			request.signal?.throwIfAborted();
			const rootId = request.rootId ?? this.defaultRootId;
			if (!rootId.trim() || rootId.includes("\0"))
				throw new Error("Usage rootId must be a nonempty string without NUL");
			if (this.document.roots[rootId]?.imported) throw new Error(`Usage root ${rootId} is an imported history root`);
			if (!(["task", "model", "jev"] as string[]).includes(request.kind)) throw new Error("Invalid usage kind");
			if (request.requestKey !== undefined && !request.requestKey.trim())
				throw new Error("Usage requestKey is empty");
			if (request.budgetId !== undefined && !request.budgetId.trim()) throw new Error("Usage budgetId is empty");
			if (request.timeoutMs !== undefined) finiteInteger(request.timeoutMs, "timeoutMs", 1);
			if (request.deadlineAt !== undefined) finiteNumber(request.deadlineAt, "deadlineAt");
			const root = this.document.roots[rootId] ?? emptyRoot(rootId);
			const existing = request.requestKey
				? root.reservations.find((reservation) => reservation.requestKey === request.requestKey)
				: undefined;
			if (existing) return cloneReservation(existing);
			const now = this.now();
			const goal = rootId.startsWith(GOAL_USAGE_ROOT_PREFIX);
			if (root.startedAt === null) {
				root.startedAt = now;
				root.deadlineAt = !goal && Number.isFinite(this.limits.maxWallMs) ? now + this.limits.maxWallMs : null;
			}
			if (root.deadlineAt !== null && now >= root.deadlineAt)
				throw this.refused(`Usage wall deadline exceeded for root ${rootId}`);
			if (
				!goal &&
				request.kind === "task" &&
				root.reservations.filter((reservation) => reservation.kind === "task").length >=
					this.limits.maxAdmittedTasks
			)
				throw this.refused(`Usage admitted-task limit exceeded for root ${rootId}`);
			const modelWork = request.kind === "model" || (request.kind === "task" && request.modelBacked !== false);
			const exhausted = modelWork ? rootExhaustion(root, this.limits)?.reason : undefined;
			if (exhausted) throw this.refused(exhausted);
			const asked = request.deadlineAt ?? (request.timeoutMs === undefined ? null : now + request.timeoutMs);
			// A child inherits whatever remains of the root's wall budget; a longer request is capped, not refused.
			const requestedDeadline =
				asked === null ? root.deadlineAt : root.deadlineAt === null ? asked : Math.min(asked, root.deadlineAt);
			if (requestedDeadline !== null && requestedDeadline <= now)
				throw this.refused(`Usage deadline has expired for root ${rootId}`);
			const reservation: StoredReservation = {
				id: `ultron-usage-${cryptoRandomUUID()}`,
				rootId,
				kind: request.kind,
				...(request.taskId === undefined ? {} : { taskId: request.taskId }),
				...(request.parentTaskId === undefined ? {} : { parentTaskId: request.parentTaskId }),
				...(request.requestKey === undefined ? {} : { requestKey: request.requestKey }),
				...(request.budgetId === undefined ? {} : { budgetId: request.budgetId }),
				admittedAt: now,
				deadlineAt: requestedDeadline,
			};
			root.reservations.push(reservation);
			this.document.roots[rootId] = root;
			await this.write(this.document);
			return cloneReservation(reservation);
		});
	}

	settle(reservation: NativeUsageReservation, settlement: NativeUsageSettlement = {}): Promise<void> {
		return this.enqueue(async () => {
			const root = this.document.roots[reservation.rootId];
			if (!root) throw new Error("Unknown usage root");
			const activeIndex = root.reservations.findIndex((item) => item.id === reservation.id);
			if (activeIndex === -1) {
				if (root.calls.some((call) => call.reservationId === reservation.id)) return;
				throw new Error("Unknown usage reservation");
			}
			const active = root.reservations[activeIndex]!;
			const now = this.now();
			const status = settlement.status ?? "unknown";
			if (!(["succeeded", "failed", "cancelled", "unknown"] as string[]).includes(status))
				throw new Error("Invalid usage settlement status");
			const call: StoredCall = {
				id: `ultron-usage-call-${cryptoRandomUUID()}`,
				reservationId: active.id,
				rootId: active.rootId,
				kind: active.kind,
				...(active.taskId === undefined ? {} : { taskId: active.taskId }),
				...(active.parentTaskId === undefined ? {} : { parentTaskId: active.parentTaskId }),
				...(active.requestKey === undefined ? {} : { requestKey: active.requestKey }),
				...(active.budgetId === undefined ? {} : { budgetId: active.budgetId }),
				admittedAt: active.admittedAt,
				settledAt: now,
				status,
				usage: normalizeMeasurement(settlement.usage, Math.max(0, now - active.admittedAt)),
			};
			root.reservations.splice(activeIndex, 1);
			root.calls.push(call);
			await this.write(this.document);
		});
	}

	async status(rootId = this.defaultRootId): Promise<NativeUsageStatus> {
		return this.enqueue(async () => {
			const root = this.document.roots[rootId] ?? emptyRoot(rootId);
			const now = this.now();
			return {
				rootId,
				limits: publicLimits(this.limits),
				cost: spend(root, this.limits),
				turns: turnUsage(root, this.limits),
				admittedTasks: root.reservations.filter((reservation) => reservation.kind === "task").length,
				activeReservations: root.reservations.length,
				startedAt: root.startedAt,
				deadlineAt: root.deadlineAt,
				remainingWallMs: root.deadlineAt === null ? null : Math.max(0, root.deadlineAt - now),
				usage: totals(root.calls),
				reservations: root.reservations.map(cloneReservation),
				session: this.sessionTotals(),
			};
		});
	}

	/** Whether a per-tree limit (turns, tokens or cost) is set, so each response is written through. */
	private get enforcesTree(): boolean {
		return (
			Number.isFinite(this.limits.maxTotalTurns) ||
			Number.isFinite(this.limits.maxTotalTokens) ||
			Number.isFinite(this.limits.defaultMaxTotalTokens) ||
			Number.isFinite(this.limits.maxCostUsd)
		);
	}

	/**
	 * Counts one model response (one provider turn) of `rootId` or any lane of its tree: its tokens and cost. The
	 * tally is always kept so `status` shows the tree's spend; it is written through only while a turn, token or
	 * cost limit is set, so sessions without one never write the ledger per response (it is saved with the next
	 * reservation or settlement instead).
	 */
	recordTurn(rootId: string | undefined, measurement: NativeUsageTurnMeasurement = {}): Promise<void> {
		return this.enqueue(async () => {
			const id = rootId ?? this.defaultRootId;
			const root = this.document.roots[id] ?? emptyRoot(id);
			if (root.imported) return;
			const known = (amount: number | null | undefined): amount is number =>
				typeof amount === "number" && Number.isFinite(amount) && amount >= 0;
			const turns = root.turns ?? { count: 0, tokens: 0, lastAt: 0 };
			root.turns = {
				count: turns.count + 1,
				tokens: turns.tokens + (known(measurement.totalTokens) ? measurement.totalTokens : 0),
				lastAt: this.now(),
				cost: (turns.cost ?? 0) + (known(measurement.cost) ? measurement.cost : 0),
				unpriced: (turns.unpriced ?? 0) + (known(measurement.cost) ? 0 : 1),
			};
			this.document.roots[id] = root;
			if (this.enforcesTree) await this.write(this.document);
		});
	}

	/**
	 * The limit error for `rootId` once its tree reached the turn, token or cost limit; undefined while work may
	 * continue.
	 */
	turnBudgetExhausted(rootId: string | undefined): Promise<string | undefined> {
		return this.budgetExhaustion(rootId).then((exhausted) => exhausted?.reason);
	}

	/** Which limit `rootId`'s tree reached, with its error text; undefined while work may continue. */
	budgetExhaustion(rootId: string | undefined): Promise<NativeUsageExhaustion | undefined> {
		if (!this.enforcesTree) return Promise.resolve(undefined);
		return this.enqueue(async () => {
			const root = this.document.roots[rootId ?? this.defaultRootId];
			return root ? rootExhaustion(root, this.limits) : undefined;
		});
	}

	/** What `rootId` has left under its turn and token caps (null where there is no cap). */
	remaining(rootId: string | undefined): Promise<NativeUsageRemaining> {
		return this.enqueue(async () => {
			const id = rootId ?? this.defaultRootId;
			const root = this.document.roots[id] ?? emptyRoot(id);
			const cap = tokenCap(root, this.limits);
			const turns = root.turns?.count ?? 0;
			const tokens = root.turns?.tokens ?? 0;
			return {
				turnsLeft: Number.isFinite(this.limits.maxTotalTurns)
					? Math.max(0, this.limits.maxTotalTurns - turns)
					: null,
				tokensLeft: Number.isFinite(cap) ? Math.max(0, cap - tokens) : null,
				tokenCap: Number.isFinite(cap) ? cap : null,
			};
		});
	}

	private sessionTotals(): NativeUsageSessionTotals {
		const history = this.document.history;
		let usage = history ? structuredClone(history.usage) : emptyTotals();
		let spentUsd = history?.spentUsd ?? 0;
		let unknownPricedCalls = history?.unknownPricedCalls ?? 0;
		const roots = Object.values(this.document.roots);
		for (const root of roots) {
			usage = mergeTotals(usage, totals(root.calls));
			const cost = spend(root, this.limits);
			spentUsd += cost.spentUsd;
			unknownPricedCalls += cost.unknownPricedCalls;
		}
		return { roots: roots.length, foldedRoots: history?.roots ?? 0, usage, spentUsd, unknownPricedCalls };
	}

	/**
	 * Adds a historical root for usage recorded before this ledger existed (e.g. a Pi import). Its calls count in
	 * session totals but it never admits work. Idempotent per `rootId`.
	 */
	importHistory(
		rootId: string,
		source: string,
		calls: ReadonlyArray<{ at: number; usage: NativeUsageMeasurement; status?: NativeUsageCallStatus }>,
	): Promise<void> {
		return this.enqueue(async () => {
			if (!rootId.trim() || rootId.includes("\0"))
				throw new Error("Usage rootId must be a nonempty string without NUL");
			if (this.document.roots[rootId]) return;
			const stored: StoredCall[] = calls.map((call, index) => ({
				id: `ultron-usage-call-${rootId}-${index}`,
				reservationId: `ultron-usage-${rootId}-${index}`,
				rootId,
				kind: "model",
				admittedAt: call.at,
				settledAt: call.at,
				status: call.status ?? "succeeded",
				usage: normalizeMeasurement(call.usage, 0),
			}));
			const startedAt = stored.reduce<number | null>(
				(min, call) => Math.min(min ?? call.admittedAt, call.admittedAt),
				null,
			);
			this.document.roots[rootId] = {
				rootId,
				startedAt,
				deadlineAt: startedAt,
				reservations: [],
				calls: stored,
				imported: { source, entries: stored.length },
			};
			await this.write(this.document);
		});
	}

	reconcile(activeTaskIds: readonly string[]): Promise<void> {
		return this.enqueue(async () => {
			const active = new Set(activeTaskIds);
			let changed = false;
			const now = this.now();
			for (const root of Object.values(this.document.roots)) {
				const abandoned = root.reservations.filter((reservation) => {
					if (reservation.kind === "task")
						return reservation.taskId === undefined || !active.has(reservation.taskId);
					return reservation.parentTaskId === undefined || !active.has(reservation.parentTaskId);
				});
				for (const reservation of abandoned) {
					const index = root.reservations.findIndex((item) => item.id === reservation.id);
					if (index === -1) continue;
					root.reservations.splice(index, 1);
					root.calls.push({
						id: `ultron-usage-call-${cryptoRandomUUID()}`,
						reservationId: reservation.id,
						rootId: reservation.rootId,
						kind: reservation.kind,
						...(reservation.taskId === undefined ? {} : { taskId: reservation.taskId }),
						...(reservation.parentTaskId === undefined ? {} : { parentTaskId: reservation.parentTaskId }),
						...(reservation.requestKey === undefined ? {} : { requestKey: reservation.requestKey }),
						admittedAt: reservation.admittedAt,
						settledAt: now,
						status: "unknown",
						usage: normalizeMeasurement(undefined, Math.max(0, now - reservation.admittedAt)),
					});
					changed = true;
				}
			}
			// Folding happens on every write; a restart with nothing to settle still compacts an oversized document.
			if (this.fold(this.document) || changed) await this.write(this.document);
		});
	}
}

export type NativeUsageLedgerOptions = {
	limits?: NativeUsageLimits;
	rootId?: string;
	now?: () => number;
	/** Told when a reservation is refused because a limit was reached (wall, admitted tasks, turns, tokens, cost). */
	onRefused?: (reason: string) => void;
	/** Most recently active roots kept call by call (default 50); older idle roots fold into the history summary. */
	detailedRoots?: number;
};

function cryptoRandomUUID(): string {
	return globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

export function createSessionUsageStore(session: Pick<Session, "getValue" | "setValue">): NativeUsageStore {
	const address = value<JsonValue>("ultron.usage", "root");
	return {
		read: async () => (await session.getValue(address, BACKGROUND_CONTEXT))?.value,
		write: (document) => session.setValue(address, document, BACKGROUND_CONTEXT),
	};
}

export function createSessionUsageLedger(
	session: Pick<Session, "getValue" | "setValue">,
	options: NativeUsageLedgerOptions = {},
): NativeUsageLedger {
	return new NativeUsageLedger(createSessionUsageStore(session), options);
}

export const DEFAULT_NATIVE_USAGE_ROOT_ID = DEFAULT_ROOT_ID;
export type NativeUsageContext = Context;
