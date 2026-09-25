import { randomUUID } from "node:crypto";
import { isJsonValue, type JsonValue } from "@earendil-works/chord";
import type { Context } from "@earendil-works/pi-agent-core";
import Type from "typebox";
import { Check } from "typebox/value";
import type { HostModuleRequest, HostModuleStore, NativeHostApi, NativeHostModule } from "./rlm/host-module.ts";
import type { NativeTask } from "./rlm/task-store.ts";

export interface ProgressModuleOptions {
	store: HostModuleStore;
	now?: () => number;
	/** A running task with no receipt newer than this is stalled. */
	stallAfterMs?: number;
	maxReceiptsPerTask?: number;
}

export type ProgressClassification = "progressing" | "busy" | "stalled" | "finished";

type Evidence = { kind: string; ref: string; sha256?: string };

type Receipt = {
	id: string;
	task_id: string;
	/** Taken from the calling lane, never from the payload. Null is the root agent. */
	reporter_task_id: string | null;
	at: number;
	summary: string;
	evidence: Evidence[];
	metrics?: Record<string, number>;
	/** Evidence keys that were unseen, or carried a changed hash, when this receipt was recorded. */
	new_evidence: string[];
};

type Assessment = {
	task_id: string;
	classification: ProgressClassification;
	task_state: NativeTask["state"];
	result_status?: string;
	reasons: string[];
	receipts: string[];
	stall_after_ms: number;
	assessed_at: number;
	verification: "unverified";
};

type ClaimDecision = {
	claim: "complete";
	decision: "pending" | "verified" | "unverified" | "rejected";
	reason: string;
	verifier?: string;
	verifier_task_id?: string;
	verifier_status?: string;
};

type Decision = {
	id: string;
	task_id: string;
	requester_task_id: string | null;
	at: number;
	assessment: { classification: ProgressClassification; reasons: string[]; receipts: string[] };
	budget?: { requested: JsonValue; decision: "denied"; reason: string };
	claim?: ClaimDecision;
};

type ProgressDocument = { version: 1; receipts: Receipt[]; decisions: Decision[] };

const DEFAULT_STALL_AFTER_MS = 10 * 60 * 1000;
const DEFAULT_MAX_RECEIPTS = 256;
const MAX_EVIDENCE = 32;
const MAX_METRICS = 32;
const MAX_SUMMARY = 4096;
const MAX_REF = 1024;
const DURABILITY_ERROR = "Progress store durability is uncertain; reopen the owner";
const DEFINITION_PATTERN = /^[a-z][a-z0-9-]*@[0-9]+$/;
const KIND_PATTERN = /^[a-z][a-z0-9_.-]{0,63}$/;
const SHA256_PATTERN = /^[a-f0-9]{64}$/;
const TERMINAL = new Set<NativeTask["state"]>(["completed", "failed", "cancelled", "interrupted"]);
const CLASSIFICATIONS = [
	Type.Literal("progressing"),
	Type.Literal("busy"),
	Type.Literal("stalled"),
	Type.Literal("finished"),
];

const evidenceSchema = Type.Object(
	{ kind: Type.String(), ref: Type.String(), sha256: Type.Optional(Type.String()) },
	{ additionalProperties: false },
);
const receiptSchema = Type.Object(
	{
		id: Type.String({ minLength: 1 }),
		task_id: Type.String({ minLength: 1 }),
		reporter_task_id: Type.Union([Type.String({ minLength: 1 }), Type.Null()]),
		at: Type.Number(),
		summary: Type.String(),
		evidence: Type.Array(evidenceSchema),
		metrics: Type.Optional(Type.Record(Type.String(), Type.Number())),
		new_evidence: Type.Array(Type.String()),
	},
	{ additionalProperties: false },
);
const decisionSchema = Type.Object(
	{
		id: Type.String({ minLength: 1 }),
		task_id: Type.String({ minLength: 1 }),
		requester_task_id: Type.Union([Type.String({ minLength: 1 }), Type.Null()]),
		at: Type.Number(),
		assessment: Type.Object(
			{
				classification: Type.Union(CLASSIFICATIONS),
				reasons: Type.Array(Type.String()),
				receipts: Type.Array(Type.String()),
			},
			{ additionalProperties: false },
		),
		budget: Type.Optional(
			Type.Object(
				{ requested: Type.Unknown(), decision: Type.Literal("denied"), reason: Type.String() },
				{ additionalProperties: false },
			),
		),
		claim: Type.Optional(
			Type.Object(
				{
					claim: Type.Literal("complete"),
					decision: Type.Union([
						Type.Literal("pending"),
						Type.Literal("verified"),
						Type.Literal("unverified"),
						Type.Literal("rejected"),
					]),
					reason: Type.String(),
					verifier: Type.Optional(Type.String()),
					verifier_task_id: Type.Optional(Type.String()),
					verifier_status: Type.Optional(Type.String()),
				},
				{ additionalProperties: false },
			),
		),
	},
	{ additionalProperties: false },
);
const documentSchema = Type.Object(
	{ version: Type.Literal(1), receipts: Type.Array(receiptSchema), decisions: Type.Array(decisionSchema) },
	{ additionalProperties: false },
);

function fields(payload: Record<string, unknown>, allowed: string[]): void {
	for (const key of Object.keys(payload)) {
		if (!allowed.includes(key)) throw new Error(`Unknown payload field: ${key}`);
	}
}

function taskIdField(value: unknown): string {
	if (typeof value !== "string" || !value.trim()) throw new Error("task_id must be a nonempty string");
	return value;
}

function plainObject(value: unknown, name: string): Record<string, unknown> {
	if (value === null || typeof value !== "object" || Array.isArray(value))
		throw new Error(`${name} must be a JSON object`);
	return value as Record<string, unknown>;
}

function evidenceList(value: unknown): Evidence[] {
	if (!Array.isArray(value) || value.length === 0 || value.length > MAX_EVIDENCE)
		throw new Error(`evidence must be a list of 1 to ${MAX_EVIDENCE} entries`);
	return value.map((item, index) => {
		const entry = plainObject(item, `evidence[${index}]`);
		fields(entry, ["kind", "ref", "sha256"]);
		if (typeof entry.kind !== "string" || !KIND_PATTERN.test(entry.kind))
			throw new Error(`evidence[${index}].kind must be a short lowercase identifier`);
		if (typeof entry.ref !== "string" || !entry.ref.trim() || entry.ref.length > MAX_REF)
			throw new Error(`evidence[${index}].ref must be a nonempty string of at most ${MAX_REF} characters`);
		if (entry.sha256 !== undefined && entry.sha256 !== null) {
			if (typeof entry.sha256 !== "string" || !SHA256_PATTERN.test(entry.sha256))
				throw new Error(`evidence[${index}].sha256 must be a lowercase SHA-256 hex string`);
			return { kind: entry.kind, ref: entry.ref, sha256: entry.sha256 };
		}
		return { kind: entry.kind, ref: entry.ref };
	});
}

function metricsField(value: unknown): Record<string, number> | undefined {
	if (value === undefined || value === null) return undefined;
	const metrics = plainObject(value, "metrics");
	const keys = Object.keys(metrics);
	if (keys.length > MAX_METRICS) throw new Error(`metrics may hold at most ${MAX_METRICS} entries`);
	for (const key of keys) {
		const metric = metrics[key];
		if (key.length === 0 || key.length > 64) throw new Error("metric names must be 1 to 64 characters");
		if (typeof metric !== "number" || !Number.isFinite(metric))
			throw new Error(`metric ${key} must be a finite number`);
	}
	return metrics as Record<string, number>;
}

function evidenceKey(entry: Evidence): string {
	return `${entry.kind}:${entry.ref}`;
}

function invalidDocument(): Error {
	return new Error("Invalid progress document");
}

/**
 * Evidence-based progress receipts and reassessment. Progress is inferred only from recorded
 * evidence and task state; this module can never raise a budget, and a completion claim is
 * verified only by a verifier task that the host ran and that returned `passed: true`.
 */
export function createProgressModule(options: ProgressModuleOptions): NativeHostModule {
	const stallAfterMs = options.stallAfterMs ?? DEFAULT_STALL_AFTER_MS;
	const maxReceipts = options.maxReceiptsPerTask ?? DEFAULT_MAX_RECEIPTS;
	if (!Number.isSafeInteger(stallAfterMs) || stallAfterMs < 1)
		throw new Error("stallAfterMs must be a positive integer");
	if (!Number.isSafeInteger(maxReceipts) || maxReceipts < 1)
		throw new Error("maxReceiptsPerTask must be a positive integer");
	let document: ProgressDocument = { version: 1, receipts: [], decisions: [] };
	let loading: Promise<void> | undefined;
	let tail: Promise<void> = Promise.resolve();
	let broken = false;
	let clock: () => number = options.now ?? Date.now;

	async function write(next: ProgressDocument): Promise<void> {
		try {
			await options.store.write(structuredClone(next) as unknown as JsonValue);
		} catch {
			broken = true;
			throw new Error(DURABILITY_ERROR);
		}
		document = next;
	}

	async function load(): Promise<void> {
		const saved = await options.store.read();
		if (saved === undefined) return;
		if (!isJsonValue(saved) || !Check(documentSchema, saved)) throw invalidDocument();
		const parsed = structuredClone(saved) as unknown as ProgressDocument;
		// A verifier that was still running when the owner ended produced no evidence of success.
		const interrupted = parsed.decisions.some((decision) => decision.claim?.decision === "pending");
		if (!interrupted) {
			document = parsed;
			return;
		}
		await write({
			...parsed,
			decisions: parsed.decisions.map((decision) =>
				decision.claim?.decision === "pending"
					? {
							...decision,
							claim: {
								...decision.claim,
								decision: "unverified",
								reason: "Owner ended before the verifier returned",
							},
						}
					: decision,
			),
		});
	}

	function enqueue<T>(change: () => Promise<T> | T): Promise<T> {
		const pending = tail.then(async () => {
			if (broken) throw new Error(DURABILITY_ERROR);
			loading ??= load();
			await loading;
			if (broken) throw new Error(DURABILITY_ERROR);
			return change();
		});
		tail = pending.then(
			() => {},
			() => {},
		);
		return pending;
	}

	async function findTask(host: NativeHostApi, taskId: string): Promise<{ task: NativeTask; all: NativeTask[] }> {
		const all = await host.tasks();
		const task = all.find((candidate) => candidate.id === taskId);
		if (!task) throw new Error("Unknown Ultron task");
		return { task, all };
	}

	/** The caller itself, the root agent, or any ancestor of the task may inspect and reassess it. */
	function assertOwnerOrAncestor(caller: string | null, task: NativeTask, all: NativeTask[]): void {
		if (caller === null || caller === task.id) return;
		const byId = new Map(all.map((candidate) => [candidate.id, candidate]));
		const seen = new Set<string>();
		for (let parent = task.parentId; parent !== undefined; parent = byId.get(parent)?.parentId) {
			if (parent === caller) return;
			if (seen.has(parent)) break;
			seen.add(parent);
		}
		throw new Error("Caller may only access its own progress or that of its descendants");
	}

	function assess(task: NativeTask): Assessment {
		const now = clock();
		const receipts = document.receipts.filter((receipt) => receipt.task_id === task.id);
		const base = {
			task_id: task.id,
			task_state: task.state,
			stall_after_ms: stallAfterMs,
			assessed_at: now,
			verification: "unverified" as const,
		};
		if (TERMINAL.has(task.state)) {
			return {
				...base,
				classification: "finished",
				...(task.result ? { result_status: task.result.status } : {}),
				reasons: [
					`Task is ${task.state}${task.result ? ` with result ${task.result.status}` : ""}`,
					"A terminal state is not verification; only a passing verifier can verify completion",
				],
				receipts: receipts.map((receipt) => receipt.id),
			};
		}
		const recent = receipts.filter((receipt) => now - receipt.at <= stallAfterMs);
		if (recent.length === 0) {
			const last = receipts.at(-1);
			return {
				...base,
				classification: "stalled",
				reasons: [
					last === undefined
						? `Task is ${task.state} and has never reported progress`
						: `Task is ${task.state} and its last receipt is ${now - last.at}ms old, beyond the ${stallAfterMs}ms window`,
				],
				receipts: last === undefined ? [] : [last.id],
			};
		}
		const novel = recent.filter((receipt) => receipt.new_evidence.length > 0);
		if (novel.length > 0) {
			return {
				...base,
				classification: "progressing",
				reasons: novel.map(
					(receipt) => `Receipt ${receipt.id} added new or changed evidence: ${receipt.new_evidence.join(", ")}`,
				),
				receipts: novel.map((receipt) => receipt.id),
			};
		}
		return {
			...base,
			classification: "busy",
			reasons: [
				`${recent.length} receipt(s) within ${stallAfterMs}ms repeated previously seen evidence; activity is not progress`,
			],
			receipts: recent.map((receipt) => receipt.id),
		};
	}

	async function report(request: HostModuleRequest, host: NativeHostApi): Promise<Receipt> {
		const { payload } = request;
		fields(payload, ["task_id", "summary", "evidence", "metrics"]);
		const caller = host.callerTaskId(request.caller);
		const taskId = payload.task_id === undefined || payload.task_id === null ? caller : taskIdField(payload.task_id);
		if (taskId === null) throw new Error("The root agent must name the task_id it reports for");
		if (typeof payload.summary !== "string" || !payload.summary.trim() || payload.summary.length > MAX_SUMMARY)
			throw new Error(`summary must be a nonempty string of at most ${MAX_SUMMARY} characters`);
		const summary = payload.summary;
		const evidence = evidenceList(payload.evidence);
		const metrics = metricsField(payload.metrics);
		const { task } = await findTask(host, taskId);
		if (task.id !== caller && (task.parentId ?? null) !== caller)
			throw new Error("Caller may only report progress for itself or its direct children");
		return enqueue(async () => {
			const previous = document.receipts.filter((receipt) => receipt.task_id === taskId);
			if (previous.length >= maxReceipts)
				throw new Error(`Task ${taskId} already has the maximum of ${maxReceipts} progress receipts`);
			const known = new Map<string, string | undefined>();
			for (const receipt of previous) {
				for (const entry of receipt.evidence) {
					const key = evidenceKey(entry);
					known.set(key, entry.sha256 ?? known.get(key));
				}
			}
			const fresh = new Set<string>();
			for (const entry of evidence) {
				const key = evidenceKey(entry);
				if (!known.has(key) || (entry.sha256 !== undefined && entry.sha256 !== known.get(key))) fresh.add(key);
			}
			const receipt: Receipt = {
				id: `progress-receipt-${randomUUID()}`,
				task_id: taskId,
				reporter_task_id: caller,
				at: clock(),
				summary,
				evidence,
				...(metrics === undefined ? {} : { metrics }),
				new_evidence: [...fresh],
			};
			await write({ ...document, receipts: [...document.receipts, receipt] });
			return structuredClone(receipt);
		});
	}

	async function recordDecision(decision: Decision): Promise<void> {
		await enqueue(() => write({ ...document, decisions: [...document.decisions, decision] }));
	}

	async function updateClaim(id: string, claim: ClaimDecision): Promise<Decision> {
		return enqueue(async () => {
			const decisions = document.decisions.map((decision) =>
				decision.id === id ? { ...decision, claim } : decision,
			);
			await write({ ...document, decisions });
			return structuredClone(decisions.find((decision) => decision.id === id)!);
		});
	}

	async function verify(
		host: NativeHostApi,
		task: NativeTask,
		verifier: string,
		input: JsonValue,
		caller: string | null,
		context: Context,
	): Promise<ClaimDecision> {
		const base = { claim: "complete" as const, verifier };
		let verifierTaskId: string;
		try {
			verifierTaskId = (await host.spawn({ definition: verifier, input }, caller, context)).id;
		} catch (error) {
			return {
				...base,
				decision: "unverified",
				reason: `Verifier could not be started: ${error instanceof Error ? error.message : String(error)}`,
			};
		}
		let result: Awaited<ReturnType<NativeHostApi["result"]>>;
		try {
			result = await host.result(verifierTaskId);
		} catch (error) {
			return {
				...base,
				verifier_task_id: verifierTaskId,
				decision: "unverified",
				reason: `Verifier result is unavailable: ${error instanceof Error ? error.message : String(error)}`,
			};
		}
		const withTask = { ...base, verifier_task_id: verifierTaskId, verifier_status: result.status };
		if (result.status !== "succeeded")
			return {
				...withTask,
				decision: "unverified",
				reason: `Verifier ${result.status}${result.error ? `: ${result.error}` : ""}`,
			};
		const value = result.value;
		const passed =
			value !== null && typeof value === "object" && !Array.isArray(value)
				? (value as { passed?: unknown }).passed
				: undefined;
		if (passed === true)
			return { ...withTask, decision: "verified", reason: `Verifier ${verifier} passed for task ${task.id}` };
		if (passed === false) return { ...withTask, decision: "unverified", reason: `Verifier ${verifier} did not pass` };
		return { ...withTask, decision: "unverified", reason: "Verifier output has no boolean passed field" };
	}

	async function reassess(request: HostModuleRequest, host: NativeHostApi): Promise<Decision> {
		const { payload } = request;
		fields(payload, ["task_id", "extend_budget", "claim", "verifier", "verifier_input"]);
		const taskId = taskIdField(payload.task_id);
		const extend = payload.extend_budget ?? undefined;
		if (extend !== undefined && typeof extend !== "boolean" && (typeof extend !== "object" || Array.isArray(extend)))
			throw new Error("extend_budget must be a boolean or a JSON object");
		const claim = payload.claim ?? undefined;
		if (claim !== undefined && claim !== "complete") throw new Error('claim must be "complete"');
		const verifier = payload.verifier ?? undefined;
		if (verifier !== undefined && (typeof verifier !== "string" || !DEFINITION_PATTERN.test(verifier)))
			throw new Error("verifier must be id@version");
		const verifierInput = payload.verifier_input ?? undefined;
		if ((verifier !== undefined || verifierInput !== undefined) && claim === undefined)
			throw new Error("verifier and verifier_input require claim");
		if (verifierInput !== undefined && !isJsonValue(verifierInput)) throw new Error("verifier_input must be JSON");
		const caller = host.callerTaskId(request.caller);
		const { task, all } = await findTask(host, taskId);
		assertOwnerOrAncestor(caller, task, all);

		const assessment = await enqueue(() => assess(task));
		const decision: Decision = {
			id: `progress-decision-${randomUUID()}`,
			task_id: taskId,
			requester_task_id: caller,
			at: clock(),
			assessment: {
				classification: assessment.classification,
				reasons: assessment.reasons,
				receipts: assessment.receipts,
			},
		};
		if (extend !== undefined && extend !== false) {
			decision.budget = {
				requested: extend as JsonValue,
				decision: "denied",
				reason: "Budgets are fixed at admission; progress reassessment cannot extend them",
			};
		}
		if (claim === undefined) {
			await recordDecision(decision);
			return structuredClone(decision);
		}
		if (task.state !== "completed" || task.result?.status !== "succeeded") {
			decision.claim = {
				claim: "complete",
				decision: "rejected",
				reason: `Task is ${task.state}; only a succeeded task can be claimed complete`,
			};
			await recordDecision(decision);
			return structuredClone(decision);
		}
		if (verifier === undefined) {
			decision.claim = {
				claim: "complete",
				decision: "unverified",
				reason: "No verifier was named; a task result alone is not verification",
			};
			await recordDecision(decision);
			return structuredClone(decision);
		}
		// Record the intent first so an owner crash mid-verification leaves an unverified trace.
		decision.claim = {
			claim: "complete",
			decision: "pending",
			reason: "Verifier running",
			verifier,
		};
		await recordDecision(decision);
		const input =
			verifierInput === undefined
				? ({
						task_id: task.id,
						result: task.result?.value ?? null,
						evidence: document.receipts
							.filter((receipt) => receipt.task_id === task.id)
							.flatMap((receipt) => receipt.evidence),
					} as JsonValue)
				: (verifierInput as JsonValue);
		const outcome = await verify(host, task, verifier, input, caller, request.context);
		return updateClaim(decision.id, outcome);
	}

	async function history(request: HostModuleRequest, host: NativeHostApi): Promise<unknown> {
		fields(request.payload, ["task_id"]);
		const taskId = taskIdField(request.payload.task_id);
		const { task, all } = await findTask(host, taskId);
		assertOwnerOrAncestor(host.callerTaskId(request.caller), task, all);
		return enqueue(() =>
			structuredClone({
				task_id: taskId,
				receipts: document.receipts.filter((receipt) => receipt.task_id === taskId),
				decisions: document.decisions.filter((decision) => decision.task_id === taskId),
			}),
		);
	}

	async function assessRequest(request: HostModuleRequest, host: NativeHostApi): Promise<Assessment> {
		fields(request.payload, ["task_id"]);
		const taskId = taskIdField(request.payload.task_id);
		const { task, all } = await findTask(host, taskId);
		assertOwnerOrAncestor(host.callerTaskId(request.caller), task, all);
		return enqueue(() => assess(task));
	}

	return {
		prefixes: ["progress."],
		async start(host) {
			if (options.now === undefined) clock = () => host.now();
			await enqueue(() => {});
		},
		async handle(request, host) {
			if (options.now === undefined) clock = () => host.now();
			switch (request.type) {
				case "progress.report":
					return report(request, host);
				case "progress.assess":
					return assessRequest(request, host);
				case "progress.reassess":
					return reassess(request, host);
				case "progress.history":
					return history(request, host);
				default:
					throw new Error(`Unknown progress request: ${request.type}`);
			}
		},
	};
}
