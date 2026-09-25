import type { JsonValue } from "@ultron/chord";

/**
 * Pure event reconstruction and accounting reconciliation (A24). It reads the durable documents
 * the host already writes (task journal, usage ledger, progress and schedule module documents) and
 * never a UI cache, so a disagreement here is a disagreement in the one logical record.
 */

type Json = Record<string, unknown>;

export type ReconciledUsage = {
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

export type ReconciledTask = {
	id: string;
	key: string;
	definition: string;
	state: string;
	resultStatus: string | null;
	parentId: string | null;
	/**
	 * Settlement of the task's admission reservation, or "active" while it is still reserved. "folded" means the
	 * admission's root was folded into the ledger's history summary, which still counts it by status.
	 */
	admission: "active" | "succeeded" | "failed" | "cancelled" | "unknown" | "missing" | "folded";
	admittedAt: number | null;
	settledAt: number | null;
	/** Every call attributed to the task: its admission plus its model calls. */
	usage: ReconciledUsage;
	/** Model calls only, i.e. provider-reported spend; admissions carry no provider measurement. */
	modelUsage: ReconciledUsage;
};

export type TimelineEvent = {
	at: number;
	kind:
		| "task.admitted"
		| "task.settled"
		| "model.call"
		| "jev.call"
		| "usage.unattributed"
		| "progress.receipt"
		| "progress.decision"
		| "goal.verification"
		| "schedule.fired";
	taskId: string | null;
	detail: string;
};

export type Reconciliation = {
	tasks: ReconciledTask[];
	/** Calls that belong to no journal task: Jev calls and admissions that never created a task. */
	unattributed: ReconciledUsage;
	/** Historical usage written by an import (e.g. a Pi session's reported usage); it has no journal tasks. */
	imported: ReconciledUsage;
	/** Sum of every task's usage plus unattributed and imported usage, computed from individual call records. */
	totals: ReconciledUsage;
	activeReservations: number;
	/** The reconciled root, or null when every root was read. */
	scope: string | null;
	/** Per detailed root: its own call totals and open reservations. */
	roots: Record<string, { usage: ReconciledUsage; activeReservations: number }>;
	/** Totals of roots folded into the ledger's history summary, or null if none were folded. */
	history: ReconciledUsage | null;
	/** `totals` plus `history`: everything the session ever spent. Equals `totals` when scoped to one root. */
	sessionTotals: ReconciledUsage;
	timeline: TimelineEvent[];
	discrepancies: string[];
};

export type ReconcileInput = {
	tasks: JsonValue | undefined;
	usage: JsonValue | undefined;
	progress?: JsonValue;
	schedules?: JsonValue;
	rootId?: string;
};

const EXPECTED_ADMISSION: Record<string, ReconciledTask["admission"]> = {
	admitted: "active",
	running: "active",
	completed: "succeeded",
	failed: "failed",
	cancelled: "cancelled",
	interrupted: "unknown",
};

function object(value: unknown, name: string): Json {
	if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error(`${name} is not an object`);
	return value as Json;
}

function list(value: unknown, name: string): Json[] {
	if (!Array.isArray(value)) throw new Error(`${name} is not an array`);
	return value.map((item, index) => object(item, `${name}[${index}]`));
}

function str(value: unknown): string | null {
	return typeof value === "string" ? value : null;
}

function num(value: unknown): number | null {
	return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function emptyUsage(): ReconciledUsage {
	return {
		calls: 0,
		taskCalls: 0,
		modelCalls: 0,
		jevCalls: 0,
		wallMs: 0,
		inputTokens: 0,
		outputTokens: 0,
		totalTokens: 0,
		cost: 0,
		unknownCalls: 0,
	};
}

function add(total: ReconciledUsage, call: Json): void {
	const usage = object(call.usage, "usage call measurement");
	total.calls += 1;
	if (call.kind === "task") total.taskCalls += 1;
	if (call.kind === "model") total.modelCalls += 1;
	if (call.kind === "jev") total.jevCalls += 1;
	total.wallMs += num(usage.wallMs) ?? 0;
	let unknown = false;
	for (const field of ["inputTokens", "outputTokens", "totalTokens", "cost"] as const) {
		const measured = num(usage[field]);
		if (measured === null) unknown = true;
		// Missing usage stays unresolved: one unknown call makes the sum unknown, never zero.
		total[field] = total[field] === null || measured === null ? null : total[field] + measured;
	}
	if (unknown) total.unknownCalls += 1;
}

function merge(target: ReconciledUsage, source: ReconciledUsage): void {
	target.calls += source.calls;
	target.taskCalls += source.taskCalls;
	target.modelCalls += source.modelCalls;
	target.jevCalls += source.jevCalls;
	target.wallMs += source.wallMs;
	target.unknownCalls += source.unknownCalls;
	for (const field of ["inputTokens", "outputTokens", "totalTokens", "cost"] as const)
		target[field] = target[field] === null || source[field] === null ? null : target[field] + source[field];
}

export function reconcileRecords(input: ReconcileInput): Reconciliation {
	const discrepancies: string[] = [];
	const timeline: TimelineEvent[] = [];
	const journal = input.tasks === undefined ? [] : list(object(input.tasks, "task journal").tasks, "tasks");
	const roots = input.usage === undefined ? {} : object(object(input.usage, "usage ledger").roots, "usage roots");
	const rootIds = input.rootId === undefined ? Object.keys(roots) : [input.rootId];
	const calls: Json[] = [];
	const reservations: Json[] = [];
	const perRoot: Reconciliation["roots"] = {};
	const imported = emptyUsage();
	for (const rootId of rootIds) {
		if (roots[rootId] === undefined) continue;
		const root = object(roots[rootId], `usage root ${rootId}`);
		const rootCalls = list(root.calls, "usage calls");
		const rootReservations = list(root.reservations, "usage reservations");
		const usage = emptyUsage();
		for (const call of rootCalls) add(usage, call);
		reservations.push(...rootReservations);
		if (root.imported !== undefined) merge(imported, usage);
		else calls.push(...rootCalls);
		perRoot[rootId] = { usage, activeReservations: rootReservations.length };
	}
	const storedHistory =
		input.usage === undefined || input.rootId !== undefined
			? undefined
			: (object(input.usage, "usage ledger").history as unknown);
	const history =
		storedHistory === undefined || storedHistory === null
			? null
			: historyUsage(object(object(storedHistory, "usage history").usage, "usage history totals"));
	// Folded task admissions by status; journal tasks without a detailed admission draw from these.
	const foldedAdmissions = new Map<string, number>(
		storedHistory === undefined || storedHistory === null
			? []
			: Object.entries(object(object(storedHistory, "usage history").taskAdmissions, "folded admissions")).map(
					([status, count]) => [status, num(count) ?? 0],
				),
	);

	const byId = new Map<string, ReconciledTask>();
	const byKey = new Map<string, ReconciledTask>();
	for (const stored of journal) {
		const result = stored.result === undefined ? undefined : object(stored.result, "task result");
		const task: ReconciledTask = {
			id: String(stored.id),
			key: String(stored.key),
			definition: String(stored.definition),
			state: String(stored.state),
			resultStatus: str(result?.status),
			parentId: str(stored.parentId),
			admission: "missing",
			admittedAt: null,
			settledAt: null,
			usage: emptyUsage(),
			modelUsage: emptyUsage(),
		};
		byId.set(task.id, task);
		byKey.set(task.key, task);
	}

	const unattributed = emptyUsage();
	const taskCalls = new Map<string, Json[]>();
	for (const call of calls) {
		const admittedAt = num(call.admittedAt) ?? 0;
		const settledAt = num(call.settledAt) ?? admittedAt;
		const status = String(call.status);
		if (call.kind === "task") {
			const task = byKey.get(String(call.requestKey));
			if (!task) {
				add(unattributed, call);
				timeline.push({
					at: settledAt,
					kind: "usage.unattributed",
					taskId: null,
					detail: `admission ${String(call.requestKey)} created no task (${status})`,
				});
				continue;
			}
			taskCalls.set(task.id, [...(taskCalls.get(task.id) ?? []), call]);
			add(task.usage, call);
			continue;
		}
		if (call.kind === "model") {
			const task = byId.get(String(call.taskId));
			if (!task) {
				discrepancies.push(`model call ${String(call.id)} names unknown task ${String(call.taskId)}`);
				add(unattributed, call);
				continue;
			}
			add(task.usage, call);
			add(task.modelUsage, call);
			timeline.push({ at: settledAt, kind: "model.call", taskId: task.id, detail: status });
			continue;
		}
		add(unattributed, call);
		timeline.push({ at: settledAt, kind: "jev.call", taskId: null, detail: status });
	}

	const activeByKey = new Map<string, Json[]>();
	for (const reservation of reservations) {
		if (reservation.kind !== "task") continue;
		const key = String(reservation.requestKey);
		activeByKey.set(key, [...(activeByKey.get(key) ?? []), reservation]);
	}

	for (const task of byId.values()) {
		const settled = taskCalls.get(task.id) ?? [];
		const active = activeByKey.get(task.key) ?? [];
		activeByKey.delete(task.key);
		if (settled.length + active.length > 1)
			discrepancies.push(`task ${task.id} has ${settled.length + active.length} admission records`);
		if (settled.length === 1) {
			task.admission = String(settled[0]!.status) as ReconciledTask["admission"];
			task.admittedAt = num(settled[0]!.admittedAt);
			task.settledAt = num(settled[0]!.settledAt);
		} else if (active.length === 1) {
			task.admission = "active";
			task.admittedAt = num(active[0]!.admittedAt);
		}
		const expected = EXPECTED_ADMISSION[task.state];
		if (task.admission === "missing" && expected !== undefined && (foldedAdmissions.get(expected) ?? 0) > 0) {
			foldedAdmissions.set(expected, foldedAdmissions.get(expected)! - 1);
			task.admission = "folded";
		} else if (task.admission !== expected)
			discrepancies.push(`task ${task.id} is ${task.state} but its admission is ${task.admission}`);
		if (task.admittedAt !== null)
			timeline.push({ at: task.admittedAt, kind: "task.admitted", taskId: task.id, detail: task.definition });
		if (task.settledAt !== null)
			timeline.push({
				at: task.settledAt,
				kind: "task.settled",
				taskId: task.id,
				detail: `${task.state}${task.resultStatus === null ? "" : `/${task.resultStatus}`}`,
			});
		if (task.parentId !== null && !byId.has(task.parentId))
			discrepancies.push(`task ${task.id} names unknown parent ${task.parentId}`);
	}
	for (const [key] of activeByKey) discrepancies.push(`active admission ${key} has no journal task`);

	if (input.progress !== undefined) {
		const progress = object(input.progress, "progress document");
		for (const receipt of list(progress.receipts, "progress receipts")) {
			const taskId = String(receipt.task_id);
			if (!byId.has(taskId)) discrepancies.push(`progress receipt ${String(receipt.id)} names unknown task`);
			timeline.push({ at: num(receipt.at) ?? 0, kind: "progress.receipt", taskId, detail: String(receipt.summary) });
		}
		for (const decision of list(progress.decisions, "progress decisions")) {
			const taskId = String(decision.task_id);
			if (!byId.has(taskId)) discrepancies.push(`progress decision ${String(decision.id)} names unknown task`);
			const claim = decision.claim === undefined ? undefined : object(decision.claim, "claim");
			if (claim?.verifier_task_id !== undefined) {
				const verifier = byId.get(String(claim.verifier_task_id));
				if (!verifier) discrepancies.push(`decision ${String(decision.id)} names an unknown verifier task`);
				else if (claim.verifier_status !== undefined && claim.verifier_status !== verifier.resultStatus)
					discrepancies.push(
						`decision ${String(decision.id)} records verifier ${String(claim.verifier_status)}, journal has ${String(verifier.resultStatus)}`,
					);
				if (claim.decision === "verified" && verifier?.resultStatus !== "succeeded")
					discrepancies.push(`decision ${String(decision.id)} is verified without a succeeded verifier`);
			}
			timeline.push({
				at: num(decision.at) ?? 0,
				kind: "progress.decision",
				taskId,
				detail: `${String(object(decision.assessment, "assessment").classification)}${claim ? `/${String(claim.decision)}` : ""}`,
			});
		}
	}

	if (input.schedules !== undefined) {
		const schedules = object(input.schedules, "schedule document");
		for (const goal of list(schedules.goals, "goals")) {
			for (const taskId of (goal.task_ids as unknown[]) ?? [])
				if (!byId.has(String(taskId))) discrepancies.push(`goal ${String(goal.id)} names unknown task ${taskId}`);
			if (goal.verification === null || goal.verification === undefined) continue;
			const verification = object(goal.verification, "goal verification");
			for (const check of list(verification.checks, "goal checks")) {
				if (check.task_id === null) continue;
				const task = byId.get(String(check.task_id));
				if (!task) discrepancies.push(`goal ${String(goal.id)} check names unknown task`);
				else if (check.status !== task.resultStatus)
					discrepancies.push(
						`goal ${String(goal.id)} check ${String(check.definition)} records ${String(check.status)}, journal has ${String(task.resultStatus)}`,
					);
			}
			if (verification.status === "achieved" && goal.state !== "achieved")
				discrepancies.push(`goal ${String(goal.id)} verified achieved but is ${String(goal.state)}`);
			timeline.push({
				at: num(verification.at) ?? 0,
				kind: "goal.verification",
				taskId: null,
				detail: `${String(goal.id)}:${String(verification.status)}`,
			});
		}
		for (const schedule of list(schedules.schedules, "schedules")) {
			if (schedule.last_task_id === null || schedule.last_task_id === undefined) continue;
			if (!byId.has(String(schedule.last_task_id)))
				discrepancies.push(`schedule ${String(schedule.id)} names unknown task`);
			timeline.push({
				at: num(schedule.last_fired_at) ?? 0,
				kind: "schedule.fired",
				taskId: String(schedule.last_task_id),
				detail: String(schedule.id),
			});
		}
	}

	const tasks = [...byId.values()];
	const totals = emptyUsage();
	for (const task of tasks) merge(totals, task.usage);
	merge(totals, unattributed);
	merge(totals, imported);
	const sessionTotals = structuredClone(totals);
	if (history) merge(sessionTotals, history);
	// Stable: equal timestamps keep journal/call order.
	timeline.sort((left, right) => left.at - right.at);
	return {
		tasks,
		unattributed,
		imported,
		totals,
		activeReservations: reservations.length,
		scope: input.rootId ?? null,
		roots: perRoot,
		history,
		sessionTotals,
		timeline,
		discrepancies,
	};
}

function historyUsage(stored: Json): ReconciledUsage {
	const usage = emptyUsage();
	for (const field of ["calls", "taskCalls", "modelCalls", "jevCalls", "wallMs", "unknownCalls"] as const)
		usage[field] = num(stored[field]) ?? 0;
	for (const field of ["inputTokens", "outputTokens", "totalTokens", "cost"] as const)
		usage[field] = num(stored[field]);
	return usage;
}

function usageDiscrepancies(label: string, expected: ReconciledUsage, actualValue: unknown): string[] {
	const actual = object(actualValue, `${label} totals`);
	const problems: string[] = [];
	for (const field of Object.keys(expected) as (keyof ReconciledUsage)[]) {
		const want = expected[field];
		const got = actual[field];
		const equal =
			typeof want === "number" && typeof got === "number"
				? Math.abs(want - got) <= 1e-9 * Math.max(1, Math.abs(want))
				: want === got;
		if (!equal) problems.push(`${label} ${field} ${String(got)} != records ${String(want)}`);
	}
	return problems;
}

/** Compare the `agents.status` inspection view with a reconciliation of the durable records. */
export function inspectionDiscrepancies(reconciliation: Reconciliation, status: unknown): string[] {
	const view = object(status, "agents.status");
	const discrepancies: string[] = [];
	const viewTasks = list(view.tasks, "agents.status tasks");
	if (viewTasks.length !== reconciliation.tasks.length)
		discrepancies.push(`inspection lists ${viewTasks.length} tasks, journal has ${reconciliation.tasks.length}`);
	const byId = new Map(reconciliation.tasks.map((task) => [task.id, task]));
	for (const viewed of viewTasks) {
		const task = byId.get(String(viewed.id));
		if (!task) {
			discrepancies.push(`inspection shows unknown task ${String(viewed.id)}`);
			continue;
		}
		const result = viewed.result === undefined ? undefined : object(viewed.result, "inspected result");
		if (viewed.state !== task.state) discrepancies.push(`task ${task.id} inspected ${String(viewed.state)}`);
		if ((str(result?.status) ?? null) !== task.resultStatus)
			discrepancies.push(`task ${task.id} inspected result ${String(result?.status)}`);
		if ((str(viewed.parentId) ?? null) !== task.parentId)
			discrepancies.push(`task ${task.id} inspected parent ${String(viewed.parentId)}`);
	}
	if (view.usage !== null && view.usage !== undefined) {
		const inspected = object(view.usage, "inspected usage");
		// `agents.status` shows one root; compare it with that root's records, not with every root's sum.
		const rootId = str(inspected.rootId);
		const scoped =
			rootId === null
				? { usage: reconciliation.totals, activeReservations: reconciliation.activeReservations }
				: (reconciliation.roots[rootId] ?? { usage: emptyUsage(), activeReservations: 0 });
		discrepancies.push(...usageDiscrepancies("inspected usage", scoped.usage, inspected.usage));
		if (inspected.activeReservations !== scoped.activeReservations)
			discrepancies.push(
				`inspected ${String(inspected.activeReservations)} active reservations, records have ${scoped.activeReservations}`,
			);
		// The session view (every root plus folded history) is comparable only with an unscoped reconciliation.
		if (inspected.session !== undefined && reconciliation.scope === null)
			discrepancies.push(
				...usageDiscrepancies(
					"inspected session usage",
					reconciliation.sessionTotals,
					object(inspected.session, "inspected session").usage,
				),
			);
	}
	return discrepancies;
}
