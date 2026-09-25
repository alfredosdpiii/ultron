import type { Context, JsonValue } from "@earendil-works/chord";
import { BACKGROUND_CONTEXT } from "@earendil-works/chord/context";
import { type Session, value } from "@earendil-works/pi-agent-core";

export type NativeUsageKind = "task" | "model" | "jev";
export type NativeUsageCallStatus = "succeeded" | "failed" | "cancelled" | "unknown";

export type NativeUsageLimits = {
	/** Unfinished tasks admitted at once under one root. */
	maxAdmittedTasks?: number;
	/** Wall budget of one root, counted from its first reservation. */
	maxWallMs?: number;
	/** Optional spend cap per root, from provider-reported model cost. Unset means no cap. */
	maxCostUsd?: number;
};

/** Worker defaults: 24 unfinished tasks and a 30-minute wall budget per root turn; no cost cap. */
export const DEFAULT_NATIVE_USAGE_LIMITS = { maxAdmittedTasks: 24, maxWallMs: 30 * 60 * 1000 } as const;

/**
 * Limits from `ULTRON_MAX_WALL_MS`, `ULTRON_MAX_ADMITTED_TASKS` and `ULTRON_MAX_COST_USD`. A missing or
 * invalid value keeps the default; `none`, `off` or `unlimited` removes the wall or admission limit.
 * The cost cap is off unless set to a nonnegative number (an optional leading `$` is accepted).
 */
export function nativeUsageLimitsFromEnv(env: Record<string, string | undefined> = process.env): NativeUsageLimits {
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
	};
	/** Settled model spend of this root against the optional cap. */
	cost: NativeUsageCost;
	admittedTasks: number;
	activeReservations: number;
	startedAt: number | null;
	deadlineAt: number | null;
	remainingWallMs: number | null;
	usage: NativeUsageTotals;
	reservations: NativeUsageReservation[];
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
};

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
};

type StoredDocument = {
	version: 1;
	roots: Record<string, StoredRoot>;
};

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
	for (const name of ["taskId", "parentTaskId"] as const)
		if (item[name] !== undefined && (typeof item[name] !== "string" || !item[name]))
			throw new Error("Invalid usage ledger call task ID");
	finiteNumber(item.admittedAt, "admittedAt");
	finiteNumber(item.settledAt, "settledAt");
	if (!(["succeeded", "failed", "cancelled", "unknown"] as string[]).includes(String(item.status)))
		throw new Error("Invalid usage ledger call status");
	validateMeasurement(item.usage);
}

function validateDocument(valueToCheck: JsonValue): StoredDocument {
	const document = object(valueToCheck);
	if (document.version !== USAGE_DOCUMENT_VERSION) throw new Error("Unsupported usage ledger version");
	const roots = object(document.roots);
	for (const [rootId, valueToValidate] of Object.entries(roots)) {
		const root = object(valueToValidate);
		if (root.rootId !== rootId || typeof root.rootId !== "string" || !root.rootId)
			throw new Error("Invalid usage ledger root");
		optionalFiniteNumber(root.startedAt, "startedAt");
		optionalFiniteNumber(root.deadlineAt, "deadlineAt");
		if (!Array.isArray(root.reservations) || !Array.isArray(root.calls))
			throw new Error("Invalid usage ledger root records");
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
	if (maxAdmittedTasks !== undefined) finiteInteger(maxAdmittedTasks, "maxAdmittedTasks", 0);
	if (maxWallMs !== undefined) finiteInteger(maxWallMs, "maxWallMs", 1);
	if (maxCostUsd !== undefined && finiteNumber(maxCostUsd, "maxCostUsd") < 0)
		throw new Error("Invalid usage ledger maxCostUsd");
	return {
		maxAdmittedTasks: maxAdmittedTasks ?? Number.POSITIVE_INFINITY,
		maxWallMs: maxWallMs ?? Number.POSITIVE_INFINITY,
		maxCostUsd: maxCostUsd ?? Number.POSITIVE_INFINITY,
	};
}

function publicLimits(limits: Required<NativeUsageLimits>): NativeUsageStatus["limits"] {
	return {
		maxAdmittedTasks: Number.isFinite(limits.maxAdmittedTasks) ? limits.maxAdmittedTasks : null,
		maxWallMs: Number.isFinite(limits.maxWallMs) ? limits.maxWallMs : null,
		maxCostUsd: Number.isFinite(limits.maxCostUsd) ? limits.maxCostUsd : null,
	};
}

function spend(root: StoredRoot, limits: Required<NativeUsageLimits>): NativeUsageCost {
	const models = root.calls.filter((call) => call.kind === "model");
	const spentUsd = models.reduce((total, call) => total + (call.usage.cost ?? 0), 0);
	const maxCostUsd = Number.isFinite(limits.maxCostUsd) ? limits.maxCostUsd : null;
	return {
		maxCostUsd,
		spentUsd,
		unknownPricedCalls: models.filter((call) => call.usage.cost === null).length,
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

export class NativeUsageLedger implements NativeUsageLedgerLike {
	private readonly store: NativeUsageStore;
	private readonly limits: Required<NativeUsageLimits>;
	private readonly defaultRootId: string;
	private readonly now: () => number;
	private document: StoredDocument = emptyDocument();
	private loading?: Promise<void>;
	private tail: Promise<void> = Promise.resolve();
	private broken = false;

	constructor(
		store: NativeUsageStore = memoryStore(),
		options: { limits?: NativeUsageLimits; rootId?: string; now?: () => number } = {},
	) {
		if (options.rootId !== undefined && (!options.rootId.trim() || options.rootId.includes("\0")))
			throw new Error("Usage rootId must be a nonempty string without NUL");
		this.store = store;
		this.limits = normalizeLimits(options.limits);
		this.defaultRootId = options.rootId ?? DEFAULT_ROOT_ID;
		this.now = options.now ?? Date.now;
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
		this.document = saved === undefined ? emptyDocument() : validateDocument(saved);
	}

	private ensureLoaded(): Promise<void> {
		this.loading ??= this.load();
		return this.loading;
	}

	private async write(document: StoredDocument): Promise<void> {
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
			if (!(["task", "model", "jev"] as string[]).includes(request.kind)) throw new Error("Invalid usage kind");
			if (request.requestKey !== undefined && !request.requestKey.trim())
				throw new Error("Usage requestKey is empty");
			if (request.timeoutMs !== undefined) finiteInteger(request.timeoutMs, "timeoutMs", 1);
			if (request.deadlineAt !== undefined) finiteNumber(request.deadlineAt, "deadlineAt");
			const root = this.document.roots[rootId] ?? emptyRoot(rootId);
			const existing = request.requestKey
				? root.reservations.find((reservation) => reservation.requestKey === request.requestKey)
				: undefined;
			if (existing) return cloneReservation(existing);
			const now = this.now();
			if (root.startedAt === null) {
				root.startedAt = now;
				root.deadlineAt = Number.isFinite(this.limits.maxWallMs) ? now + this.limits.maxWallMs : null;
			}
			if (root.deadlineAt !== null && now >= root.deadlineAt)
				throw new Error(`Usage wall deadline exceeded for root ${rootId}`);
			if (
				request.kind === "task" &&
				root.reservations.filter((reservation) => reservation.kind === "task").length >=
					this.limits.maxAdmittedTasks
			)
				throw new Error(`Usage admitted-task limit exceeded for root ${rootId}`);
			if (
				Number.isFinite(this.limits.maxCostUsd) &&
				(request.kind === "model" || (request.kind === "task" && request.modelBacked !== false))
			) {
				const cost = spend(root, this.limits);
				if (cost.unknownPricedCalls > 0)
					throw new Error(
						`Usage pricing unknown; cannot enforce cost cap of $${this.limits.maxCostUsd} for root ${rootId} (${cost.unknownPricedCalls} model call(s) reported no cost)`,
					);
				if (cost.spentUsd >= this.limits.maxCostUsd)
					throw new Error(
						`Usage cost cap reached for root ${rootId}: spent $${cost.spentUsd} of $${this.limits.maxCostUsd}`,
					);
			}
			const asked = request.deadlineAt ?? (request.timeoutMs === undefined ? null : now + request.timeoutMs);
			// A child inherits whatever remains of the root's wall budget; a longer request is capped, not refused.
			const requestedDeadline =
				asked === null ? root.deadlineAt : root.deadlineAt === null ? asked : Math.min(asked, root.deadlineAt);
			if (requestedDeadline !== null && requestedDeadline <= now)
				throw new Error(`Usage deadline has expired for root ${rootId}`);
			const reservation: StoredReservation = {
				id: `ultron-usage-${cryptoRandomUUID()}`,
				rootId,
				kind: request.kind,
				...(request.taskId === undefined ? {} : { taskId: request.taskId }),
				...(request.parentTaskId === undefined ? {} : { parentTaskId: request.parentTaskId }),
				...(request.requestKey === undefined ? {} : { requestKey: request.requestKey }),
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
				admittedTasks: root.reservations.filter((reservation) => reservation.kind === "task").length,
				activeReservations: root.reservations.length,
				startedAt: root.startedAt,
				deadlineAt: root.deadlineAt,
				remainingWallMs: root.deadlineAt === null ? null : Math.max(0, root.deadlineAt - now),
				usage: totals(root.calls),
				reservations: root.reservations.map(cloneReservation),
			};
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
			if (changed) await this.write(this.document);
		});
	}
}

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
	options: { limits?: NativeUsageLimits; rootId?: string; now?: () => number } = {},
): NativeUsageLedger {
	return new NativeUsageLedger(createSessionUsageStore(session), options);
}

export const DEFAULT_NATIVE_USAGE_ROOT_ID = DEFAULT_ROOT_ID;
export type NativeUsageContext = Context;
